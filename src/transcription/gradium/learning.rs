//! The dictionary Gradium dictation learns from.
//!
//! Three sources feed it, in priority order: the person's vocabulary and its
//! "sounds like" spellings, explicit pairs in `gradium.json`, and mishearings
//! this module corrected on earlier clips. Corrections apply to every
//! transcript, and every known term rides along as a Gradium keyword so the
//! model needs the correction less often.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::dictation::vocabulary::VocabularyEntry;

/// Most mishearings kept; oldest go first.
const MAX_CORRECTIONS: usize = 500;
/// Most learned terms kept; oldest go first.
const MAX_TERMS: usize = 200;
/// Gradium's keyword cap in practice: past this the boost spreads thin.
const MAX_KEYWORDS: usize = 64;
/// Longest keyword Gradium is sent.
const MAX_TERM_CHARS: usize = 64;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct Learned {
    #[serde(default)]
    pub corrections: Vec<Correction>,
    #[serde(default)]
    pub terms: Vec<LearnedTerm>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Correction {
    pub heard: String,
    pub right: String,
    #[serde(default)]
    pub count: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct LearnedTerm {
    pub term: String,
    #[serde(default)]
    pub count: u32,
}

fn path() -> Option<PathBuf> {
    crate::app_paths::support_dir()
        .ok()
        .map(|dir| dir.join("gradium-learning.json"))
}

impl Learned {
    /// The saved pairs, or an empty dictionary before the first correction.
    pub(crate) fn load() -> Self {
        let Some(path) = path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match serde_json::from_str(&text) {
            Ok(learned) => learned,
            Err(error) => {
                tracing::warn!(error = %error, "could not read the Gradium learning file");
                Self::default()
            }
        }
    }

    fn save(&self) {
        let Some(path) = path() else {
            return;
        };
        let Ok(text) = serde_json::to_string_pretty(self) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let temporary = path.with_extension("json.tmp");
        if let Err(error) =
            std::fs::write(&temporary, text).and_then(|()| std::fs::rename(&temporary, &path))
        {
            tracing::warn!(error = %error, "could not save the Gradium learning file");
        }
    }

    /// Fixes a transcript with the vocabulary, the configured pairs, and the
    /// pairs learned before. Returns the text, how many words changed, and
    /// how many new pairs this clip taught.
    pub(crate) fn apply(
        &mut self,
        raw: &str,
        entries: &[VocabularyEntry],
        configured: &[(String, String)],
    ) -> (String, usize, usize) {
        let mut exact: HashMap<String, String> = HashMap::new();
        for entry in entries {
            let term = entry.term.trim();
            if term.is_empty() {
                continue;
            }
            exact
                .entry(term.to_lowercase())
                .or_insert_with(|| term.to_owned());
            for sound in &entry.sounds_like {
                let sound = sound.trim();
                if !sound.is_empty() {
                    exact
                        .entry(sound.to_lowercase())
                        .or_insert_with(|| term.to_owned());
                }
            }
        }

        // Configured pairs win over learned ones: the file is the last word.
        let mut corrections: HashMap<String, String> = HashMap::new();
        for (heard, right) in configured {
            let heard = heard.trim().to_lowercase();
            if !heard.is_empty() && !right.trim().is_empty() {
                corrections.insert(heard, right.trim().to_owned());
            }
        }
        for correction in &self.corrections {
            corrections
                .entry(correction.heard.to_lowercase())
                .or_insert_with(|| correction.right.clone());
        }

        let mut out = String::with_capacity(raw.len());
        let mut last = 0;
        let mut changed = 0;
        let mut learned = Vec::new();

        for (start, end) in word_spans(raw) {
            out.push_str(&raw[last..start]);
            let word = &raw[start..end];
            let lower = word.to_lowercase();
            let replacement = if let Some(right) = corrections.get(&lower) {
                Some(right.clone())
            } else if let Some(term) = exact.get(&lower) {
                (term.as_str() != word).then(|| term.clone())
            } else if let Some(term) = fuzzy(word, entries) {
                learned.push((word.to_owned(), term.clone()));
                Some(term)
            } else {
                None
            };
            match replacement {
                Some(text) => {
                    out.push_str(&text);
                    changed += 1;
                }
                None => out.push_str(word),
            }
            last = end;
        }
        out.push_str(&raw[last..]);

        for (heard, right) in &learned {
            self.learn(heard, right);
        }
        if !learned.is_empty() {
            self.save();
        }
        (out, changed, learned.len())
    }

    fn learn(&mut self, heard: &str, right: &str) {
        let heard_key = heard.to_lowercase();
        if let Some(existing) = self
            .corrections
            .iter_mut()
            .find(|correction| correction.heard.to_lowercase() == heard_key)
        {
            existing.count = existing.count.saturating_add(1);
            existing.right = right.to_owned();
        } else {
            self.corrections.push(Correction {
                heard: heard.to_owned(),
                right: right.to_owned(),
                count: 1,
            });
            if self.corrections.len() > MAX_CORRECTIONS {
                self.corrections.remove(0);
            }
        }

        let term_key = right.to_lowercase();
        if let Some(existing) = self
            .terms
            .iter_mut()
            .find(|term| term.term.to_lowercase() == term_key)
        {
            existing.count = existing.count.saturating_add(1);
        } else {
            self.terms.push(LearnedTerm {
                term: right.to_owned(),
                count: 1,
            });
            if self.terms.len() > MAX_TERMS {
                self.terms.remove(0);
            }
        }
    }
}

/// Words Gradium is told to expect on the next clip: the vocabulary first,
/// then the configured and learned corrections. Multi-word entries and pure
/// numbers are dropped because Gradium matches keywords token by token.
pub(crate) fn keywords(
    entries: &[VocabularyEntry],
    learned: &Learned,
    configured: &[(String, String)],
) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut words = Vec::new();
    for entry in entries {
        push_keyword(&mut words, &mut seen, &entry.term);
        for sound in &entry.sounds_like {
            push_keyword(&mut words, &mut seen, sound);
        }
    }
    for term in &learned.terms {
        push_keyword(&mut words, &mut seen, &term.term);
    }
    for (_, right) in configured {
        push_keyword(&mut words, &mut seen, right);
    }
    for correction in &learned.corrections {
        push_keyword(&mut words, &mut seen, &correction.right);
    }
    words
}

fn push_keyword(words: &mut Vec<String>, seen: &mut HashSet<String>, term: &str) {
    let term = term.trim();
    if term.is_empty()
        || term.chars().count() > MAX_TERM_CHARS
        || term.chars().any(char::is_whitespace)
        || !term.chars().any(char::is_alphabetic)
    {
        return;
    }
    if seen.insert(term.to_lowercase()) && words.len() < MAX_KEYWORDS {
        words.push(term.to_owned());
    }
}

/// A one-edit mishearing of a term long enough that a common word is
/// unlikely to collide. Same first letter, so "rest" never becomes "Rust".
fn fuzzy(word: &str, entries: &[VocabularyEntry]) -> Option<String> {
    let lower = word.to_lowercase();
    let chars = lower.chars().count();
    if chars < 4 {
        return None;
    }
    let first = lower.chars().next()?;
    let mut best: Option<(usize, &str)> = None;
    for entry in entries {
        let term = entry.term.trim();
        if term.is_empty() || term.chars().any(char::is_whitespace) {
            continue;
        }
        let term_lower = term.to_lowercase();
        let term_chars = term_lower.chars().count();
        // Short names are what "sounds like" is for; fuzzy only takes long
        // terms, where a common word is far less likely to sit one edit away.
        if term_chars < 6 || term_lower == lower {
            continue;
        }
        if term_lower.chars().next() != Some(first) {
            continue;
        }
        let limit = if term_chars >= 10 { 2 } else { 1 };
        if chars.abs_diff(term_chars) > limit {
            continue;
        }
        let Some(distance) = distance_within(&lower, &term_lower, limit) else {
            continue;
        };
        if distance == 0 {
            continue;
        }
        if best.is_none_or(|(current, _)| distance < current) {
            best = Some((distance, term));
        }
    }
    best.map(|(_, term)| term.to_owned())
}

/// Levenshtein distance, abandoned as soon as a row passes `limit`.
fn distance_within(left: &str, right: &str, limit: usize) -> Option<usize> {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    if left.len().abs_diff(right.len()) > limit {
        return None;
    }
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0; right.len() + 1];
    for (row, left_char) in left.iter().enumerate() {
        current[0] = row + 1;
        let mut row_min = current[0];
        for (column, right_char) in right.iter().enumerate() {
            let substitution = previous[column] + usize::from(left_char != right_char);
            current[column + 1] = substitution
                .min(previous[column + 1] + 1)
                .min(current[column] + 1);
            row_min = row_min.min(current[column + 1]);
        }
        if row_min > limit {
            return None;
        }
        std::mem::swap(&mut previous, &mut current);
    }
    (previous[right.len()] <= limit).then_some(previous[right.len()])
}

