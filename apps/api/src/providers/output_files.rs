use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{Cursor, Read as _};
use std::ops::Range;
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Component, Path, PathBuf};

use markdown::mdast::Node;
use uuid::Uuid;

use crate::app::messages::{
    Attachment, MAX_ATTACHMENT_BYTES, MAX_MESSAGE_FILES, MessagePart, flatten, media_type,
    validate_name,
};

pub(crate) struct OutputFile {
    pub file: Attachment,
    pub data: Vec<u8>,
}

pub(crate) struct FileReply {
    pub text: String,
    pub parts: Vec<MessagePart>,
    pub files: Vec<OutputFile>,
}

/// Markdown image syntax shows a picture, so only decodable images attach;
/// a link attaches any regular file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Reference {
    Image,
    Link,
}

pub(crate) fn prepare(project: &Path, text: &str) -> Option<FileReply> {
    if !text.contains("](") && !text.contains("]:") {
        return None;
    }
    let tree = markdown::to_mdast(text, &markdown::ParseOptions::default()).ok()?;
    let mut definitions = HashMap::new();
    definitions_in(&tree, &mut definitions);
    let mut candidates = Vec::new();
    references_in(&tree, &definitions, &mut candidates);
    let root = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(project)
        .ok()?;
    let mut files = Vec::new();
    let mut saved = HashMap::<(PathBuf, Reference), Attachment>::new();
    let mut parts = Vec::new();
    let mut offset = 0;
    for (range, url, reference) in candidates {
        if range.start < offset {
            continue;
        }
        let Ok(path) = percent_encoding::percent_decode_str(&url).decode_utf8() else {
            continue;
        };
        let Some(relative) = relative_path(project, &path) else {
            continue;
        };
        let key: PathBuf = relative
            .components()
            .filter(|component| *component != Component::CurDir)
            .collect();
        let file = if let Some(file) = saved.get(&(key.clone(), reference)) {
            file.clone()
        } else {
            if files.len() == MAX_MESSAGE_FILES {
                continue;
            }
            let Some(output) = read_file(&root, &key, reference) else {
                continue;
            };
            let file = output.file.clone();
            saved.insert((key, reference), file.clone());
            files.push(output);
            file
        };
        if offset < range.start {
            parts.push(MessagePart::Text {
                text: text[offset..range.start].to_owned(),
            });
        }
        offset = range.end;
        parts.push(MessagePart::File { file });
    }
    if files.is_empty() {
        return None;
    }
    if offset < text.len() {
        parts.push(MessagePart::Text {
            text: text[offset..].to_owned(),
        });
    }
    Some(FileReply {
        text: flatten(&parts),
        parts,
        files,
    })
}

fn definitions_in(node: &Node, definitions: &mut HashMap<String, String>) {
    if let Node::Definition(definition) = node {
        definitions
            .entry(definition.identifier.clone())
            .or_insert_with(|| definition.url.clone());
    }
    for child in node.children().into_iter().flatten() {
        definitions_in(child, definitions);
    }
}

fn references_in(
    node: &Node,
    definitions: &HashMap<String, String>,
    references: &mut Vec<(Range<usize>, String, Reference)>,
) {
    let reference = match node {
        Node::Image(image) => Some((&image.url, Reference::Image)),
        Node::ImageReference(image) => definitions
            .get(&image.identifier)
            .map(|url| (url, Reference::Image)),
        Node::Link(link) => Some((&link.url, Reference::Link)),
        Node::LinkReference(link) => definitions
            .get(&link.identifier)
            .map(|url| (url, Reference::Link)),
        _ => None,
    };
    if let (Some((url, reference)), Some(position)) = (reference, node.position()) {
        references.push((
            position.start.offset..position.end.offset,
            url.clone(),
            reference,
        ));
    }
    for child in node.children().into_iter().flatten() {
        references_in(child, definitions, references);
    }
}

fn relative_path<'a>(project: &'a Path, url: &'a str) -> Option<&'a Path> {
    if reqwest::Url::parse(url).is_ok() {
        return None;
    }
    if let Some(path) = url.strip_prefix("~/work/") {
        return Some(Path::new(path));
    }
    let path = Path::new(url);
    if !path.is_absolute() {
        return Some(path);
    }
    [
        project,
        Path::new(crate::computers::GUEST_PROJECT),
        Path::new(crate::computers::ROOT_PROJECT),
    ]
    .into_iter()
    .find_map(|root| path.strip_prefix(root).ok())
}

fn open_beneath(root: &File, path: &Path) -> Option<File> {
    let components: Vec<_> = path.components().collect();
    let mut directory = root.try_clone().ok()?;
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(component) = component else {
            if *component == Component::CurDir {
                continue;
            }
            return None;
        };
        let last = index == components.len() - 1;
        let name = CString::new(component.as_bytes()).ok()?;
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | if last {
                libc::O_NONBLOCK
            } else {
                libc::O_DIRECTORY
            };
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return None;
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    Some(directory)
}

fn read_file(root: &File, path: &Path, reference: Reference) -> Option<OutputFile> {
    let name = path.file_name()?.to_str()?;
    validate_name(name).ok()?;
    let file = open_beneath(root, path)?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_ATTACHMENT_BYTES || metadata.nlink() != 1 {
        return None;
    }
    let mut data = Vec::new();
    file.take(MAX_ATTACHMENT_BYTES + 1)
        .read_to_end(&mut data)
        .ok()?;
    if data.len() as u64 > MAX_ATTACHMENT_BYTES {
        return None;
    }
    let media_type = match (image_type(&data), reference) {
        (Some(image), _) => image.to_owned(),
        (None, Reference::Image) => return None,
        (None, Reference::Link) => guessed_type(name),
    };
    Some(OutputFile {
        file: Attachment {
            id: Uuid::new_v4(),
            name: name.to_owned(),
            media_type,
            size: data.len() as u64,
        },
        data,
    })
}

/// Raster image types are reserved for bytes that decode, since the
/// desktop shows those in its gallery.
fn guessed_type(name: &str) -> String {
    let guessed = mime_guess::from_path(name).first_raw();
    match guessed.map(media_type) {
        Some(Ok(guessed))
            if !matches!(
                guessed.as_str(),
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            ) =>
        {
            guessed
        }
        _ => "application/octet-stream".to_owned(),
    }
}

fn image_type(data: &[u8]) -> Option<&'static str> {
    let mut reader = image::ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let media_type = match reader.format()? {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::WebP => "image/webp",
        _ => return None,
    };
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    reader.decode().ok()?;
    Some(media_type)
}
