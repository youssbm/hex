//! OpenAI speech-to-text for dictation.
//!
//! Speech goes to one of two endpoints, exactly as the `openai-stt` skill's
//! `server.py` sends it: the official transcription API with an
//! `OPENAI_API_KEY`, or the ChatGPT desktop session's transcribe endpoint with
//! the access token in `~/.codex/auth.json`. Hex talks to the upstream
//! directly, so no sidecar has to be running while dictation does.
//!
//! `%APPDATA%\hex\openai-stt.json`:
//! `{"backend": "codex_session", "model": "gpt-4o-mini-transcribe",
//!   "language": "en", "auth_path": "", "api_key": ""}`.
//! A missing file with `OPENAI_API_KEY` set uses the API-key backend.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use color_eyre::eyre::{Result, WrapErr, eyre};
use serde::Deserialize;

use crate::dictation::vocabulary::VocabularyEntry;

const API_URL: &str = "https://api.openai.com/v1/audio/transcriptions";
const CODEX_URL: &str = "https://chatgpt.com/backend-api/transcribe";
const MAX_AUDIO_BYTES: usize = 25 * 1024 * 1024;
/// A transcript is a few KiB; anything past this is a broken answer.
const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(120);
const SAMPLE_RATE: u32 = 16_000;
/// Characters of context sent as the transcription prompt. The service's
/// prompt slot is about 224 tokens, so this stays inside it.
pub(crate) const PROMPT_CHARS: usize = 900;
/// The person's instruction's share, leaving room for vocabulary terms.
const INSTRUCTION_CHARS: usize = 800;

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub(crate) struct Config {
    /// `api_key` (the official API) or `codex_session` (the ChatGPT app's
    /// session token). The skill calls the second backend `codex_session`.
    pub backend: String,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub language: Option<String>,
    /// The Codex `auth.json` to read; empty means `~/.codex/auth.json`.
    pub auth_path: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            backend: "api_key".into(),
            model: None,
            api_key: None,
            language: None,
            auth_path: None,
        }
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// The configured account, or `None` when dictation should stay local. The
/// environment can name another backend, model, or language for one launch.
pub(crate) fn active() -> Option<Config> {
    let path = crate::app_paths::support_dir()
        .ok()?
        .join("openai-stt.json");
    let mut config = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Config>(&text).ok())
        .or_else(|| {
            env("OPENAI_API_KEY").map(|api_key| Config {
                api_key: Some(api_key),
                ..Config::default()
            })
        })?;
    if let Some(backend) = env("OPENAI_STT_BACKEND") {
        config.backend = backend;
    }
    if let Some(model) = env("OPENAI_STT_MODEL") {
        config.model = Some(model);
    }
    if let Some(language) = env("OPENAI_STT_LANGUAGE") {
        config.language = Some(language);
    }
    if let Some(auth_path) = env("OPENAI_STT_AUTH") {
        config.auth_path = Some(auth_path);
    }
    Some(config)
}

fn endpoint(backend: &str) -> Result<&'static str> {
    match backend.trim().to_ascii_lowercase().as_str() {
        "api_key" => Ok(API_URL),
        "codex_session" => Ok(CODEX_URL),
        other => Err(eyre!(
            "unknown OpenAI STT backend {other:?} (use api_key or codex_session)"
        )),
    }
}

fn default_model(backend: &str) -> &'static str {
    if backend.trim().eq_ignore_ascii_case("codex_session") {
        "gpt-4o-mini-transcribe"
    } else {
        "gpt-transcribe"
    }
}

fn codex_auth_path(config: &Config) -> Result<std::path::PathBuf> {
    if let Some(path) = config
        .auth_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    {
        return Ok(std::path::PathBuf::from(path));
    }
    let home = env("USERPROFILE")
        .or_else(|| env("HOME"))
        .ok_or_else(|| eyre!("could not find the home folder for the Codex auth file"))?;
    Ok(std::path::PathBuf::from(home)
        .join(".codex")
        .join("auth.json"))
}

#[derive(Deserialize)]
struct CodexAuth {
    tokens: CodexTokens,
}

#[derive(Deserialize)]
struct CodexTokens {
    access_token: String,
}