/// Byte spans of words in `text`: letters and digits, with inner apostrophes
/// and hyphens, trailing punctuation excluded.
fn word_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    for (index, character) in text.char_indices() {
        let alphanumeric = character.is_alphanumeric();
        let inner = alphanumeric || character == '\'' || character == '-';
        match start {
            None if alphanumeric => start = Some(index),
            Some(begin) if !inner => {
                spans.push((begin, trim_end(text, begin, index)));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(begin) = start {
        spans.push((begin, trim_end(text, begin, text.len())));
    }
    spans
}

fn trim_end(text: &str, begin: usize, mut end: usize) -> usize {
    while end > begin {
        let last = text[begin..end].chars().next_back().expect("nonempty span");
        if last == '\'' || last == '-' {
            end -= last.len_utf8();
        } else {
            break;
        }
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(term: &str, sounds_like: &[&str]) -> VocabularyEntry {
        VocabularyEntry {
            term: term.into(),
            sounds_like: sounds_like.iter().map(|sound| (*sound).into()).collect(),
        }
    }

    #[test]
    fn vocabulary_restores_case_and_sounds_like() {
        let entries = vec![entry("GitHub", &["git hub"]), entry("Phonon", &["fonon"])];
        let mut learned = Learned::default();
        let (text, changed, _) = learned.apply("push it to github, ask fonon", &entries, &[]);
        assert_eq!(text, "push it to GitHub, ask Phonon");
        assert_eq!(changed, 2);
    }

    #[test]
    fn configured_pairs_beat_learned_ones() {
        let mut learned = Learned::default();
        learned.corrections.push(Correction {
            heard: "grafana".into(),
            right: "Grafana Cloud".into(),
            count: 2,
        });
        let (text, _, _) = learned.apply(
            "grafana dashboards",
            &[],
            &[("grafana".into(), "Grafana".into())],
        );
        assert_eq!(text, "Grafana dashboards");
    }

    #[test]
    fn a_misheard_long_term_is_learned_once_and_fixed_again() {
        let entries = vec![entry("Gradium", &[])];
        let mut learned = Learned::default();
        let (text, changed, learned_pairs) = learned.apply("gradum works", &entries, &[]);
        assert_eq!(text, "Gradium works");
        assert_eq!(changed, 1);
        assert_eq!(learned_pairs, 1);
        assert_eq!(learned.corrections[0].heard, "gradum");
        assert_eq!(learned.corrections[0].right, "Gradium");

        let (text, changed, learned_pairs) = learned.apply("gradum again", &entries, &[]);
        assert_eq!(text, "Gradium again");
        assert_eq!(changed, 1);
        assert_eq!(learned_pairs, 0);
    }

    #[test]
    fn common_words_near_short_terms_are_left_alone() {
        let entries = vec![entry("Rust", &[])];
        let mut learned = Learned::default();
        let (text, changed, _) = learned.apply("take a rest", &entries, &[]);
        assert_eq!(text, "take a rest");
        assert_eq!(changed, 0);
    }

    #[test]
    fn keywords_skip_phrases_and_numbers_and_stop_at_the_cap() {
        let entries: Vec<_> = (0..70)
            .map(|index| entry(&format!("Tool{index}"), &[]))
            .collect();
        let words = keywords(&entries, &Learned::default(), &[]);
        assert_eq!(words.len(), MAX_KEYWORDS);
        assert_eq!(words[0], "Tool0");

        let entries = vec![
            entry("Open Code", &[]),
            entry("42", &[]),
            entry("Rune", &["rune's"]),
        ];
        let words = keywords(&entries, &Learned::default(), &[]);
        assert_eq!(words, vec!["Rune", "rune's"]);
    }

    #[test]
    fn word_spans_keep_inner_punctuation() {
        assert_eq!(
            word_spans("well-known, don't stop."),
            vec![(0, 10), (12, 17), (18, 22)]
        );
    }

    #[test]
    fn distance_stops_at_the_limit() {
        assert_eq!(distance_within("gradium", "gradum", 1), Some(1));
        assert_eq!(distance_within("gradium", "gradium", 1), Some(0));
        assert_eq!(distance_within("gradium", "grad", 1), None);
    }
}
