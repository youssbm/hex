//! Gradium speech-to-text for dictation.
//!
//! When `gradium.json` sits beside `settings.json` (or `GRADIUM_API_KEY` is
//! set), a finished 16 kHz dictation clip goes to Gradium's speech endpoint
//! instead of the local GGUF model. The rest of the pipeline (HUD, output
//! processing, history, paste) is unchanged: Gradium supplies only the text.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use color_eyre::eyre::{Result, WrapErr, eyre};
use serde::Deserialize;

pub(crate) mod learning;

const ENDPOINT: &str = "https://api.gradium.ai/api/speech/asr";
/// A transcript is a few KiB; anything past this is a broken answer.
const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(60);
const SAMPLE_RATE: u32 = 16_000;

/// `%APPDATA%\hex\gradium.json`:
/// `{"api_key": "gsk_…", "language": "en", "model": "default",
///   "boost": 3, "learn": true, "corrections": {"fonon": "Phonon"}}`.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Config {
    pub api_key: String,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Gradium's keyword boost, -6 to 6. 3 is its recommended default.
    #[serde(default)]
    pub boost: Option<i32>,
    /// Correct and learn from vocabulary and mishearings. On by default.
    #[serde(default)]
    pub learn: Option<bool>,
    /// Word pairs to apply and boost ahead of anything learned, "fonon" to
    /// "Phonon". Each side is one word; they match whole tokens.
    #[serde(default)]
    pub corrections: BTreeMap<String, String>,
}

/// The key from the environment wins, so a terminal launch can test another
/// account without editing the file.
fn env_key() -> Option<String> {
    std::env::var("GRADIUM_API_KEY")
        .ok()
        .map(|key| key.trim().to_owned())
        .filter(|key| !key.is_empty())
}

fn env_config() -> Config {
    Config {
        api_key: env_key().unwrap_or_default(),
        language: std::env::var("GRADIUM_LANGUAGE").ok(),
        model: std::env::var("GRADIUM_MODEL").ok(),
        boost: None,
        learn: None,
        corrections: BTreeMap::new(),
    }
}

