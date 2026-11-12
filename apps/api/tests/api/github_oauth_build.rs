#[path = "../../build_support/github_oauth.rs"]
mod github_oauth;

#[test]
fn registration_is_parsed_without_copying_other_credentials() {
    let registration = github_oauth::parse(
        r#"{
        "client_id":"fixture-id", "client_secret":"fixture-secret", "access_token":"must-not-embed"
    }"#,
    )
    .unwrap();
    assert_eq!(registration.client_id, "fixture-id");
    assert_eq!(registration.client_secret, "fixture-secret");
}

#[test]
fn malformed_or_incomplete_registration_is_rejected_without_echoing_input() {
    for input in [
        "private-invalid-json",
        r#"{"client_id":"fixture-id"}"#,
        r#"{"client_secret":"private-secret"}"#,
        r#"{"client_id":"fixture-id","client_secret":""}"#,
        r#"{"client_id":"fixture-id","client_secret":"private\nsecret"}"#,
        r#"{"client_id":"fixture id","client_secret":"private-secret"}"#,
    ] {
        let error = github_oauth::parse(input)
            .err()
            .expect("invalid registration");
        assert!(!error.contains("private"));
    }
}
