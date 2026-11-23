//! Post Transcription: the global OpenCode formatter that runs after a
//! dictation is transcribed. The pane connects the person's OpenCode CLI,
//! lists the models that CLI reports, and edits the formatting instructions.

use std::time::{Duration, Instant};

use rune::{El, TextEdit, Tx, col, row, text};
use rune_kit::Switch;
use rune_kit::TextArea;

use super::controls::{
    ButtonKind, DESCRIPTION, METRICS, Paint, button, page, raised, section_with, setting_row,
    status_dot,
};
use super::{Ui, models};
use crate::app_settings::AppSettings;
use crate::desktop::i18n::{tr, tr_fill};

/// How long the Save button says Saved after it commits.
const SAVED: Duration = Duration::from_secs(2);

#[derive(Default)]
pub(super) struct PostTranscriptionPane {
    /// The formatting instructions, as edited on this pane.
    prompt: TextEdit,
    /// The saved prompt the draft above started from.
    prompt_base: String,
    /// When Save last committed, so its label says Saved briefly.
    saved_at: Option<Instant>,
    /// Whether this process asked OpenCode to reload its configuration once
    /// the pane opened, so a newly authenticated provider is in the service's
    /// registry before the first request.
    reloaded: bool,
}

impl PostTranscriptionPane {
    pub(super) fn new(settings: &AppSettings) -> Self {
        let prompt = settings
            .dictation_processing
            .post_transcription
            .prompt
            .clone();
        Self {
            prompt: TextEdit::new(prompt.clone()),
            prompt_base: prompt,
            ..Self::default()
        }
    }

    /// Writes the prompt field into `settings`, as the other editors do.
    pub(super) fn project(&self, settings: &mut AppSettings) {
        settings.dictation_processing.post_transcription.prompt = self.prompt.text.clone();
    }

    /// Takes a prompt saved elsewhere when this field holds no unsaved typing.
    pub(super) fn rebase_prompt(&mut self, current: &str) {
        if self.prompt.text == self.prompt_base && self.prompt_base != current {
            self.prompt = TextEdit::new(current.to_owned());
        }
        self.prompt_base = current.to_owned();
    }
}

/// Clears the Save button's Saved label once it has shown long enough, and
/// asks OpenCode to reload once when the pane first opens.
pub(super) fn poll(ui: &mut Ui) -> bool {
    if !ui.post_transcription.reloaded && ui.live() {
        ui.post_transcription.reloaded = true;
        reload_opencode_in_background();
    }
    if ui
        .post_transcription
        .saved_at
        .is_some_and(|at| at.elapsed() >= SAVED)
    {
        ui.post_transcription.saved_at = None;
        return true;
    }
    false
}

pub(super) fn pane(ui: &Ui, p: Paint, layer: &mut Vec<El>) -> El {
    let content = page("post-transcription-content")
        .child(intro(ui, p))
        .child(connection(ui, p))
        .child(model(ui, p, layer))
        .child(formatting(ui, p));
    super::scroll_body(p.k, "post-transcription-scroll", content)
}

/// The pane's opening card: what runs, when, and what happens on failure.
fn intro(ui: &Ui, p: Paint) -> El {
    raised(p, "post-transcription-intro").child(
        col()
            .key("copy")
            .px(METRICS.row_px)
            .py(14.0)
            .gap(4.0)
            .child(
                text(tr("Clean up each dictation with OpenCode."))
                    .font(p.k.ty.body_medium)
                    .color(p.text()),
            )
            .child(
                text(tr(
                    "Hex sends the finished transcript to the model below, then pastes the formatted text. If the model fails, the original transcript is pasted. It adds a few seconds to each dictation.",
                ))
                .font(DESCRIPTION)
                .color(p.muted())
                .wrap(),
            ),
    )
}

/// The OpenCode CLI's status, with the two actions that get it ready.
fn connection(ui: &Ui, p: Paint) -> El {
    let connected = ui.models.available();
    let (status, detail, dot) = if connected {
        let count = ui.models.model_count().unwrap_or(0);
        (
            tr("OpenCode is connected"),
            tr_fill(
                "{} models available. Choose one below, then turn on post transcription.",
                &count.to_string(),
            ),
            status_dot(p.success()),
        )
    } else if ui.models.is_not_running() {
        (
            tr("OpenCode is not running"),
            tr("Start it, or sign in to the OpenCode CLI, then check again."),
            status_dot(p.warning()),
        )
    } else {
        (
            tr("OpenCode is not connected yet"),
            tr("Sign in to the OpenCode CLI in the terminal, then check again."),
            status_dot(p.warning()),
        )
    };
    let mut actions = row().key("actions").gap(8.0).center_y();
    if ui.models.is_not_running() {
        actions = actions.child(button(
            p,
            "post-transcription-start",
            tr("Start OpenCode"),
            ButtonKind::Secondary,
            |ui: &mut Ui, _: &mut Tx| {
                if let Err(error) = start_opencode() {
                    ui.error = Some(error);
                } else {
                    ui.models.reload();
                }
            },
        ));
    }
    actions = actions
        .child(button(
            p,
            "post-transcription-check",
            tr("Check again"),
            ButtonKind::Secondary,
            |ui: &mut Ui, _: &mut Tx| {
                ui.post_transcription.reloaded = false;
                ui.models.reload();
            },
        ))
        .child(button(
            p,
            "post-transcription-sign-in",
            tr("Sign in"),
            ButtonKind::Primary,
            |ui: &mut Ui, _: &mut Tx| {
                if let Err(error) = open_auth_terminal() {
                    ui.error = Some(error);
                } else {
                    ui.post_transcription.reloaded = false;
                }
            },
        ));
    raised(p, "post-transcription-connection").child(
        row()
            .key("connection")
            .px(METRICS.row_px)
            .py(14.0)
            .gap(12.0)
            .center_y()
            .flex_wrap()
            .child(dot)
            .child(
                col()
                    .key("copy")
                    .grow(1.0)
                    .shrink()
                    .gap(2.0)
                    .child(text(status).font(p.k.ty.ui).color(p.text()))
                    .child(
                        text(detail)
                            .key("detail")
                            .font(DESCRIPTION)
                            .color(p.muted())
                            .wrap(),
                    ),
            )
            .child(actions),
    )
}

