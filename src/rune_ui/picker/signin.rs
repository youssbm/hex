//! Sign-in for the two cloud speech models: Gradium's API key and the Codex
//! session OpenAI STT reads. Both are saved on this computer, so another
//! computer signs in with its own key or session instead of the files this
//! one has.

use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Duration;

use rune::{El, TextEdit, col, edit_access, row, text};
use rune_kit::TextInput;

use crate::desktop::i18n::{tr, tr_fill};
use crate::rune_ui::Ui;
use crate::rune_ui::controls::{ButtonKind, Paint, button, note};
use crate::transcription::gradium;
use crate::transcription::models::TranscriptionModelId;
use crate::transcription::openai_stt::{self, SignIn as OpenAiSignIn};

pub(super) const WIDTH: f32 = 560.0;
const FIELD_WIDTH: f32 = WIDTH - 48.0;

#[derive(Default)]
pub(super) struct SignIn {
    pub(super) model: Option<TranscriptionModelId>,
    key: TextEdit,
    hint: Option<String>,
    error: Option<String>,
    saved: bool,
    worker: Option<Receiver<Result<OpenAiSignIn, String>>>,
    result: Option<Result<OpenAiSignIn, String>>,
}

pub(super) fn title(model: TranscriptionModelId) -> &'static str {
    if model == TranscriptionModelId::Gradium {
        tr("Sign in to Gradium")
    } else {
        tr("Sign in to OpenAI STT")
    }
}

pub(super) fn open(ui: &mut Ui, model: TranscriptionModelId) {
    {
        let state = &mut ui.picker.signin;
        state.model = Some(model);
        state.key = TextEdit::default();
        state.hint = None;
        state.error = None;
        state.saved = false;
        state.worker = None;
        state.result = None;
        if model == TranscriptionModelId::Gradium {
            state.key.focused = true;
            state.hint = gradium::key_hint();
        }
    }
    if model == TranscriptionModelId::OpenAiStt {
        start_openai(ui);
    }
}

pub(super) fn close(ui: &mut Ui) {
    ui.picker.signin = SignIn::default();
}

pub(super) fn poll(ui: &mut Ui) -> bool {
    let result = match ui.picker.signin.worker.as_ref().map(Receiver::try_recv) {
        Some(Ok(result)) => Some(result),
        Some(Err(TryRecvError::Disconnected)) => Some(Err(tr("The sign-in stopped.").into())),
        Some(Err(TryRecvError::Empty)) | None => None,
    };
    let Some(result) = result else {
        return false;
    };
    ui.picker.signin.worker = None;
    ui.picker.signin.result = Some(result);
    true
}

pub(super) fn body(ui: &Ui, p: Paint, model: TranscriptionModelId) -> El {
    let content = if model == TranscriptionModelId::Gradium {
        gradium_body(ui, p)
    } else {
        openai_body(ui, p)
    };
    col().key("signin-body").px(24.0).pb(16.0).child(content)
}

fn gradium_body(ui: &Ui, p: Paint) -> El {
    let state = &ui.picker.signin;
    let mut body = col().key("gradium-signin").w_full().gap(12.0).child(
        text(tr(
            "Paste the API key from your Gradium account. It is saved in gradium.json on this computer, so each computer keeps its own key.",
        ))
        .key("description")
        .font(p.k.ty.caption())
        .color(p.muted())
        .wrap(),
    );
    if let Some(hint) = &state.hint {
        body = body.child(note(
            "key-hint",
            &tr_fill(
                "A key ending in {} is saved. Paste a new one to replace it.",
                hint,
            ),
            p.muted(),
        ));
    }
    body = body.child(
        TextInput::new(&state.key)
            .placeholder(tr("gsk_..."))
            .width(Some(FIELD_WIDTH))
            .on_edit(edit_access(|ui: &mut Ui| &mut ui.picker.signin.key))
            .on_submit(|ui: &mut Ui, _| save_key(ui))
            .build(p.k)
            .key("gradium-key"),
    );
    if let Some(error) = &state.error {
        body = body.child(note("signin-error", error, p.danger()));
    }
    if state.saved {
        body = body.child(note(
            "signin-saved",
            tr("Saved. Gradium will use this key on the next dictation."),
            p.muted(),
        ));
    }
    let primary = if state.saved {
        button(
            p,
            "finish-signin",
            tr("Done"),
            ButtonKind::Primary,
            |ui: &mut Ui, _| close(ui),
        )
    } else {
        button(
            p,
            "save-gradium-key",
            tr("Save"),
            ButtonKind::Primary,
            |ui: &mut Ui, _| save_key(ui),
        )
    };
    let cancel = button(
        p,
        "cancel-signin",
        tr("Cancel"),
        ButtonKind::Secondary,
        |ui: &mut Ui, _| close(ui),
    );
    body.child(row().key("actions").gap(8.0).child(primary).child(cancel))
}

