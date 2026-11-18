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
use futures_util::FutureExt as _;
use futures_util::future::{Either, Shared};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::{
    ActiveTheme as _, Icon, Selectable as _, Sizable as _, Theme, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

/// Tallest the scrolling message area grows before it scrolls.
const BODY_HEIGHT: Pixels = px(560.);
/// How long a fetch runs before the reader says it is slow.
const SLOW: Duration = Duration::from_secs(5);
/// How long the body's width must hold before the HTML draws again.
const RESIZE_SETTLE: Duration = Duration::from_millis(150);
/// Emails the session keeps.
const CACHED_EMAILS: usize = 16;
/// Bytes of drawn pages the session keeps, one page per email at most.
const CACHED_PAGE_BYTES: usize = 192 * 1024 * 1024;

enum Reading {
    Loading { slow: bool, _task: Task<()> },
    Failed(String),
    Loaded(Arc<EmailMessage>),
}

type Fetch = Shared<Task<Result<Arc<EmailMessage>, String>>>;

/// One email of one inbox.
#[derive(Clone, PartialEq, Eq, Hash)]
struct MailKey {
    account_id: uuid::Uuid,
    message: crate::api::MessageRef,
}

impl From<&TurnMail> for MailKey {
    fn from(mail: &TurnMail) -> Self {
        Self {
            account_id: mail.account_id,
            message: mail.message.clone(),
        }
    }
}

/// Emails and drawn pages this session already loaded, so hovering View
/// email starts the fetch and reopening an email shows it at once.
#[derive(Default)]
struct ReaderCache {
    emails: HashMap<MailKey, Fetch>,
    order: VecDeque<MailKey>,
    /// Each email's last drawn page, oldest first.
    pages: VecDeque<(MailKey, Key, Arc<Page>)>,
}

impl Global for ReaderCache {}

impl ReaderCache {
    /// The email's fetch: the cached one, the one under way, or a new one
    /// when there is none or the last one failed.
    fn fetch(client: &Arc<Client>, mail: &TurnMail, cx: &mut App) -> Fetch {
        let key = MailKey::from(mail);
        if let Some(fetch) = cx.default_global::<Self>().emails.get(&key)
            && !matches!(fetch.peek(), Some(Err(_)))
        {
            return fetch.clone();
        }
        let client = client.clone();
        let (account, message) = (mail.account_id, mail.message.clone());
        let fetch = cx
            .spawn(async move |_| {
                client
                    .view_email(account, message)
                    .await
                    .map(Arc::new)
                    .map_err(|error| error.to_string())
            })
            .shared();
        // `peek` sees a result only once a clone has polled it, and a
        // prefetch has no reader awaiting it yet.
        let polled = fetch.clone();
        cx.spawn(async move |_| {
            let _ = polled.await;
        })
        .detach();
        let cache = cx.global_mut::<Self>();
        if cache.emails.insert(key.clone(), fetch.clone()).is_none() {
            cache.order.push_back(key);
        }
        while cache.order.len() > CACHED_EMAILS {
            let Some(oldest) = cache.order.pop_front() else {
                break;
            };
            cache.emails.remove(&oldest);
            cache.pages.retain(|(mail, _, _)| *mail != oldest);
        }
        fetch
    }

    fn page(&self, mail: &MailKey, key: Key) -> Option<Arc<Page>> {
        self.pages
            .iter()
            .find(|(m, k, _)| m == mail && *k == key)
            .map(|(_, _, page)| page.clone())
    }

    fn keep_page(&mut self, mail: MailKey, key: Key, page: Arc<Page>) {
        self.pages.retain(|(m, _, _)| *m != mail);
        self.pages.push_back((mail, key, page));
        while self.pages.iter().map(|(_, _, p)| p.bytes).sum::<usize>() > CACHED_PAGE_BYTES
            && self.pages.len() > 1
        {
            self.pages.pop_front();
        }
    }
}

/// Which of the message's bodies the reader shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shown {
    Html,
    Text,
}

/// One render of the HTML body at a width and scale.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    width: u32,
    scale: u32,
}

enum Html {
    /// A draw under way. `stale` is the page drawn at the previous width,
    /// shown until this one is ready.
    Rendering {
        key: Key,
        stale: Option<(Key, Arc<Page>)>,
        _task: Task<()>,
    },
    Ready {
        key: Key,
        page: Arc<Page>,
    },
    Failed {
        key: Key,
        error: String,
    },
}

struct Page {
    image: Arc<RenderImage>,
    /// The image's size in memory.
    bytes: usize,
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

