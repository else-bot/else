//! The owner's reader for the email behind a mail-triggered turn. The
//! message comes from the server with the inbox's saved sign-in, never
//! through the bot, and stays unread. Its HTML renders offline in the
//! visual worker: remote content never loads, scripts never run, and links
//! open only from the list below the message.

use super::ElseApp;
use super::link_target::LinkTarget;
use crate::api::{Client, EmailMessage, EmailPerson};
use crate::attachments::byte_size;
use crate::controls::{Control as _, Scale, Tier};
use crate::model::TurnMail;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::{
    ActiveTheme as _, Icon, Selectable as _, Sizable as _, Theme, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::sync::Arc;

/// Tallest the scrolling message area grows before it scrolls.
const BODY_HEIGHT: Pixels = px(560.);

enum Reading {
    Loading { _task: Task<()> },
    Failed(String),
    Loaded(Arc<EmailMessage>),
}

/// Which of the message's bodies the reader shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shown {
    Html,
    Text,
}

/// One render of the HTML body at a width and scale.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Key {
    width: u32,
    scale: u32,
}

enum Html {
    Rendering { key: Key, _task: Task<()> },
    Ready { key: Key, page: Page },
    Failed { key: Key, error: String },
}

struct Page {
    image: Arc<RenderImage>,
    height: u32,
    clipped: bool,
    blocked: usize,
    links: Vec<String>,
}

pub(super) struct EmailReader {
    client: Arc<Client>,
    /// The turn's emails, oldest first, and the one shown.
    mails: Vec<TurnMail>,
    index: usize,
    reading: Reading,
    shown: Shown,
    html: Option<Html>,
    /// The body column's width in CSS pixels, known after the first layout.
    width: Option<u32>,
    links_open: bool,
}

impl EmailReader {
    fn new(client: Arc<Client>, mails: Vec<TurnMail>, cx: &mut Context<Self>) -> Self {
        let mut reader = Self {
            client,
            mails,
            index: 0,
            reading: Reading::Failed(String::new()),
            shown: Shown::Html,
            html: None,
            width: None,
            links_open: false,
        };
        reader.load(0, cx);
        reader
    }

    /// Shows the turn's email at `index`, fetching it again.
    fn load(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(mail) = self.mails.get(index).cloned() else {
            return;
        };
        let client = self.client.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = client.view_email(mail.account_id, mail.message).await;
            let _ = this.update(cx, |this, cx| {
                this.reading = match result {
                    Ok(message) => {
                        this.shown = if message.html.is_some() {
                            Shown::Html
                        } else {
                            Shown::Text
                        };
                        Reading::Loaded(Arc::new(message))
                    }
                    Err(error) => Reading::Failed(error.to_string()),
                };
                cx.notify();
            });
        });
        self.index = index;
        self.reading = Reading::Loading { _task: task };
        self.html = None;
        self.links_open = false;
        cx.notify();
    }

    fn title(&self) -> SharedString {
        match &self.reading {
            Reading::Loaded(message) => message
                .subject
                .clone()
                .filter(|subject| !subject.trim().is_empty())
                .unwrap_or_else(|| "(no subject)".into())
                .into(),
            _ => "Email".into(),
        }
    }

    /// Starts a render when the HTML body is shown and none matches `key`.
    fn render_html(&mut self, message: &Arc<EmailMessage>, key: Key, cx: &mut Context<Self>) {
        let current = match &self.html {
            Some(Html::Rendering { key: k, .. } | Html::Ready { key: k, .. })
            | Some(Html::Failed { key: k, .. }) => *k == key,
            None => false,
        };
        let Some(html) = message.html.clone().filter(|_| !current) else {
            return;
        };
        let source = else_visual::email::Source {
            html,
            inline: message.inline.clone(),
        };
        let task = cx.spawn(async move |this, cx| {
            let scale = key.scale as f32 / 100.0;
            let result = match else_visual::isolate::submit_email(&source, key.width, scale).await {
                Ok(rendered) => {
                    let (clipped, blocked, links) = (
                        rendered.clipped,
                        rendered.blocked.len(),
                        rendered.links.clone(),
                    );
                    cx.background_executor()
                        .spawn(async move {
                            super::visuals::picture(rendered, else_visual::email::MAX_HEIGHT)
                        })
                        .await
                        .map(|(image, height)| Page {
                            image,
                            height,
                            clipped,
                            blocked,
                            links,
                        })
                }
                Err(error) => Err(error.to_string()),
            };
            let _ = this.update(cx, |this, cx| {
                if matches!(&this.html, Some(Html::Rendering { key: k, .. }) if *k == key) {
                    this.html = Some(match result {
                        Ok(page) => Html::Ready { key, page },
                        Err(error) => Html::Failed { key, error },
                    });
                    cx.notify();
                }
            });
        });
        self.html = Some(Html::Rendering { key, _task: task });
    }

    fn show(&mut self, shown: Shown, cx: &mut Context<Self>) {
        self.shown = shown;
        cx.notify();
    }
}

