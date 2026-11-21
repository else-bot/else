//! The owner's reader for the email behind a mail-triggered turn. The
//! message comes from the server with the inbox's saved sign-in, never
//! through the bot, and stays unread. Its HTML renders offline in the
//! visual worker: remote content never loads and scripts never run. Its
//! `http`, `https` and `mailto` links open in place, in the system's browser
//! or mail client, and show where they go on hover.

use super::ElseApp;
use crate::api::Client;
use crate::attachments::byte_size;
use crate::controls::{Control as _, Scale, Tier};
use else_wire::email::{EmailMessage, EmailPerson, MailLocation, MessageRef, TurnMail};
use futures_util::FutureExt as _;
use futures_util::future::{Either, Shared};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::tooltip::Tooltip;
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
/// How long a started fetch serves the reader, so a hover's fetch serves
/// the click that follows it. The server caches the message for longer.
const FETCH_REUSE: Duration = Duration::from_secs(30);
/// Fetches the session keeps.
const CACHED_EMAILS: usize = 16;
/// Bytes of drawn pages the session keeps, one page per email at most.
const CACHED_PAGE_BYTES: usize = 192 * 1024 * 1024;

enum Reading {
    Loading { slow: bool, _task: Task<()> },
    Failed(FetchError),
    Loaded(Arc<EmailMessage>),
}

/// Why an email did not open, and whether asking again could help: not
/// when the email is gone, too large, or its inbox is disconnected.
#[derive(Clone)]
struct FetchError {
    message: String,
    retry: bool,
}

impl From<crate::api::ClientError> for FetchError {
    fn from(error: crate::api::ClientError) -> Self {
        let lasting = matches!(&error, crate::api::ClientError::Api { code, .. }
            if matches!(code.as_str(), "message_deleted" | "message_not_found"
                | "stale_message_reference" | "mail_forbidden" | "mail_message_too_large"));
        Self {
            message: error.to_string(),
            retry: !lasting,
        }
    }
}

type Fetch = Shared<Task<Result<Arc<EmailMessage>, FetchError>>>;

/// One email of one inbox on one server connection.
#[derive(Clone, PartialEq, Eq, Hash)]
struct MailKey {
    client: usize,
    account_id: uuid::Uuid,
    message: MessageRef,
}

impl MailKey {
    fn new(client: &Arc<Client>, mail: &TurnMail) -> Self {
        Self {
            client: Arc::as_ptr(client) as usize,
            account_id: mail.account_id,
            message: mail.message.clone(),
        }
    }
}

/// Recent fetches and drawn pages, so hovering View email starts the
/// fetch for the click and reopening an email redraws nothing. Each open
/// still asks the server, which refuses a paused or removed inbox.
#[derive(Default)]
struct ReaderCache {
    emails: HashMap<MailKey, (std::time::Instant, Fetch)>,
    order: VecDeque<MailKey>,
    /// Each email's last drawn page, oldest first.
    pages: VecDeque<(MailKey, Key, Arc<Page>)>,
}

impl Global for ReaderCache {}

