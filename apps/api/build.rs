use std::{env, fs, path::PathBuf};

#[path = "build_support/github_oauth.rs"]
mod github_oauth;
#[path = "../../build_support/release_version.rs"]
mod release_version;

fn main() {
    println!("cargo:rerun-if-changed=../../build_support/release_version.rs");
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest dir"));
    let workspace = env::var("CARGO_PKG_VERSION").expect("Cargo package version");
    println!(
        "cargo:rustc-env=ELSE_VERSION={}",
        release_version::version(&manifest_dir, &workspace)
    );
    // Embedded migrations must rebuild the binary when a migration is added or
    // changed; otherwise `migrate` could report success with stale contents.
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
