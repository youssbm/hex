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
    /// Word or phrase pairs to apply and boost ahead of anything learned,
    /// "fonon" to "Phonon" or a whole spoken email to the address. Matching
    /// ignores case and the punctuation between the words.
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

/// Where the key is saved: beside `settings.json`.
fn config_path() -> Option<std::path::PathBuf> {
    crate::app_paths::support_dir()
        .ok()
        .map(|dir| dir.join("gradium.json"))
}

/// The last four characters of the configured key, for a status line. Never
/// enough of the key to use it.
pub(crate) fn key_hint() -> Option<String> {
    let key = active()?.api_key;
    let key = key.trim();
    (!key.is_empty()).then(|| key_hint_of(key))
}

/// Saves `key` into `gradium.json`, keeping the language, model, boost,
/// learning switch, and corrections already there, so another computer only
/// has to paste its own key.
pub(crate) fn save_api_key(key: &str) -> Result<()> {
    let path = config_path().ok_or_else(|| eyre!("the settings folder is unavailable"))?;
    save_api_key_at(&path, key)
}

fn save_api_key_at(path: &std::path::Path, key: &str) -> Result<()> {
    let key = key.trim();
    if key.is_empty() {
        return Err(eyre!("paste the API key from your Gradium account"));
    }
    if key.chars().any(char::is_whitespace) {
        return Err(eyre!("the API key cannot contain spaces"));
    }
    if !key.starts_with("gsk_") {
        return Err(eyre!("Gradium API keys start with gsk_"));
    }
    if key.chars().count() < 20 {
        return Err(eyre!("that key looks too short"));
    }
    let mut root: serde_json::Value = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let object = root
        .as_object_mut()
        .ok_or_else(|| eyre!("gradium.json is not a JSON object"))?;
    object.insert("api_key".into(), serde_json::Value::String(key.to_owned()));
    let text =
        serde_json::to_string_pretty(&root).wrap_err("could not write the Gradium settings")?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, text)
        .and_then(|()| std::fs::rename(&temporary, path))
        .wrap_err("could not save gradium.json")?;
    tracing::info!(hint = key_hint_of(key), "saved the Gradium API key");
    Ok(())
}

fn key_hint_of(key: &str) -> String {
    let count = key.chars().count();
    key.chars().skip(count.saturating_sub(4)).collect()
}

/// Whether to learn a new correction and watch an edited field. The app-wide
/// switch and the configured file both have to allow it.
pub(crate) fn learning_enabled() -> bool {
    crate::dictation::vocabulary::learning_enabled()
        && active().is_some_and(|config| config.learn.unwrap_or(true))
}

/// Whether either the app-wide switch or the configured file turned new
/// learning off. Existing learned corrections still apply while it is off.
pub(crate) fn learning_disabled() -> bool {
    !crate::dictation::vocabulary::learning_enabled()
        || active().is_some_and(|config| !config.learn.unwrap_or(true))
}

/// Transcribes a complete 16 kHz mono clip. Returns an error, rather than an
/// empty transcript, when Gradium cannot answer; the caller then falls back
/// to the local model.
pub(crate) fn transcribe(config: &Config, samples: &[f32], selection: &str) -> Result<String> {
    let started = Instant::now();
    let language = language(config, selection);
    let model = config.model.as_deref().unwrap_or("default");
    let boost = config.boost.unwrap_or(3).clamp(-6, 6);
    let vocabulary_on = crate::dictation::vocabulary::enabled();
    let learn_new = vocabulary_on
        && crate::dictation::vocabulary::learning_enabled()
        && config.learn.unwrap_or(true);
    let configured: Vec<(String, String)> = config
        .corrections
        .iter()
        .map(|(heard, right)| (heard.clone(), right.clone()))
        .collect();
    let entries = crate::dictation::vocabulary::entries();
    let mut learned = if vocabulary_on {
        learning::Learned::load()
    } else {
        learning::Learned::default()
    };
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
    let (transcript, corrected, learned_pairs) = if vocabulary_on {
        learned.apply_with(&transcript, &entries, &configured, learn_new)
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
            boost: None,
            learn: None,
            corrections: BTreeMap::new(),
        };
        assert_eq!(language(&config, "en"), "en");
        assert_eq!(language(&config, "pt-BR"), "pt");
        assert_eq!(language(&config, "ja"), "any");
    }
}

#[cfg(test)]
mod signin_tests {
    use super::*;

    fn directory(name: &str) -> std::path::PathBuf {
        let directory =
            std::env::temp_dir().join(format!("hex-gradium-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn saving_a_key_keeps_the_account_settings() {
        let directory = directory("key");
        let path = directory.join("gradium.json");
        std::fs::write(
            &path,
            r#"{"api_key":"gsk_old000000000000","language":"fr","model":"default","boost":5,"learn":false,"corrections":{"a":"b"}}"#,
        )
        .unwrap();
        save_api_key_at(&path, "  gsk_new000000000000  ").unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["api_key"], "gsk_new000000000000");
        assert_eq!(saved["language"], "fr");
        assert_eq!(saved["model"], "default");
        assert_eq!(saved["boost"], 5);
        assert_eq!(saved["learn"], false);
        assert_eq!(saved["corrections"]["a"], "b");
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn a_key_needs_the_gradium_prefix_and_no_spaces() {
        let directory = directory("bad-key");
        let path = directory.join("gradium.json");
        assert!(save_api_key_at(&path, "not-a-key").is_err());
        assert!(save_api_key_at(&path, "gsk_has spaces in it").is_err());
        assert!(save_api_key_at(&path, "gsk_short").is_err());
        assert!(!path.exists());
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn the_hint_is_the_last_four_characters() {
        assert_eq!(key_hint_of("gsk_abcdefgh1234"), "1234");
        assert_eq!(key_hint_of("abc"), "abc");
    }
}