/// The bearer token for the configured backend, read fresh each clip so a
/// token refreshed by Codex takes effect without restarting Hex.
fn credential(config: &Config) -> Result<String> {
    let backend = config.backend.trim().to_ascii_lowercase();
    if backend == "api_key" {
        let key = config
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(str::to_owned)
            .or_else(|| env("OPENAI_API_KEY"));
        return key.ok_or_else(|| eyre!("OPENAI_API_KEY is not set"));
    }
    if backend != "codex_session" {
        return Err(eyre!(
            "unknown OpenAI STT backend {:?} (use api_key or codex_session)",
            config.backend
        ));
    }
    let path = codex_auth_path(config)?;
    let text = std::fs::read_to_string(&path).wrap_err_with(|| {
        format!(
            "could not read the Codex auth file at {}; sign in to Codex first",
            path.display()
        )
    })?;
    let auth: CodexAuth =
        serde_json::from_str(&text).wrap_err("the Codex auth file has no readable access token")?;
    let token = auth.tokens.access_token.trim().to_owned();
    if token.is_empty() {
        return Err(eyre!("the Codex access token is empty"));
    }
    if jwt_expired(&token) {
        return Err(eyre!(
            "the Codex access token expired; sign in to Codex again to refresh it"
        ));
    }
    Ok(token)
}

/// Whether a JWT-shaped token's `exp` is in the past. A token Hex cannot read
/// is left to the server to judge.
fn jwt_expired(token: &str) -> bool {
    let Some(payload) = token.split('.').nth(1) else {
        return false;
    };
    let Some(bytes) = base64url(payload) else {
        return false;
    };
    let Ok(claims) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    let Some(exp) = claims.get("exp").and_then(|exp| exp.as_u64()) else {
        return false;
    };
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .is_ok_and(|now| now.as_secs() >= exp)
}

fn base64url(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => break,
            _ => return None,
        } as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

/// The OpenAI language for a Hex selection, when there is one. The endpoint
/// takes ISO-639-1 codes; Auto sends nothing and lets the model detect it.
fn language(config: &Config, selection: &str) -> Option<String> {
    let requested = config.language.as_deref().unwrap_or(selection).trim();
    if requested.is_empty() || requested.eq_ignore_ascii_case("auto") {
        return None;
    }
    Some(
        requested
            .split(['-', '_'])
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase(),
    )
}

/// Transcribes a complete 16 kHz mono clip. Returns an error, rather than an
/// empty transcript, when the service cannot answer; the caller then falls
/// back to the local model.
pub(crate) fn transcribe(
    config: &Config,
    samples: &[f32],
    selection_language: &str,
    configured: &[(String, String)],
) -> Result<String> {
    let started = Instant::now();
    let url = endpoint(&config.backend)?;
    let token = credential(config)?;
    let model = config
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or_else(|| default_model(&config.backend));
    let language = language(config, selection_language);
    let learn = !crate::transcription::gradium::learning_disabled();
    let mut learned = if learn {
        crate::transcription::gradium::learning::Learned::load()
    } else {
        crate::transcription::gradium::learning::Learned::default()
    };
    let mut entries = crate::dictation::vocabulary::entries();
    if learn {
        // A correction learned since the Vocabulary pane last imported is
        // still sent, so a fresh mishearing reaches the model right away.
        for correction in &learned.corrections {
            let right = correction.right.trim();
            if !right.is_empty() && !crate::dictation::vocabulary::contains(&entries, right) {
                entries.push(VocabularyEntry {
                    term: right.to_owned(),
                    sounds_like: Vec::new(),
                });
            }
        }
    }
    let instruction = crate::app_settings::AppSettings::read()
        .ok()
        .map(|settings| {
            settings
                .dictation_processing
                .default_mode
                .post_processing
                .prompt
        })
        .unwrap_or_default();
    let context = context_prompt(&instruction, &entries);
    let wav = wav(samples);
    if wav.len() > MAX_AUDIO_BYTES {
        return Err(eyre!(
            "the clip is {} MB; the transcription endpoint takes at most 25 MB",
            wav.len() / (1024 * 1024)
        ));
    }

    let boundary = boundary();
    let body = multipart(
        &boundary,
        model,
        language.as_deref(),
        context.as_deref(),
        &config.backend,
        &wav,
    );
    let agent = crate::net::agent(url, |config| {
        config
            .http_status_as_error(false)
            .timeout_global(Some(DEADLINE))
    });
    let request = ureq::http::Request::builder()
        .method("POST")
        .uri(url)
        .header("Authorization", format!("Bearer {token}"))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header("Accept", "application/json")
        .body(body)
        .map_err(|error| eyre!("could not build the OpenAI STT request: {error}"))?;
    let mut response = agent
        .run(request)
        .map_err(|error| eyre!("could not reach OpenAI: {}", crate::net::reason(&error)))?;
    let status = response.status();
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_vec()
        .wrap_err("could not read OpenAI's answer")?;

    if !status.is_success() {
        return Err(eyre!(
            "OpenAI STT answered HTTP {}: {}",
            status.as_u16(),
            upstream_error(&body)
        ));
    }

    let raw = parse(&body)?;
    let (transcript, corrected, learned_pairs) = if learn {
        learned.apply(&raw, &entries, configured)
    } else {
        (raw, 0, 0)
    };
    tracing::info!(
        ms = started.elapsed().as_millis(),
        backend = %config.backend,
        model = %model,
        prompt_chars = context.as_ref().map_or(0, |prompt| prompt.chars().count()),
        corrected,
        learned = learned_pairs,
        chars = transcript.chars().count(),
        "transcribed a clip with OpenAI STT"
    );
    Ok(transcript)
}

