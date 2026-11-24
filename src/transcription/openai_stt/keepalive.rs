//! Renews the Codex access token before it expires, so OpenAI STT does not
//! fall back to Parakeet after the ten-day token runs out. The refresh uses
//! the same serialized path as the Sign in sheet, and a failed attempt backs
//! off instead of spending the rotating token again and again.

use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{SignIn, active, codex_auth_path, jwt_expiry, renew_before};

/// Renew this long before the access token expires.
const MARGIN: Duration = Duration::from_secs(24 * 60 * 60);
/// How often to look while the token is healthy.
const CHECK: Duration = Duration::from_secs(30 * 60);
/// How long to wait after a failed refresh before trying again.
const RETRY: Duration = Duration::from_secs(6 * 60 * 60);
/// A first check after the listener has settled.
const FIRST: Duration = Duration::from_secs(15);

static STARTED: OnceLock<()> = OnceLock::new();

/// Starts the renewal thread once. A test build leaves it off so unit tests
/// never touch a real Codex session.
pub(crate) fn start() {
    if cfg!(test) {
        return;
    }
    STARTED.get_or_init(|| {
        if let Err(error) = thread::Builder::new()
            .name("hex-codex-token".into())
            .spawn(run)
        {
            tracing::warn!(%error, "could not start the Codex token renewal thread");
        }
    });
}

fn run() {
    tracing::info!("Codex token renewal started");
    if !sleep_until_shutdown(FIRST) {
        return;
    }
    loop {
        if crate::SHUTDOWN.load(Ordering::Relaxed) {
            return;
        }
        let outcome = check();
        let wait = match outcome {
            Outcome::Failed => RETRY,
            Outcome::Fresh | Outcome::Refreshed | Outcome::Idle => CHECK,
        };
        if !sleep_until_shutdown(wait) {
            return;
        }
    }
}

enum Outcome {
    Fresh,
    Refreshed,
    Failed,
    Idle,
}

fn check() -> Outcome {
    let Some(config) = active() else {
        return Outcome::Idle;
    };
    if !config.backend.trim().eq_ignore_ascii_case("codex_session") {
        return Outcome::Idle;
    }
    let Ok(path) = codex_auth_path(&config) else {
        return Outcome::Idle;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Outcome::Idle;
    };
    let Ok(root) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Outcome::Failed;
    };
    let Some(access) = root
        .pointer("/tokens/access_token")
        .and_then(|token| token.as_str())
    else {
        return Outcome::Idle;
    };
    let Some(exp) = jwt_expiry(access) else {
        return Outcome::Idle;
    };
    if !needs_renewal(exp, now_secs(), MARGIN) {
        return Outcome::Fresh;
    }
    match renew_before(MARGIN) {
        Ok(SignIn::Refreshed { expires_in }) => {
            tracing::info!(
                hours = expires_in.map_or(0, |left| left.as_secs() / 3_600),
                "refreshed the Codex token before it expired"
            );
            Outcome::Refreshed
        }
        Ok(_) => Outcome::Fresh,
        Err(error) => {
            tracing::warn!(
                %error,
                "the Codex token could not be refreshed automatically; sign in from the speech model picker"
            );
            Outcome::Failed
        }
    }
}

fn needs_renewal(exp: u64, now: u64, margin: Duration) -> bool {
    exp.saturating_sub(now) <= margin.as_secs()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|now| now.as_secs())
        .unwrap_or(0)
}

/// Sleeps in slices, so a quit does not wait for a thirty-minute check.
fn sleep_until_shutdown(duration: Duration) -> bool {
    let step = Duration::from_secs(30);
    let mut left = duration;
    while !left.is_zero() {
        if crate::SHUTDOWN.load(Ordering::Relaxed) {
            return false;
        }
        let slice = left.min(step);
        thread::sleep(slice);
        left -= slice;
    }
    !crate::SHUTDOWN.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400;

    #[test]
    fn renewal_starts_a_day_before_expiry() {
        let now = 1_000_000;
        assert!(!needs_renewal(now + 2 * DAY, now, MARGIN));
        assert!(needs_renewal(now + 23 * 3_600, now, MARGIN));
        assert!(needs_renewal(now, now, MARGIN));
        assert!(needs_renewal(now - 60, now, MARGIN));
    }
}
