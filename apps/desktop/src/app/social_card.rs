use gpui_kit::component::{ActiveTheme as _, v_flex};
use gpui_kit::*;
use uuid::Uuid;

use super::{Connection, ElseApp, RowKey};

pub(super) struct SocialCard {
    view: Entity<crate::socials::SocialsView>,
    status: String,
    _layout: Subscription,
}

impl ElseApp {
    pub(super) fn clear_social_forms(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = &mut self.view {
            for (_, card) in view.social_forms.drain() {
                card.view.update(cx, |form, cx| form.clear(window, cx));
            }
        }
    }

    pub(super) fn sync_social_forms(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.hidden || self.connection != Connection::Connected {
            self.clear_social_forms(window, cx);
            return;
        }
        let Some(view) = &mut self.view else {
            return;
        };
        let setups: Vec<_> = view
            .conversation
            .iter()
            .flat_map(|c| c.turns_in_order())
            .flat_map(|t| &t.social_setups)
            .filter(|s| s.is_pending() || s.status == "connected")
            .cloned()
            .collect();
        view.social_forms.retain(|id, card| {
            let keep = setups.iter().any(|s| s.id == *id);
            if !keep {
                card.view.update(cx, |form, cx| form.clear(window, cx));
            }
            keep
        });
        let bot = view.bot_id;
        for setup in setups {
            if let Some(card) = view.social_forms.get_mut(&setup.id) {
                if card.status != setup.status {
                    card.status = setup.status.clone();
                    if let Some(connection) =
                        setup.connection_id.filter(|_| setup.status == "connected")
                    {
                        card.view.update(cx, |form, cx| {
                            form.show_receipt(connection, setup.provider, window, cx)
                        });
                    }
                }
            } else {
                let id = setup.id;
                let client = self.client.clone();
                let bots = self
                    .bots
                    .iter()
                    .filter(|b| b.is_active())
                    .cloned()
                    .collect();
                let form = cx.new(|cx| {
                    if let Some(connection) =
                        setup.connection_id.filter(|_| setup.status == "connected")
                    {
                        crate::socials::SocialsView::new_receipt(
                            client,
                            bots,
                            (id, setup.provider, bot),
                            connection,
                            window,
                            cx,
                        )
                    } else {
                        crate::socials::SocialsView::new(
                            client,
                            bots,
                            Some((id, setup.provider, bot)),
                            window,
                            cx,
                        )
                    }
                });
                let layout = cx.observe_in(&form, window, move |app, _, _, cx| {
                    app.remeasure_row(RowKey::SocialSetup(id), cx);
                });
                view.social_forms.insert(
                    id,
                    SocialCard {
                        view: form,
                        status: setup.status.clone(),
                        _layout: layout,
                    },
                );
            }
        }
    }

    pub(super) fn render_social_card(&self, id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let Some(view) = &self.view else {
            return div().into_any_element();
        };
        let Some(setup) = view.conversation.as_ref().and_then(|c| c.social_setup(id)) else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let mut row = v_flex().w_full().gap_2();
        if setup.is_pending() {
            row = row.child(div().text_sm().child(setup.reason.clone()));
        }
        if let Some(card) = view.social_forms.get(&id) {
            return row.child(card.view.clone()).into_any_element();
        }
        let text = match setup.status.as_str() {
            "connected" => format!(
                "{} connected. Manage access and instructions in Settings → Socials.",
                setup.provider.label()
            ),
            "cancelled" => format!("{} connection request cancelled.", setup.provider.label()),
            _ => "Reconnect to the API to set up this connection.".into(),
        };
        row.child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(text),
        )
        .into_any_element()
    }
}