/// The catalog's model row and its thinking variant, when the model has one.
fn model(ui: &Ui, p: Paint, layer: &mut Vec<El>) -> El {
    let rows = models::rows(ui, p, models::Target::PostTranscription, layer);
    section_with(
        p,
        "post-transcription-model",
        tr("Formatting model"),
        text(tr("OpenCode"))
            .key("hint")
            .font(p.k.ty.small)
            .color(p.faint()),
        rows,
    )
}

/// The on/off switch and the instructions the model follows.
fn formatting(ui: &Ui, p: Paint) -> El {
    let settings = &ui.settings.dictation_processing.post_transcription;
    let ready = ui.models.available() && settings.model.is_some();
    let can_toggle = settings.enabled || ready;
    let reason = if ready {
        tr("Runs after transcription and before your transformations.")
    } else if settings.enabled {
        tr("OpenCode is not connected, so dictations paste unchanged until it is.")
    } else {
        tr("Choose a formatting model to turn this on.")
    };
    let switch = Switch::new(settings.enabled)
        .disabled(!can_toggle)
        .on_toggle(|ui: &mut Ui| {
            let on = !ui.settings.dictation_processing.post_transcription.enabled;
            ui.update(|settings| {
                settings.dictation_processing.post_transcription.enabled = on;
            });
        })
        .build(p.k)
        .key("post-transcription-toggle")
        .label(tr("Post transcription"));
    let save = button(
        p,
        "save-post-transcription",
        if ui.post_transcription.saved_at.is_some() {
            tr("Saved")
        } else {
            tr("Save")
        },
        ButtonKind::Primary,
        |ui: &mut Ui, _: &mut Tx| {
            ui.flush_drafts();
            if ui.error.is_none() {
                ui.post_transcription.saved_at = Some(Instant::now());
            }
        },
    );
    let rows = vec![
        setting_row(
            p,
            "post-transcription-enabled",
            tr("Post transcription"),
            Some(reason),
            switch,
        ),
        prompt_row(ui, p),
    ];
    section_with(
        p,
        "post-transcription-formatting",
        tr("Formatting"),
        save,
        rows,
    )
}

fn prompt_row(ui: &Ui, p: Paint) -> El {
    col()
        .key("post-transcription-prompt")
        .px(METRICS.row_px)
        .py(14.0)
        .gap(6.0)
        .child(
            text(tr("Instructions"))
                .key("title")
                .font(p.k.ty.ui)
                .color(p.text()),
        )
        .child(
            text(tr(
                "Tell the model how to write your dictation. Hex asks for a JSON reply and keeps the transcript when the reply is unusable.",
            ))
            .key("description")
            .font(DESCRIPTION)
            .color(p.muted())
            .wrap(),
        )
        .child(
            TextArea::new(&ui.post_transcription.prompt)
                .placeholder(tr("Describe how the model should format your dictation."))
                .rows(8)
                .width(None)
                .on_edit(|ui: &mut Ui| &mut ui.post_transcription.prompt)
                .build(p.k)
                .key("post-transcription-prompt-input"),
        )
}

/// Opens a terminal on the OpenCode sign-in flow. The CLI's login is
/// interactive, so Hex hands it to the person instead of hiding it.
fn open_auth_terminal() -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        std::process::Command::new("powershell.exe")
            .args(["-NoExit", "-Command", "opencode auth login"])
            .creation_flags(CREATE_NEW_CONSOLE)
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("Could not open the OpenCode sign-in: {error}"))
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("osascript")
            .args([
                "-e",
                "tell application \"Terminal\" to do script \"opencode auth login\"",
                "-e",
                "tell application \"Terminal\" to activate",
            ])
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("Could not open the OpenCode sign-in: {error}"))
    }
    #[cfg(target_os = "linux")]
    {
        for (program, args) in [
            (
                "x-terminal-emulator",
                vec!["-e", "opencode", "auth", "login"],
            ),
            ("gnome-terminal", vec!["--", "opencode", "auth", "login"]),
            ("konsole", vec!["-e", "opencode", "auth", "login"]),
        ] {
            if std::process::Command::new(program)
                .args(args)
                .spawn()
                .is_ok()
            {
                return Ok(());
            }
        }
        Err(tr("Open a terminal and run: opencode auth login").to_owned())
    }
}

/// Starts the OpenCode background service that the CLI uses.
fn start_opencode() -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("opencode")
            .args(["service", "start"])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("Could not start OpenCode: {error}"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::process::Command::new("opencode")
            .args(["service", "start"])
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("Could not start OpenCode: {error}"))
    }
}

/// Reloads OpenCode's configuration off the UI thread once the pane opens.
/// A login or a new provider can be in the CLI's config while the running
/// service still has the previous model registry.
fn reload_opencode_in_background() {
    std::thread::spawn(|| {
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            let _ = std::process::Command::new("opencode")
                .args(["reload"])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = std::process::Command::new("opencode")
                .args(["reload"])
                .output();
        }
    });
}