    /// Shows the turn's email at `index`, from the session's cache when it
    /// is there.
    fn load(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(mail) = self.mails.get(index) else {
            return;
        };
        let fetch = ReaderCache::fetch(&self.client, mail, cx);
        self.index = index;
        self.html = None;
        self.links_open = false;
        cx.notify();
        if let Some(Ok(message)) = fetch.peek() {
            self.loaded(message.clone());
            return;
        }
        let task = cx.spawn(async move |this, cx| {
            let timer = cx.background_executor().timer(SLOW);
            let result = match futures_util::future::select(fetch, timer).await {
                Either::Left((result, _)) => result,
                Either::Right((_, fetch)) => {
                    let _ = this.update(cx, |this, cx| {
                        if let Reading::Loading { slow, .. } = &mut this.reading {
                            *slow = true;
                            cx.notify();
                        }
                    });
                    fetch.await
                }
            };
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(message) => this.loaded(message),
                    Err(error) => this.reading = Reading::Failed(error),
                }
                cx.notify();
            });
        });
        self.reading = Reading::Loading {
            slow: false,
            _task: task,
        };
    }

    fn loaded(&mut self, message: Arc<EmailMessage>) {
        self.shown = if message.html.is_some() {
            Shown::Html
        } else {
            Shown::Text
        };
        self.reading = Reading::Loaded(message);
    }

    fn mail_key(&self) -> Option<MailKey> {
        self.mails.get(self.index).map(MailKey::from)
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
    /// A page this session drew at `key` shows at once. A new width waits
    /// for `RESIZE_SETTLE` so a resize draws once, keeping the old page up.
    fn render_html(&mut self, message: &Arc<EmailMessage>, key: Key, cx: &mut Context<Self>) {
        let current = match &self.html {
            Some(Html::Rendering { key: k, .. } | Html::Ready { key: k, .. })
            | Some(Html::Failed { key: k, .. }) => *k == key,
            None => false,
        };
        let (Some(html), Some(mail)) = (message.html.clone().filter(|_| !current), self.mail_key())
        else {
            return;
        };
        if let Some(page) = cx.default_global::<ReaderCache>().page(&mail, key) {
            self.html = Some(Html::Ready { key, page });
            return;
        }
        let stale = match self.html.take() {
            Some(Html::Ready { key, page }) => Some((key, page)),
            Some(Html::Rendering { stale, .. }) => stale,
            _ => None,
        };
        let settle = stale.is_some();
        let source = else_visual::email::Source {
            html,
            inline: message.inline.clone(),
        };
        let task = cx.spawn(async move |this, cx| {
            if settle {
                cx.background_executor().timer(RESIZE_SETTLE).await;
            }
            let scale = key.scale as f32 / 100.0;
            let result = match else_visual::isolate::submit_email(&source, key.width, scale).await {
                Ok(rendered) => {
                    let (bytes, clipped, blocked, links) = (
                        rendered.rgba.len(),
                        rendered.clipped,
                        rendered.blocked.len(),
                        rendered.links.clone(),
                    );
                    cx.background_executor()
                        .spawn(async move {
                            super::visuals::picture(rendered, else_visual::email::MAX_HEIGHT)
                        })
                        .await
                        .map(|(image, height)| {
                            Arc::new(Page {
                                image,
                                bytes,
                                height,
                                clipped,
                                blocked,
                                links,
                            })
                        })
                }
                Err(error) => Err(error.to_string()),
            };
            let _ = this.update(cx, |this, cx| {
                if matches!(&this.html, Some(Html::Rendering { key: k, .. }) if *k == key) {
                    this.html = Some(match result {
                        Ok(page) => {
                            cx.default_global::<ReaderCache>()
                                .keep_page(mail, key, page.clone());
                            Html::Ready { key, page }
                        }
                        Err(error) => Html::Failed { key, error },
                    });
                    cx.notify();
                }
            });
        });
        self.html = Some(Html::Rendering {
            key,
            stale,
            _task: task,
        });
    }

    fn show(&mut self, shown: Shown, cx: &mut Context<Self>) {
        self.shown = shown;
        cx.notify();
    }
}

