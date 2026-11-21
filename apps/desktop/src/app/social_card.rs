use crate::app::conversation::ConversationView;
use crate::app::transcript::CardKey;
use else_wire::socials::SetupStatus;
use gpui_kit::component::{ActiveTheme as _, v_flex};
use gpui_kit::*;
use uuid::Uuid;

use super::RowKey;

pub(super) struct SocialCard {
    view: Entity<crate::socials::SocialsView>,
    status: SetupStatus,
    _layout: Subscription,
}

impl ConversationView {
    pub(super) fn clear_social_forms(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for (_, card) in self.cards.socials.drain() {
            card.view.update(cx, |form, cx| form.clear(window, cx));
        }
    }

    pub(super) fn sync_social_cards(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.inputs.hidden || !self.inputs.connected {
            self.clear_social_forms(window, cx);
            return;
        }
        let setups: Vec<_> = self
            .conversation
            .iter()
            .flat_map(|c| c.turns_in_order())
            .flat_map(|t| &t.social_setups)
            .filter(|s| s.is_pending() || s.status == SetupStatus::Connected)
            .cloned()
            .collect();
        self.cards.socials.retain(|id, card| {
            let keep = setups.iter().any(|s| s.id == *id);
            if !keep {
                card.view.update(cx, |form, cx| form.clear(window, cx));
            }
            keep
        });
        for setup in setups {
            if let Some(card) = self.cards.socials.get_mut(&setup.id) {
                if card.status != setup.status {
                    card.status = setup.status;
                    if let Some(connection) = setup
                        .connection_id
                        .filter(|_| setup.status == SetupStatus::Connected)
                    {
                        card.view.update(cx, |form, cx| {
                            form.show_receipt(connection, setup.provider, window, cx)
                        });
                    }
                }
            } else {
                let id = setup.id;
                let client = self.client.clone();
                let form = cx.new(|cx| {
                    if let Some(connection) = setup
                        .connection_id
                        .filter(|_| setup.status == SetupStatus::Connected)
                    {
                        crate::socials::SocialsView::new_receipt(
                            client,
                            (id, setup.provider),
                            connection,
                            window,
                            cx,
                        )
                    } else {
                        crate::socials::SocialsView::new(
                            client,
                            Some((id, setup.provider)),
                            window,
                            cx,
                        )
                    }
                });
                let layout = cx.observe_in(&form, window, move |app, _, _, cx| {
                    app.remeasure_row(RowKey::Card(CardKey::SocialSetup(id)), cx);
                });
                self.cards.socials.insert(
                    id,
                    SocialCard {
                        view: form,
                        status: setup.status,
                        _layout: layout,
                    },
                );
            }
        }
    }

    pub(super) fn render_social_card(&self, id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let Some(setup) = self.conversation.as_ref().and_then(|c| c.social_setup(id)) else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let mut row = v_flex().w_full().gap_2();
        if setup.is_pending() {
            row = row.child(div().text_sm().child(setup.reason.clone()));
        }
        if let Some(card) = self.cards.socials.get(&id) {
            return row.child(card.view.clone()).into_any_element();
        }
        let text = match setup.status {
            SetupStatus::Connected => format!(
                "{} connected. Manage connections in Settings → Socials.",
                setup.provider.label()
            ),
            SetupStatus::Cancelled => {
                format!("{} connection request cancelled.", setup.provider.label())
            }
            SetupStatus::Pending | SetupStatus::Unknown => {
                "Reconnect to the API to set up this connection.".into()
            }
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
