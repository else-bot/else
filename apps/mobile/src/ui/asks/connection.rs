//! A bot asking to connect an account: an MCP server's sign-in or an inbox.
//! The phone signs in as the desktop does (else_client::connect): the
//! provider's page opens in an iOS sign-in sheet, a loopback listener in
//! the app catches its redirect, and the API redeems it. The listener then
//! sends the sheet to the app's scheme, which closes it.

use else_client::api::ClientError;
use else_client::connect::{Consent, Flow, Resolution, SignIn};
use else_client::oauth::callback::Finish;
use else_wire::questions::Question;
use std::time::Duration;

use futures_util::future::{Either, select};
use nsh_base::input;
use nsh_core::auth_session::{AuthSessionOutcome, AuthSessionRequest};
use nsh_core::prelude::*;
use uuid::Uuid;

use super::super::parts::colors;
use super::super::session;
use super::{Fill, button, eyebrow, frame};

/// The scheme the loopback listener redirects to once the provider sent
/// the owner back; the sheet closes on it.
const SCHEME: &str = "elsebot-signin";

/// Where the card is, as the owner sees it.
#[derive(Debug, Clone, PartialEq)]
enum Phase {
    /// Reading whether the account is already connected.
    Checking,
    /// Waiting for the owner, with why the last attempt did not finish.
    Ready(Option<String>),
    SigningIn,
    /// The provider sent the owner back; the API redeems the sign-in.
    Exchanging,
    Confirming,
    /// Connected, but approving the card failed: confirm again.
    ConfirmationRequired(String),
    CheckFailed(String),
    Cancelling,
    /// Settled; the question's update replaces the card.
    Done,
}

impl Phase {
    fn of(result: Result<Resolution, ClientError>, hint: Option<String>) -> Self {
        match result {
            Ok(Resolution::Disconnected) => Self::Ready(hint),
            Ok(Resolution::ConfirmationRequired(error)) => Self::ConfirmationRequired(error),
            Ok(Resolution::Recorded | Resolution::Cancelled) => Self::Done,
            Err(error) => Self::CheckFailed(error.to_string()),
        }
    }

    fn working(&self) -> Option<&'static str> {
        match self {
            Self::Checking => Some("Checking the connection…"),
            Self::SigningIn => Some("Waiting for the sign-in…"),
            Self::Exchanging => Some("Finishing the sign-in…"),
            Self::Confirming => Some("Connecting…"),
            Self::Cancelling => Some("Declining…"),
            Self::Done => Some("Done"),
            Self::Ready(_) | Self::ConfirmationRequired(_) | Self::CheckFailed(_) => None,
        }
    }
}

/// Whether `question` asks to connect an account this card handles.
pub(super) fn handles(question: &Question) -> bool {
    Consent::of(question).is_some()
}

