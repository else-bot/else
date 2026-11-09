//! Agent Skills fetched from GitHub. A skill is a directory holding a
//! `SKILL.md` whose frontmatter names it, plus any reference files and
//! scripts. The API pins the requested ref to a commit and fetches every
//! file itself, so the owner approves exactly the bytes that get installed.

use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::{StreamExt as _, TryStreamExt as _};
use reqwest::Url;
use serde::{Deserialize, Serialize};

pub const MAX_FILES: usize = 64;
pub const MAX_FILE_BYTES: usize = 256 * 1024;
pub const MAX_TOTAL_BYTES: usize = 1024 * 1024;
pub const MAX_ID: usize = 34;
pub const MAX_PATH: usize = 255;
const MAX_DESCRIPTION: usize = 1024;
const SKILL_FILE: &str = "SKILL.md";

/// Where skills are fetched from: GitHub's REST API and raw file host.
/// Tests point these at a local server.
#[derive(Debug, Clone)]
pub struct Sources {
    pub api: Url,
    pub raw: Url,
}

impl Default for Sources {
    fn default() -> Self {
        Self {
            api: Url::parse("https://api.github.com/").expect("static URL"),
            raw: Url::parse("https://raw.githubusercontent.com/").expect("static URL"),
        }
    }
}

/// Where a skill came from, pinned to the commit its files were read at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub url: String,
    pub repo: String,
    pub path: String,
    pub commit: String,
}

/// A fetched skill, ready for the owner's approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Package {
    pub id: String,
    pub name: String,
    pub description: String,
    pub source: Source,
    /// Every file by its path relative to the skill directory.
    pub files: BTreeMap<String, String>,
}

/// One file as cards and lists show it, without its content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileInfo {
    pub path: String,
    pub size: usize,
    pub script: bool,
}

pub fn file_infos(files: &BTreeMap<String, String>) -> Vec<FileInfo> {
    files
        .iter()
        .map(|(path, content)| FileInfo {
            path: path.clone(),
            size: content.len(),
            script: is_script(path, content),
        })
        .collect()
}

/// Whether a file looks like code a bot would run rather than read.
pub fn is_script(path: &str, content: &str) -> bool {
    const EXTENSIONS: &[&str] = &[
        ".sh", ".bash", ".py", ".js", ".mjs", ".ts", ".rb", ".pl", ".ps1",
    ];
    path.starts_with("scripts/")
        || EXTENSIONS.iter().any(|ext| path.ends_with(ext))
        || content.starts_with("#!")
}

/// The task secret holding a skill's keys.
pub fn secret_name(id: &str) -> String {
    format!("skill-{id}")
}

pub fn valid_id(id: &str) -> bool {
    (1..=MAX_ID).contains(&id.len())
        && id.starts_with(|c: char| c.is_ascii_lowercase())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FetchError {
    #[error("{0}")]
    Invalid(String),
    #[error("GitHub has no {0}")]
    NotFound(String),
    #[error("GitHub could not be reached: {0}")]
    Unavailable(String),
}

/// A GitHub location: `owner/repo`, a ref, and a directory inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Location {
    owner: String,
    repo: String,
    reference: String,
    path: String,
}

fn parse_url(input: &str) -> Result<Location, FetchError> {
    let invalid = || {
        FetchError::Invalid(
            "give a github.com link to a skill directory, such as \
             https://github.com/owner/repo/tree/main/skills/name"
                .into(),
        )
    };
    let url = Url::parse(input.trim()).map_err(|_| invalid())?;
    if url.scheme() != "https" || !matches!(url.host_str(), Some("github.com" | "www.github.com")) {
        return Err(invalid());
    }
    let segments: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let name = |s: &str| {
        !s.is_empty()
            && s.len() <= 100
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            && s != "."
            && s != ".."
    };
    let (owner, repo) = match segments.as_slice() {
        [owner, repo, ..] if name(owner) && name(repo) => (*owner, repo.trim_end_matches(".git")),
        _ => return Err(invalid()),
    };
    let (reference, mut path) = match &segments[2..] {
        [] => ("HEAD".to_owned(), Vec::new()),
        ["tree" | "blob", reference, rest @ ..] => ((*reference).to_owned(), rest.to_vec()),
        _ => return Err(invalid()),
    };
    if path.last() == Some(&SKILL_FILE) {
        path.pop();
    }
    let mut decoded = Vec::new();
    for segment in path {
        let segment = percent_encoding::percent_decode_str(segment)
            .decode_utf8()
            .map_err(|_| invalid())?;
        if segment == "." || segment == ".." || segment.contains(['\\', '\0']) {
            return Err(invalid());
        }
        decoded.push(segment.into_owned());
    }
    Ok(Location {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        reference,
        path: decoded.join("/"),
    })
}