fn people(label: &'static str, people: &[EmailPerson], theme: &Theme) -> Option<Div> {
    let names: Vec<String> = people.iter().map(EmailPerson::label).collect();
    (!names.is_empty()).then(|| {
        h_flex()
            .gap_2()
            .items_start()
            .text_sm()
            .child(
                div()
                    .w(px(40.))
                    .flex_none()
                    .text_color(theme.muted_foreground)
                    .child(label),
            )
            .child(div().min_w_0().flex_1().child(names.join(", ")))
    })
}

/// The sender's date in the viewer's zone, or as written when it is not a date.
fn sent_at(message: &EmailMessage) -> Option<String> {
    let raw = message.date.as_deref().or(message.received_at.as_deref())?;
    Some(match chrono::DateTime::parse_from_rfc3339(raw) {
        Ok(at) => super::message_row::stamp(at.with_timezone(&chrono::Utc)),
        Err(_) => raw.to_owned(),
    })
}

impl Render for EmailReader {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let count = self.mails.len();
        let navigation = (count > 1).then(|| {
            let index = self.index;
            h_flex()
                .gap_1()
                .items_center()
                .child(
                    Button::new("email-previous")
                        .control(Tier::Ghost, Scale::Compact, index == 0, cx)
                        .label("Previous")
                        .on_click(cx.listener(move |this, _, _, cx| this.load(index - 1, cx))),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("{} of {count}", index + 1)),
                )
                .child(
                    Button::new("email-next")
                        .control(Tier::Ghost, Scale::Compact, index + 1 == count, cx)
                        .label("Next")
                        .on_click(cx.listener(move |this, _, _, cx| this.load(index + 1, cx))),
                )
        });
        let message = match &self.reading {
            Reading::Loading { .. } => {
                return v_flex()
                    .w_full()
                    .gap_3()
                    .children(navigation)
                    .child(
                        v_flex()
                            .h(px(160.))
                            .items_center()
                            .justify_center()
                            .child(Icon::new(IconName::LoaderCircle).small()),
                    )
                    .into_any_element();
            }
            Reading::Failed(error) => {
                let retry = self.index;
                return v_flex()
                    .gap_2()
                    .items_start()
                    .children(navigation)
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.danger)
                            .child(format!("Could not open this email. {error}")),
                    )
                    .child(
                        Button::new("email-retry")
                            .control(Tier::Secondary, Scale::Compact, false, cx)
                            .label("Try again")
                            .on_click(cx.listener(move |this, _, _, cx| this.load(retry, cx))),
                    )
                    .into_any_element();
            }
            Reading::Loaded(message) => message.clone(),
        };
        if self.shown == Shown::Html
            && let Some(width) = self.width
        {
            let scale = (window.scale_factor() * 100.0).round() as u32;
            self.render_html(&message, Key { width, scale }, cx);
        }
        let entity = cx.entity().downgrade();
        let measure = canvas(
            move |bounds, _, cx| {
                let width = f32::from(bounds.size.width).floor() as u32;
                let _ = entity.update(cx, |this, cx| {
                    if width > 0 && this.width != Some(width) {
                        this.width = Some(width);
                        cx.notify();
                    }
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        let headers = v_flex()
            .gap_1()
            .children(people("From", &message.from, &theme))
            .children(people("To", &message.to, &theme))
            .children(people("Cc", &message.cc, &theme))
            .children(sent_at(&message).map(|at| {
                h_flex()
                    .gap_2()
                    .text_sm()
                    .child(
                        div()
                            .w(px(40.))
                            .flex_none()
                            .text_color(theme.muted_foreground)
                            .child("Date"),
                    )
                    .child(at)
            }));
        let toggle = message.html.is_some().then(|| {
            h_flex()
                .gap_1()
                .child(
                    Button::new("email-show-html")
                        .control(Tier::Ghost, Scale::Compact, false, cx)
                        .selected(self.shown == Shown::Html)
                        .label("HTML")
                        .on_click(cx.listener(|this, _, _, cx| this.show(Shown::Html, cx))),
                )
                .child(
                    Button::new("email-show-text")
                        .control(Tier::Ghost, Scale::Compact, false, cx)
                        .selected(self.shown == Shown::Text)
                        .label("Plain text")
                        .on_click(cx.listener(|this, _, _, cx| this.show(Shown::Text, cx))),
                )
        });
        let note = |text: String| {
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(text)
        };
        let mut notes = Vec::new();
        if message.html_omitted.is_some() {
            notes.push(note(
                "The HTML version is too large to show, so this is its text.".into(),
            ));
        }
        let page = match (&self.shown, &self.html) {
            (Shown::Html, Some(Html::Ready { page, .. })) => Some(page),
            _ => None,
        };
        if let Some(page) = page {
            if page.blocked > 0 {
                notes.push(note(format!(
                    "Remote images blocked ({}). Nothing was loaded from the sender's servers.",
                    page.blocked
                )));
            }
            if page.clipped {
                notes.push(note(
                    "This email is too long to draw in full. Switch to Plain text to read the rest."
                        .into(),
                ));
            }
        }
        if self.shown == Shown::Text && message.text_truncated {
            notes.push(note("The text is cut off at 200,000 characters.".into()));
        }
        let body = match (self.shown, &self.html) {
            (Shown::Html, Some(Html::Ready { key, page })) => div()
                .w(px(key.width as f32))
                .h(px(page.height as f32))
                .child(
                    img(ImageSource::Render(page.image.clone()))
                        .w(px(key.width as f32))
                        .h(px(page.height as f32)),
                )
                .into_any_element(),
            (Shown::Html, Some(Html::Failed { error, .. })) => div()
                .text_sm()
                .text_color(theme.danger)
                .child(format!(
                    "Could not draw this email: {error} Switch to Plain text to read it."
                ))
                .into_any_element(),
            (Shown::Html, _) => v_flex()
                .h(px(160.))
                .items_center()
                .justify_center()
                .child(Icon::new(IconName::LoaderCircle).small())
                .into_any_element(),
            (Shown::Text, _) => div()
                .text_sm()
                .whitespace_normal()
                .child(if message.text.trim().is_empty() {
                    "This email has no text.".to_owned()
                } else {
                    message.text.clone()
                })
                .into_any_element(),
        };
        let links: &[String] = page.map_or(&[], |page| &page.links);
        let links_section = (!links.is_empty()).then(|| {
            let open = self.links_open;
            v_flex()
                .gap_1()
                .child(
                    Button::new("email-links")
                        .control(Tier::Ghost, Scale::Compact, false, cx)
                        .icon(if open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .label(format!("Links ({})", links.len()))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.links_open = !this.links_open;
                            cx.notify();
                        })),
                )
                .when(open, |section| {
                    section.children(links.iter().enumerate().map(|(ix, link)| {
                        let target = LinkTarget::parse(link);
                        let row = div()
                            .id(("email-link", ix))
                            .text_xs()
                            .truncate()
                            .child(link.clone());
                        match target {
                            Some(target) => row
                                .cursor_pointer()
                                .text_color(theme.link)
                                .on_click(move |_, _, cx| target.open(cx))
                                .into_any_element(),
                            None => row.text_color(theme.muted_foreground).into_any_element(),
                        }
                    }))
                })
        });
        let attachments = (!message.attachments.is_empty()).then(|| {
            v_flex()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!(
                            "Attachments ({}). Ask the bot to open one; you approve it first.",
                            message.attachments.len()
                        )),
                )
                .children(message.attachments.iter().map(|file| {
                    h_flex()
                        .gap_2()
                        .text_sm()
                        .child(Icon::new(IconName::FileText).small())
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .child(file.name.clone().unwrap_or_else(|| "attachment".into())),
                        )
                        .child(div().text_color(theme.muted_foreground).child(format!(
                            "{} · {}",
                            file.media_type,
                            byte_size(file.size)
                        )))
                }))
        });
        v_flex()
            .w_full()
            .gap_3()
            .children(navigation)
            .child(
                h_flex()
                    .w_full()
                    .items_start()
                    .justify_between()
                    .gap_3()
                    .child(headers.min_w_0().flex_1())
                    .children(toggle),
            )
            .children(notes)
            .child(
                div()
                    .id("email-body")
                    .w_full()
                    .max_h(BODY_HEIGHT)
                    .overflow_y_scroll()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .when(self.shown == Shown::Text, |body| body.p_3())
                    .child(div().w_full().relative().child(measure).child(body)),
            )
            .children(attachments)
            .children(links_section)
            .into_any_element()
    }
}

