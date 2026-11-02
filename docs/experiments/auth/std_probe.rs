use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let scratch = PathBuf::from(std::env::args_os().nth(1).expect("scratch directory"));
    let path = scratch.join("synthetic-owner-token");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .expect("create synthetic credential");
    file.write_all(b"synthetic fixture, never an application token\n")
        .unwrap();
    file.sync_all().unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .is_err());
    println!("PASS Rust creates mode 0600 credential and refuses overwrite");

    let child = Command::new("/usr/bin/env")
        .env_clear()
        .env("ELSE_AUTH_SYNTHETIC_ALLOWLIST", "fixture")
        .output()
        .expect("launch harmless environment probe");
    assert!(child.status.success());
    assert_eq!(child.stdout, b"ELSE_AUTH_SYNTHETIC_ALLOWLIST=fixture\n");
    println!("PASS Rust env_clear child inherits only synthetic allowlist");
}
