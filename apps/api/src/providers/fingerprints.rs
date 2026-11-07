use super::{claude, codex};

#[test]
fn fingerprints_keep_their_values() {
    let mut mismatches = Vec::new();
    for (provider, pins) in [
        ("codex", codex::tests::pinned_fingerprints()),
        ("claude", claude::tests::pinned_fingerprints()),
    ] {
        for (origin, expected, actual) in pins {
            if actual != expected {
                mismatches.push(format!(
                    "| {provider} | {origin:?} | `{expected}` | `{actual}` |"
                ));
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "Session fingerprints changed. Review compatibility before updating the pins.\n\n\
         | Provider | Origin | Expected | Actual |\n\
         | --- | --- | --- | --- |\n{}",
        mismatches.join("\n")
    );
}
