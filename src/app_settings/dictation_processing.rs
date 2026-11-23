//! What happens to a dictation's text before it pastes: Global's steps and
//! the modes chosen by application or website.

use serde::{Deserialize, Serialize};

use super::{KeyboardModelChoice, TextReplacement};
use crate::dictation::vocabulary::{VocabularyEntry, serialize_saved};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct DictationProcessingSettings {
    pub default_mode: DictationMode,
    pub modes: Vec<DictationMode>,
    /// Spoken shortcuts expanded in every dictation, whatever its mode.
    /// A snippet is the text you type all the time: an email, a link, an
    /// address, a signature.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub snippets: Vec<TextReplacement>,
    /// A dictation keeps the final period its text ends with. Off removes one
    /// trailing period after the last transformation, before it pastes.
    #[serde(default = "default_true")]
    pub end_with_dot: bool,
    /// OpenCode formats each dictation after transcription, before it pastes.
    #[serde(default)]
    pub post_transcription: PostTranscriptionSettings,
}

fn default_true() -> bool {
    true
}

/// A global OpenCode formatter that runs after transcription and before the
/// mode's transformations and the final paste.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct PostTranscriptionSettings {
    pub enabled: bool,
    /// `provider/model` from the OpenCode catalog, or `None` until one is
    /// chosen. A model is required before the switch can turn on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    /// The formatting instructions. Empty means the built-in prompt.
    pub prompt: String,
}

impl Default for PostTranscriptionSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            model: None,
            variant: None,
            prompt: crate::dictation::post_transcription::DEFAULT_PROMPT.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_transcription_defaults_off_with_the_built_in_prompt() {
        let settings = DictationProcessingSettings::default();
        assert!(!settings.post_transcription.enabled);
        assert!(settings.post_transcription.model.is_none());
        assert!(
            settings
                .post_transcription
                .prompt
                .contains("format raw dictation")
        );
    }

    #[test]
    fn older_settings_without_the_key_load_the_default() {
        let settings: DictationProcessingSettings =
            serde_json::from_str(r#"{"default_mode":{"name":"Global"}}"#).unwrap();
        assert!(!settings.post_transcription.enabled);
        assert!(settings.post_transcription.model.is_none());
        assert!(!settings.post_transcription.prompt.is_empty());
    }

    #[test]
    fn post_transcription_settings_round_trip() {
        let settings = DictationProcessingSettings {
            post_transcription: PostTranscriptionSettings {
                enabled: true,
                model: Some("opencode-go/glm-5.3-flash".into()),
                variant: Some("high".into()),
                prompt: "Keep it short.".into(),
            },
            ..DictationProcessingSettings::default()
        };
        let json = serde_json::to_string(&settings).unwrap();
        let back: DictationProcessingSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, settings);
    }
}

impl Default for DictationProcessingSettings {
    fn default() -> Self {
        Self {
            default_mode: DictationMode {
                name: "Global".into(),
                ..Default::default()
            },
            modes: Vec::new(),
            snippets: Vec::new(),
            end_with_dot: true,
            post_transcription: PostTranscriptionSettings::default(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct DictationMode {
    pub name: String,
    pub applications: Vec<String>,
    pub browser_hosts: Vec<String>,
    pub replacements: Vec<TextReplacement>,
    pub transformations: Vec<String>,
    pub post_processing: DictationPostProcessing,
    /// The speech model and language this mode selects while it matches the
    /// foreground app (Labs › Switch model with app). None means don't switch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speech: Option<KeyboardModelChoice>,
    /// Names heard and spelled here ahead of the shared vocabulary.
    #[serde(
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "serialize_saved"
    )]
    pub vocabulary: Vec<VocabularyEntry>,
}

/// Persisted OpenCode rewrite settings for one mode.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct DictationPostProcessing {
    pub enabled: bool,
    pub prompt: String,
    /// Optional `provider/model` key from the OpenCode catalog.
    pub model: Option<String>,
    pub variant: Option<String>,
    pub deadline_seconds: u64,
}

impl Default for DictationPostProcessing {
    fn default() -> Self {
        Self {
            enabled: false,
            prompt: "Rewrite the transcript into clear, natural text while preserving the speaker's intended meaning.".into(),
            model: None,
            variant: None,
            deadline_seconds: 30,
        }
    }
}
