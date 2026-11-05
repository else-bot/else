//! else.bot desktop: a native GPUI messaging client for the else.bot API.

mod api;
mod app;
mod appearance;
mod attachments;
mod avatar;
mod config;
mod connection;
mod inbox;
mod launch;
mod model;
mod preferences;
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
        ChevronDown,
        Reply,
        FaceSlightlySmilingPlus,
        Copy,
        Close,
        ArrowUp,
        ArrowDown,
        SquarePen,
        FolderPlus,
        Laptop,
        Eye,
        EyeOff,
        Cpu,
        SlidersHorizontal,
        Zap,
        Clock,
        Circle,
        Search,
        ChevronUp,
        FileText,
        Pause,
        LoaderCircle,
        CircleQuestionMark,
        BadgeCheck,
        LockKeyhole,
        Square,
        CircleX,
        CircleOff,
        MessageSquareOff,
        Folder,
        ArrowRight,
        CalendarX
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

fn settings() -> impl Fn(&str) -> Option<String> {
    let dotenv: Vec<(String, String)> = dotenvy::dotenv_iter()
        .map(|iter| iter.filter_map(Result::ok).collect())
        .unwrap_or_default();
    move |name| {
        env(name).or_else(|| {
            dotenv
                .iter()
                .rev()
                .find(|(key, value)| key == name && !value.is_empty())
                .map(|(_, value)| value.clone())
        })
    }
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
    // HTTP and streaming run on this runtime, separate from GPUI's executor.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("else-api-client")
        .build()
        .expect("start the API client runtime");
    let get = settings();
    let exe = std::env::current_exe().ok();
    let launch = launch::Launch::discover(&get, exe.as_deref());
    let handle = runtime.handle().clone();
    let title = get("ELSE_DESKTOP_TITLE").unwrap_or_else(|| "else.bot".into());

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
                cx.new(|cx| connection::Shell::new(launch, handle, window, cx))
            })
            .expect("open the else.bot window");
        });
    drop(runtime);
}