/// The `name` and `description` of a SKILL.md frontmatter block.
fn frontmatter(text: &str) -> Result<(String, String), FetchError> {
    let invalid = |why: &str| FetchError::Invalid(format!("SKILL.md {why}"));
    let body = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
        .ok_or_else(|| invalid("must start with a --- frontmatter block"))?;
    let end = body
        .lines()
        .position(|line| line.trim_end() == "---")
        .ok_or_else(|| invalid("frontmatter has no closing ---"))?;
    let lines: Vec<&str> = body.lines().take(end).collect();
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in lines {
        let continuation = line.starts_with([' ', '\t']);
        if continuation {
            if let Some(key) = &current {
                let value = fields.entry(key.clone()).or_default();
                if !value.is_empty() {
                    value.push(' ');
                }
                value.push_str(line.trim());
            }
            continue;
        }
        current = None;
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_owned();
        let value = value.trim();
        let value = if matches!(value, ">" | "|" | ">-" | "|-") {
            ""
        } else {
            value
        };
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        fields.insert(key.clone(), value.to_owned());
        current = Some(key);
    }
    let name = fields
        .remove("name")
        .filter(|n| !n.is_empty())
        .ok_or_else(|| invalid("frontmatter needs a name"))?;
    let description = fields
        .remove("description")
        .filter(|d| !d.is_empty())
        .ok_or_else(|| invalid("frontmatter needs a description"))?;
    if !valid_id(&name) {
        return Err(invalid(&format!(
            "name must be 1 to {MAX_ID} of a-z, 0-9 and -, starting with a letter"
        )));
    }
    if description.len() > MAX_DESCRIPTION {
        return Err(invalid("description is longer than 1024 bytes"));
    }
    Ok((name, description))
}

#[derive(Deserialize)]
struct Tree {
    tree: Vec<Entry>,
    #[serde(default)]
    truncated: bool,
}

#[derive(Deserialize)]
struct Entry {
    path: String,
    mode: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    size: Option<usize>,
}

fn client() -> Result<reqwest::Client, FetchError> {
    reqwest::Client::builder()
        .user_agent(concat!("else.bot/", env!("ELSE_VERSION")))
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()
        .map_err(|e| FetchError::Unavailable(e.to_string()))
}

fn join(base: &Url, segments: &[&str]) -> Url {
    let mut url = base.clone();
    url.path_segments_mut()
        .expect("http base URL")
        .pop_if_empty()
        .extend(segments);
    url
}

async fn get(
    client: &reqwest::Client,
    url: Url,
    accept: &str,
    what: &str,
) -> Result<reqwest::Response, FetchError> {
    let response = client
        .get(url)
        .header(reqwest::header::ACCEPT, accept)
        .send()
        .await
        .map_err(|e| FetchError::Unavailable(e.without_url().to_string()))?;
    match response.status() {
        s if s.is_success() => Ok(response),
        reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::UNPROCESSABLE_ENTITY => {
            Err(FetchError::NotFound(what.to_owned()))
        }
        s => Err(FetchError::Unavailable(format!("{what} returned HTTP {s}"))),
    }
}

