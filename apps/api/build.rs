use std::{env, fs, path::PathBuf};

#[path = "build_support/github_oauth.rs"]
mod github_oauth;

// Embedded migrations must rebuild the binary when a migration is added or
// changed; otherwise `migrate` could report success with stale contents.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build_support/github_oauth.rs");
    println!("cargo:rerun-if-env-changed=ELSE_GITHUB_OAUTH_CONFIG");
    let source = match env::var_os("ELSE_GITHUB_OAUTH_CONFIG") {
        None => "None".to_owned(),
        Some(path) => {
            let path = PathBuf::from(path);
            println!("cargo:rerun-if-changed={}", path.display());
            let input = fs::read_to_string(path)
                .expect("Cannot read the file specified by ELSE_GITHUB_OAUTH_CONFIG.");
            let registration =
                github_oauth::parse(&input).unwrap_or_else(|error| panic!("{error}"));
            format!(
                "Some(({:?}, {:?}))",
                registration.client_id, registration.client_secret
            )
        }
    };
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    fs::write(output.join("github_oauth.rs"), source)
        .expect("Cannot write GitHub build configuration.");
}
