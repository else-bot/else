use std::{env, fs, path::PathBuf};

#[path = "build_support/google_oauth.rs"]
mod google_oauth;

fn main() {
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
}