/// The configured Gradium account, or `None` when dictation stays local.
pub(crate) fn active() -> Option<Config> {
    if let Some(api_key) = env_key() {
        return Some(Config {
            api_key,
            ..env_config()
        });
    }
    let path = crate::app_paths::support_dir().ok()?.join("gradium.json");
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Whether to correct transcripts and watch an edited field for corrections.
pub(crate) fn learning_enabled() -> bool {
    active().is_some_and(|config| config.learn.unwrap_or(true))
}

/// Transcribes a complete 16 kHz mono clip. Returns an error, rather than an
/// empty transcript, when Gradium cannot answer; the caller then falls back
/// to the local model.
pub(crate) fn transcribe(config: &Config, samples: &[f32], selection: &str) -> Result<String> {
    let started = Instant::now();
    let language = language(config, selection);
    let model = config.model.as_deref().unwrap_or("default");
    let boost = config.boost.unwrap_or(3).clamp(-6, 6);
    let learn = config.learn.unwrap_or(true);
    let configured: Vec<(String, String)> = config
        .corrections
        .iter()
        .map(|(heard, right)| (heard.clone(), right.clone()))
        .collect();
    let entries = crate::dictation::vocabulary::entries();
    let mut learned = learning::Learned::load();
    let words = learning::keywords(&entries, &learned, &configured);

    // 10 frames (800 ms) of context is the realtime default; the REST
    // endpoint otherwise waits 50 frames (4 s) before its first segment.
    let mut request_config = serde_json::json!({
        "language": &language,
        "delay_in_frames": 10,
    });
    if !words.is_empty() {
        request_config["keywords"] = serde_json::json!({
            "words": &words,
            "boost": boost,
        });
    }
    let json_config = request_config.to_string();

    let mut url = url::Url::parse(ENDPOINT)?;
    url.query_pairs_mut()
        .append_pair("model", model)
        .append_pair("json_config", &json_config);

    let agent = crate::net::agent(url.as_str(), |config| {
        config
            .http_status_as_error(false)
            .timeout_global(Some(DEADLINE))
    });
    let request = ureq::http::Request::builder()
        .method("POST")
        .uri(url.as_str())
        .header("x-api-key", &config.api_key)
        .header("Content-Type", "audio/wav")
        .header("Accept", "application/x-ndjson")
        .body(wav(samples))
        .map_err(|error| eyre!("could not build the Gradium request: {error}"))?;
    let mut response = agent
        .run(request)
        .map_err(|error| eyre!("could not reach Gradium: {}", crate::net::reason(&error)))?;
    let status = response.status();
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_vec()
        .wrap_err("could not read Gradium's answer")?;

    if !status.is_success() {
        let detail = String::from_utf8_lossy(&body);
        let detail: String = detail.trim().chars().take(300).collect();
        return Err(eyre!("Gradium answered HTTP {}: {detail}", status.as_u16()));
    }

    let transcript = parse(&body)?;
    let (transcript, corrected, learned_pairs) = if learn {
        learned.apply(&transcript, &entries, &configured)
    } else {
        (transcript, 0, 0)
    };
    tracing::info!(
        ms = started.elapsed().as_millis(),
        language = %language,
        keywords = words.len(),
        corrected,
        learned = learned_pairs,
        chars = transcript.chars().count(),
        "transcribed a clip with Gradium"
    );
    Ok(transcript)
}

/// Gradium answers with one JSON message per line: `text` segments, timing
/// and voice-activity `step`s, and `end_of_stream`.
fn parse(body: &[u8]) -> Result<String> {
    let text = String::from_utf8_lossy(body);
    let mut parts = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match message.get("type").and_then(|kind| kind.as_str()) {
            Some("text") => {
                if let Some(part) = message.get("text").and_then(|part| part.as_str()) {
                    parts.push(part.to_owned());
                }
            }
            Some("error") => {
                let detail = message
                    .get("message")
                    .and_then(|detail| detail.as_str())
                    .unwrap_or("Gradium reported an error");
                return Err(eyre!("Gradium: {detail}"));
            }
            _ => {}
        }
    }
    Ok(parts.join(" ").trim().to_owned())
}

/// Gradium speaks `en`, `fr`, `de`, `es`, `pt`, or `any`; Hex may be set to
/// another language's model, which Gradium's auto-detect handles.
fn language(config: &Config, selection: &str) -> String {
    let requested = config.language.as_deref().unwrap_or(selection);
    let base = requested
        .split(['-', '_'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match base.as_str() {
        "en" | "fr" | "de" | "es" | "pt" => base,
        _ => "any".to_owned(),
    }
}

/// A 16-bit mono PCM WAV, so Gradium resamples the clip itself.
fn wav(samples: &[f32]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        let sample = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_text_segments_and_ignores_timing() {
        let body = br#"{"type":"text","text":"hello world","start_s":0.1}
{"type":"step","vad":[]}
{"type":"text","text":"again"}
{"type":"end_of_stream"}"#;
        assert_eq!(parse(body).unwrap(), "hello world again");
    }

    #[test]
    fn an_error_message_is_an_error() {
        let body = br#"{"type":"error","message":"bad key"}"#;
        assert!(parse(body).is_err());
    }

    #[test]
    fn wav_header_matches_the_clip() {
        let bytes = wav(&[0.0, 1.0, -1.0]);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(bytes.len(), 44 + 6);
        assert_eq!(
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            16_000
        );
    }

    #[test]
    fn only_gradium_languages_reach_the_server() {
        let config = Config {
            api_key: String::new(),
            language: None,
            model: None,
        };
        assert_eq!(language(&config, "en"), "en");
        assert_eq!(language(&config, "pt-BR"), "pt");
        assert_eq!(language(&config, "ja"), "any");
    }
}