/// What the message area shows.
#[derive(Clone, Copy)]
enum Body<'a> {
    Page(Key, &'a Arc<Page>),
    /// The text, also while the HTML draws for the first time.
    Text {
        rendering: bool,
    },
    Failed(&'a str),
}

/// The HTML page once drawn, the last page while a new width draws, and
/// the text until then.
fn body(shown: Shown, html: Option<&Html>) -> Body<'_> {
    match (shown, html) {
        (Shown::Text, _) => Body::Text { rendering: false },
        (Shown::Html, Some(Html::Ready { key, page }))
        | (
            Shown::Html,
            Some(Html::Rendering {
                stale: Some((key, page)),
                ..
            }),
        ) => Body::Page(*key, page),
        (Shown::Html, Some(Html::Failed { error, .. })) => Body::Failed(error),
        (Shown::Html, Some(Html::Rendering { stale: None, .. }) | None) => {
            Body::Text { rendering: true }
        }
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
            Reading::Loading { slow, .. } => {
                let retry = self.index;
                let bar = |width: f32| {
                    div()
                        .h(px(10.))
                        .w(px(width))
                        .rounded(theme.radius)
                        .bg(theme.muted)
                };
                return v_flex()
                    .w_full()
                    .gap_3()
                    .children(navigation)
                    .child(v_flex().gap_2().children([200., 260., 140.].map(bar)))
                    .when(*slow, |this| {
                        this.child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child("The mail server is slow to answer."),
                                )
                                .child(
                                    Button::new("email-retry")
                                        .control(Tier::Ghost, Scale::Compact, false, cx)
                                        .label("Try again")
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| this.load(retry, cx)),
                                        ),
                                ),
                        )
                    })
                    .child(
                        v_flex()
                            .w_full()
                            .gap_2()
                            .p_3()
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(theme.border)
                            .children([520., 600., 440., 560., 300.].map(bar)),
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
        let shows = body(self.shown, self.html.as_ref());
        let page = match shows {
            Body::Page(key, page) => Some((key, page)),
            _ => None,
        };
        let drawing = matches!(shows, Body::Text { rendering: true });
        if drawing {
            notes.push(note("Rendering…".into()));
        }
        if let Some((_, page)) = page {
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
        if matches!(shows, Body::Text { .. }) && message.text_truncated {
            notes.push(note("The text is cut off at 200,000 characters.".into()));
        }
        let body = match shows {
            Body::Page(key, page) => div()
                .w(px(key.width as f32))
                .h(px(page.height as f32))
                .child(
                    img(ImageSource::Render(page.image.clone()))
                        .w(px(key.width as f32))
                        .h(px(page.height as f32)),
                )
                .into_any_element(),
            Body::Failed(error) => div()
                .text_sm()
                .text_color(theme.danger)
                .child(format!(
                    "Could not draw this email: {error} Switch to Plain text to read it."
                ))
                .into_any_element(),
            Body::Text { .. } => div()
                .p_3()
                .text_sm()
                .whitespace_normal()
                .child(if message.text.trim().is_empty() {
                    "This email has no text.".to_owned()
                } else {
                    message.text.clone()
                })
                .into_any_element(),
        };
        let links: &[String] = page.map_or(&[], |(_, page)| &page.links);
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
    .on_hover({
        let app = cx.entity().downgrade();
        let first = turn.mail[0].clone();
        move |hovered, _, cx| {
            if let (true, Some(app)) = (*hovered, app.upgrade()) {
                let client = app.read(cx).client.clone();
                drop(ReaderCache::fetch(&client, &first, cx));
            }
        }
    })
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

#[cfg(test)]
mod tests {
    use super::{Body, Html, Key, Page, Shown, body};
    use gpui_kit::{RenderImage, Task};
    use std::sync::Arc;

    const KEY: Key = Key {
        width: 700,
        scale: 200,
    };
    const WIDER: Key = Key {
        width: 760,
        scale: 200,
    };

    fn page() -> Arc<Page> {
        Arc::new(Page {
            image: Arc::new(RenderImage::new([image::Frame::new(
                image::RgbaImage::new(1, 1),
            )])),
            bytes: 4,
            height: 1,
            clipped: false,
            blocked: 0,
            links: Vec::new(),
        })
    }

    fn rendering(stale: Option<(Key, Arc<Page>)>) -> Html {
        Html::Rendering {
            key: WIDER,
            stale,
            _task: Task::ready(()),
        }
    }

    #[test]
    fn the_text_shows_while_the_html_draws_for_the_first_time() {
        assert!(matches!(
            body(Shown::Html, None),
            Body::Text { rendering: true }
        ));
        assert!(matches!(
            body(Shown::Html, Some(&rendering(None))),
            Body::Text { rendering: true }
        ));
    }

    #[test]
    fn a_drawn_page_stays_up_while_a_new_width_draws() {
        let page = page();
        let ready = Html::Ready {
            key: KEY,
            page: page.clone(),
        };
        assert!(matches!(
            body(Shown::Html, Some(&ready)),
            Body::Page(KEY, _)
        ));
        let redrawing = rendering(Some((KEY, page)));
        assert!(matches!(
            body(Shown::Html, Some(&redrawing)),
            Body::Page(KEY, _)
        ));
        assert!(matches!(
            body(Shown::Text, Some(&redrawing)),
            Body::Text { rendering: false }
        ));
    }

    #[test]
    fn a_failed_draw_says_so_instead_of_rendering_forever() {
        let failed = Html::Failed {
            key: KEY,
            error: "too deep.".into(),
        };
        assert!(matches!(
            body(Shown::Html, Some(&failed)),
            Body::Failed("too deep.")
        ));
    }
}
