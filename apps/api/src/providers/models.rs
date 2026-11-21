//! Each CLI's own model list. The refresher asks every available CLI at
//! startup and then hourly, keeps the last answer in a cache file so a
//! failed ask after a restart still shows it, and falls back to the
//! definition's bundled list only when the CLI never answered. Each hour it
//! also checks again whether an unavailable CLI can run now.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::Registry;
use super::definitions::Definition;
use else_wire::settings::{Catalog, ModelInfo};

use crate::app::providers::{Profile, Provider};

/// How often every CLI is asked again, which also picks up a CLI upgrade.
pub const INTERVAL: Duration = Duration::from_secs(60 * 60);
/// How long one CLI may take to list its models.
pub const ASK_TIMEOUT: Duration = Duration::from_secs(60);
const CACHE_FILE: &str = "provider-models.json";

/// A model as a CLI listed it. `None` efforts or tiers mean the CLI does not
/// say, so the adapter's own apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listed {
    pub id: String,
    #[serde(default)]
    pub efforts: Option<Vec<String>>,
    #[serde(default)]
    pub tiers: Option<Vec<String>>,
}

impl Listed {
    pub fn id(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            efforts: None,
            tiers: None,
        }
    }
}

/// One provider's current catalog, written by model refreshes.
pub struct Models {
    definition: &'static Definition,
    catalog: watch::Sender<Arc<Catalog>>,
}

impl Models {
    /// The bundled list, until the CLI lists its own.
    pub fn new(definition: &'static Definition) -> Self {
        let bundled = definition.fallback.iter().map(|id| Listed::id(*id));
        Self {
            definition,
            catalog: watch::Sender::new(Arc::new(Catalog {
                models: catalog_models(definition, bundled.collect()),
                listed_at: None,
                refresh_error: None,
            })),
        }
    }

    pub fn current(&self) -> Arc<Catalog> {
        self.catalog.borrow().clone()
    }

    /// Adopt what the CLI listed at `at`, unless a later list is shown. A
    /// list with no model this adapter can apply counts as a failed ask.
    pub fn listed(&self, listed: Vec<Listed>, at: DateTime<Utc>) -> Result<(), String> {
        if self.current().listed_at.is_some_and(|shown| shown > at) {
            return Ok(());
        }
        let models = catalog_models(self.definition, listed);
        if models.is_empty() {
            let error = format!("{} listed no model else.bot can run", self.definition.label);
            self.failed(error.clone());
            return Err(error);
        }
        self.catalog.send_replace(Arc::new(Catalog {
            models,
            listed_at: Some(at),
            refresh_error: None,
        }));
        Ok(())
    }

    /// Keep the models shown and record why the latest ask failed.
    pub fn failed(&self, error: String) {
        self.catalog.send_modify(|catalog| {
            Arc::make_mut(catalog).refresh_error = Some(error);
        });
    }

    /// Reject a profile the adapter cannot apply exactly. No substitution.
    pub fn validate(&self, profile: &Profile) -> Result<(), String> {
        let definition = self.definition;
        definition.validate(profile)?;
        let catalog = self.current();
        let label = definition.label;
        let Some(model) = catalog.model(&profile.model) else {
            return Err(format!(
                "Unsupported {label} model {}. Supported: {}.{}",
                profile.model,
                catalog.ids().join(", "),
                definition.model_note
            ));
        };
        if !model.efforts.contains(&profile.reasoning_effort) {
            return Err(format!(
                "Unsupported {label} reasoning effort {} on {}. Supported: {}.{}",
                profile.reasoning_effort,
                model.id,
                model.efforts.join(", "),
                definition.effort_note
            ));
        }
        if !model.tiers.contains(&profile.service_tier) {
            return Err(format!(
                "Unsupported {label} service tier {} on {}. Supported: {}.{}",
                profile.service_tier,
                model.id,
                model.tiers.join(", "),
                definition.tier_note
            ));
        }
        Ok(())
    }
}

