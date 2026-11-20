use super::{claude, codex, cursor, grok};
use crate::app::providers::{Origin, TurnInput};

/// The four tool policies a fingerprint distinguishes, in the order of each
/// provider's `expected` pins: owner and event turns with else.bot's tools,
/// then without them. `fingerprint` takes the input and whether the
/// provider still offers else.bot's tools; once false it stays false.
pub(super) fn each_case(
    input: &mut TurnInput,
    expected: [&'static str; 4],
    mut fingerprint: impl FnMut(&TurnInput, bool) -> String,
) -> Vec<(String, &'static str, String)> {
    let cases = [
        (Origin::OwnerRequest, true),
        (Origin::ConnectorEvent, true),
        (Origin::OwnerRequest, false),
        (Origin::ConnectorEvent, false),
    ];
    cases
        .into_iter()
        .zip(expected)
        .map(|((origin, tools), expected)| {
            input.delegation.origin = origin;
            let case = format!("{origin:?}, else.bot tools: {tools}");
            (case, expected, fingerprint(input, tools))
        })
        .collect()
}

#[test]
fn fingerprints_keep_their_values() {
    let mut mismatches = Vec::new();
    let codex = codex::tests::pinned_fingerprints()
        .map(|(origin, expected, actual)| (format!("{origin:?}"), expected, actual));
    for (provider, pins) in [
        ("codex", codex.to_vec()),
        ("claude", claude::tests::pinned_fingerprints()),
        ("grok", grok::tests::pinned_fingerprints()),
        ("cursor", cursor::tests::pinned_fingerprints()),
    ] {
        for (case, expected, actual) in pins {
            if actual != expected {
                mismatches.push(format!(
                    "| {provider} | {case} | `{expected}` | `{actual}` |"
                ));
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "Session fingerprints changed. Review compatibility before updating the pins.\n\n\
         | Provider | Case | Expected | Actual |\n\
         | --- | --- | --- | --- |\n{}",
        mismatches.join("\n")
    );
}