impl ReaderCache {
    /// The email's fetch: one started within `FETCH_REUSE` that has not
    /// failed, or a new one. `fresh` always starts a new one.
    fn fetch(client: &Arc<Client>, mail: &TurnMail, fresh: bool, cx: &mut App) -> Fetch {
        let key = MailKey::new(client, mail);
        if let Some((started, fetch)) = cx.default_global::<Self>().emails.get(&key)
            && !fresh
            && started.elapsed() < FETCH_REUSE
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
                    .map_err(FetchError::from)
            })
            .shared();
        let cache = cx.global_mut::<Self>();
        if cache
            .emails
            .insert(key.clone(), (std::time::Instant::now(), fetch.clone()))
            .is_none()
        {
            cache.order.push_back(key);
        }
        while cache.order.len() > CACHED_EMAILS {
            let Some(oldest) = cache.order.pop_front() else {
                break;
            };
            cache.emails.remove(&oldest);
        }
        fetch
    }

    fn page(&self, mail: &MailKey, key: Key) -> Option<Arc<Page>> {
        self.pages
            .iter()
            .find(|(m, k, _)| m == mail && *k == key)
            .map(|(_, _, page)| page.clone())
    }

    /// Keeps `page` as the email's only page and frees the textures of the
    /// pages it pushes out.
    fn keep_page(mail: MailKey, key: Key, page: Arc<Page>, cx: &mut App) {
        let pages = &mut cx.default_global::<Self>().pages;
        let mut evicted = Vec::new();
        if let Some(ix) = pages.iter().position(|(m, _, _)| *m == mail) {
            evicted.extend(pages.remove(ix));
        }
        pages.push_back((mail, key, page));
        while pages.iter().map(|(_, _, p)| p.bytes).sum::<usize>() > CACHED_PAGE_BYTES
            && pages.len() > 1
        {
            evicted.extend(pages.pop_front());
        }
        for (_, _, page) in evicted {
            cx.drop_image(page.image.clone(), None);
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
    /// The render's size in device pixels, which its links' boxes use.
    pixels: (u32, u32),
    links: Vec<else_visual::email::Link>,
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
    /// Each shown link's focus, by its [`else_visual::email::Link::index`],
    /// so focus stays on the same link when the page draws again.
    link_focus: HashMap<usize, FocusHandle>,
    /// The link last scrolled into view because it took focus, and the
    /// page it sat on: a redraw at another width moves it.
    revealed: Option<(Key, usize)>,
    scroll: ScrollHandle,
}

impl EmailReader {
    fn new(client: Arc<Client>, mails: Vec<TurnMail>, cx: &mut Context<Self>) -> Self {
        let mut reader = Self {
            client,
            mails,
            index: 0,
            reading: Reading::Failed(FetchError {
                message: String::new(),
                retry: false,
            }),
            shown: Shown::Html,
            html: None,
            width: None,
            link_focus: HashMap::new(),
            revealed: None,
            scroll: ScrollHandle::new(),
        };
        reader.load(0, false, cx);
        reader
    }

    /// Shows the turn's email at `index`. `fresh` asks the server again
    /// even when a recent fetch is still under way.
    fn load(&mut self, index: usize, fresh: bool, cx: &mut Context<Self>) {
        let Some(mail) = self.mails.get(index) else {
            return;
        };
        let fetch = ReaderCache::fetch(&self.client, mail, fresh, cx);
        self.index = index;
        self.html = None;
        self.link_focus.clear();
        self.revealed = None;
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
        self.mails
            .get(self.index)
            .map(|mail| MailKey::new(&self.client, mail))
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
    /// A page this session drew at `key` shows at once. A draw that
    /// replaces another waits for `RESIZE_SETTLE`, so a resize draws once,
    /// keeping any drawn page up meanwhile.
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
        let previous = self.html.take();
        let settle = previous.is_some();
        let stale = match previous {
            Some(Html::Ready { key, page }) => Some((key, page)),
            Some(Html::Rendering { stale, .. }) => stale,
            _ => None,
        };
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
                Ok(mut rendered) => {
                    let (bytes, clipped, blocked, pixels) = (
                        rendered.rgba.len(),
                        rendered.clipped,
                        rendered.blocked.len(),
                        (rendered.width, rendered.height),
                    );
                    let links = std::mem::take(&mut rendered.links);
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
                                pixels,
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
                            ReaderCache::keep_page(mail, key, page.clone(), cx);
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
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.load(index - 1, false, cx)),
                        ),
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
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.load(index + 1, false, cx)),
                        ),
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
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.load(retry, true, cx)
                                        })),
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
                let index = self.index;
                let (text, color) = if error.retry {
                    (
                        format!("Could not open this email. {}", error.message),
                        theme.danger,
                    )
                } else {
                    (error.message.clone(), theme.muted_foreground)
                };
                return v_flex()
                    .gap_2()
                    .items_start()
                    .children(navigation)
                    .child(div().text_sm().text_color(color).child(text))
                    .when(error.retry, |this| {
                        this.child(
                            Button::new("email-retry")
                                .control(Tier::Secondary, Scale::Compact, false, cx)
                                .label("Try again")
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.load(index, true, cx)),
                                ),
                        )
                    })
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
            Body::Page(key, page) => {
                let shown = size(px(key.width as f32), px(page.height as f32));
                let mut focused = None;
                let links: Vec<_> = page
                    .links
                    .iter()
                    .map(|link| {
                        let focus = self
                            .link_focus
                            .entry(link.index)
                            .or_insert_with(|| cx.focus_handle().tab_stop(true))
                            .clone();
                        if focus.is_focused(window) {
                            focused = Some(link);
                        }
                        (link, focus)
                    })
                    .collect();
                match focused {
                    Some(link) if self.revealed != Some((key, link.index)) => {
                        self.revealed = Some((key, link.index));
                        if let Some(rect) = link.rects.first() {
                            reveal(&self.scroll, area(*rect, page.pixels, shown));
                        }
                    }
                    Some(_) => {}
                    None => self.revealed = None,
                }
                let highlight = theme.link.opacity(0.18);
                div()
                    .relative()
                    .w(shown.width)
                    .h(shown.height)
                    .child(
                        img(ImageSource::Render(page.image.clone()))
                            .w(shown.width)
                            .h(shown.height),
                    )
                    .children(links.into_iter().flat_map(|(link, focus)| {
                        let lit = focus.is_focused(window);
                        link.rects.iter().enumerate().map(move |(part, rect)| {
                            link_area(link, part, &focus, area(*rect, page.pixels, shown))
                                .when(lit, |area| area.bg(highlight))
                        })
                    }))
                    .into_any_element()
            }
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
                    .track_scroll(&self.scroll)
                    .w_full()
                    .max_h(BODY_HEIGHT)
                    .overflow_y_scroll()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .child(div().w_full().relative().child(measure).child(body)),
            )
            .children(attachments)
            .into_any_element()
    }
}

