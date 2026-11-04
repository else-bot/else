//! else.bot desktop: a native GPUI messaging client for the else.bot API.

mod api;
mod app;
mod appearance;
mod avatar;
mod config;
mod connection;
mod inbox;
mod model;
mod profile_picker;
mod sidebar;

use gpui_kit::*;

gpui_kit::assets::icon_assets!(
    AppIcons,
    [
        FolderInput,
        BellDot,
        PencilLine,
        PanelRightOpen,
        PanelRightClose,
        Trash,
        TriangleAlert,
        Check,
        ChevronRight,
        Reply,
        Copy,
        Close,
        ArrowUp,
        ArrowDown,
        Ellipsis,
        SquarePen,
        FolderPlus
    ]
);

struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        if let Some(bytes) = AppIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(AppIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// macOS draws its traffic lights over the app's own title bar; elsewhere
/// the desktop keeps its usual decorations.
fn window_chrome(title: String) -> WindowOptions {
    let title = Some(title.into());
    if cfg!(target_os = "macos") {
        let base = gpui_kit::component::TitleBar::window_options();
        WindowOptions {
            titlebar: base.titlebar.map(|bar| TitlebarOptions { title, ..bar }),
            ..base
        }
    } else {
        WindowOptions {
            titlebar: Some(TitlebarOptions {
                title,
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}

fn main() {
    if matches!(
        std::env::args().nth(1).as_deref(),
        Some("version" | "--version" | "-V")
    ) {
        println!("else-desktop {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let _ = dotenvy::dotenv();
    // HTTP and streaming run on this runtime, separate from GPUI's executor.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("else-api-client")
        .build()
        .expect("start the API client runtime");
    let store = config::ProfileStore::from_env(env);
    let seed = config::Seed::from_env(env);
    let handle = runtime.handle().clone();
    let title = env("ELSE_DESKTOP_TITLE").unwrap_or_else(|| "else.bot".into());

    gpui_kit::application()
        .with_assets(AppAssets)
        .run(move |cx| {
            cx.set_app_identity("else-bot", "else.bot");
            gpui_kit::init(cx);
            appearance::init(cx);
            app::bind_keys(cx);
            app::install_menus(cx);
            // Closing the window ends the desktop only; API work continues.
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            let options = WindowOptions {
                app_id: Some("else-bot".into()),
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1100.), px(720.)),
                    cx,
                ))),
                window_min_size: Some(size(px(640.), px(420.))),
                ..window_chrome(title)
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                appearance::observe_window(window, cx);
                cx.new(|cx| connection::Shell::new(store, seed, handle, window, cx))
            })
            .expect("open the else.bot window");
        });
    drop(runtime);
}