fn save_key(ui: &mut Ui) {
    let key = ui.picker.signin.key.text.clone();
    match gradium::save_api_key(&key) {
        Ok(()) => {
            ui.picker.signin.error = None;
            ui.picker.signin.saved = true;
            ui.picker.signin.hint = gradium::key_hint();
        }
        Err(error) => ui.picker.signin.error = Some(format!("{error:#}")),
    }
}

fn openai_body(ui: &Ui, p: Paint) -> El {
    let state = &ui.picker.signin;
    let mut body = col().key("openai-signin").w_full().gap(12.0).child(
        text(tr(
            "Hex uses the Codex session on this computer: it reads ~/.codex/auth.json and refreshes the access token with the same grant Codex uses. No token is copied anywhere else.",
        ))
        .key("description")
        .font(p.k.ty.caption())
        .color(p.muted())
        .wrap(),
    );
    if state.worker.is_some() {
        body = body.child(note(
            "signin-working",
            tr("Checking your Codex session."),
            p.muted(),
        ));
    }
    if let Some(result) = &state.result {
        let (copy, color) = match result {
            Ok(OpenAiSignIn::ApiKey) => (
                tr("Signed in with the OpenAI API key.").to_owned(),
                p.muted(),
            ),
            Ok(OpenAiSignIn::AlreadySignedIn { expires_in }) => (
                format!(
                    "{}{}",
                    tr("The Codex session is already signed in"),
                    valid_for(*expires_in)
                ),
                p.muted(),
            ),
            Ok(OpenAiSignIn::Refreshed { expires_in }) => (
                format!(
                    "{}{}",
                    tr("The Codex access token was refreshed"),
                    valid_for(*expires_in)
                ),
                p.muted(),
            ),
            Err(error) => (error.clone(), p.danger()),
        };
        body = body.child(note("signin-result", &copy, color));
    }
    let done = button(
        p,
        "finish-signin",
        tr("Done"),
        ButtonKind::Primary,
        |ui: &mut Ui, _| close(ui),
    );
    let mut actions = row().key("actions").gap(8.0).child(done);
    if state.result.as_ref().is_some_and(|result| result.is_err()) {
        actions = actions.child(button(
            p,
            "retry-signin",
            tr("Try again"),
            ButtonKind::Secondary,
            |ui: &mut Ui, _| {
                if let Some(model) = ui.picker.signin.model {
                    open(ui, model);
                }
            },
        ));
    }
    body.child(actions)
}

fn valid_for(expires_in: Option<Duration>) -> String {
    match expires_in {
        Some(duration) if duration.as_secs() >= 3_600 => {
            let hours = duration.as_secs() / 3_600;
            if hours >= 48 {
                tr_fill(
                    " and is valid for about {} days.",
                    &(hours / 24).to_string(),
                )
            } else {
                tr_fill(" and is valid for about {} hours.", &hours.to_string())
            }
        }
        _ => ".".into(),
    }
}

fn start_openai(ui: &mut Ui) {
    let (sender, receiver) = channel();
    let wake = wake(ui.modes.waker.clone());
    let spawned = std::thread::Builder::new()
        .name("hex-openai-signin".into())
        .spawn(move || {
            let result = openai_stt::sign_in().map_err(|error| format!("{error:#}"));
            let _ = sender.send(result);
            wake();
        });
    match spawned {
        Ok(_) => ui.picker.signin.worker = Some(receiver),
        Err(error) => ui.picker.signin.result = Some(Err(error.to_string())),
    }
}

fn wake(waker: Option<rune::Waker>) -> impl Fn() + Send + 'static {
    move || {
        if let Some(waker) = &waker {
            waker();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_settings::AppSettings;
    use crate::rune_ui::Service;

    fn ui() -> Ui {
        Ui::with(AppSettings::default(), Service::Fixture(Vec::new()))
    }

    #[test]
    fn opening_gradium_focuses_the_key_field() {
        let mut ui = ui();
        open(&mut ui, TranscriptionModelId::Gradium);
        assert_eq!(ui.picker.signin.model, Some(TranscriptionModelId::Gradium));
        assert_eq!(
            ui.picker.signin.model.map(title),
            Some(tr("Sign in to Gradium"))
        );
        assert!(ui.picker.signin.key.focused);
        close(&mut ui);
        assert_eq!(ui.picker.signin.model, None);
    }

    #[test]
    fn a_bad_key_stays_in_the_sheet_with_an_error() {
        let mut ui = ui();
        open(&mut ui, TranscriptionModelId::Gradium);
        ui.picker.signin.key.text = "not-a-key".into();
        save_key(&mut ui);
        assert!(ui.picker.signin.error.is_some());
        assert!(!ui.picker.signin.saved);
    }

    #[test]
    fn a_long_expiry_reads_in_days() {
        assert_eq!(
            valid_for(Some(Duration::from_secs(10 * 86_400))),
            " and is valid for about 10 days."
        );
        assert_eq!(
            valid_for(Some(Duration::from_secs(7_200))),
            " and is valid for about 2 hours."
        );
        assert_eq!(valid_for(None), ".");
    }
}
