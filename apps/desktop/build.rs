use std::{env, path::PathBuf};

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
    // The installer builds from a detached checkout, so HEAD changes with
    // every build; release_version already reruns this script when it does.
    let commit = std::process::Command::new("git")
        .current_dir(&manifest_dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .unwrap_or_default();
    println!("cargo:rustc-env=ELSE_COMMIT={}", commit.trim());
}
