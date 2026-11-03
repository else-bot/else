//! else.bot desktop: a native GPUI messaging client for the else.bot API.

mod api;
mod app;
mod config;
mod model;
mod sidebar;

use std::sync::Arc;

use gpui_kit::*;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn main() {
    if matches!(
        std::env::args().nth(1).as_deref(),
        Some("version" | "--version" | "-V")
    ) {
        println!("else-desktop {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    // Same `.env` as the API, so `ELSE_DATA_DIR` or `OWNER_TOKEN_FILE` set
    // there point both at one installation. Process variables win.
    let _ = dotenvy::dotenv();
    // HTTP and streaming run on this runtime, separate from GPUI's executor.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("else-api-client")
        .build()
        .expect("start the API client runtime");
    let client = config::Config::from_env(env).and_then(|config| {
        let token = config.load_token(env)?;
        Ok(Arc::new(api::Client::new(
            config.origin,
            &token,
            runtime.handle().clone(),
        )))
    });

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            app::bind_keys(cx);
            // Closing the window ends the desktop only; API work continues.
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            let options = WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("else.bot".into()),
                    ..Default::default()
                }),
                app_id: Some("else-bot".into()),
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1100.), px(720.)),
                    cx,
                ))),
                window_min_size: Some(size(px(640.), px(420.))),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| app::ElseApp::new(client.clone(), window, cx))
            })
            .expect("open the else.bot window");
        });
    drop(runtime);
}
