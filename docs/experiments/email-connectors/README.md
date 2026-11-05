# Email protocol dependency probe

This isolated workspace checks that the selected published Pimalaya protocol crates resolve and compile together on the local Rust toolchain. It changes no production dependencies.

Run `cargo check --manifest-path docs/experiments/email-connectors/Cargo.toml` from the repository root.

The probe enables only I/O-free protocol layers. It does not verify high-level blocking clients, TLS configuration, OAuth, a Tokio protocol driver, provider behavior, or actual mail delivery. See `docs/design/email-connectors/research.md` for the adoption gates.

The first run passed with Rust 1.98.0. Its output is saved in `docs/design/email-connectors/dependency-check.log`. The lockfile fixes the dependency versions for reproduction.
