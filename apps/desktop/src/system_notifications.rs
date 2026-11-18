//! Notifications posted to the operating system. On Linux gpui sends them
//! without an icon or desktop entry, so notification centers draw a letter
//! instead of the app icon, and it neither replaces nor takes down a
//! notification by its tag; there they go out from here, naming the
//! installed desktop entry.

use gpui_kit::{App, SystemNotification, SystemNotificationResponse};

pub const APP_ID: &str = "else-bot";
pub const APP_NAME: &str = "else.bot";

/// Post `notification`, replacing the one still showing under its tag.
pub fn show(cx: &mut App, notification: SystemNotification) {
    #[cfg(target_os = "linux")]
    linux::show(cx, notification);
    #[cfg(not(target_os = "linux"))]
    cx.show_system_notification(notification);
}

/// Take down the notification showing under `tag`, if any.
pub fn dismiss(cx: &mut App, tag: &str) {
    #[cfg(target_os = "linux")]
    linux::dismiss(cx, tag);
    #[cfg(not(target_os = "linux"))]
    cx.dismiss_system_notification(tag);
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
    use futures_util::StreamExt;
    use gpui_kit::{
        App, AppContext as _, Global, SharedString, SystemNotification, SystemNotificationResponse,
    };
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;
    use tokio::sync::mpsc;
    use zbus::zvariant::Value;

    const OPEN: &str = "default";
    /// Opens are clicks; a full queue drops the extra ones.
    const PENDING_OPENS: usize = 16;
    /// Posts and take-downs; the UI never waits, so a full queue drops one.
    const PENDING_COMMANDS: usize = 64;

    type Callback = Box<dyn FnMut(SystemNotificationResponse, &mut App)>;

    enum Command {
        Show(SystemNotification),
        Dismiss(SharedString),
    }

    struct Notifier {
        commands: mpsc::Sender<Command>,
        callback: Rc<RefCell<Option<Callback>>>,
        _deliver: gpui_kit::Task<()>,
        _post: gpui_kit::Task<()>,
    }

    impl Global for Notifier {}

    fn notifier(cx: &mut App) -> &Notifier {
        if !cx.has_global::<Notifier>() {
            let (commands, received) = mpsc::channel(PENDING_COMMANDS);
            let (opened, mut tags) = mpsc::channel(PENDING_OPENS);
            let callback: Rc<RefCell<Option<Callback>>> = Rc::default();
            let current = callback.clone();
            let deliver = cx.spawn(async move |cx| {
                while let Some(tag) = tags.recv().await {
                    let response = SystemNotificationResponse {
                        tag,
                        action_id: None,
                    };
                    // Taken out for the call, which may register a new handler.
                    let taken = current.borrow_mut().take();
                    if let Some(mut taken) = taken {
                        cx.update(|cx| taken(response, cx));
                        current.borrow_mut().get_or_insert(taken);
                    }
                }
            });
            let post = cx.background_spawn(async move {
                if let Err(error) = post(received, opened).await {
                    eprintln!("system notifications stopped: {error}");
                }
            });
            cx.set_global(Notifier {
                commands,
                callback,
                _deliver: deliver,
                _post: post,
            });
        }
        cx.global::<Notifier>()
    }

    fn send(cx: &mut App, command: Command) {
        if let Err(mpsc::error::TrySendError::Full(_)) = notifier(cx).commands.try_send(command) {
            eprintln!("dropped a system notification: too many pending");
        }
    }

    pub(super) fn show(cx: &mut App, notification: SystemNotification) {
        send(cx, Command::Show(notification));
    }

    pub(super) fn dismiss(cx: &mut App, tag: &str) {
        send(cx, Command::Dismiss(tag.to_owned().into()));
    }

    pub(super) fn on_response(
        cx: &mut App,
        callback: impl FnMut(SystemNotificationResponse, &mut App) + 'static,
    ) {
        *notifier(cx).callback.borrow_mut() = Some(Box::new(callback));
    }

    /// What the notification server shows for each tag: its ID, and where on
    /// the connection the post that got it was answered. Posting a tag again
    /// replaces that notification instead of adding another.
    #[derive(Debug)]
    struct Showing<At> {
        by_tag: HashMap<SharedString, (u32, At)>,
    }

    impl<At: Ord + Copy> Default for Showing<At> {
        fn default() -> Self {
            Self {
                by_tag: HashMap::new(),
            }
        }
    }

    impl<At: Ord + Copy> Showing<At> {
        /// The ID a post under `tag` replaces; 0 asks for a new one.
        fn replaces(&self, tag: &str) -> u32 {
            self.by_tag.get(tag).map_or(0, |(id, _)| *id)
        }

        fn posted(&mut self, tag: SharedString, id: u32, at: At) {
            self.by_tag.insert(tag, (id, at));
        }

        fn take(&mut self, tag: &str) -> Option<u32> {
            self.by_tag.remove(tag).map(|(id, _)| id)
        }

        fn tag(&self, id: u32) -> Option<&SharedString> {
            self.by_tag
                .iter()
                .find(|(_, (shown, _))| *shown == id)
                .map(|(tag, _)| tag)
        }

        /// The server closed `id`: the owner dismissed it, or it expired. A
        /// close sent before a later post reused the ID ended the old card,
        /// not the one now showing.
        fn closed(&mut self, id: u32, at: At) {
            self.by_tag
                .retain(|_, (shown, posted)| *shown != id || *posted > at);
        }

        /// Another server took over; its IDs mean nothing to the old ones.
        fn restarted(&mut self) {
            self.by_tag.clear();
        }
    }

    /// Owns the session bus connection and what it has showing. Commands
    /// and the server's signals are handled one at a time, the signals in the
    /// order the server sent them, so a click is routed before its close.
    async fn post(
        mut commands: mpsc::Receiver<Command>,
        opened: mpsc::Sender<SharedString>,
    ) -> zbus::Result<()> {
        let connection = zbus::Connection::session().await?;
        let server = zbus::Proxy::new(
            &connection,
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
        )
        .await?;
        let mut signals = server.receive_all_signals().await?;
        let mut owners = server.receive_owner_changed().await?;
        let mut showing = Showing::default();
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(Command::Show(notification)) => {
                        let replaces = showing.replaces(&notification.tag);
                        let hints = HashMap::from([("desktop-entry", Value::from(APP_ID))]);
                        let body = (
                            APP_NAME,
                            replaces,
                            APP_ID,
                            notification.title.as_ref(),
                            notification.body.as_ref(),
                            vec![OPEN, OPEN],
                            hints,
                            -1i32,
                        );
                        let reply = server.call_method("Notify", &body).await;
                        match reply.and_then(|reply| Ok((reply.body().deserialize::<u32>()?, reply))) {
                            Ok((id, reply)) => {
                                showing.posted(notification.tag, id, reply.recv_position())
                            }
                            Err(error) => eprintln!("failed to show system notification: {error}"),
                        }
                    }
                    Some(Command::Dismiss(tag)) => {
                        if let Some(id) = showing.take(&tag)
                            && let Err(error) = server.call_method("CloseNotification", &id).await
                        {
                            eprintln!("failed to close system notification: {error}");
                        }
                    }
                    None => return Ok(()),
                },
                Some(signal) = signals.next() => {
                    let body = signal.body();
                    match signal.header().member().map(|member| member.as_str()) {
                        Some("ActionInvoked") => {
                            if let Ok((id, action)) = body.deserialize::<(u32, String)>()
                                && action == OPEN
                                && let Some(tag) = showing.tag(id)
                            {
                                let _ = opened.try_send(tag.clone());
                            }
                        }
                        Some("NotificationClosed") => {
                            if let Ok((id, _reason)) = body.deserialize::<(u32, u32)>() {
                                showing.closed(id, signal.recv_position());
                            }
                        }
                        _ => {}
                    }
                }
                Some(_) = owners.next() => showing.restarted(),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::Showing;

        #[test]
        fn a_tag_posted_again_replaces_its_notification_until_it_closes() {
            let mut showing = Showing::default();
            assert_eq!(showing.replaces("bot-a"), 0);
            showing.posted("bot-a".into(), 7, 1);
            showing.posted("bot-b".into(), 8, 2);
            assert_eq!(showing.replaces("bot-a"), 7);
            assert_eq!(showing.tag(8).map(AsRef::as_ref), Some("bot-b"));

            showing.closed(7, 3);
            assert_eq!(showing.replaces("bot-a"), 0, "closed by the owner");
            assert_eq!(showing.tag(7), None);

            assert_eq!(showing.take("bot-b"), Some(8));
            assert_eq!(showing.take("bot-b"), None, "taken down once");
        }

        #[test]
        fn a_close_sent_before_a_replacement_keeps_the_replacement() {
            let mut showing = Showing::default();
            showing.posted("bot-a".into(), 7, 1);
            // The old card expired while the server handled the replacement,
            // which kept its ID.
            showing.posted("bot-a".into(), 7, 3);
            showing.closed(7, 2);
            assert_eq!(showing.replaces("bot-a"), 7);
        }

        #[test]
        fn a_new_server_forgets_the_old_servers_notifications() {
            let mut showing = Showing::default();
            showing.posted("bot-a".into(), 1, 1);
            showing.restarted();
            assert_eq!(showing.replaces("bot-a"), 0);
            assert_eq!(showing.tag(1), None, "ID 1 is another app's now");
        }
    }
}
