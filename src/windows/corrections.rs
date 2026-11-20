//! Learns a dictation correction from the field it was pasted into.
//!
//! After a paste, the same field is read through Windows UI Automation for a
//! short while. When the person selects a word and retypes it, the one-for-one
//! replacement becomes a Gradium correction, so the next dictation gets the
//! word right. The field's text is never stored or logged, password fields are
//! skipped, and reading stops after the watch window or when another dictation
//! starts.

use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationTextPattern, IUIAutomationValuePattern,
    UIA_TextPatternId, UIA_ValuePatternId,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GA_ROOT, GetAncestor};

/// The paste needs to land before the field is read as the baseline.
const SETTLE: Duration = Duration::from_millis(500);
/// How often the field is read while watching.
const POLL: Duration = Duration::from_millis(900);
/// How long a correction after the paste still counts.
const WATCH: Duration = Duration::from_secs(45);
/// A whole-document read is not free; a huge document is not a dictation
/// field worth watching.
const MAX_CHARS: usize = 120_000;

/// The newest watcher; an older one exits at its next poll.
static LATEST: AtomicU64 = AtomicU64::new(0);

/// Starts watching `window` for an edit of `pasted`. Does nothing unless
/// Gradium learning is on.
pub(crate) fn watch(window: isize, pasted: String) {
    if window == 0 || !crate::transcription::gradium::learning_enabled() {
        return;
    }
    let id = LATEST.fetch_add(1, Ordering::AcqRel) + 1;
    if let Err(error) = thread::Builder::new()
        .name("gradium-corrections".into())
        .spawn(move || run(id, window, pasted))
    {
        tracing::warn!(%error, "could not start the Gradium correction watcher");
    }
}

fn run(id: u64, window: isize, pasted: String) {
    thread::sleep(SETTLE);
    let Some(reader) = Reader::open(window) else {
        return;
    };
    let Some(baseline) = reader.text() else {
        return;
    };
    if baseline.chars().count() > MAX_CHARS
        || !crate::transcription::gradium::learning::shares_word(&pasted, &baseline)
    {
        return;
    }
    tracing::debug!(
        hwnd = window,
        "watching the dictated field for a correction"
    );

    let deadline = Instant::now() + WATCH;
    while Instant::now() < deadline {
        thread::sleep(POLL);
        if LATEST.load(Ordering::Acquire) != id {
            return;
        }
        let Some(current) = reader.text() else {
            continue;
        };
        if current == baseline {
            continue;
        }
        if current.chars().count() > MAX_CHARS {
            return;
        }
        let Some(pairs) =
            crate::transcription::gradium::learning::corrections_between(&baseline, &current)
        else {
            // A word half deleted or half retyped is not a correction yet.
            continue;
        };
        let mut learned = 0;
        for (heard, right) in pairs {
            if !crate::transcription::gradium::learning::contains_word(&pasted, &heard) {
                continue;
            }
            if crate::transcription::gradium::learning::remember(&heard, &right) {
                learned += 1;
            }
        }
        if learned > 0 {
            tracing::info!(learned, "learned a Gradium correction from an edit");
        }
        return;
    }
}

/// UI Automation on this watcher's thread, for the field with keyboard focus.
struct Reader {
    automation: IUIAutomation,
    window: isize,
    com: bool,
}

impl Reader {
    fn open(window: isize) -> Option<Self> {
        // SAFETY: COM is initialized on this thread before any UIA call, and
        // released in Drop when this thread initialized it.
        unsafe {
            let com = CoInitializeEx(None, COINIT_MULTITHREADED);
            let created: windows::core::Result<IUIAutomation> =
                CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER);
            match created {
                Ok(automation) => Some(Self {
                    automation,
                    window,
                    com: com.is_ok(),
                }),
                Err(_) => {
                    if com.is_ok() {
                        CoUninitialize();
                    }
                    None
                }
            }
        }
    }

    /// The focused field's text, when it belongs to the window the dictation
    /// was pasted into and is not a password field.
    fn text(&self) -> Option<String> {
        // SAFETY: UIA client calls on this thread, which initialized COM.
        unsafe {
            let element = self.automation.GetFocusedElement().ok()?;
            if element
                .CurrentIsPassword()
                .map(|value| value.as_bool())
                .unwrap_or(false)
            {
                return None;
            }
            if let Ok(hwnd) = element.CurrentNativeWindowHandle() {
                if !hwnd.0.is_null() {
                    let root = GetAncestor(hwnd.0, GA_ROOT);
                    if root as isize != self.window {
                        return None;
                    }
                }
            }
            if let Ok(pattern) =
                element.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
            {
                if let Ok(range) = pattern.DocumentRange() {
                    if let Ok(text) = range.GetText(-1) {
                        return Some(text.to_string());
                    }
                }
            }
            if let Ok(pattern) =
                element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
            {
                if let Ok(value) = pattern.CurrentValue() {
                    return Some(value.to_string());
                }
            }
            None
        }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        if self.com {
            // SAFETY: balances this thread's successful CoInitializeEx.
            unsafe { CoUninitialize() };
        }
    }
}
