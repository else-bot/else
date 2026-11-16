//! Notifications posted to the operating system. On Linux gpui sends them
//! without an icon or desktop entry, so notification centers draw a letter
//! instead of the app icon; there they go out from here, naming the
//! installed desktop entry.

use gpui_kit::{App, SystemNotification, SystemNotificationResponse};

pub const APP_ID: &str = "else-bot";
pub const APP_NAME: &str = "else.bot";

pub fn show(cx: &App, notification: SystemNotification) {
    #[cfg(target_os = "linux")]
    linux::show(cx, notification);
    #[cfg(not(target_os = "linux"))]
    cx.show_system_notification(notification);
}

/// Call `callback` when the user opens a notification. The process keeps one
/// handler; a later call replaces it.
pub fn on_response(
    cx: &mut App,
    callback: impl FnMut(SystemNotificationResponse, &mut App) + 'static,
) {
    #[cfg(target_os = "linux")]
    linux::on_response(cx, callback);
    #[cfg(not(target_os = "linux"))]
    cx.on_system_notification_response(callback);
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{APP_ID, APP_NAME};
    use gpui_kit::{App, Global, SharedString, SystemNotification, SystemNotificationResponse};
    use notify_rust::{Hint, Notification};
    use tokio::sync::mpsc;

    const OPEN: &str = "default";

    struct Responses {
        opened: mpsc::UnboundedSender<SharedString>,
        _deliver: gpui_kit::Task<()>,
    }

    impl Global for Responses {}

    pub(super) fn show(cx: &App, notification: SystemNotification) {
        let opened = cx.try_global::<Responses>().map(|r| r.opened.clone());
        let built = Notification::new()
            .appname(APP_NAME)
            .icon(APP_ID)
            .hint(Hint::DesktopEntry(APP_ID.into()))
            .summary(&notification.title)
            .body(&notification.body)
            .action(OPEN, OPEN)
            .finalize();
        let tag = notification.tag;
        // `show` connects to the session bus and `wait_for_action` blocks
        // until the notification closes, so both run off the UI thread.
        let spawned = std::thread::Builder::new()
            .name("system-notification".into())
            .spawn(move || match built.show() {
                Ok(handle) => handle.wait_for_action(|action| {
                    if let (OPEN, Some(opened)) = (action, &opened) {
                        let _ = opened.send(tag.clone());
                    }
                }),
                Err(error) => eprintln!("failed to show system notification: {error}"),
            });
        if let Err(error) = spawned {
            eprintln!("failed to spawn system notification thread: {error}");
        }
    }

    pub(super) fn on_response(
        cx: &mut App,
        mut callback: impl FnMut(SystemNotificationResponse, &mut App) + 'static,
    ) {
        let (opened, mut tags) = mpsc::unbounded_channel();
        let deliver = cx.spawn(async move |cx| {
            while let Some(tag) = tags.recv().await {
                let response = SystemNotificationResponse {
                    tag,
                    action_id: None,
                };
                cx.update(|cx| callback(response, cx));
            }
        });
        cx.set_global(Responses {
            opened,
            _deliver: deliver,
        });
    }
}
