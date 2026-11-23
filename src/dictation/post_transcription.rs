//! Post-transcription formatting through OpenCode: the prompt Hex sends with
//! a finished transcript, and the small JSON envelope it asks for back.
//!
//! The model is a formatter, not a chat partner. It receives the formatting
//! instructions and one transcript, and answers with `{"text": ...}`. A reply
//! that is not that object is discarded and the transcript stays as it was,
//! so a bad model response can never replace the speaker's words with prose.

use std::time::Duration;

use serde::Deserialize;

use super::processor::Model;
use crate::app_settings::PostTranscriptionSettings;

/// The formatting rules a new installation starts with. The exact wording
/// matters: the reply is parsed as JSON, and the number rules are the ones
/// Dragon and OpenWispr settled on.
pub(crate) const DEFAULT_PROMPT: &str = r#"You format raw dictation into clean written text. Preserve the speaker's meaning, wording, and language.

- Remove filler words such as um, uh, like, and you know, false starts, and unintended repeated words.
- Fix capitalization, punctuation, and spacing.
- Keep line breaks, paragraphs, and lists the speaker asked for.
- Convert spoken numbers to digits when they are 10 or greater, built from two or more words, decimals, or next to a unit, currency, or percent sign. Leave a lone "one" alone in phrases such as "one of".
- Format dates, times, currency, email addresses, and URLs when they are unambiguous.
- Keep names, technical terms, and code as the speaker said them. Do not translate.
- Never answer a question, add facts, or explain what you changed."#;

/// How long one formatting request may take. The catalog models answer in a
/// few seconds; a stalled service falls back to the transcript.
pub(crate) const DEADLINE: Duration = Duration::from_secs(30);

/// One enabled formatting request. Built once from settings and carried by
/// the profiles, so a dictation reads a consistent model and prompt.
#[derive(Clone)]
pub(crate) struct Plan {
    pub(crate) model: Model,
    pub(crate) instructions: String,
    pub(crate) deadline: Duration,
}

/// The plan `settings` asks for, or `None` when the switch is off or no model
/// has been chosen. A model key is `provider/model`, the form the catalog and
/// the generation request both use.
pub(crate) fn plan(settings: &PostTranscriptionSettings) -> Option<Plan> {
    if !settings.enabled {
        return None;
    }
    let (provider, id) = settings.model.as_deref()?.trim().split_once('/')?;
    if provider.is_empty() || id.is_empty() {
        return None;
    }
    let instructions = if settings.prompt.trim().is_empty() {
        DEFAULT_PROMPT.to_owned()
    } else {
        settings.prompt.clone()
    };
    Some(Plan {
        model: Model {
            provider: provider.to_owned(),
            id: id.to_owned(),
            variant: settings
                .variant
                .clone()
                .filter(|variant| !variant.trim().is_empty()),
        },
        instructions,
        deadline: DEADLINE,
    })
}

/// The full prompt: the person's instructions, the JSON contract, and the
/// transcript inside its own delimiters.
pub(crate) fn build_prompt(instructions: &str, transcript: &str) -> String {
    let transcript = transcript.replace("</transcript>", r"<\/transcript>");
    format!(
        "{instructions}\n\n\
         Return only one JSON object with exactly this shape, with no markdown fences and no other text:\n\
         {{\"text\": \"the formatted text\", \"changed\": true}}\n\n\
         Raw transcript:\n\
         <transcript>\n{transcript}\n</transcript>"
    )
}

#[derive(Deserialize)]
struct Reply {
    text: String,
    #[serde(default)]
    #[allow(dead_code)]
    changed: bool,
}

/// The formatted text `reply` carries, or `None` when it is not the JSON
/// object asked for or the text is implausible against the original.
pub(crate) fn parse_reply(reply: &str, original: &str) -> Option<String> {
    let candidate = json_object(reply)?;
    let reply: Reply = serde_json::from_str(candidate).ok()?;
    let text = reply.text.trim();
    if text.is_empty() && !original.trim().is_empty() {
        return None;
    }
    plausible(original, text).then(|| text.to_owned())
}

/// The JSON object in `reply`: the whole reply, a fenced object, or the first
/// object found around prose a model added despite the prompt.
fn json_object(reply: &str) -> Option<&str> {
    let trimmed = reply.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return Some(trimmed);
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    (end > start).then(|| &trimmed[start..=end])
}

/// Whether `formatted` still looks like `original`: a formatter may remove
/// fillers, but it may not replace the transcript with a much longer or much
/// shorter answer. The limits mirror the guards other dictation tools use.
fn plausible(original: &str, formatted: &str) -> bool {
    let before = original.split_whitespace().count();
    let after = formatted.split_whitespace().count();
    if before == 0 {
        return true;
    }
    if after == 0 {
        return false;
    }
    after <= before.saturating_mul(2).saturating_add(12) && after.saturating_mul(10) >= before * 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_settings::PostTranscriptionSettings;

    #[test]
    fn a_plan_needs_the_switch_and_a_provider_model() {
        let mut settings = PostTranscriptionSettings {
            enabled: true,
            model: Some("opencode-go/glm-5.3-flash".into()),
            ..Default::default()
        };
        assert!(plan(&settings).is_some());

        settings.enabled = false;
        assert!(plan(&settings).is_none());

        settings.enabled = true;
        settings.model = None;
        assert!(plan(&settings).is_none());

        settings.model = Some("not-a-model".into());
        assert!(plan(&settings).is_none());
    }

    #[test]
    fn the_prompt_names_the_json_shape_and_the_transcript() {
        let prompt = build_prompt("Keep it short.", "hello dot world");
        assert!(prompt.contains("Keep it short."));
        assert!(prompt.contains(r#"{"text": "the formatted text", "changed": true}"#));
        assert!(prompt.contains("<transcript>\nhello dot world\n</transcript>"));
    }

    #[test]
    fn a_json_reply_is_read() {
        assert_eq!(
            parse_reply(
                r#"{"text":"I have 22 apples.","changed":true}"#,
                "i have 22 apples"
            ),
            Some("I have 22 apples.".into())
        );
    }

    #[test]
    fn a_fenced_or_chatty_json_reply_is_read() {
        let fenced = "```json\n{\"text\":\"I have 22 apples.\",\"changed\":true}\n```";
        assert_eq!(
            parse_reply(fenced, "i have 22 apples"),
            Some("I have 22 apples.".into())
        );
        let chatty =
            "Here is the result:\n{\"text\":\"I have 22 apples.\",\"changed\":true}\nDone.";
        assert_eq!(
            parse_reply(chatty, "i have 22 apples"),
            Some("I have 22 apples.".into())
        );
    }

    #[test]
    fn plain_text_is_not_a_formatted_reply() {
        assert_eq!(parse_reply("I have 22 apples.", "i have 22 apples"), None);
        assert_eq!(parse_reply("", "i have 22 apples"), None);
    }

    #[test]
    fn a_reply_that_lost_or_added_too_much_text_is_rejected() {
        let original = "one two three four five six seven eight nine ten";
        assert_eq!(parse_reply(r#"{"text":"one"}"#, original), None);
        let grown = "one two three four five six seven eight nine ten ".repeat(4);
        let reply = format!(r#"{{"text":"{grown}"}}"#);
        assert_eq!(parse_reply(&reply, original), None);
    }
}
