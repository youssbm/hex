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

use crate::dictation::text_replacements::{ReplacementSet, TextReplacement};
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
    /// How often a term was recognized in a transcript, keyed lowercased.
    /// This ranks the boost list by use, including terms added by hand.
    #[serde(default, skip_serializing_if = "is_empty_usage")]
    pub usage: HashMap<String, u32>,
    /// Corrections already imported into the Vocabulary pane, kept so the
    /// Learning card still has a history and a most-corrected word.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<Correction>,
}

fn is_empty_usage(usage: &HashMap<String, u32>) -> bool {
    usage.is_empty()
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

    /// Forgets the corrections named by `(heard, right)` pairs after they
    /// have been saved into the vocabulary. Newer corrections with other
    /// spellings stay for the next import.
    pub(crate) fn forget(pairs: &[(String, String)]) {
        let mut learned = Self::load();
        let before = learned.corrections.len();
        let terms_before = learned.terms.len();
        let mut moved = Vec::new();
        learned.corrections.retain(|correction| {
            let keep = !pairs
                .iter()
                .any(|(heard, right)| correction.heard == *heard && correction.right == *right);
            if !keep {
                moved.push(correction.clone());
            }
            keep
        });
        learned.terms.retain(|term| {
            !pairs
                .iter()
                .any(|(_, right)| term.term.trim().eq_ignore_ascii_case(right.trim()))
        });
        for correction in moved {
            learned.remember_history(correction);
        }
        if learned.corrections.len() != before || learned.terms.len() != terms_before {
            learned.save();
        }
    }

    /// [`Self::apply_with`] with learning on, for tests and callers that have
    /// already checked the switch.
    pub(crate) fn apply(
        &mut self,
        raw: &str,
        entries: &[VocabularyEntry],
        configured: &[(String, String)],
    ) -> (String, usize, usize) {
        self.apply_with(raw, entries, configured, true)
    }

    /// Fixes a transcript with the vocabulary, the configured pairs, and the
    /// pairs learned before. `learn_new` adds a fuzzy match to the learning
    /// file so the next clip gets it right. Returns the text, how many words
    /// changed, and how many new pairs this clip taught.
    pub(crate) fn apply_with(
        &mut self,
        raw: &str,
        entries: &[VocabularyEntry],
        configured: &[(String, String)],
        learn_new: bool,
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

        let phrases = build_phrases(configured, &self.corrections);
        let spans = word_spans(raw);
        let words: Vec<String> = spans
            .iter()
            .map(|(start, end)| raw[*start..*end].to_lowercase())
            .collect();

        let mut out = String::with_capacity(raw.len());
        let mut cursor = 0;
        let mut index = 0;
        let mut changed = 0;
        let mut learned = Vec::new();
        let mut used = Vec::new();

        while index < spans.len() {
            let mut matched = None;
            for phrase in &phrases {
                let end = index + phrase.words.len();
                if end > spans.len() {
                    continue;
                }
                if phrase
                    .words
                    .iter()
                    .zip(&words[index..end])
                    .all(|(expected, actual)| expected == actual)
                {
                    matched = Some((phrase, end));
                    break;
                }
            }
            if let Some((phrase, end)) = matched {
                out.push_str(&raw[cursor..spans[index].0]);
                out.push_str(&phrase.right);
                used.push(phrase.right.clone());
                cursor = spans[end - 1].1;
                index = end;
                changed += 1;
                continue;
            }

            let (start, end) = spans[index];
            out.push_str(&raw[cursor..start]);
            let word = &raw[start..end];
            let replacement = if let Some(term) = exact.get(&words[index]) {
                used.push(term.clone());
                (term.as_str() != word).then(|| term.clone())
            } else if learn_new {
                fuzzy(word, entries).inspect(|term| {
                    used.push(term.clone());
                    learned.push((word.to_owned(), term.clone()));
                })
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
            cursor = end;
            index += 1;
        }
        out.push_str(&raw[cursor..]);

        if learn_new {
            for (heard, right) in &learned {
                self.learn(heard, right);
            }
        }
        for term in &used {
            self.note_use(term);
        }
        if (learn_new && !learned.is_empty()) || !used.is_empty() {
            self.save();
        }
        (out, changed, learned.len())
    }

    /// Counts one recognition of `term`, so the boost list can put the terms
    /// a person actually says first. Case does not matter.
    fn note_use(&mut self, term: &str) {
        let term = term.trim().to_lowercase();
        if term.is_empty() {
            return;
        }
        *self.usage.entry(term).or_insert(0) += 1;
    }

    /// Keeps an imported correction in the learning history. The Vocabulary
    /// pane's stats survive the import because of this list.
    fn remember_history(&mut self, correction: Correction) {
        if let Some(existing) = self.history.iter_mut().find(|existing| {
            existing.heard.eq_ignore_ascii_case(&correction.heard)
                && existing.right == correction.right
        }) {
            existing.count = existing.count.saturating_add(correction.count);
            return;
        }
        self.history.push(correction);
        if self.history.len() > MAX_CORRECTIONS {
            self.history.remove(0);
        }
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

struct Phrase {
    /// The heard phrase's words, lowercased.
    words: Vec<String>,
    right: String,
}

/// Configured pairs first, then learned ones, longest phrase first, so a
/// whole spoken email wins over a shorter rule inside it.
fn build_phrases(configured: &[(String, String)], learned: &[Correction]) -> Vec<Phrase> {
    let mut seen = HashSet::new();
    let mut list = Vec::new();
    for (heard, right) in configured {
        add_phrase(&mut list, &mut seen, heard, right);
    }
    for correction in learned {
        add_phrase(&mut list, &mut seen, &correction.heard, &correction.right);
    }
    list.sort_by(|left, right| right.words.len().cmp(&left.words.len()));
    list
}

fn add_phrase(list: &mut Vec<Phrase>, seen: &mut HashSet<String>, heard: &str, right: &str) {
    let right = right.trim();
    let words: Vec<String> = words(heard)
        .iter()
        .map(|word| word.to_lowercase())
        .collect();
    if words.is_empty() || right.is_empty() {
        return;
    }
    if seen.insert(words.join(" ")) {
        list.push(Phrase {
            words,
            right: right.to_owned(),
        });
    }
}

/// Words Gradium is told to expect on the next clip: the vocabulary first,
/// then the configured and learned corrections. Multi-word entries and pure
/// numbers are dropped because Gradium matches keywords token by token.
/// Misheard spellings are deliberately left out: boosting them would teach
/// the model the wrong spelling; they only feed the local corrections.
pub(crate) fn keywords(
    entries: &[VocabularyEntry],
    learned: &Learned,
    configured: &[(String, String)],
) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut words = Vec::new();
    for entry in entries {
        push_keyword(&mut words, &mut seen, &entry.term);
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

/// How often each right spelling has been seen, for ranking the boost list.
/// Learned terms and corrections both count; a term the person never had to
/// correct has no entry and keeps its saved position.
pub(crate) fn usage_counts() -> HashMap<String, u32> {
    let learned = Learned::load();
    let mut counts = learned.usage.clone();
    // Files written before usage existed still have the learning counts.
    for term in &learned.terms {
        counts.entry(term.term.to_lowercase()).or_insert(term.count);
    }
    for correction in learned.corrections.iter().chain(&learned.history) {
        counts
            .entry(correction.right.to_lowercase())
            .or_insert(correction.count);
    }
    counts
}

/// What the Vocabulary pane shows about the learning loop.
#[derive(Clone, Debug, Default)]
pub(crate) struct Stats {
    /// How many hand corrections the learning file remembers.
    pub corrections: u32,
    /// The correction heard most often: heard, right, and its count.
    pub most_corrected: Option<(String, String, u32)>,
}

impl Learned {
    /// This learning file as the Vocabulary pane summarizes it.
    pub(crate) fn stats(&self) -> Stats {
        let corrections = self
            .corrections
            .iter()
            .chain(&self.history)
            .map(|correction| correction.count)
            .sum();
        let most_corrected = self
            .corrections
            .iter()
            .chain(&self.history)
            .max_by_key(|correction| correction.count)
            .map(|correction| {
                (
                    correction.heard.clone(),
                    correction.right.clone(),
                    correction.count,
                )
            });
        Stats {
            corrections,
            most_corrected,
        }
    }
}

/// Adds the learned terms and mishearings to a vocabulary list, so Parakeet's
/// own bias hears with everything Gradium learned. A learned correction's
/// heard spelling becomes a "sounds like" of its right term.
pub(crate) fn augment(entries: &mut Vec<VocabularyEntry>) {
    if !crate::dictation::vocabulary::enabled() {
        return;
    }
    let learned = Learned::load();
    for term in &learned.terms {
        add_term(entries, &term.term, &[]);
    }
    for correction in &learned.corrections {
        add_term(
            entries,
            &correction.right,
            std::slice::from_ref(&correction.heard),
        );
    }
}

fn add_term(entries: &mut Vec<VocabularyEntry>, term: &str, sounds_like: &[String]) {
    let term = term.trim();
    if term.is_empty() || term.chars().count() > MAX_TERM_CHARS {
        return;
    }
    if let Some(entry) = entries
        .iter_mut()
        .find(|entry| entry.term.eq_ignore_ascii_case(term))
    {
        for sound in sounds_like {
            let sound = sound.trim();
            if sound.is_empty()
                || sound.chars().count() > MAX_TERM_CHARS
                || entry.sounds_like.len() >= 16
                || entry
                    .sounds_like
                    .iter()
                    .any(|known| known.eq_ignore_ascii_case(sound))
            {
                continue;
            }
            entry.sounds_like.push(sound.to_owned());
        }
        return;
    }
    if entries.len() >= 500 {
        return;
    }
    entries.push(VocabularyEntry {
        term: term.to_owned(),
        sounds_like: sounds_like
            .iter()
            .map(|sound| sound.trim())
            .filter(|sound| !sound.is_empty() && sound.chars().count() <= MAX_TERM_CHARS)
            .map(str::to_owned)
            .collect(),
    });
}

/// Applies the shared dictionary to a local transcript, so a word learned
/// through either engine fixes the other. Returns the text unchanged when
/// nothing matches.
pub(crate) fn correct_with(text: &str, configured: &[(String, String)]) -> String {
    if text.trim().is_empty() || !crate::dictation::vocabulary::enabled() {
        return text.to_owned();
    }
    let entries = crate::dictation::vocabulary::transcription_entries();
    let mut learned = Learned::load();
    let (text, corrected, learned) = learned.apply_with(
        text,
        &entries,
        configured,
        crate::dictation::vocabulary::learning_enabled(),
    );
    if corrected > 0 || learned > 0 {
        tracing::info!(
            corrected,
            learned,
            "corrected a local transcript with the learned vocabulary"
        );
    }
    text
}

/// Applies the vocabulary and learned corrections to text a formatter has
/// just rewritten. This is the deterministic half of the post-transcription
/// pass: the model handles style, and these whole-word rules put exact
/// spellings back after it, so a formatter cannot undo a learned name.
pub(crate) fn enforce(text: &str) -> String {
    if text.trim().is_empty() || !crate::dictation::vocabulary::enabled() {
        return text.to_owned();
    }
    enforce_with(
        text,
        &crate::dictation::vocabulary::transcription_entries(),
        &Learned::load().corrections,
    )
}

fn enforce_with(text: &str, entries: &[VocabularyEntry], corrections: &[Correction]) -> String {
    let mut rules = Vec::new();
    for entry in entries {
        rules.push(TextReplacement {
            matched_phrase: entry.term.clone(),
            output: entry.term.clone(),
        });
        for spelling in &entry.sounds_like {
            rules.push(TextReplacement {
                matched_phrase: spelling.clone(),
                output: entry.term.clone(),
            });
        }
    }
    for correction in corrections {
        rules.push(TextReplacement {
            matched_phrase: correction.heard.clone(),
            output: correction.right.clone(),
        });
    }
    if rules.is_empty() {
        return text.to_owned();
    }
    ReplacementSet::new(&rules).replace(text)
}

/// A word and the text before it, so an edit that changes a separator (the
/// space before `gmail` becoming `@`) is not mistaken for an unchanged
/// suffix.
#[derive(PartialEq)]
struct Token {
    gap: String,
    word: String,
    start: usize,
    end: usize,
}

fn tokens(text: &str) -> Vec<Token> {
    let spans = word_spans(text);
    let mut tokens = Vec::with_capacity(spans.len());
    let mut previous_end = 0;
    for (index, (start, end)) in spans.iter().enumerate() {
        tokens.push(Token {
            gap: if index == 0 {
                String::new()
            } else {
                text[previous_end..*start].to_owned()
            },
            word: text[*start..*end].to_owned(),
            start: *start,
            end: *end,
        });
        previous_end = *end;
    }
    tokens
}

/// The one contiguous edit from `before` to `after`, up to eight words on
/// either side. A replacement may change the word count, so a spoken email
/// becomes the address typed over it. `None` for insertions, deletions, or
/// wider rewrites, so a watcher keeps waiting instead of learning noise.
pub(crate) fn corrections_between(before: &str, after: &str) -> Option<Vec<(String, String)>> {
    let left = tokens(before);
    let right = tokens(after);
    if left == right {
        return None;
    }
    let mut prefix = 0;
    while prefix < left.len()
        && prefix < right.len()
        && left[prefix].gap == right[prefix].gap
        && left[prefix].word == right[prefix].word
    {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < left.len().saturating_sub(prefix)
        && suffix < right.len().saturating_sub(prefix)
        && left[left.len() - 1 - suffix].gap == right[right.len() - 1 - suffix].gap
        && left[left.len() - 1 - suffix].word == right[right.len() - 1 - suffix].word
    {
        suffix += 1;
    }
    let left_middle = &left[prefix..left.len() - suffix];
    let right_middle = &right[prefix..right.len() - suffix];
    if left_middle.is_empty() || right_middle.is_empty() {
        return None;
    }
    if left_middle.len() > 8 || right_middle.len() > 8 {
        return None;
    }
    let heard = &before[left_middle[0].start..left_middle[left_middle.len() - 1].end];
    let replacement = &after[right_middle[0].start..right_middle[right_middle.len() - 1].end];
    if heard == replacement {
        return None;
    }
    Some(vec![(heard.to_owned(), replacement.to_owned())])
}

/// Whether two texts share a word of four characters or more, so a field is
/// the one the dictation landed in rather than an unrelated control.
pub(crate) fn shares_word(left: &str, right: &str) -> bool {
    words(left)
        .iter()
        .any(|word| word.chars().count() >= 4 && contains_phrase(right, word))
}

/// Whether `text` holds `phrase` as consecutive words, ignoring case and
/// surrounding punctuation.
pub(crate) fn contains_phrase(text: &str, phrase: &str) -> bool {
    let hay: Vec<String> = words(text).iter().map(|word| word.to_lowercase()).collect();
    let needle: Vec<String> = words(phrase)
        .iter()
        .map(|word| word.to_lowercase())
        .collect();
    if needle.is_empty() || needle.len() > hay.len() {
        return false;
    }
    hay.windows(needle.len())
        .any(|window| window == needle.as_slice())
}

fn words(text: &str) -> Vec<String> {
    word_spans(text)
        .into_iter()
        .map(|(start, end)| text[start..end].to_owned())
        .collect()
}

/// Whether a corrected spelling looks like a name, brand, or other term worth
/// learning. This is the precision gate: a formatter may fix "teh" to "the"
/// all day, and that never belongs in a vocabulary. Names, compounds, mixed
/// case, acronyms, and email addresses pass; common words, everyday acronyms,
/// paths, URLs, code, and numbers do not.
pub(crate) fn looks_like_proper_noun(heard: &str, right: &str) -> bool {
    let heard = heard.trim();
    let right = right.trim();
    if heard.is_empty() || right.is_empty() || right.chars().count() > MAX_TERM_CHARS {
        return false;
    }
    if right.contains("://")
        || right.to_lowercase().starts_with("www.")
        || right.contains(&['/', '\\', '{', '}', '(', ')', '[', ']', '<', '>', ';', '='][..])
    {
        return false;
    }
    if right.contains('@') {
        let mut parts = right.split('@');
        let local = parts.next().unwrap_or_default();
        let domain = parts.next().unwrap_or_default();
        return !local.is_empty()
            && domain.contains('.')
            && !domain.starts_with('.')
            && !domain.ends_with('.')
            && parts.next().is_none();
    }
    if !right.chars().any(char::is_alphabetic) {
        return false;
    }
    let words: Vec<&str> = right.split_whitespace().collect();
    if words.is_empty() || words.len() > 4 {
        return false;
    }
    if words.len() == 1 && is_common_word(words[0]) {
        return false;
    }
    if words.iter().all(|word| is_everyday_acronym(word)) {
        return false;
    }
    let known = crate::dictation::vocabulary::entries()
        .iter()
        .any(|entry| entry.term.eq_ignore_ascii_case(right));
    known
        || words.iter().any(|word| {
            let mut chars = word.chars();
            let first_upper = chars.next().is_some_and(char::is_uppercase);
            let rest = chars.collect::<String>();
            (first_upper && rest.chars().any(char::is_lowercase))
                || word
                    .chars()
                    .filter(|character| character.is_uppercase())
                    .count()
                    >= 2
                || word.chars().any(|character| character.is_ascii_digit())
        })
}

/// Function words and the common work nouns people correct by accident. A
/// capitalized "Sprint" at the start of a sentence still fails this, which is
/// the point: a dictionary of ordinary words is noise.
const COMMON_WORDS: &str = "the a an and or but if then than that this these those i you he she it we \
    they me him her us them is are was were be been being am to of in on at for with from by as \
    about not no yes so just also very really can could should would will may might must do does \
    did have has had here there when where what who why how today tomorrow yesterday sprint \
    roadmap deploy meeting project team email message call note draft invoice report update \
    review launch plan task ticket issue bug feature product customer client company user data \
    code app file page site link doc document text word line point thing stuff way time day week \
    month year";

fn is_common_word(word: &str) -> bool {
    let word = word.to_lowercase();
    COMMON_WORDS
        .split_whitespace()
        .any(|common| common == word.as_str())
}

/// Acronyms people type every day and do not need a personal dictionary for.
const EVERYDAY_ACRONYMS: &str = "pdf url http https html css json xml csv api asap btw fyi ok okay";

fn is_everyday_acronym(word: &str) -> bool {
    let word = word.to_lowercase();
    EVERYDAY_ACRONYMS
        .split_whitespace()
        .any(|common| common == word.as_str())
}

/// Records a correction the person made by hand. Returns whether the
/// dictionary changed; a repeat of a pair it already knows is not new.
pub(crate) fn remember(heard: &str, right: &str) -> bool {
    let heard = heard.trim();
    let right = right.trim();
    if heard.is_empty()
        || right.is_empty()
        || heard == right
        || right.chars().count() > MAX_TERM_CHARS
        || !crate::dictation::vocabulary::learning_enabled()
    {
        return false;
    }
    let mut learned = Learned::load();
    if learned
        .corrections
        .iter()
        .any(|correction| correction.heard.eq_ignore_ascii_case(heard) && correction.right == right)
    {
        return false;
    }
    learned.learn(heard, right);
    learned.save();
    true
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
        assert_eq!(words, vec!["Rune"]);
    }

    #[test]
    fn a_selected_word_retyped_is_a_correction() {
        let pairs = corrections_between("Gradiom is the name", "Gradium is the name").unwrap();
        assert_eq!(pairs, vec![("Gradiom".to_owned(), "Gradium".to_owned())]);
    }

    #[test]
    fn a_case_only_edit_counts() {
        let pairs = corrections_between("ask github now", "ask GitHub now").unwrap();
        assert_eq!(pairs, vec![("github".to_owned(), "GitHub".to_owned())]);
    }

    #[test]
    fn punctuation_insertions_and_deletions_are_not_corrections() {
        assert!(corrections_between("hello, world", "hello world").is_none());
        assert!(corrections_between("hello", "hello there").is_none());
        assert!(corrections_between("hello there", "hello").is_none());
    }

    #[test]
    fn a_contiguous_replaced_phrase_is_one_correction() {
        let pairs = corrections_between("a b c d", "a x y d").unwrap();
        assert_eq!(pairs, vec![("b c".to_owned(), "x y".to_owned())]);
        assert!(
            corrections_between("a b c d e f g h i j k", "a v w x y z aa bb cc dd k").is_none()
        );
    }

    #[test]
    fn a_spoken_email_is_learned_as_one_phrase() {
        let pairs =
            corrections_between("Yusuf BM 2008 at gmail.com", "youssefbm2008@gmail.com").unwrap();
        assert_eq!(
            pairs,
            vec![(
                "Yusuf BM 2008 at gmail".to_owned(),
                "youssefbm2008@gmail".to_owned()
            )]
        );
    }

    #[test]
    fn a_full_configured_email_phrase_applies_with_its_punctuation() {
        let mut learned = Learned::default();
        let (text, changed, _) = learned.apply(
            "my Yusuf BM 2008 at gmail.com, thanks",
            &[],
            &[(
                "Yusuf BM 2008 at gmail.com".to_owned(),
                "youssefbm2008@gmail.com".to_owned(),
            )],
        );
        assert_eq!(text, "my youssefbm2008@gmail.com, thanks");
        assert_eq!(changed, 1);
    }

    #[test]
    fn a_learned_partial_email_keeps_its_shared_suffix() {
        let mut learned = Learned::default();
        learned.corrections.push(Correction {
            heard: "Yusuf BM 2008 at gmail".into(),
            right: "youssefbm2008@gmail".into(),
            count: 1,
        });
        let (text, changed, _) = learned.apply("Yusuf BM 2008 at gmail.com", &[], &[]);
        assert_eq!(text, "youssefbm2008@gmail.com");
        assert_eq!(changed, 1);
    }

    #[test]
    fn a_longer_phrase_wins_over_a_shorter_one() {
        let mut learned = Learned::default();
        let (text, _, _) = learned.apply(
            "my Yusuf BM 2008 at gmail.com",
            &[],
            &[
                ("gmail.com".to_owned(), "wrong".to_owned()),
                (
                    "Yusuf BM 2008 at gmail.com".to_owned(),
                    "youssefbm2008@gmail.com".to_owned(),
                ),
            ],
        );
        assert_eq!(text, "my youssefbm2008@gmail.com");
    }

    #[test]
    fn a_phrase_is_found_in_a_longer_macro() {
        assert!(contains_phrase(
            "Yusuf BM 2008 at gmail.com",
            "Yusuf BM 2008 at gmail"
        ));
        assert!(!contains_phrase("Yusuf BM 2008 at gmail.com", "BM Yusuf"));
    }

    #[test]
    fn shared_words_ignore_short_ones() {
        assert!(shares_word("Gradium is here", "gradium"));
        assert!(!shares_word("the cat", "a cat"));
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

    #[test]
    fn the_local_gate_keeps_names_and_rejects_noise() {
        assert!(looks_like_proper_noun("Gradiom", "Gradium"));
        assert!(looks_like_proper_noun("github", "GitHub"));
        assert!(looks_like_proper_noun("open code", "OpenCode"));
        assert!(looks_like_proper_noun(
            "Yusuf BM 2008 at gmail",
            "youssefbm2008@gmail.com"
        ));
        assert!(!looks_like_proper_noun("teh", "the"));
        assert!(!looks_like_proper_noun("sprint", "Sprint"));
        assert!(!looks_like_proper_noun("a pie", "API"));
        assert!(!looks_like_proper_noun("see", "https://example.com"));
        assert!(!looks_like_proper_noun("file", "src/main.rs"));
        assert!(!looks_like_proper_noun("two", "2"));
    }

    #[test]
    fn learning_off_keeps_known_pairs_but_stops_fuzzy_learning() {
        let entries = vec![entry("Gradium", &[])];
        let mut learned = Learned::default();
        learned.corrections.push(Correction {
            heard: "gradum".into(),
            right: "Gradium".into(),
            count: 1,
        });
        let (text, changed, learned_pairs) =
            learned.apply_with("gradum works", &entries, &[], false);
        assert_eq!(text, "Gradium works");
        assert_eq!(changed, 1);
        assert_eq!(learned_pairs, 0);
        assert_eq!(learned.corrections.len(), 1);

        let (text, changed, learned_pairs) =
            learned.apply_with("gradium works", &entries, &[], false);
        assert_eq!(text, "Gradium works");
        assert_eq!(changed, 1);
        assert_eq!(learned_pairs, 0);
    }

    #[test]
    fn enforcement_puts_vocabulary_back_after_a_formatter() {
        let entries = vec![entry("GitHub", &["git hub"])];
        let corrections = vec![Correction {
            heard: "Gradiom".into(),
            right: "Gradium".into(),
            count: 1,
        }];
        assert_eq!(
            enforce_with("push to git hub and Gradiom", &entries, &corrections),
            "push to GitHub and Gradium"
        );
    }

    #[test]
    fn stats_summarize_the_most_corrected_word() {
        let learned = Learned {
            corrections: vec![
                Correction {
                    heard: "gradiom".into(),
                    right: "Gradium".into(),
                    count: 3,
                },
                Correction {
                    heard: "opencode".into(),
                    right: "OpenCode".into(),
                    count: 1,
                },
            ],
            terms: Vec::new(),
            usage: Default::default(),
            history: Vec::new(),
        };
        let stats = learned.stats();
        assert_eq!(stats.corrections, 4);
        assert_eq!(
            stats.most_corrected,
            Some(("gradiom".into(), "Gradium".into(), 3))
        );
    }
}