/// Where a link's box in device pixels of a render of `pixels` sits on the
/// page shown at `shown`.
fn area(rect: else_visual::email::Rect, pixels: (u32, u32), shown: Size<Pixels>) -> Bounds<Pixels> {
    let x = f32::from(shown.width) / pixels.0.max(1) as f32;
    let y = f32::from(shown.height) / pixels.1.max(1) as f32;
    Bounds::new(
        point(px(rect.x * x), px(rect.y * y)),
        size(px(rect.width * x), px(rect.height * y)),
    )
}

/// One box of `link` over the page. A link that wraps has a box per line;
/// only its first takes focus and speaks for the link. The topmost box under the pointer takes the
/// click, so links drawn over each other never both open.
fn link_area(
    link: &else_visual::email::Link,
    part: usize,
    focus: &FocusHandle,
    at: Bounds<Pixels>,
) -> Stateful<Div> {
    let destination = SharedString::from(destination(&link.href));
    let label = if link.text.is_empty() {
        link.href.clone()
    } else {
        link.text.clone()
    };
    let (click, confirm) = (link.href.clone(), link.href.clone());
    div()
        .id(SharedString::from(format!(
            "email-link-{}-{part}",
            link.index
        )))
        .absolute()
        .left(at.origin.x)
        .top(at.origin.y)
        .w(at.size.width)
        .h(at.size.height)
        .cursor_pointer()
        .block_mouse_except_scroll()
        .when(part == 0, |area| {
            area.role(Role::Link)
                .aria_label(label)
                .aria_description(link.href.clone())
                .track_focus(focus)
        })
        .on_click(move |_, _, cx| open_link(&click, cx))
        // The reader's dialog binds Enter to Confirm, which would close it
        // before the focused link sees the key.
        .on_action(move |_: &gpui_kit::base::actions::Confirm, _, cx| open_link(&confirm, cx))
        .tooltip(move |window, cx| Tooltip::new(destination.clone()).build(window, cx))
}

/// Scrolls the message area just enough to show `at`, a box on the page.
fn reveal(scroll: &ScrollHandle, at: Bounds<Pixels>) {
    let view = scroll.bounds().size.height;
    let mut offset = scroll.offset();
    let top = -offset.y;
    if at.top() < top {
        offset.y = -at.top();
    } else if at.bottom() > top + view {
        offset.y = -(at.bottom() - view);
    }
    scroll.set_offset(offset);
}

