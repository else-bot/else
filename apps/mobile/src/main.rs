//! Runs the app on the Mac's native backend, for quick iteration.

#[cfg(target_os = "macos")]
fn main() -> impl std::process::Termination {
    env_logger::init();
    nsh_platform_macos::run_app(
        nsh_platform_macos::AppSource::Rust(nsh_platform_macos::Root::of::<else_mobile::App>()),
        nsh_platform_macos::AppOptions {
            title: "else.bot".to_owned(),
            size: (402, 874),
            ..Default::default()
        },
    )
}

#[cfg(not(target_os = "macos"))]
fn main() {}
