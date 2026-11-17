//! The one registration of each provider CLI: its identity and profile
//! rules, shared by the running adapters and by offline checks (restore)
//! that must not probe an installed CLI, and the constructor the runtime
//! registry calls. Adding a CLI is one entry in [`DEFINITIONS`].

use crate::app::providers::Profile;

/// One provider's identity and the exact profiles it applies.
#[derive(Debug)]
pub struct Definition {
    pub id: &'static str,
    pub label: &'static str,
    /// The models shown until the installed CLI lists its own, newest
    /// first per family.
    pub fallback: &'static [&'static str],
    /// The efforts and tiers this adapter can apply. A listed model keeps
    /// only these; a CLI that lists none per model gets all of them.
    pub efforts: &'static [&'static str],
    pub tiers: &'static [&'static str],
    /// Whether a model ID names one model exactly, not an alias that
    /// follows the CLI's default.
    pub pinned: fn(&str) -> bool,
    /// Added to each rejection of a model, effort, or tier.
    pub model_note: &'static str,
    pub effort_note: &'static str,
    pub tier_note: &'static str,
    /// Construct the running adapter. Only [`super::build`] calls it;
    /// offline checks never do.
    pub build: fn(&super::Wiring) -> super::Built,
}

/// Every provider CLI. Registration order is default-selection order.
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

/// Any ID without spaces: the CLI has no aliases that move.
pub fn any_id(model: &str) -> bool {
    !model.is_empty() && !model.contains(char::is_whitespace)
}

impl Definition {
    /// Reject a profile the adapter could never apply, without asking the
    /// CLI which models it runs: restore checks this offline. Running
    /// adapters also check the listed catalog.
    pub fn validate(&self, profile: &Profile) -> Result<(), String> {
        let label = self.label;
        if !(self.pinned)(&profile.model) {
            return Err(format!(
                "Unsupported {label} model {}.{}",
                profile.model, self.model_note
            ));
        }
        if !self.efforts.contains(&profile.reasoning_effort.as_str()) {
            return Err(format!(
                "Unsupported {label} reasoning effort {}. Supported: {}.{}",
                profile.reasoning_effort,
                self.efforts.join(", "),
                self.effort_note
            ));
        }
        if !self.tiers.contains(&profile.service_tier.as_str()) {
            return Err(format!(
                "Unsupported {label} service tier {}. Supported: {}.{}",
                profile.service_tier,
                self.tiers.join(", "),
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
    fn restore_takes_any_pinned_model_with_a_supported_effort_and_tier() {
        for definition in DEFINITIONS {
            for model in definition.fallback {
                assert!((definition.pinned)(model), "{model}");
                for effort in definition.efforts {
                    for tier in definition.tiers {
                        let profile = Profile {
                            model: (*model).into(),
                            reasoning_effort: (*effort).into(),
                            service_tier: (*tier).into(),
                        };
                        assert_eq!(validate_restored(definition.id, &profile), Ok(()));
                    }
                }
            }
            let valid = Profile {
                model: definition.fallback[0].into(),
                reasoning_effort: definition.efforts[0].into(),
                service_tier: definition.tiers[0].into(),
            };
            let newer = Profile {
                model: format!("{}-next", definition.fallback[0]),
                ..valid.clone()
            };
            assert_eq!(validate_restored(definition.id, &newer), Ok(()));
            for bad in [
                Profile {
                    model: "two words".into(),
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
                assert!(validate_restored(definition.id, &bad).is_err(), "{bad:?}");
            }
        }
        let alias = Profile {
            model: "fable".into(),
            reasoning_effort: "high".into(),
            service_tier: "default".into(),
        };
        assert!(validate_restored("claude", &alias).is_err());
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
