//! Email in the chat: the mail that woke a bot, before what it did with it,
//! with where each one is now; a tap reads the email as the server fetches
//! it from the inbox: who sent it to whom, when, its text and its files.

use else_wire::email::{EmailMessage, EmailPerson, MailLocation, TurnMail};
use nsh_base::nav_page;
use nsh_core::prelude::*;
use uuid::Uuid;

use super::files::size_label;
use super::list;
use super::parts::{self, colors};
use super::session;

/// Where one email is now, such as "Archived" or "Moved to Receipts".
fn place(location: &MailLocation) -> Option<String> {
    match location {
        MailLocation::Arrived | MailLocation::Unknown => None,
        MailLocation::Archived(_) => Some("Archived".to_owned()),
        MailLocation::Trashed(_) => Some("In Trash".to_owned()),
        MailLocation::Moved(moved) => Some(format!("Moved to {}", moved.folder)),
        MailLocation::Deleted(_) => Some("Deleted".to_owned()),
    }
}

fn people(people: &[EmailPerson]) -> String {
    people
        .iter()
        .map(EmailPerson::label)
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn card(mails: Vec<TurnMail>) -> impl IntoElement {
    let rows: Vec<AnyElement> = mails
        .into_iter()
        .map(|mail| {
            let subject = mail
                .subject
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "(no subject)".to_owned());
            let from = mail.sender.clone().unwrap_or_else(|| "Someone".to_owned());
            let gone = matches!(mail.location, MailLocation::Deleted(_));
            let where_now = place(&mail.location);
            view()
                .gap(px(2.0))
                .active(|s| s.opacity(0.6))
                .role(AccessibilityRole::Button)
                .label(format!("Email from {from}: {subject}"))
                .on_press(move || {
                    if !gone {
                        open(mail.clone())
                    }
                })
                .child(
                    text(format!("From {from}"))
                        .number_of_lines(1)
                        .text_size(px(13.0))
                        .text_color(colors::secondary()),
                )
                .child(
                    text(subject)
                        .number_of_lines(2)
                        .text_size(px(15.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(colors::label()),
                )
                .child(where_now.map(|line| {
                    text(line)
                        .text_size(px(13.0))
                        .text_color(colors::secondary())
                }))
                .into_element()
        })
        .collect();
    view()
        .mx_4()
        .my_2()
        .p_4()
        .gap_3()
        .rounded(px(16.0))
        .border_curve(BorderCurve::Continuous)
        .bg(colors::card())
        .border_1()
        .border_color(colors::separator())
        .child(
            text("NEW EMAIL")
                .text_size(px(13.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors::tint()),
        )
        .children(rows)
}

fn open(mail: TurnMail) {
    let key = format!("email-{}-{}", mail.account_id, mail.message.uid);
    super::push(key.clone(), move || reader(mail.clone(), key.clone()));
}

/// One email as the inbox has it now.
fn reader(mail: TurnMail, key: String) -> Screen {
    let account: Uuid = mail.account_id;
    let message = mail.message.clone();
    let email = resource(
        || Some(()),
        move |(), _| {
            let client = session::server().client.clone();
            let message = message.clone();
            async move {
                client
                    .view_email(account, message)
                    .await
                    .map_err(|err| err.to_string())
            }
        },
    );
    let body = dynamic(move || match (email.get(), email.error()) {
        (None, None) => Some(parts::footnote("Loading…").into_element()),
        (None, Some(message)) => Some(parts::footnote(message).into_element()),
        (Some(email), _) => Some(untracked(|| shown(email)).into_element()),
    });
    nav_page()
        .key(key)
        .title(
            mail.subject
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "Email".to_owned()),
        )
        .header_large_title(false)
        .child(
            scroll()
                .flex_1()
                .bg(colors::background())
                .child(view().py_6().gap_6().child(body)),
        )
}

fn shown(email: EmailMessage) -> impl IntoElement {
    let header = |title: &'static str, value: String| list::link_row(title, value, None::<fn()>);
    let mut headers = vec![header("From", people(&email.from))];
    if !email.to.is_empty() {
        headers.push(header("To", people(&email.to)));
    }
    if !email.cc.is_empty() {
        headers.push(header("Cc", people(&email.cc)));
    }
    if let Some(date) = email.date.clone().or_else(|| email.received_at.clone()) {
        let date = chrono::DateTime::parse_from_rfc3339(&date)
            .map(|d| {
                d.with_timezone(&chrono::Local)
                    .format("%a %-d %b %Y, %H:%M")
                    .to_string()
            })
            .unwrap_or(date);
        headers.push(header("Date", date));
    }
    let files: Vec<AnyElement> = email
        .attachments
        .iter()
        .map(|file| {
            header(
                "File",
                format!(
                    "{} · {}",
                    file.name.clone().unwrap_or_else(|| "Unnamed".to_owned()),
                    size_label(file.size)
                ),
            )
        })
        .collect();
    view()
        .gap_6()
        .child(list::section(None, headers, None))
        .child(
            view().child(
                parts::group().p_4().child(
                    text(email.text.clone())
                        .selectable(true)
                        .text_size(px(15.0))
                        .line_height(px(21.0))
                        .text_color(colors::label()),
                ),
            ),
        )
        .child(email.text_truncated.then(|| {
            parts::footnote("The text is cut short here; the full email is in the inbox.")
        }))
        .child((!files.is_empty()).then(|| list::section(Some("Attachments"), files, None)))
}
