#[path = "../build_support/google_oauth.rs"]
mod google_oauth;

#[test]
fn desktop_registration_is_parsed_without_copying_other_credentials() {
    let registration = google_oauth::parse(r#"{
        "installed": {"client_id":"fixture.apps.googleusercontent.com", "client_secret":"fixture-secret", "project_id":"fixture"}
    }"#).unwrap();
    assert_eq!(registration.client_id, "fixture.apps.googleusercontent.com");
    assert_eq!(registration.client_secret, "fixture-secret");
}

#[test]
fn malformed_web_or_incomplete_registration_is_rejected_without_echoing_input() {
    for input in [
        "private-invalid-json",
        r#"{"web":{"client_id":"fixture.apps.googleusercontent.com","client_secret":"private-web-secret"}}"#,
        r#"{"installed":{"client_id":"bad-id","client_secret":"private-secret"}}"#,
        r#"{"installed":{"client_id":"fixture.apps.googleusercontent.com"}}"#,
        r#"{"installed":{"client_id":"fixture.apps.googleusercontent.com","client_secret":""}}"#,
    ] {
        let error = google_oauth::parse(input)
            .err()
            .expect("invalid configuration must fail");
        assert!(!error.contains("private"));
    }
}