impl ElseApp {
    /// Opens the reader on one email behind a mail-triggered turn.
    pub(super) fn open_email(
        &mut self,
        mails: Vec<TurnMail>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let client = self.client.clone();
        let reader = cx.new(|cx| EmailReader::new(client, mails, cx));
        window.open_dialog(cx, move |dialog, _, cx| {
            dialog
                .w(px(780.))
                .title(reader.read(cx).title())
                .child(reader.clone())
        });
    }
}

/// The row above a mail-triggered reply: the email that woke the bot, and
/// a button per message that opens it in the reader.
pub(super) fn mail_banner(turn: &crate::model::Turn, cx: &mut Context<ElseApp>) -> Option<Div> {
    if turn.mail.is_empty() {
        return None;
    }
    let theme = cx.theme();
    let count = turn.mail.len();
    let mails = turn.mail.clone();
    let view = super::action_card::event_action(
        SharedString::from(format!("view-email-{}", turn.id)),
        if count == 1 {
            "View email"
        } else {
            "View emails"
        },
        false,
        cx,
    )
    .on_click(cx.listener(move |app, _, window, cx| app.open_email(mails.clone(), window, cx)));
    Some(
        super::action_card::event_row(theme)
            .child(super::action_card::event_icon(IconName::Mail))
            .child(super::action_card::event_verb(
                theme,
                if count == 1 {
                    "New email".to_owned()
                } else {
                    format!("{count} new emails")
                },
            ))
            .child(super::action_card::event_end(
                theme,
                super::action_card::EventEnd::Actions(vec![view]),
                super::action_card::Disclosure::None,
            )),
    )
}