/// The transcription prompt: the person's writing instruction, then the
/// vocabulary and learned spellings that fit. The service reads it as
/// context, so the instruction comes first and the terms come last.
fn context_prompt(instruction: &str, entries: &[VocabularyEntry]) -> Option<String> {
    let instruction = instruction.trim();
    let mut prompt = String::new();
    if !instruction.is_empty() {
        prompt.extend(instruction.chars().take(INSTRUCTION_CHARS));
        let trimmed = prompt.trim_end().len();
        prompt.truncate(trimmed);
    }
    let remaining = PROMPT_CHARS.saturating_sub(prompt.chars().count());
    let prefix = "Names and terms: ";
    let suffix = ".";
    let budget = remaining.saturating_sub(prefix.chars().count() + suffix.chars().count());
    if budget > 0
        && let Some(terms) = crate::dictation::vocabulary::prompt_for(entries, budget)
    {
        if !prompt.is_empty() {
            prompt.push_str("\n\n");
        }
        prompt.push_str(prefix);
        prompt.push_str(&terms);
        prompt.push_str(suffix);
    }
    (!prompt.trim().is_empty()).then_some(prompt)
}

fn boundary() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!("hex-openai-{}-{nanos}", std::process::id())
}

/// The multipart body `server.py` builds: `model`, an optional `language`,
/// the vocabulary `prompt`, `response_format` for the official API, and the
/// WAV file.
fn multipart(
    boundary: &str,
    model: &str,
    language: Option<&str>,
    prompt: Option<&str>,
    backend: &str,
    wav: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(wav.len() + 512);
    field(&mut out, boundary, "model", model);
    if let Some(language) = language {
        field(&mut out, boundary, "language", language);
    }
    if let Some(prompt) = prompt {
        field(&mut out, boundary, "prompt", prompt);
    }
    if !backend.trim().eq_ignore_ascii_case("codex_session") {
        field(&mut out, boundary, "response_format", "json");
    }
    out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    out.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\n",
    );
    out.extend_from_slice(b"Content-Type: audio/wav\r\n\r\n");
    out.extend_from_slice(wav);
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    out
}

fn field(out: &mut Vec<u8>, boundary: &str, name: &str, value: &str) {
    out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    out.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
    );
    out.extend_from_slice(value.as_bytes());
    out.extend_from_slice(b"\r\n");
}

fn parse(body: &[u8]) -> Result<String> {
    let value: serde_json::Value =
        serde_json::from_slice(body).wrap_err("OpenAI STT's answer was not JSON")?;
    Ok(value
        .get("text")
        .and_then(|text| text.as_str())
        .unwrap_or_default()
        .trim()
        .to_owned())
}

/// The upstream error's code and message, with anything credential-shaped
/// replaced, so a failed request never writes a key to the log.
fn upstream_error(body: &[u8]) -> String {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return "the service returned no readable error".into();
    };
    let error = value.get("error");
    let code = error
        .and_then(|error| {
            error
                .get("code")
                .or_else(|| error.get("type"))
                .and_then(|code| code.as_str())
        })
        .or_else(|| value.get("detail").and_then(|detail| detail.as_str()))
        .unwrap_or("unknown_error");
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(|message| message.as_str())
        .unwrap_or_default();
    let message: String = message.chars().take(200).collect();
    let message = sanitize(&message);
    if message.is_empty() {
        code.to_owned()
    } else {
        format!("{code}: {message}")
    }
}

