//! The first screen: the server's address and owner key, checked against
//! the server before they are saved.

use nsh_base::{input, nav_page, nav_stack};
use nsh_core::prelude::*;

use super::parts::{self, SERVER, colors};
use super::session;
use crate::api::{Client, ClientError};
use crate::server::Profile;

/// Where the connect form is.
#[derive(Clone, PartialEq)]
enum Step {
    Editing,
    Checking,
    Failed(String),
}

/// The first screen, with no server saved yet.
pub fn screen() -> impl IntoElement {
    nav_stack().flex_1().child(page(true))
}

pub const ADD_KEY: &str = "add-server";

/// The same form pushed over the bot list, to save another server.
pub fn open_add() {
    super::push(ADD_KEY, || page(false));
}

fn page(first: bool) -> Screen {
    let address = signal(String::new());
    let key = signal(String::new());
    let step = signal(Step::Editing);
    let checking = computed(move || step.get() == Step::Checking);
    let ready = computed(move || {
        !address.with(|a| a.trim().is_empty())
            && !key.with(|k| k.trim().is_empty())
            && !checking.get()
    });

    let submit = move || {
        if !ready.get_untracked() {
            return;
        }
        let profile = match Profile::parse(&address.get_untracked(), &key.get_untracked()) {
            Ok(profile) => profile,
            Err(err) => return step.set(Step::Failed(err)),
        };
        let client = match Client::new(&profile) {
            Ok(client) => client,
            Err(err) => return step.set(Step::Failed(err)),
        };
        step.set(Step::Checking);
        spawn(async move {
            match client.bots().await {
                Ok(_) => {
                    if let Err(err) = session::connect(profile) {
                        step.set(Step::Failed(err));
                    }
                }
                Err(ClientError::Unauthorized) => step.set(Step::Failed(
                    "The server rejected this owner key. Copy it again from ~/.else/data/owner-token."
                        .into(),
                )),
                Err(err) => step.set(Step::Failed(err.to_string())),
            }
        });
    };

    let field = |label: &'static str| {
        view()
            .flex_row()
            .items_center()
            .min_h(px(44.0))
            .pl_4()
            .child(
                text(label)
                    .w(px(96.0))
                    .text_size(px(17.0))
                    .text_color(colors::label()),
            )
    };
    let form = parts::group()
        .child(
            field("Server").child(
                input()
                    .flex_1()
                    .pr_4()
                    .min_h(px(44.0))
                    .value(address)
                    .on_change(move |t: &str| address.set(t.to_owned()))
                    .placeholder("100.x.y.z:7878 or https://…")
                    .keyboard_type(KeyboardType::Url)
                    .auto_capitalize(AutoCapitalize::None)
                    .auto_correct(false)
                    .return_key_type(ReturnKeyType::Next)
                    .text_size(px(17.0))
                    .text_color(colors::label())
                    .placeholder_color(|| color(colors::secondary()))
                    .label("Server address"),
            ),
        )
        .child(view().ml_4().h(px(0.5)).bg(colors::separator()))
        .child(
            field("Owner key").child(
                input()
                    .flex_1()
                    .pr_4()
                    .min_h(px(44.0))
                    .value(key)
                    .on_change(move |t: &str| key.set(t.to_owned()))
                    .placeholder("Required")
                    .masked(true)
                    .auto_capitalize(AutoCapitalize::None)
                    .auto_correct(false)
                    .return_key_type(ReturnKeyType::Go)
                    .on_submit(move |_: &str| submit())
                    .text_size(px(17.0))
                    .text_color(colors::label())
                    .placeholder_color(|| color(colors::secondary()))
                    .label("Owner key"),
            ),
        );

    let error = dynamic(move || match step.get() {
        Step::Failed(message) => Some(
            text(message)
                .mx_4()
                .px_4()
                .pt_3()
                .text_size(px(15.0))
                .line_height(px(20.0))
                .text_color(colors::destructive()),
        ),
        _ => None,
    });

    let button = view()
        .mx_4()
        .h(px(50.0))
        .rounded(px(25.0))
        .items_center()
        .justify_center()
        .bg(colors::tint())
        .opacity(move || {
            if ready.get() || checking.get() {
                1.0_f32
            } else {
                0.4
            }
        })
        .active(|s| s.opacity(0.7))
        .role(AccessibilityRole::Button)
        .label("Connect")
        .on_press(submit)
        .child(
            text(move || {
                if checking.get() {
                    "Connecting…"
                } else {
                    "Connect"
                }
            })
            .text_size(px(17.0))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(colors::on_tint()),
        );

    let content = scroll()
        .flex_1()
        .keyboard_dismiss_mode(KeyboardDismissMode::Interactive)
        .keyboard_taps(KeyboardTaps::Handled)
        .child(
            view()
                .pt(px(24.0))
                .pb(px(32.0))
                .gap_2()
                .child(
                    view()
                        .items_center()
                        .gap_3()
                        .px_6()
                        .pb_6()
                        .child(
                            view()
                                .size(px(84.0))
                                .rounded(px(20.0))
                                .border_curve(BorderCurve::Continuous)
                                .bg(colors::tint())
                                .items_center()
                                .justify_center()
                                .child(image(SERVER).size(px(48.0)).tint_color(colors::on_tint())),
                        )
                        .child(
                            text(if first { "Connect to Your Server" } else { "Add a Server" })
                                .text_size(px(28.0))
                                .font_weight(FontWeight::BOLD)
                                .text_center()
                                .text_color(colors::label()),
                        )
                        .child(
                            text("else.bot runs on your own computer. Enter its address and owner key to chat with your bots from this iPhone.")
                                .text_size(px(15.0))
                                .line_height(px(20.0))
                                .text_center()
                                .text_color(colors::secondary()),
                        ),
                )
                .child(form)
                .child(parts::footnote(
                    "Use the server's Tailscale address with Tailscale connected on this iPhone, or an https:// address. Read the key on the server with: cat ~/.else/data/owner-token",
                ))
                .child(error)
                .child(view().h(px(24.0)))
                .child(button),
        );

    let body = view()
        .flex_1()
        .bg(colors::background())
        .pb_keyboard()
        .child(content);
    if first {
        nav_page()
            .key("connect")
            .header_visible(false)
            .child(body.p_safe_area())
    } else {
        nav_page()
            .key(ADD_KEY)
            .title("Add Server")
            .header_large_title(false)
            .child(body)
    }
}