pub(super) fn card(bot: Uuid, question: Question) -> impl IntoElement {
    let consent = Consent::of(&question).expect("a connection question");
    let flow = Flow {
        client: session::server().client.clone(),
        bot,
        question: question.id,
        consent: consent.clone(),
        finish: Finish::Redirect(format!("{SCHEME}://done")),
    };
    let phase = signal(Phase::Checking);
    let password = signal(String::new());
    let mut copy = flow.ready_copy();
    // The page opens in a sheet over the app, not in a browser.
    if copy.opens_browser {
        if copy.action == "Connect in browser" {
            copy.action = "Sign in";
        }
        copy.hint = "Opens the provider's sign-in page";
    }
    let takes_password = flow.takes_password();
    let flow = stored(flow);

    // The account may be connected already, from another card or bot.
    let check = move || {
        phase.set(Phase::Checking);
        let flow = flow.get();
        session::spawn_held(async move {
            let result = flow.recover().await;
            phase.try_set(Phase::of(result, None));
        });
    };
    check();

    let sign_in = move || {
        phase.set(Phase::SigningIn);
        let flow = flow.get();
        session::spawn_held(async move {
            let hint = match signed_in(&flow, phase).await {
                Ok(()) => {
                    phase.try_set(Phase::Confirming);
                    let result = flow.confirm().await;
                    phase.try_set(Phase::of(result, None));
                    return;
                }
                Err(error) => error,
            };
            let result = flow.recover().await;
            phase.try_set(Phase::of(result, Some(hint)));
        });
    };
    let save_password = move || {
        let typed = password.get_untracked().trim().to_owned();
        if typed.is_empty() {
            phase.set(Phase::Ready(Some(
                "Paste the app-specific password first.".into(),
            )));
            return;
        }
        // The password leaves the field with the request.
        password.set(String::new());
        phase.set(Phase::Exchanging);
        let flow = flow.get();
        session::spawn_held(async move {
            let (result, hint) = match flow.save_password(typed).await {
                Ok(()) => {
                    phase.try_set(Phase::Confirming);
                    (flow.confirm().await, None)
                }
                Err(error) => (flow.recover().await, Some(error.to_string())),
            };
            phase.try_set(Phase::of(result, hint));
        });
    };
    let confirm = move || {
        phase.set(Phase::Confirming);
        let flow = flow.get();
        session::spawn_held(async move {
            let result = flow.confirm().await;
            phase.try_set(Phase::of(result, None));
        });
    };
    let decline = move || {
        phase.set(Phase::Cancelling);
        let flow = flow.get();
        session::spawn_held(async move {
            let result = flow.cancel().await;
            phase.try_set(Phase::of(result, None));
        });
    };

    // An MCP card's context already names the provider and its scopes;
    // an inbox card names the address.
    let account = match &consent {
        Consent::Mcp(_) => None,
        Consent::Email(email) => Some(format!(
            "{} · {}{}",
            email.address,
            email.provider.label(),
            if email.replaces.is_some() {
                ". Replaces its current sign-in."
            } else {
                ""
            }
        )),
    };
    let actions =
        dynamic(move || {
            let current = phase.get();
            if let Some(label) = current.working() {
                return text(label)
                    .text_size(px(15.0))
                    .text_color(colors::secondary())
                    .into_element();
            }
            let (label, press): (&'static str, Box<dyn Fn()>) = match &current {
                Phase::ConfirmationRequired(_) => ("Confirm", Box::new(confirm)),
                Phase::CheckFailed(_) => ("Check again", Box::new(check)),
                Phase::Ready(Some(_)) if !takes_password => (copy.retry, Box::new(sign_in)),
                Phase::Ready(_) if takes_password => (copy.action, Box::new(save_password)),
                _ => (copy.action, Box::new(sign_in)),
            };
            let can_decline = matches!(current, Phase::Ready(_) | Phase::CheckFailed(_));
            untracked(|| {
                view()
                    .gap_2()
                    .child(
                        (takes_password && matches!(current, Phase::Ready(_))).then(|| {
                            input()
                                .min_h(px(40.0))
                                .px_3()
                                .rounded(px(10.0))
                                .bg(colors::fill())
                                .value(password)
                                .on_change(move |t: &str| password.set(t.to_owned()))
                                .masked(true)
                                .auto_capitalize(AutoCapitalize::None)
                                .auto_correct(false)
                                .placeholder("App-specific password")
                                .text_size(px(15.0))
                                .text_color(colors::label())
                                .caret_color(|| color(colors::tint()))
                                .label("App-specific password")
                        }),
                    )
                    .child(
                        view()
                            .flex_row()
                            .gap_2()
                            .child(can_decline.then(|| {
                                button("Decline", Fill::PlainDestructive, || true, decline)
                            }))
                            .child(button(label, Fill::Prominent, || true, press)),
                    )
                    .into_element()
            })
        });
    let note = dynamic(move || {
        let (message, failed) = match phase.get() {
            Phase::Ready(Some(hint)) => (hint, true),
            Phase::ConfirmationRequired(error) | Phase::CheckFailed(error) => (error, true),
            Phase::Ready(None) => (copy.hint.to_owned(), false),
            _ => return None,
        };
        Some(
            text(message)
                .text_size(px(13.0))
                .line_height(px(18.0))
                .text_color(if failed {
                    colors::destructive()
                } else {
                    colors::secondary()
                }),
        )
    });
    frame()
        .child(eyebrow("CONNECT AN ACCOUNT"))
        .child(
            text(question.title.clone())
                .text_size(px(17.0))
                .line_height(px(22.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors::label()),
        )
        .child(
            question
                .context
                .clone()
                .filter(|c| !c.trim().is_empty())
                .map(|context| {
                    text(context)
                        .text_size(px(15.0))
                        .line_height(px(20.0))
                        .text_color(colors::secondary())
                }),
        )
        .child(account.map(|account| {
            text(account)
                .selectable(true)
                .text_size(px(15.0))
                .text_color(colors::label())
        }))
        .child(actions)
        .child(note)
}

/// Opens the provider's page in a sign-in sheet and waits for the
/// provider to send the owner back. Until then, closing the sheet or the
/// question settling elsewhere abandons the sign-in; once the callback is
/// in, it is redeemed whatever the sheet does, so an exchange the server
/// started is never dropped. The error is the hint the card shows next.
async fn signed_in(flow: &Flow, phase: Signal<Phase>) -> Result<(), String> {
    // The card's own view may be scrolled away; the question decides.
    let withdrawn = || session::question_pending(flow.bot, flow.question) == Some(false);
    let mut signin: SignIn = flow.sign_in().await.map_err(|e| e.to_string())?;
    if withdrawn() {
        return Err("The connection request was withdrawn.".into());
    }
    let sheet = open_auth_session(AuthSessionRequest::new(signin.url(), SCHEME));
    let settled = async {
        while !withdrawn() {
            timer(Duration::from_secs(1)).await;
        }
    };
    let received = {
        let received = signin.received();
        futures_util::pin_mut!(received, settled);
        match select(received, select(sheet, settled)).await {
            Either::Left((received, _)) => {
                close_auth_session();
                received
            }
            Either::Right((Either::Left((outcome, _)), received)) => match outcome {
                // The listener redirected to the app: its callback is in,
                // or about to be.
                Ok(AuthSessionOutcome::Completed(_)) => received.await,
                // Closed by the owner, or replaced by another sheet.
                Ok(_) => return Err("Sign-in was closed before it finished.".into()),
                Err(error) => return Err(format!("The sign-in page could not open: {error}")),
            },
            Either::Right((Either::Right(((), _)), _)) => {
                close_auth_session();
                return Err("The connection request was withdrawn.".into());
            }
        }
    }
    .map_err(|e| e.to_string())?;
    phase.try_set(Phase::Exchanging);
    signin.redeem(received).await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disconnected_account_returns_to_ready_with_the_reason() {
        assert_eq!(
            Phase::of(Ok(Resolution::Disconnected), Some("Closed.".into())),
            Phase::Ready(Some("Closed.".into()))
        );
        assert_eq!(Phase::of(Ok(Resolution::Recorded), None), Phase::Done);
        assert_eq!(
            Phase::of(Err(ClientError::Protocol("offline".into())), None),
            Phase::CheckFailed("Unexpected response from the API: offline".into())
        );
    }
}