/// Where a link goes, as its hover shows it: the site or address first,
/// then the whole URL, since the link's own text can say anything.
fn destination(href: &str) -> String {
    match url::Url::parse(href) {
        Ok(url) if url.scheme() == "mailto" => format!("Email {}\n{href}", url.path()),
        Ok(url) => match url.host_str() {
            Some(host) => format!("{host}\n{href}"),
            None => href.to_owned(),
        },
        Err(_) => href.to_owned(),
    }
}

/// Opens an email's link in the system's browser or mail client. Only
/// `http`, `https` and `mailto` open; the renderer offers nothing else.
fn open_link(href: &str, cx: &App) {
    if let Ok(url) = url::Url::parse(href)
        && matches!(url.scheme(), "http" | "https" | "mailto")
    {
        cx.open_url(url.as_str());
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

/// The row above a mail-triggered reply: the email that woke the bot,
/// where it is now, and a button that opens the emails still there.
pub(super) fn mail_banner(
    turn: &else_wire::turns::Turn,
    bots: &[else_wire::bots::Bot],
    app: &WeakEntity<ElseApp>,
    cx: &App,
) -> Option<Div> {
    let first = turn.mail.first()?;
    let theme = cx.theme().clone();
    let count = turn.mail.len();
    let openable: Vec<TurnMail> = turn
        .mail
        .iter()
        .filter(|mail| !matches!(mail.location, MailLocation::Deleted(_)))
        .cloned()
        .collect();
    let (verb, object) = if count == 1 {
        let name = |id: Option<uuid::Uuid>| {
            id.and_then(|id| bots.iter().find(|bot| bot.id == id))
                .map(|bot| bot.name.as_str())
        };
        let stamp = |at: &str| {
            at.parse()
                .map(super::message_row::short_stamp)
                .unwrap_or_else(|_| at.to_owned())
        };
        let verb = match &first.location {
            MailLocation::Arrived | MailLocation::Unknown => "New email".to_owned(),
            MailLocation::Archived(m) | MailLocation::Trashed(m) | MailLocation::Moved(m) => {
                place_label(&first.location, name(m.by_bot_id), &stamp(&m.at))
            }
            MailLocation::Deleted(d) => {
                place_label(&first.location, name(d.by_bot_id), &stamp(&d.at))
            }
        };
        let object = first.subject.clone().or_else(|| first.sender.clone());
        (verb, object)
    } else {
        (format!("{count} new emails"), places_summary(&turn.mail))
    };
    let view = (!openable.is_empty()).then(|| {
        let hover = openable[0].clone();
        super::action_card::event_action(
            SharedString::from(format!("view-email-{}", turn.id)),
            if openable.len() == 1 {
                "View email"
            } else {
                "View emails"
            },
            false,
            cx,
        )
        .on_hover({
            let app = app.clone();
            move |hovered, _, cx| {
                if let (true, Some(app)) = (*hovered, app.upgrade()) {
                    let client = app.read(cx).client.clone();
                    drop(ReaderCache::fetch(&client, &hover, false, cx));
                }
            }
        })
        .on_click(super::conversation::on_app(
            app,
            move |app, _, window, cx| app.open_email(openable.clone(), window, cx),
        ))
    });
    Some(
        super::action_card::event_row(&theme)
            .child(super::action_card::event_icon(IconName::Mail))
            .child(super::action_card::event_verb(&theme, verb))
            .children(object.map(|object| super::action_card::event_object(&theme, object)))
            .children(view.map(|view| {
                super::action_card::event_end(
                    &theme,
                    super::action_card::EventEnd::Actions(vec![view]),
                    super::action_card::Disclosure::None,
                )
            })),
    )
}

/// Where one email went, who moved it, and when, such as "Archived by
/// Mail · 18:08". `stamp` is when it moved.
fn place_label(location: &MailLocation, by: Option<&str>, stamp: &str) -> String {
    let by = by.map(|name| format!(" by {name}")).unwrap_or_default();
    match location {
        MailLocation::Arrived | MailLocation::Unknown => "New email".to_owned(),
        MailLocation::Archived(_) => format!("Archived{by} · {stamp}"),
        MailLocation::Trashed(_) if by.is_empty() => format!("In Trash · {stamp}"),
        MailLocation::Trashed(_) => format!("In Trash · moved{by} · {stamp}"),
        MailLocation::Moved(m) => format!("Moved to {}{by} · {stamp}", m.folder),
        MailLocation::Deleted(_) => format!("Email deleted{by} · {stamp}"),
    }
}

/// How many of a turn's emails left where they arrived, by where they
/// went, such as "2 archived, 1 deleted"; none when all are still there.
fn places_summary(mails: &[TurnMail]) -> Option<String> {
    let count =
        |want: fn(&MailLocation) -> bool| mails.iter().filter(|m| want(&m.location)).count();
    let parts: Vec<String> = [
        (
            count(|l| matches!(l, MailLocation::Archived(_))),
            "archived",
        ),
        (count(|l| matches!(l, MailLocation::Trashed(_))), "in Trash"),
        (count(|l| matches!(l, MailLocation::Moved(_))), "moved"),
        (count(|l| matches!(l, MailLocation::Deleted(_))), "deleted"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, what)| format!("{n} {what}"))
    .collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// What a bot's turn did to the emails that woke it, for a turn that
/// wrote no reply, such as "Archived the email."
pub(super) fn mail_outcome(turn: &else_wire::turns::Turn) -> Option<String> {
    let mine: Vec<&MailLocation> = turn
        .mail
        .iter()
        .map(|mail| &mail.location)
        .filter(|location| location.by_turn() == Some(turn.id))
        .collect();
    if let [only] = mine.as_slice() {
        return Some(match only {
            MailLocation::Arrived | MailLocation::Unknown => return None,
            MailLocation::Archived(_) => "Archived the email.".to_owned(),
            MailLocation::Trashed(_) => "Moved the email to Trash.".to_owned(),
            MailLocation::Moved(m) => format!("Moved the email to {}.", m.folder),
            MailLocation::Deleted(_) => "Deleted the email.".to_owned(),
        });
    }
    let count = |want: fn(&MailLocation) -> bool| mine.iter().filter(|l| want(l)).count();
    let emails = |n: usize| match n {
        1 => "1 email".to_owned(),
        n => format!("{n} emails"),
    };
    let parts: Vec<String> = [
        (
            count(|l| matches!(l, MailLocation::Archived(_))),
            "archived {}",
        ),
        (
            count(|l| matches!(l, MailLocation::Trashed(_))),
            "moved {} to Trash",
        ),
        (count(|l| matches!(l, MailLocation::Moved(_))), "moved {}"),
        (
            count(|l| matches!(l, MailLocation::Deleted(_))),
            "deleted {}",
        ),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, phrase)| phrase.replace("{}", &emails(n)))
    .collect();
    let text = parts.join(", ");
    let mut chars = text.chars();
    let first = chars.next()?;
    Some(format!("{}{}.", first.to_uppercase(), chars.as_str()))
}

#[cfg(test)]
mod tests {
    use super::{Body, Html, Key, Page, Shown, area, body, place_label, places_summary};
    use else_visual::email::Rect;
    use else_wire::email::{MailDeletion, MailLocation, MailMove, MessageRef, TurnMail};
    use gpui_kit::{Bounds, point, px, size};
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
            pixels: (1, 1),
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

    fn mail(location: MailLocation) -> TurnMail {
        TurnMail {
            account_id: uuid::Uuid::nil(),
            message: MessageRef {
                folder: "INBOX".into(),
                uid_validity: 1,
                uid: 9,
            },
            subject: Some("Verify your email".into()),
            sender: None,
            location,
        }
    }

    fn moved(folder: &str, turn: Option<uuid::Uuid>) -> MailMove {
        MailMove {
            folder: folder.into(),
            by_bot_id: None,
            by_turn_id: turn,
            at: "2026-11-01T07:00:00Z".into(),
        }
    }

    #[test]
    fn a_moved_email_says_where_it_went_and_who_moved_it() {
        let archived = MailLocation::Archived(moved("Archive", None));
        assert_eq!(
            place_label(&archived, Some("Mail"), "18:08"),
            "Archived by Mail · 18:08"
        );
        let trashed = MailLocation::Trashed(moved("Trash", None));
        assert_eq!(
            place_label(&trashed, Some("Mail"), "18:08"),
            "In Trash · moved by Mail · 18:08"
        );
        assert_eq!(place_label(&trashed, None, "18:08"), "In Trash · 18:08");
        let elsewhere = MailLocation::Moved(moved("Receipts", None));
        assert_eq!(
            place_label(&elsewhere, None, "18:08"),
            "Moved to Receipts · 18:08"
        );
    }

    #[test]
    fn a_turns_emails_sum_up_where_they_went() {
        let mails = [
            mail(MailLocation::Arrived),
            mail(MailLocation::Archived(moved("Archive", None))),
            mail(MailLocation::Archived(moved("Archive", None))),
            mail(MailLocation::Deleted(MailDeletion {
                by_bot_id: None,
                by_turn_id: None,
                at: "2026-11-01T07:00:00Z".into(),
            })),
        ];
        assert_eq!(
            places_summary(&mails).as_deref(),
            Some("2 archived, 1 deleted")
        );
        assert_eq!(places_summary(&mails[..1]), None);
    }

    /// A completed turn as the server sends it, woken by `mail`.
    fn mail_turn(mail: serde_json::Value) -> else_wire::turns::Turn {
        serde_json::from_value(serde_json::json!({
            "id": "00000000-0000-0000-0000-00000000000a",
            "request_id": "00000000-0000-0000-0000-00000000000b", "turn_no": 1,
            "provider_id": "synthetic", "status": "completed", "error": null,
            "effect_uncertain": false, "created_at": "2026-11-20T18:00:00Z",
            "mail": mail,
        }))
        .unwrap()
    }

    fn turn_email(location: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "account_id": "00000000-0000-0000-0000-000000000001",
            "message": {"folder": "INBOX", "uid_validity": 1, "uid": 9},
            "subject": "Verify your email", "sender": "Coupon Co", "location": location,
        })
    }

    #[test]
    fn a_silent_mail_turn_says_what_it_did_to_its_emails() {
        let by_this_turn = |state: &str| {
            turn_email(serde_json::json!({
                "state": state, "folder": "Trash",
                "by_bot_id": "00000000-0000-0000-0000-000000000002",
                "by_turn_id": "00000000-0000-0000-0000-00000000000a",
                "at": "2026-11-20T18:08:00.123+00:00",
            }))
        };
        assert_eq!(
            super::mail_outcome(&mail_turn(serde_json::json!([by_this_turn("trashed")])))
                .as_deref(),
            Some("Moved the email to Trash.")
        );
        let deleted = serde_json::json!({"state": "deleted", "by_bot_id": null,
            "by_turn_id": "00000000-0000-0000-0000-00000000000a",
            "at": "2026-11-20T18:08:00.123+00:00"});
        assert_eq!(
            super::mail_outcome(&mail_turn(serde_json::json!([
                by_this_turn("archived"),
                by_this_turn("archived"),
                turn_email(deleted),
            ])))
            .as_deref(),
            Some("Archived 2 emails, deleted 1 email.")
        );
        let elsewhere = serde_json::json!({"state": "archived", "folder": "Archive",
            "by_bot_id": null, "by_turn_id": null, "at": "2026-11-20T18:08:00.123+00:00"});
        assert_eq!(
            super::mail_outcome(&mail_turn(serde_json::json!([
                turn_email(elsewhere),
                turn_email(serde_json::json!({"state": "arrived"})),
            ]))),
            None,
            "moves by other clients are not this turn's work"
        );
        assert_eq!(
            mail_turn(
                serde_json::json!([{"account_id": "00000000-0000-0000-0000-000000000001",
                "message": {"folder": "INBOX", "uid_validity": 1, "uid": 9}}])
            )
            .mail[0]
                .location,
            MailLocation::Arrived,
            "an older server's email reads as where it arrived"
        );
    }

    #[test]
    fn a_link_box_lands_where_the_page_shows_it_at_any_scale() {
        let shown = size(px(700.), px(300.));
        let rect = |scale: f32| Rect {
            x: 100. * scale,
            y: 50. * scale,
            width: 40. * scale,
            height: 20. * scale,
        };
        let expected = Bounds::new(point(px(100.), px(50.)), size(px(40.), px(20.)));
        assert_eq!(area(rect(1.), (700, 300), shown), expected);
        assert_eq!(area(rect(2.), (1400, 600), shown), expected);
    }
}
