//! Read acknowledgement and system notifications for bot replies.
//!
//! A reply counts as read only when its end is on screen: the window is
//! focused and drawn, the messaging view is shown, and the bottom of the
//! latest bot message of the loaded transcript is inside the list's
//! viewport after layout. Only that loaded text is acknowledged.

use std::time::Duration;

use super::conversation::ReplyLayout;
use super::{Connection, ElseApp};
use crate::api::{ClientError, ReadCommand, ReplyPosition};
use crate::inbox::{Mark, Outcome};
use gpui_kit::*;
use uuid::Uuid;

const ALERT_DELAY: Duration = Duration::from_millis(1500);
const RETRY_DELAY: Duration = Duration::from_secs(2);
const READ_SPACING: Duration = Duration::from_millis(400);
const UNREAD_DIVIDER_DELAY: Duration = Duration::from_secs(5);

impl ElseApp {
    pub(super) fn check_reading(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.view.as_mut() {
            view._unread_dismissal = None;
            view.reply_layout = None;
            view.latest_seen = false;
            window.refresh();
            cx.notify();
        }
    }

    fn dismiss_unread_after_viewing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let attending = self.viewing_transcript(window, cx);
        let Some(view) = self.view.as_mut() else {
            return;
        };
        if !attending || !view.latest_seen {
            view._unread_dismissal = None;
            return;
        }
        if view.unread_since.is_none() || view._unread_dismissal.is_some() {
            return;
        }
        let bot = view.bot_id;
        view._unread_dismissal = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(UNREAD_DIVIDER_DELAY).await;
            let _ = this.update_in(cx, |app, window, cx| {
                if !app.viewing_transcript(window, cx) {
                    return;
                }
                let Some(view) = app.view.as_mut().filter(|view| view.bot_id == bot) else {
                    return;
                };
                if view.latest_seen {
                    view.unread_since = None;
                    view._unread_dismissal = None;
                    app.sync_rows(window, cx);
                }
            });
        }));
    }

    fn latest_loaded(&self) -> Option<ReplyPosition> {
        let item = self.view.as_ref()?.conversation.as_ref()?.latest_reply()?;
        Some(ReplyPosition {
            item_seq: item.seq,
            text_bytes: item.text.len() as i64,
        })
    }

    pub(super) fn reply_laid_out(
        &mut self,
        bot: Uuid,
        layout: ReplyLayout,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .latest_loaded()
            .is_none_or(|position| position.item_seq != layout.position.item_seq)
        {
            return;
        }
        let Some(view) = self.view.as_mut().filter(|view| view.bot_id == bot) else {
            return;
        };
        view.reply_layout = Some(layout);
        self.acknowledge_visible(window, cx);
    }

    fn attending(&self, window: &Window) -> bool {
        !self.hidden && self.welcome.is_none() && window.is_window_active() && window.is_visible()
    }

    fn viewing_transcript(&self, window: &Window, cx: &App) -> bool {
        self.attending(window)
            && self.image_viewer.is_none()
            && self
                .computer
                .as_ref()
                .is_none_or(|computer| !computer.read(cx).expanded)
    }

    fn acknowledge_visible(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(position) = self.latest_loaded() else {
            return;
        };
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let Some(layout) = view
            .reply_layout
            .filter(|layout| layout.position.item_seq == position.item_seq)
        else {
            return;
        };
        let viewport = view.list.viewport_bounds();
        let seen = viewport.size.height > px(0.)
            && layout.bounds.bottom() > viewport.top()
            && layout.bounds.bottom() <= viewport.bottom() + px(1.);
        let latest_seen = seen && layout.position == position;
        if view.latest_seen != latest_seen {
            view.latest_seen = latest_seen;
            cx.notify();
        }
        let bot_id = view.bot_id;
        if seen
            && layout.position.text_bytes > 0
            && self.attending(window)
            && let Some(bot) = self
                .bots
                .iter()
                .find(|bot| bot.id == bot_id && bot.is_active())
            && self.inbox.saw(bot, layout.position)
        {
            self.send_reads(bot_id, cx);
            self.settle_alerts(cx);
            cx.notify();
        }
        self.dismiss_unread_after_viewing(window, cx);
    }

    pub(super) fn unseen_below(&self) -> bool {
        let (Some(view), Some(position)) = (self.view.as_ref(), self.latest_loaded()) else {
            return false;
        };
        !view.latest_seen
            && !view.list.is_following_tail()
            && self
                .bots
                .iter()
                .find(|b| b.id == view.bot_id)
                .is_some_and(|bot| position > self.inbox.seen_through(bot))
    }

    pub(super) fn jump_to_latest(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.view.as_ref() {
            view.list.set_follow_mode(FollowMode::Tail);
        }
        self.check_reading(window, cx);
        cx.notify();
    }

    pub(super) fn is_unread(&self, id: Uuid) -> bool {
        self.bots
            .iter()
            .find(|b| b.id == id)
            .is_some_and(|bot| self.inbox.is_unread(bot))
    }

    pub(super) fn mark_bot(&mut self, id: Uuid, mark: Mark, cx: &mut Context<Self>) {
        let Some(bot) = self.active_bots().find(|b| b.id == id).cloned() else {
            return;
        };
        self.inbox.mark(&bot, mark);
        self.send_reads(id, cx);
        self.settle_alerts(cx);
        cx.notify();
    }

    fn send_reads(&mut self, bot: Uuid, cx: &mut Context<Self>) {
        if self.connection != Connection::Connected {
            return;
        }
        let Some(command) = self.inbox.next_command(bot) else {
            return;
        };
        let client = self.client.clone();
        cx.spawn(async move |this, cx| {
            let result = client.read(bot, command).await;
            let outcome = match &result {
                Ok(_) => Outcome::Done,
                Err(err) if err.outcome_unknown() => Outcome::Retry,
                Err(ClientError::Api {
                    retryable: true, ..
                }) => Outcome::Retry,
                Err(_) => Outcome::Refused,
            };
            let pause = match (outcome, command) {
                (Outcome::Retry, _) => Some(RETRY_DELAY),
                (_, ReadCommand::Through { .. }) => Some(READ_SPACING),
                _ => None,
            };
            if let Some(pause) = pause {
                cx.background_executor().timer(pause).await;
            }
            let _ = this.update(cx, |app, cx| {
                if outcome == Outcome::Refused
                    && let Err(error) = &result
                    && app.active_bots().any(|active| active.id == bot)
                {
                    app.notice = Some(format!("Could not save read state: {error}"));
                }
                app.inbox.finished(bot, outcome);
                app.send_reads(bot, cx);
                app.settle_alerts(cx);
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn resume_reads(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<Uuid> = self.active_bots().map(|b| b.id).collect();
        for id in ids {
            self.send_reads(id, cx);
        }
    }

    pub(super) fn observe_replies(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.inbox.observe(&self.bots) && self._alerts.is_none() {
            self._alerts = Some(cx.spawn_in(window, async move |this, cx| {
                cx.background_executor().timer(ALERT_DELAY).await;
                let _ = this.update_in(cx, |app, window, cx| app.announce(window, cx));
            }));
        }
        self.settle_alerts(cx);
    }

    fn announce(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._alerts = None;
        self.acknowledge_visible(window, cx);
        let viewing = self
            .attending(window)
            .then_some(
                self.view
                    .as_ref()
                    .filter(|v| {
                        v.latest_seen
                            || (v.list.is_following_tail()
                                && v.list.viewport_bounds().size.height > px(0.))
                    })
                    .map(|v| v.bot_id),
            )
            .flatten();
        for alert in self.inbox.alerts(&self.bots, |id| viewing == Some(id)) {
            cx.show_system_notification(SystemNotification {
                tag: crate::inbox::tag(self.scope, alert.bot).into(),
                title: alert.title.into(),
                body: alert.body.into(),
                actions: Vec::new(),
            });
        }
    }

    fn settle_alerts(&mut self, cx: &mut Context<Self>) {
        for bot in self.inbox.settle(&self.bots) {
            cx.dismiss_system_notification(&crate::inbox::tag(self.scope, bot));
        }
    }

    pub(crate) fn notification_scope(&self) -> Uuid {
        self.scope
    }

    pub(crate) fn has_unsaved_work(&self, cx: &App) -> bool {
        !self.composer.read(cx).value().trim().is_empty()
            || self.drafts.values().any(|draft| {
                draft.message().is_some() || !draft.files.is_empty() || draft.reply_to.is_some()
            })
            || self.pending.values().any(|list| !list.is_empty())
    }

    /// Take down every notification this connection showed.
    pub fn dismiss_notifications(&mut self, cx: &mut Context<Self>) {
        for bot in self.inbox.shown() {
            cx.dismiss_system_notification(&crate::inbox::tag(self.scope, bot));
        }
    }

    /// The connection editor covers (or uncovers) this view.
    pub fn set_hidden(&mut self, hidden: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.hidden = hidden;
        if hidden {
            self.computer = None;
            self._computer_layout = None;
            self.clear_social_forms(window, cx);
        } else {
            self.sync_social_forms(window, cx);
        }
        if !hidden {
            self.ensure_settings(window, cx);
            self.check_reading(window, cx);
        }
    }

    /// A notification was opened: show its bot if it belongs to this
    /// connection and is still there. Returns whether it did.
    pub fn open_notification(
        &mut self,
        scope: Uuid,
        bot: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if scope != self.scope || !self.active_bots().any(|b| b.id == bot) {
            return false;
        }
        self.open_bot(bot, window, cx);
        self.jump_to_latest(window, cx);
        true
    }
}