/// A model ID read as a family and a version: `claude-fable-5-2` is
/// `claude-fable` 5.2, `gpt-6.1-sol` is `gpt-sol` 6.1, and a trailing
/// snapshot date (`-20251001`) is dropped.
#[derive(Debug, PartialEq, Eq)]
struct Name {
    family: String,
    version: Vec<u32>,
}

impl Name {
    fn parse(id: &str) -> Self {
        let mut family: Vec<&str> = Vec::new();
        let mut version = Vec::new();
        let parts: Vec<&str> = id.split('-').collect();
        let parts = match parts.split_last() {
            Some((last, rest)) if is_date(last) && !rest.is_empty() => rest,
            _ => &parts[..],
        };
        for part in parts {
            let numbers: Option<Vec<u32>> = part.split('.').map(|n| n.parse().ok()).collect();
            match numbers {
                Some(numbers) if !part.is_empty() => version.extend(numbers),
                _ => family.push(part),
            }
        }
        Self {
            family: family.join("-"),
            version,
        }
    }
}

fn is_date(part: &str) -> bool {
    part.len() == 8 && part.bytes().all(|b| b.is_ascii_digit())
}

/// "gpt-6.1-sol" -> "GPT 6.1 Sol", "claude-fable-5-2" -> "Claude Fable 5.2".
pub fn label(id: &str) -> String {
    let parts: Vec<&str> = id.split('-').collect();
    let parts = match parts.split_last() {
        Some((last, rest)) if is_date(last) && !rest.is_empty() => rest,
        _ => &parts[..],
    };
    let mut words: Vec<String> = Vec::new();
    for part in parts {
        match words.last_mut() {
            Some(last)
                if part.chars().all(|c| c.is_ascii_digit())
                    && last.chars().all(|c| c.is_ascii_digit() || c == '.') =>
            {
                last.push('.');
                last.push_str(part);
            }
            _ if *part == "gpt" => words.push("GPT".to_owned()),
            _ => {
                let mut chars = part.chars();
                words.push(match chars.next() {
                    Some(first) => first.to_uppercase().chain(chars).collect(),
                    None => String::new(),
                });
            }
        }
    }
    words.join(" ")
}

/// The models `definition` can apply from `listed`, in picker order: the
/// newest of each family first, in the CLI's order, then the older ones,
/// newest first per family. A model left with no effort or tier the
/// adapter applies, or one that is not pinned, is dropped.
fn catalog_models(definition: &Definition, listed: Vec<Listed>) -> Vec<ModelInfo> {
    let keep = |offered: Option<Vec<String>>, allowed: &[&str]| -> Vec<String> {
        match offered {
            Some(offered) => allowed
                .iter()
                .filter(|value| offered.iter().any(|o| o == *value))
                .map(|value| (*value).to_owned())
                .collect(),
            None => allowed.iter().map(|value| (*value).to_owned()).collect(),
        }
    };
    let mut entries: Vec<(Name, ModelInfo)> = Vec::new();
    for listed in listed {
        if !(definition.pinned)(&listed.id) || entries.iter().any(|(_, m)| m.id == listed.id) {
            continue;
        }
        let efforts = keep(listed.efforts, definition.efforts);
        let tiers = keep(listed.tiers, definition.tiers);
        if efforts.is_empty() || tiers.is_empty() {
            continue;
        }
        entries.push((
            Name::parse(&listed.id),
            ModelInfo {
                label: label(&listed.id),
                id: listed.id,
                efforts,
                tiers,
                featured: false,
                upgrade: None,
            },
        ));
    }
    let families: Vec<String> = entries.iter().fold(Vec::new(), |mut seen, (name, _)| {
        if !seen.contains(&name.family) {
            seen.push(name.family.clone());
        }
        seen
    });
    let newest: Vec<usize> = families
        .iter()
        .filter_map(|family| {
            (0..entries.len())
                .filter(|&i| entries[i].0.family == *family)
                .max_by(|&a, &b| {
                    entries[a]
                        .0
                        .version
                        .cmp(&entries[b].0.version)
                        .then(b.cmp(&a))
                })
        })
        .collect();
    let upgrades: Vec<Option<String>> = entries
        .iter()
        .map(|(name, _)| {
            newest
                .iter()
                .map(|&i| &entries[i])
                .find(|(top, _)| top.family == name.family && top.version > name.version)
                .map(|(_, model)| model.id.clone())
        })
        .collect();
    let mut ordered: Vec<(usize, (Name, ModelInfo))> = entries
        .into_iter()
        .zip(upgrades)
        .enumerate()
        .map(|(i, ((name, mut model), upgrade))| {
            model.featured = newest.contains(&i);
            model.upgrade = upgrade;
            (i, (name, model))
        })
        .collect();
    ordered.sort_by(|(a_index, (a, a_model)), (b_index, (b, b_model))| {
        let family = |name: &Name| families.iter().position(|f| *f == name.family);
        b_model.featured.cmp(&a_model.featured).then_with(|| {
            if a_model.featured {
                a_index.cmp(b_index)
            } else {
                family(a)
                    .cmp(&family(b))
                    .then_with(|| b.version.cmp(&a.version))
                    .then(a_index.cmp(b_index))
            }
        })
    });
    ordered.into_iter().map(|(_, (_, model))| model).collect()
}

