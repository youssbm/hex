//! The Snippets pane: text a spoken shortcut expands to. Snippets are the
//! things typed all the time (an email, a link, an address, a signature),
//! expanded by the same replacement engine as a mode's corrections, in every
//! dictation.

use rune::{El, TextEdit, edit_access, row, text};
use rune_kit::{ControlSize, IconButton, IconButtonVariant, TextInput};

use super::Ui;
use super::controls::{METRICS, Paint, note, page, raised, section_with};
use super::lists::remove_button;
use crate::app_settings::AppSettings;
use crate::desktop::i18n::tr;
use crate::dictation::text_replacements::TextReplacement;

static PLUS: &rune::Icon = &rune_theme_opencode::icons::PLUS;

#[derive(Default)]
pub(super) struct SnippetsPane {
    base: Vec<TextReplacement>,
    rows: Vec<(TextEdit, TextEdit)>,
}

impl SnippetsPane {
    pub(super) fn new(settings: &AppSettings) -> Self {
        let base = settings.dictation_processing.snippets.clone();
        Self {
            rows: Self::rows(&base),
            base,
        }
    }

    fn rows(snippets: &[TextReplacement]) -> Vec<(TextEdit, TextEdit)> {
        snippets
            .iter()
            .map(|snippet| {
                (
                    TextEdit::new(snippet.matched_phrase.clone()),
                    TextEdit::new(snippet.output.clone()),
                )
            })
            .collect()
    }

    /// The rows as settings, dropping a row the person added but has not
    /// filled on both sides. The value is kept exactly as typed.
    pub(super) fn project(&self) -> Vec<TextReplacement> {
        self.rows
            .iter()
            .filter_map(|(shortcut, value)| {
                let matched_phrase = shortcut.text.trim().to_owned();
                (!matched_phrase.is_empty() && !value.text.is_empty()).then(|| TextReplacement {
                    matched_phrase,
                    output: value.text.clone(),
                })
            })
            .collect()
    }

    /// Takes a saved list from elsewhere when this pane holds no unsaved
    /// typing and the saved list actually changed; a person's draft always
    /// wins, and an unrelated save leaves the fields and their caret alone.
    pub(super) fn rebase(&mut self, current: &[TextReplacement]) {
        if self.project() == self.base && self.base != current {
            self.rows = Self::rows(current);
        }
        self.base = current.to_vec();
    }
}

fn add_snippet(ui: &mut Ui) {
    ui.snippets
        .rows
        .push((TextEdit::default(), TextEdit::default()));
    if let Some((shortcut, _)) = ui.snippets.rows.last_mut() {
        shortcut.focused = true;
    }
}

fn remove_snippet(ui: &mut Ui, index: usize) {
    if index < ui.snippets.rows.len() {
        ui.snippets.rows.remove(index);
        ui.flush_drafts();
    }
}

pub(super) fn pane(ui: &Ui, p: Paint) -> El {
    let add = add_button(p).mr(-4.0);
    let content = page("snippets-content").child(intro(p)).child(section_with(
        p,
        "snippets",
        tr("Your snippets"),
        add,
        list(ui, p),
    ));
    super::scroll_body(p.k, "snippets-scroll", content)
}

/// The Add control, shared with the Output pane's snippet section.
pub(super) fn add_button(p: Paint) -> El {
    IconButton::new(PLUS)
        .variant(IconButtonVariant::Ghost)
        .control_size(ControlSize::Regular)
        .label(tr("Add snippet"))
        .on_press(add_snippet)
        .build(p.k)
        .key("add-snippet")
}

/// The snippet rows, or a note when there are none, shared with Output.
pub(super) fn list(ui: &Ui, p: Paint) -> Vec<El> {
    let mut rows = Vec::new();
    if ui.snippets.rows.is_empty() {
        rows.push(note(
            "no-snippets",
            tr("No snippets yet. Add one for your email, a link, or a signature."),
            p.muted(),
        ));
    }
    for (index, (shortcut, value)) in ui.snippets.rows.iter().enumerate() {
        rows.push(
            row()
                .key(format!("snippet{index}"))
                .px(METRICS.row_px)
                .py(6.0)
                .gap(8.0)
                .center_y()
                .child(
                    TextInput::new(shortcut)
                        .placeholder(tr("Spoken shortcut (e.g. my LinkedIn)"))
                        .width(None)
                        .on_edit(edit_access(move |ui: &mut Ui| {
                            &mut ui.snippets.rows[index].0
                        }))
                        .build(p.k)
                        .key("shortcut")
                        .grow(1.0)
                        .shrink(),
                )
                .child(text("→").key("arrow").font(p.k.ty.ui).color(p.faint()))
                .child(
                    TextInput::new(value)
                        .placeholder(tr("Text to paste (e.g. your profile link)"))
                        .width(None)
                        .on_edit(edit_access(move |ui: &mut Ui| {
                            &mut ui.snippets.rows[index].1
                        }))
                        .build(p.k)
                        .key("value")
                        .grow(1.0)
                        .shrink(),
                )
                .child(remove_button(
                    p,
                    ControlSize::Regular,
                    move |ui: &mut Ui| remove_snippet(ui, index),
                )),
        );
    }
    rows
}

fn intro(p: Paint) -> El {
    raised(p, "snippets-intro").child(
        rune::col()
            .key("copy")
            .px(METRICS.row_px)
            .py(14.0)
            .gap(4.0)
            .child(
                text(tr("Save the text you type all the time."))
                    .font(p.k.ty.body_medium)
                    .color(p.text()),
            )
            .child(
                text(tr(
                    "Speak a shortcut, and Hex pastes the saved text: an email, a link, an address, a signature.",
                ))
                .font(p.k.ty.small)
                .color(p.muted()),
            ),
    )
}