/// Read a bounded body; `limit` bytes or fewer.
async fn bounded(
    response: reqwest::Response,
    limit: usize,
    what: &str,
) -> Result<Vec<u8>, FetchError> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| FetchError::Unavailable(e.without_url().to_string()))?;
        if body.len() + chunk.len() > limit {
            return Err(FetchError::Invalid(format!(
                "{what} is larger than {limit} bytes"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Fetch the skill at a github.com link, pinned to the commit its ref names now.
pub async fn fetch(sources: &Sources, url: &str) -> Result<Package, FetchError> {
    let at = parse_url(url)?;
    let client = client()?;
    let repo = format!("{}/{}", at.owner, at.repo);
    let commit = get(
        &client,
        join(
            &sources.api,
            &["repos", &at.owner, &at.repo, "commits", &at.reference],
        ),
        "application/vnd.github.sha",
        &format!("ref {} in {repo}", at.reference),
    )
    .await?;
    let commit = String::from_utf8(bounded(commit, 128, "the commit id").await?)
        .unwrap_or_default()
        .trim()
        .to_owned();
    if commit.len() != 40 || !commit.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(FetchError::Unavailable(
            "GitHub returned no commit id".into(),
        ));
    }
    let mut tree_url = join(
        &sources.api,
        &["repos", &at.owner, &at.repo, "git", "trees", &commit],
    );
    tree_url.set_query(Some("recursive=1"));
    let tree = get(
        &client,
        tree_url,
        "application/vnd.github+json",
        &format!("tree for {repo}"),
    )
    .await?;
    let tree: Tree =
        serde_json::from_slice(&bounded(tree, 16 * 1024 * 1024, "the repository tree").await?)
            .map_err(|_| FetchError::Unavailable("GitHub returned an unreadable tree".into()))?;
    if tree.truncated {
        return Err(FetchError::Invalid(
            "GitHub truncated the repository listing, so the skill's files cannot be listed \
             completely; link a smaller repository"
                .into(),
        ));
    }
    let prefix = if at.path.is_empty() {
        String::new()
    } else {
        format!("{}/", at.path)
    };
    let mut wanted = Vec::new();
    for entry in &tree.tree {
        let Some(relative) = entry.path.strip_prefix(&prefix) else {
            continue;
        };
        match (entry.kind.as_str(), entry.mode.as_str()) {
            ("blob", "120000") => {
                return Err(FetchError::Invalid(format!(
                    "{relative} is a symlink; skills must hold plain files"
                )));
            }
            ("blob", _) => {}
            _ => continue,
        }
        if relative.split('/').any(|s| s.starts_with('.')) {
            continue;
        }
        if relative.len() > MAX_PATH {
            return Err(FetchError::Invalid(format!(
                "a file path is longer than {MAX_PATH} bytes: {}…",
                &relative[..relative.floor_char_boundary(60)]
            )));
        }
        if entry.size.unwrap_or(0) > MAX_FILE_BYTES {
            return Err(FetchError::Invalid(format!(
                "{relative} is larger than {MAX_FILE_BYTES} bytes"
            )));
        }
        wanted.push(relative.to_owned());
    }
    if !wanted.iter().any(|p| p == SKILL_FILE) {
        return Err(FetchError::NotFound(format!(
            "SKILL.md in {repo}{}",
            if at.path.is_empty() {
                String::new()
            } else {
                format!("/{}", at.path)
            }
        )));
    }
    if wanted.len() > MAX_FILES {
        return Err(FetchError::Invalid(format!(
            "the skill has {} files; the limit is {MAX_FILES}",
            wanted.len()
        )));
    }
    let files: Vec<(String, String)> = futures_util::stream::iter(wanted.into_iter().map(|path| {
        let client = &client;
        let full = format!("{prefix}{path}");
        let mut segments = vec![at.owner.as_str(), at.repo.as_str(), commit.as_str()];
        segments.extend(full.split('/'));
        let url = join(&sources.raw, &segments);
        async move {
            let response = get(client, url, "*/*", &path).await?;
            let bytes = bounded(response, MAX_FILE_BYTES, &path).await?;
            let text = String::from_utf8(bytes)
                .ok()
                .filter(|t| !t.contains('\0'))
                .ok_or_else(|| {
                    FetchError::Invalid(format!("{path} is not a text file; skills hold text only"))
                })?;
            Ok::<_, FetchError>((path, text))
        }
    }))
    .buffer_unordered(8)
    .try_collect()
    .await?;
    let files: BTreeMap<String, String> = files.into_iter().collect();
    if files.values().map(String::len).sum::<usize>() > MAX_TOTAL_BYTES {
        return Err(FetchError::Invalid(format!(
            "the skill is larger than {MAX_TOTAL_BYTES} bytes"
        )));
    }
    let (id, description) = frontmatter(&files[SKILL_FILE])?;
    Ok(Package {
        name: id.clone(),
        id,
        description,
        source: Source {
            url: url.trim().to_owned(),
            repo,
            path: at.path,
            commit,
        },
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_resolve_to_a_repository_ref_and_directory() {
        let at = parse_url("https://github.com/superdesigndev/treg/tree/main/skills/rea-growth")
            .unwrap();
        assert_eq!(
            (
                at.owner.as_str(),
                at.repo.as_str(),
                at.reference.as_str(),
                at.path.as_str()
            ),
            ("superdesigndev", "treg", "main", "skills/rea-growth")
        );
        let blob = parse_url("https://github.com/a/b/blob/v1/x/SKILL.md").unwrap();
        assert_eq!((blob.reference.as_str(), blob.path.as_str()), ("v1", "x"));
        let root = parse_url("https://github.com/a/b.git").unwrap();
        assert_eq!(
            (
                root.repo.as_str(),
                root.reference.as_str(),
                root.path.as_str()
            ),
            ("b", "HEAD", "")
        );
        for bad in [
            "http://github.com/a/b",
            "https://gitlab.com/a/b",
            "https://github.com/a",
            "https://github.com/a/b/issues/1",
            "not a url",
        ] {
            assert!(parse_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn frontmatter_reads_folded_and_quoted_values() {
        let (name, description) = frontmatter(
            "---\nname: rea-growth\ndescription: Use when the user wants\n  to know how a company grows.\nlicense: MIT\n---\n# Body\n",
        )
        .unwrap();
        assert_eq!(name, "rea-growth");
        assert_eq!(
            description,
            "Use when the user wants to know how a company grows."
        );
        let (_, quoted) = frontmatter("---\nname: x\ndescription: \"Quoted: yes\"\n---\n").unwrap();
        assert_eq!(quoted, "Quoted: yes");
        let (_, block) =
            frontmatter("---\nname: x\ndescription: >\n  Folded\n  text\n---\n").unwrap();
        assert_eq!(block, "Folded text");
        for bad in [
            "# no frontmatter",
            "---\nname: x\n",
            "---\nname: x\n---\n",
            "---\ndescription: d\n---\n",
            "---\nname: Bad_Name\ndescription: d\n---\n",
        ] {
            assert!(frontmatter(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn scripts_are_flagged_by_place_extension_or_shebang() {
        assert!(is_script("scripts/run.txt", "x"));
        assert!(is_script("tool.py", "x"));
        assert!(is_script("bin/run", "#!/bin/sh\n"));
        assert!(!is_script("references/notes.md", "# notes"));
    }
}