/// The last list each CLI gave, by provider ID.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(flatten)]
    providers: std::collections::BTreeMap<String, Cached>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Cached {
    listed_at: DateTime<Utc>,
    models: Vec<Listed>,
}

fn read_cache(path: &Path) -> Saved {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn cache_path(data_dir: &Path) -> PathBuf {
    data_dir.join(CACHE_FILE)
}

/// The cache file's only writer, so refreshes that overlap never drop
/// each other's answers.
#[derive(Clone)]
pub struct Cache(crate::actor::Actor<PathBuf>);

impl Cache {
    pub fn new(path: PathBuf) -> Self {
        Self(crate::actor::Actor::new(path))
    }

    async fn save(&self, answers: Vec<(String, Cached)>) {
        if answers.is_empty() {
            return;
        }
        self.0
            .call(move |path| {
                let mut saved = read_cache(path);
                for (id, answer) in answers {
                    let newer = saved
                        .providers
                        .get(&id)
                        .is_none_or(|cached| cached.listed_at <= answer.listed_at);
                    if newer {
                        saved.providers.insert(id, answer);
                    }
                }
                let (Some(dir), Some(name)) =
                    (path.parent(), path.file_name().and_then(|n| n.to_str()))
                else {
                    return;
                };
                let bytes = serde_json::to_vec_pretty(&saved).expect("the cache serializes");
                if let Err(error) = super::cli::files::replace_private(dir, name, &bytes) {
                    tracing::warn!(%error, "could not save the model cache");
                }
            })
            .await;
    }
}

/// Show each CLI's cached list until its first refresh answers.
pub fn restore(registry: &Registry, cache: &Path) {
    let cached = read_cache(cache);
    for provider in registry.all() {
        let (Some(models), Some(entry)) = (provider.models(), cached.providers.get(provider.id()))
        else {
            continue;
        };
        if let Err(error) = models.listed(entry.models.clone(), entry.listed_at) {
            tracing::warn!(provider = provider.id(), %error, "ignored the cached model list");
        }
    }
}

/// Ask each available CLI for its models, concurrently, and save the
/// answers. A failed ask keeps the models shown and the cached entry.
pub async fn refresh(providers: &[Arc<dyn Provider>], cache: &Cache) {
    let asks = providers
        .iter()
        .filter(|p| p.models().is_some() && p.availability() == super::Availability::Available)
        .map(|provider| async move {
            let listed = tokio::time::timeout(ASK_TIMEOUT, provider.list_models())
                .await
                .unwrap_or_else(|_| {
                    Err(format!(
                        "{} did not list its models within {}s",
                        provider.label(),
                        ASK_TIMEOUT.as_secs()
                    ))
                });
            (provider, listed, Utc::now())
        });
    let answers = futures_util::future::join_all(asks).await;
    let mut saved = Vec::new();
    for (provider, listed, now) in answers {
        let Some(models) = provider.models() else {
            continue;
        };
        let adopted = listed.and_then(|listed| {
            models.listed(listed.clone(), now)?;
            Ok(listed)
        });
        match adopted {
            Ok(listed) => saved.push((
                provider.id().to_owned(),
                Cached {
                    listed_at: now,
                    models: listed,
                },
            )),
            Err(error) => {
                tracing::warn!(provider = provider.id(), %error, "could not refresh models");
                models.failed(error);
            }
        }
    }
    cache.save(saved).await;
}

/// Refresh now, then every [`INTERVAL`] recheck unavailable CLIs and
/// refresh again, until `stop`.
pub async fn run(registry: Registry, cache: Cache, stop: CancellationToken) {
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            () = refresh(registry.all(), &cache) => {}
        }
        tokio::select! {
            () = stop.cancelled() => return,
            () = tokio::time::sleep(INTERVAL) => {}
        }
        tokio::select! {
            () = stop.cancelled() => return,
            () = super::recheck_unavailable(&registry) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(models: &[ModelInfo]) -> Vec<&str> {
        models.iter().map(|m| m.id.as_str()).collect()
    }

    #[test]
    fn names_split_into_family_and_version() {
        let name = |id| {
            let Name { family, version } = Name::parse(id);
            (family, version)
        };
        assert_eq!(
            name("claude-fable-5-2"),
            ("claude-fable".into(), vec![5, 2])
        );
        assert_eq!(name("claude-fable-5"), ("claude-fable".into(), vec![5]));
        assert_eq!(
            name("claude-haiku-4-5-20251001"),
            ("claude-haiku".into(), vec![4, 5])
        );
        assert_eq!(name("gpt-6.1-sol"), ("gpt-sol".into(), vec![6, 1]));
        assert_eq!(
            name("grok-4.7-build-fast"),
            ("grok-build-fast".into(), vec![4, 7])
        );
        assert_eq!(label("claude-fable-5-2"), "Claude Fable 5.2");
        assert_eq!(label("claude-haiku-4-5-20251001"), "Claude Haiku 4.5");
        assert_eq!(label("gpt-6.1-sol"), "GPT 6.1 Sol");
        assert_eq!(label("grok-4.7-build-fast"), "Grok 4.7 Build Fast");
    }

    #[test]
    fn the_newest_of_each_family_leads_and_older_ones_point_at_it() {
        let models = catalog_models(
            &super::super::claude::DEFINITION,
            [
                "claude-opus-5-5",
                "claude-fable-5-2",
                "claude-sonnet-5-5",
                "claude-fable-5-1",
                "claude-opus-4-8",
                "claude-fable-5",
                "claude-opus-5",
                "fable",
            ]
            .into_iter()
            .map(Listed::id)
            .collect(),
        );
        assert_eq!(
            ids(&models),
            [
                "claude-opus-5-5",
                "claude-fable-5-2",
                "claude-sonnet-5-5",
                "claude-opus-5",
                "claude-opus-4-8",
                "claude-fable-5-1",
                "claude-fable-5",
            ],
            "aliases are dropped"
        );
        let featured: Vec<bool> = models.iter().map(|m| m.featured).collect();
        assert_eq!(featured, [true, true, true, false, false, false, false]);
        let upgrade = |id| models.iter().find(|m| m.id == id).unwrap().upgrade.clone();
        assert_eq!(
            upgrade("claude-fable-5-1").as_deref(),
            Some("claude-fable-5-2")
        );
        assert_eq!(
            upgrade("claude-opus-4-8").as_deref(),
            Some("claude-opus-5-5")
        );
        assert_eq!(upgrade("claude-fable-5-2"), None);
    }

    #[test]
    fn listed_efforts_and_tiers_keep_only_what_the_adapter_applies() {
        let codex = &super::super::codex::DEFINITION;
        let models = catalog_models(
            codex,
            vec![
                Listed {
                    id: "gpt-6.2-sol".into(),
                    efforts: Some(vec!["low".into(), "max".into(), "ultra".into()]),
                    tiers: Some(vec!["default".into(), "priority".into(), "flex".into()]),
                },
                Listed {
                    id: "gpt-6.2-luna".into(),
                    efforts: Some(vec!["ultra".into()]),
                    tiers: None,
                },
            ],
        );
        assert_eq!(
            ids(&models),
            ["gpt-6.2-sol"],
            "a model with no usable effort is dropped"
        );
        assert_eq!(models[0].efforts, ["low", "max"]);
        assert_eq!(models[0].tiers, ["default", "priority"]);
    }

    #[test]
    fn the_catalog_keeps_the_flat_lists_older_desktops_read() {
        let models = Models::new(&super::super::claude::DEFINITION);
        models.failed("offline".into());
        let json = serde_json::to_value(models.current().as_ref()).unwrap();
        assert_eq!(json["models"][1], "claude-fable-5-2");
        assert_eq!(
            json["featured_models"],
            serde_json::json!([
                "claude-opus-5-5",
                "claude-fable-5-2",
                "claude-sonnet-5-5",
                "claude-haiku-5-5"
            ])
        );
        assert_eq!(
            json["efforts"],
            serde_json::json!(["low", "medium", "high", "xhigh", "max"])
        );
        assert_eq!(json["tiers"], serde_json::json!(["default"]));
        let fable = &json["details"][4];
        assert_eq!(fable["id"], "claude-fable-5-1");
        assert_eq!(fable["label"], "Claude Fable 5.1");
        assert_eq!(fable["featured"], false);
        assert_eq!(fable["upgrade"], "claude-fable-5-2");
        assert!(json["listed_at"].is_null());
        assert_eq!(json["refresh_error"], "offline");
    }

    #[test]
    fn older_desktops_get_only_combinations_every_listed_model_takes() {
        let models = Models::new(&super::super::claude::DEFINITION);
        let effort = |id: &str, efforts: &[&str]| Listed {
            id: id.into(),
            efforts: Some(efforts.iter().map(|e| (*e).to_owned()).collect()),
            tiers: None,
        };
        models
            .listed(
                vec![
                    effort("claude-opus-5-5", &["low", "xhigh"]),
                    effort("claude-opus-4-6", &["low"]),
                    effort("claude-sonnet-5-5", &["low", "xhigh", "max"]),
                ],
                Utc::now(),
            )
            .unwrap();
        let json = serde_json::to_value(models.current().as_ref()).unwrap();
        assert_eq!(
            json["models"],
            serde_json::json!(["claude-opus-5-5", "claude-sonnet-5-5"])
        );
        assert_eq!(json["efforts"], serde_json::json!(["low", "xhigh"]));
        assert_eq!(json["details"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn a_listed_model_validates_and_a_failed_ask_keeps_it() {
        let models = Models::new(&super::super::claude::DEFINITION);
        let profile = Profile {
            model: "claude-fable-5-9".into(),
            reasoning_effort: "high".into(),
            service_tier: "default".into(),
        };
        assert!(models.validate(&profile).is_err(), "not listed yet");
        let at = Utc::now();
        models
            .listed(
                vec![Listed {
                    id: "claude-fable-5-9".into(),
                    efforts: Some(vec!["low".into()]),
                    tiers: None,
                }],
                at,
            )
            .unwrap();
        assert!(
            models
                .validate(&profile)
                .unwrap_err()
                .contains("effort high on claude-fable-5-9"),
            "efforts are per model"
        );
        let low = Profile {
            reasoning_effort: "low".into(),
            ..profile
        };
        assert_eq!(models.validate(&low), Ok(()));
        models.failed("offline".into());
        let catalog = models.current();
        assert_eq!(catalog.listed_at, Some(at));
        assert_eq!(catalog.refresh_error.as_deref(), Some("offline"));
        assert_eq!(models.validate(&low), Ok(()));
        assert!(models.listed(vec![Listed::id("opus")], at).is_err());
        assert_eq!(models.current().ids(), ["claude-fable-5-9"]);
    }
}