fn sanitize(text: &str) -> String {
    text.split_whitespace()
        .map(|word| {
            let stripped = word.trim_matches(|character: char| character.is_ascii_punctuation());
            if (stripped.starts_with("sk-") && stripped.len() > 11)
                || (stripped.starts_with("eyJ") && stripped.len() > 13)
            {
                "[redacted]"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A 16-bit mono PCM WAV, which both endpoints accept directly.
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
    fn the_backend_picks_its_endpoint_and_model() {
        assert_eq!(endpoint("api_key").unwrap(), API_URL);
        assert_eq!(endpoint("codex_session").unwrap(), CODEX_URL);
        assert!(endpoint("other").is_err());
        assert_eq!(default_model("api_key"), "gpt-transcribe");
        assert_eq!(default_model("codex_session"), "gpt-4o-mini-transcribe");
    }

    #[test]
    fn multipart_carries_the_fields_and_the_wav() {
        let wav = wav(&[0.0, 0.5]);
        let body = multipart(
            "b",
            "gpt-4o-mini-transcribe",
            Some("en"),
            Some("Names and terms: Rune."),
            "codex_session",
            &wav,
        );
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("name=\"model\"\r\n\r\ngpt-4o-mini-transcribe"));
        assert!(text.contains("name=\"language\"\r\n\r\nen"));
        assert!(text.contains("name=\"prompt\"\r\n\r\nNames and terms: Rune."));
        assert!(text.contains("filename=\"audio.wav\""));
        assert!(!text.contains("response_format"));
        assert!(text.ends_with("--b--\r\n"));
        let start = body.len() - wav.len() - 9;
        assert_eq!(&body[start..start + wav.len()], &wav[..]);

        let api = multipart("b", "whisper-1", None, None, "api_key", &wav);
        assert!(String::from_utf8_lossy(&api).contains("name=\"response_format\"\r\n\r\njson"));
    }

    #[test]
    fn the_context_prompt_carries_the_instruction_then_the_terms() {
        let entries = vec![VocabularyEntry {
            term: "Gradium".into(),
            sounds_like: vec![],
        }];
        let prompt = context_prompt("Write in short sentences.", &entries).unwrap();
        assert!(prompt.starts_with("Write in short sentences."));
        assert!(prompt.ends_with("Names and terms: Gradium."));
        assert!(prompt.chars().count() <= PROMPT_CHARS);
        assert_eq!(context_prompt("  ", &[]), None);
    }

    #[test]
    fn a_transcript_is_the_text_field() {
        assert_eq!(parse(br#"{"text":"  hello  "}"#).unwrap(), "hello");
        assert!(parse(b"not json").is_err());
    }

    #[test]
    fn an_upstream_error_keeps_the_code_and_scrubs_keys() {
        let body = br#"{"error":{"code":"invalid_api_key","message":"Incorrect key sk-abcdefghijklmnop"}}"#;
        let error = upstream_error(body);
        assert!(error.starts_with("invalid_api_key: Incorrect key"));
        assert!(!error.contains("sk-abcdefghijklmnop"));
    }

    #[test]
    fn an_expired_jwt_is_recognized() {
        // {"exp":1}
        assert!(jwt_expired("x.eyJleHAiOjF9.y"));
        // {"exp":4102444800} (2100-01-01)
        assert!(!jwt_expired("x.eyJleHAiOjQxMDI0NDQ4MDB9.y"));
        assert!(!jwt_expired("not-a-jwt"));
    }

    #[test]
    fn auto_and_locales_map_to_a_language_or_none() {
        let config = Config::default();
        assert_eq!(language(&config, "en"), Some("en".into()));
        assert_eq!(language(&config, "pt-BR"), Some("pt".into()));
        assert_eq!(language(&config, "auto"), None);
        assert_eq!(
            language(
                &Config {
                    language: Some("fr".into()),
                    ..Config::default()
                },
                "en"
            ),
            Some("fr".into())
        );
    }
}
