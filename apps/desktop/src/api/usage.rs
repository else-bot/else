use super::{Client, ClientError};
use else_wire::usage::UsageReport;
use reqwest::Method;
use std::sync::Arc;

impl Client {
    pub async fn usage(self: Arc<Self>) -> Result<UsageReport, ClientError> {
        self.call::<UsageReport>(Method::GET, "/v1/usage".into(), None)
            .await
            .map(|(_, report)| report)
    }
}

#[cfg(test)]
mod tests {
    use else_wire::usage::{
        Attribution, FIVE_HOURS, UsageReport, UsageSource, UsageSourceKind, WEEK,
    };

    #[test]
    fn usage_window_selection_defaults_to_week_and_keeps_periods_separate() {
        let report: UsageReport = serde_json::from_str(include_str!("usage_fixture.json")).unwrap();
        let mut provider = report.providers[0].clone();
        assert_eq!(provider.selected_window(None).unwrap().minutes, WEEK);
        assert_eq!(
            provider.selected_window(Some(FIVE_HOURS)).unwrap().minutes,
            FIVE_HOURS
        );
        assert_eq!(provider.selected_window(Some(123)).unwrap().minutes, WEEK);
        provider.windows[1].minutes = 1440;
        assert_eq!(provider.selected_window(None).unwrap().minutes, FIVE_HOURS);
        provider.windows.clear();
        assert!(provider.selected_window(None).is_none());
    }

    #[test]
    fn usage_fixture_preserves_provider_windows_and_attribution() {
        let report: UsageReport = serde_json::from_str(include_str!("usage_fixture.json")).unwrap();
        let claude = &report.providers[0];
        assert_eq!(report.default_provider().unwrap().provider_id, "claude");
        assert_eq!(claude.windows[0].attribution.used(), Some(62.));
        assert_eq!(claude.windows[0].attribution.outside(), Some(11.));
        let bot = &claude.windows[0].bots[0];
        assert_eq!(bot.sources[0].source, UsageSourceKind::Routine);
        assert_eq!(bot.share, 0.51);
        let grok = &report.providers[1];
        assert_eq!(grok.windows[0].attribution, Attribution::Unreported);
        assert_eq!(grok.windows[0].bots[0].share, 1.);
    }

    #[test]
    fn usage_defaults_to_highest_limit_then_first_provider_without_limits() {
        let mut report: UsageReport =
            serde_json::from_str(include_str!("usage_fixture.json")).unwrap();
        report.providers[1].windows[0].attribution = Attribution::Unattributed {
            used: 83.,
            resets_at: report.generated_at,
            observed_at: report.generated_at,
        };
        assert_eq!(report.default_provider().unwrap().provider_id, "grok");
        for window in report
            .providers
            .iter_mut()
            .flat_map(|provider| &mut provider.windows)
        {
            window.attribution = Attribution::Unreported;
        }
        assert_eq!(report.default_provider().unwrap().provider_id, "claude");
        report.providers.clear();
        assert!(report.default_provider().is_none());
    }

    #[test]
    fn attribution_decodes_each_state() {
        let decode = |value| serde_json::from_value::<Attribution>(value).unwrap();
        let unattributed = decode(serde_json::json!({
            "state": "unattributed", "used": 12.0,
            "resets_at": "2026-11-16T15:40:00Z", "observed_at": "2026-11-16T15:12:00Z"
        }));
        assert_eq!(unattributed.used(), Some(12.));
        assert_eq!(unattributed.outside(), None);
        assert_eq!(
            decode(serde_json::json!({ "state": "unreported" })),
            Attribution::Unreported
        );
        assert_eq!(
            decode(serde_json::json!({ "state": "something_new" })),
            Attribution::Unreported
        );
    }

    #[test]
    fn windows_are_named_for_where_they_appear() {
        let report: UsageReport = serde_json::from_str(include_str!("usage_fixture.json")).unwrap();
        let [five, week] = &report.providers[0].windows[..] else {
            panic!("two windows")
        };
        assert_eq!(five.title(), "5-hour limit");
        assert_eq!(week.title(), "Weekly limit");
        assert_eq!(week.period(), "since the weekly limit reset");
        let mut daily = five.clone();
        daily.minutes = 2880;
        assert_eq!(daily.period(), "since the 2-day limit reset");
        daily.attribution = Attribution::Unreported;
        assert_eq!(daily.period(), "in the last 2 days");
    }

    #[test]
    fn usage_tolerates_new_sources() {
        let source: UsageSource = serde_json::from_value(serde_json::json!({
            "source": "future_source", "label": "New trigger", "turns": 1,
            "percent": 100.0, "share": 1.0
        }))
        .unwrap();
        assert_eq!(source.source, UsageSourceKind::Unknown);
        assert_eq!(source.source.label(), "Other");
    }
}
