use std::{env, fs, path::PathBuf};

#[path = "build_support/google_oauth.rs"]
mod google_oauth;
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
    println!("cargo:rerun-if-env-changed=ELSE_MICROSOFT_CLIENT_ID");
    println!("cargo:rerun-if-env-changed=ELSE_GOOGLE_OAUTH_CONFIG");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build_support/google_oauth.rs");
    let source = match env::var_os("ELSE_GOOGLE_OAUTH_CONFIG") {
        None => "None".to_owned(),
        Some(path) => {
            let path = PathBuf::from(path);
            println!("cargo:rerun-if-changed={}", path.display());
            let input = fs::read_to_string(path)
                .expect("Cannot read the file specified by ELSE_GOOGLE_OAUTH_CONFIG.");
            let registration =
                google_oauth::parse(&input).unwrap_or_else(|error| panic!("{error}"));
            format!(
                "Some(({:?}, {:?}))",
                registration.client_id, registration.client_secret
            )
        }
    };
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    fs::write(output.join("google_oauth.rs"), source)
        .expect("Cannot write Google build configuration.");
    let microsoft = env::var("ELSE_MICROSOFT_CLIENT_ID").ok().map(|id| {
        uuid::Uuid::parse_str(&id)
            .expect("ELSE_MICROSOFT_CLIENT_ID must be a UUID.")
            .to_string()
    });
    fs::write(output.join("microsoft_oauth.rs"), format!("{microsoft:?}"))
        .expect("Cannot write Microsoft build configuration.");
}
