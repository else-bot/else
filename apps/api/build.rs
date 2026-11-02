// Embedded migrations must rebuild the binary when a migration is added or
// changed; otherwise `migrate` could report success with stale contents.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
