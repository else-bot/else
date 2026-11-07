mod api;
mod app;
mod appearance;
mod attachments;
mod avatar;
mod config;
mod connection;
mod email;
#[cfg(test)]
mod icon_assets_tests;
mod inbox;
mod launch;
mod mcp_oauth;
mod model;
mod oauth_callback;
mod preferences;
mod profile_picker;
mod sidebar;
mod socials;
mod updates;
mod window_frame;

use gpui_kit::*;

gpui_kit::assets::icon_assets!(
    AppIcons,
    [
        FolderInput,
        BellDot,
        Bell,
        BellOff,
        PencilLine,
        PanelRightOpen,
        PanelRightClose,
        Trash,
        TriangleAlert,
        Check,
        ChevronRight,
        ChevronLeft,
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
        Settings,
        Server,
        Mail,
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
        CircleCheck,
        CircleOff,
        Users,
        MessageSquareOff,
        MessageSquare,
        Plug,
        CloudUpload,
        Pencil,
        Lock,
        Folder,
        ArrowRight,
        ArrowLeft,
        Maximize,
        Minimize,
        CalendarX,
        RefreshCw,
        Download,
        Terminal,
        ShieldCheck,
        Star,
        RotateCw
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
        println!("else-desktop {}", env!("ELSE_VERSION"));
        return;
    }
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
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            let frame_path = launch
                .store
                .as_ref()
                .ok()
                .filter(|_| cfg!(target_os = "macos"))
                .map(|store| store.window_path());
            let min_size = size(px(640.), px(420.));
            let restored = frame_path
                .as_deref()
                .and_then(|path| window_frame::restore(path, min_size, cx));
            let options = WindowOptions {
                app_id: Some("else-bot".into()),
                icon: Some(std::sync::Arc::new(
                    image::load_from_memory(include_bytes!(
                        "../../../media/branding/icons/else-bot-256.png"
                    ))
                    .expect("decode the bundled application icon")
                    .into_rgba8(),
                )),
                window_bounds: Some(restored.map_or_else(
                    || {
                        WindowBounds::Windowed(Bounds::centered(
                            None,
                            size(px(1100.), px(720.)),
                            cx,
                        ))
                    },
                    |(bounds, _)| bounds,
                )),
                display_id: restored.map(|(_, display)| display),
                window_min_size: Some(min_size),
                ..window_chrome(title)
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                appearance::observe_window(window, cx);
                if cfg!(target_os = "macos") {
                    window.on_window_should_close(cx, |_, cx| {
                        cx.hide();
                        false
                    });
                }
                cx.new(|cx| {
                    if let Some(path) = frame_path {
                        window_frame::remember(path, window, cx);
                    }
                    connection::Shell::new(launch, handle, window, cx)
                })
            })
            .expect("open the else.bot window");
        });
    drop(runtime);
}
