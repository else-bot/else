//! The static identity and profile rules of each provider CLI, shared by
//! the running adapters and by offline checks (restore) that must not probe
//! an installed CLI.

use crate::app::providers::{Catalog, Profile};

/// One provider's identity and the exact profiles it applies.
#[derive(Debug)]
pub struct Definition {
    pub id: &'static str,
    pub label: &'static str,
    pub catalog: Catalog,
    /// Added to each rejection of a model, effort, or tier.
    pub model_note: &'static str,
    pub effort_note: &'static str,
    pub tier_note: &'static str,
}

/// Registration order is default-selection order.
pub const DEFINITIONS: &[&Definition] = &[
    &super::codex::DEFINITION,
    &super::claude::DEFINITION,
    &super::grok::DEFINITION,
];

pub fn find(id: &str) -> Option<&'static Definition> {
    DEFINITIONS.iter().copied().find(|d| d.id == id)
}

/// Provider IDs as prose: `codex, claude, or grok`.
pub fn id_list() -> String {
    match DEFINITIONS.split_last() {
        Some((last, [])) => last.id.to_owned(),
        Some((last, rest)) => {
            let rest: Vec<&str> = rest.iter().map(|d| d.id).collect();
            format!("{}, or {}", rest.join(", "), last.id)
        }
        None => String::new(),
    }
}

/// A restored profile must be one its provider applies exactly. A profile
/// of a provider this build does not know is kept as it was: turns on it
/// fail as unavailable instead of failing the whole restore.
pub fn validate_restored(provider: &str, profile: &Profile) -> Result<(), String> {
    find(provider).map_or(Ok(()), |definition| definition.validate(profile))
}

impl Definition {
    /// Reject a profile the adapter cannot apply exactly. No substitution.
    pub fn validate(&self, profile: &Profile) -> Result<(), String> {
        let Catalog {
            models,
            efforts,
            tiers,
            ..
        } = self.catalog;
        let label = self.label;
        if !models.contains(&profile.model.as_str()) {
            return Err(format!(
                "Unsupported {label} model {}. Supported: {}.{}",
                profile.model,
                models.join(", "),
                self.model_note
            ));
        }
        if !efforts.contains(&profile.reasoning_effort.as_str()) {
            return Err(format!(
                "Unsupported {label} reasoning effort {}. Supported: {}.{}",
                profile.reasoning_effort,
                efforts.join(", "),
                self.effort_note
            ));
        }
        if !tiers.contains(&profile.service_tier.as_str()) {
            return Err(format!(
                "Unsupported {label} service tier {}. Supported: {}.{}",
                profile.service_tier,
                tiers.join(", "),
                self.tier_note
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_catalog_entry_validates_and_nothing_outside_it_does() {
        for definition in DEFINITIONS {
            let Catalog {
                models,
                featured_models,
                efforts,
                tiers,
            } = definition.catalog;
            assert!(
                featured_models.iter().all(|m| models.contains(m)),
                "{}",
                definition.id
            );
            for model in models {
                for effort in efforts {
                    for tier in tiers {
                        let profile = Profile {
                            model: (*model).into(),
                            reasoning_effort: (*effort).into(),
                            service_tier: (*tier).into(),
                        };
                        assert_eq!(definition.validate(&profile), Ok(()), "{profile:?}");
                        assert_eq!(validate_restored(definition.id, &profile), Ok(()));
                    }
                }
            }
            let valid = Profile {
                model: models[0].into(),
                reasoning_effort: efforts[0].into(),
                service_tier: tiers[0].into(),
            };
            for bad in [
                Profile {
                    model: "unknown".into(),
                    ..valid.clone()
                },
                Profile {
                    reasoning_effort: "ultra".into(),
                    ..valid.clone()
                },
                Profile {
                    service_tier: "flex".into(),
                    ..valid.clone()
                },
            ] {
                assert!(definition.validate(&bad).is_err(), "{bad:?}");
                assert!(validate_restored(definition.id, &bad).is_err());
            }
        }
    }

    #[test]
    fn unknown_historical_providers_restore_unchanged_and_ids_are_unique() {
        let anything = Profile {
            model: "retired-model".into(),
            reasoning_effort: "any".into(),
            service_tier: "any".into(),
        };
        assert_eq!(validate_restored("retired-provider", &anything), Ok(()));
        let mut ids: Vec<&str> = DEFINITIONS.iter().map(|d| d.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), DEFINITIONS.len());
        assert_eq!(id_list(), "codex, claude, or grok");
    }
}
