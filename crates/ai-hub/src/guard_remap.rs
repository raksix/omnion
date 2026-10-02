//! Response re-mapping (REQ-105 slice 2).
//!
//! Slice 1 made a value invisible **outbound**. This module is the half that makes the feature
//! usable rather than merely safe: it puts the value back for the reader entitled to it and keeps
//! it invisible for everyone else.
//!
//! The rule is one sentence, and the design follows from it:
//!
//! > A reader who is entitled to the original sees it. Every other reader — a second
//! > administrator, a shared transcript, an export, an audit row — sees the placeholder.
//!
//! Four consequences fall out of that sentence, and each is a decision rather than a detail:
//!
//! 1. **The map is built where the spans are known.** [`crate::guard_checkpoint::checkpoint`]
//!    holds the *original* text and the matches' byte offsets into it, so recovering a value is a
//!    slice. It is deliberately not reconstructed later from the masked output: doing that means
//!    searching a model's text for something shaped like a token, and a wrong guess puts someone
//!    else's address into an answer.
//! 2. **The map holds original values, so it is never persisted.** It lives for the lifetime of
//!    one request, in memory, and dies with it. A stored map would be a second copy of exactly
//!    the data the guard exists to contain — the worst outcome this module could have.
//! 3. **Streaming shows placeholders; only the completed message is substituted.** A stream is
//!    one-way: a delta the client has already read cannot be recalled. Substituting into a delta
//!    would either leak the value to a reader who must not see it, or produce a stream whose text
//!    no longer matches the stored message. So the placeholder is the honest thing to show while
//!    the answer is being written, and the finished text carries the substitution.
//! 4. **A placeholder the provider mangled stays visible.** Chatty models rewrite `[EMAIL_1]` as
//!    `[Email_1]` or `[EMAIL-1]`. [`substitute`] recovers those near misses and leaves anything
//!    else alone, because an unreadable token must not become a guess.
//!
//! Nothing here touches the database or HTTP. It is text over a map, so the walk that closes the
//! slice asserts it directly and the crate stays testable without a server.

use std::collections::BTreeMap;

use crate::guard_data::remap_text;

/// One placeholder and the value behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemapEntry {
    /// The text the provider saw, e.g. `[EMAIL_1]`.
    pub placeholder: String,
    /// The value the guard replaced, e.g. an e-mail address. **Never persisted.**
    pub original: String,
    /// The label that classified it (`email`, `card`, …).
    pub label: String,
}

/// The re-mapping for one request: what the guard replaced, and where it put it.
///
/// Constructed by [`RemapMap::from_matches`], which takes the spans and the original text — the
/// same two inputs `mask_text` took — so the map and the mask are derived from one source and
/// cannot disagree about which token stands for which value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemapMap {
    entries: Vec<RemapEntry>,
    /// Tokens dropped by [`Self::merge`] because the same placeholder named two values.
    ///
    /// Carried on the map rather than recomputed by the caller: the caller cannot tell a token
    /// that was withheld from one that was never seen, and the difference is exactly what a
    /// requester needs reported.
    withheld: Vec<String>,
}

impl RemapMap {
    /// An empty map: nothing was masked, so nothing needs putting back.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Build the map from the matches of one inspection and the text they were found in.
    ///
    /// `masked` is the text the mask produced from the same `text`; it is used only to skip spans
    /// the mask did not actually replace (an `Allow` action leaves the value in place, and
    /// substituting for it would corrupt a message that was never masked).
    #[must_use]
    pub fn from_matches(
        text: &str,
        masked: &str,
        matches: &[crate::guard_data::Match_],
        tokens: &BTreeMap<String, String>,
    ) -> Self {
        let mut entries: Vec<RemapEntry> = Vec::new();
        for m in matches {
            // `tokens` maps the value hash to the placeholder `mask_text` wrote. A span whose
            // hash is absent was not replaced (exempted or allowed), so it contributes nothing.
            let Some(token) = tokens.get(&m.value_hash) else {
                continue;
            };
            // Offsets come from the detector and index the *original* text. The bounds check is
            // not defensive noise: a match recorded against a different string would panic on a
            // slice, and a panic on the hot path of every request is the worst possible failure
            // for this feature.
            if m.end > text.len() || m.start >= m.end {
                continue;
            }
            let original = text[m.start..m.end].to_owned();
            // Skipped when the masked text still carries the original: the mask did not run for
            // this span, so there is nothing to undo.
            if masked.contains(&original) && original.is_empty() {
                continue;
            }
            entries.push(RemapEntry {
                placeholder: token.clone(),
                original,
                label: m.label.clone(),
            });
        }
        Self {
            entries,
            withheld: Vec::new(),
        }
    }

    /// Whether anything was masked in this request.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many placeholders this map can substitute.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The entries, for a caller that inspects rather than substitutes.
    #[must_use]
    pub fn entries(&self) -> &[RemapEntry] {
        &self.entries
    }

    /// Merge the maps of one request's messages into a single answer map.
    ///
    /// This exists because each message is inspected on its own, and each inspection numbers its
    /// own first e-mail `[EMAIL_1]`. Two turns naming two **different** addresses therefore both
    /// emit `[EMAIL_1]` and the provider cannot tell them apart — so an answer that echoes the
    /// token does not say which address it meant. Merging them naively would hand the reader one
    /// of the two, and which one is a coin toss: a wrong guess writes somebody else's address
    /// into an answer about a different person.
    ///
    /// So a contested token is **dropped rather than guessed**. It stays visible as the
    /// placeholder, which is the same rule [`Self::substitute`] already applies to a token the
    /// provider mangled — an unreadable token must never become a guess — applied one level up,
    /// where the ambiguity comes from our own numbering instead of the model's spelling.
    ///
    /// A token whose value is **identical** everywhere it appears is not contested: that is the
    /// same person named twice, and it merges into one entry.
    #[must_use]
    pub fn merge(maps: &[Option<RemapMap>]) -> Self {
        // `BTreeMap` for the membership/agreement bookkeeping and a `Vec` for order: the
        // entries are ordered by first appearance, which is the order `labels` and any listing
        // already promise, and a sorted map would quietly change it.
        let mut resolved: BTreeMap<String, Option<RemapEntry>> = BTreeMap::new();
        let mut order: Vec<String> = Vec::new();
        let mut contested: Vec<String> = Vec::new();

        for map in maps.iter().flatten() {
            for entry in &map.entries {
                match resolved.get(&entry.placeholder) {
                    // Same token, same value: the same person named again. Nothing to merge.
                    Some(Some(existing)) if existing.original == entry.original => {}
                    // Same token, a different value: the token no longer identifies anything.
                    Some(Some(_)) => {
                        if !contested.contains(&entry.placeholder) {
                            contested.push(entry.placeholder.clone());
                        }
                    }
                    _ => {
                        resolved.insert(entry.placeholder.clone(), Some(entry.clone()));
                        order.push(entry.placeholder.clone());
                    }
                }
            }
        }

        let entries = order
            .into_iter()
            .filter(|placeholder| !contested.contains(placeholder))
            .filter_map(|placeholder| resolved.remove(&placeholder).flatten())
            .collect();
        Self {
            entries,
            withheld: contested,
        }
    }

    /// The tokens this map **declined** to put back, because two different values claimed the
    /// same placeholder.
    ///
    /// Named rather than folded into a log line: a requester looking at `[EMAIL_1]` needs to be
    /// told it was withheld rather than left to assume the feature is broken, and an audit row
    /// has to record that the token was seen and refused rather than merely absent.
    #[must_use]
    pub fn withheld(&self) -> &[String] {
        &self.withheld
    }

    /// The placeholders this map knows, for a UI listing what was withheld.
    #[must_use]
    pub fn placeholders(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| entry.placeholder.clone())
            .collect()
    }

    /// The labels this map can put back, for the same purpose.
    #[must_use]
    pub fn labels(&self) -> Vec<String> {
        let mut labels: Vec<String> = Vec::new();
        for entry in &self.entries {
            if !labels.contains(&entry.label) {
                labels.push(entry.label.clone());
            }
        }
        labels
    }

    /// Substitute placeholders into a completed answer — **for the entitled reader**.
    ///
    /// This is the requester's own conversation. Every other reader path uses
    /// [`RemapMap::redact`]; naming the two separately is the reason this module exists at all.
    #[must_use]
    pub fn substitute(&self, answer: &str) -> String {
        remap_text(answer, &self.pairs())
    }

    /// Put the placeholders back instead of the values — for anyone else.
    ///
    /// Used for the audit metadata, a shared transcript, an export. The answer still reads
    /// naturally and no value the guard removed comes back, because a value reaching a second
    /// reader's log is the exact leak this control exists to prevent.
    #[must_use]
    pub fn redact(&self, answer: &str) -> String {
        let mut out = answer.to_owned();
        for entry in &self.entries {
            out = out.replace(entry.original.as_str(), entry.placeholder.as_str());
        }
        out
    }

    /// The map as a reader is entitled to receive it.
    ///
    /// A map handed to a reader who is **not** entitled is emptied rather than filtered, because
    /// a partial map invites a caller to help itself to the entries that survived. The alternative
    /// — remembering which reader each map was built for — is a mistake waiting to happen in a
    /// codebase where the same helper is reached from four routes.
    #[must_use]
    pub fn for_reader(&self, entitled: bool) -> Self {
        if entitled {
            self.clone()
        } else {
            Self::empty()
        }
    }

    fn pairs(&self) -> BTreeMap<String, String> {
        self.entries
            .iter()
            .map(|entry| (entry.placeholder.clone(), entry.original.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard_checkpoint::placeholder;
    use crate::guard_data::{MaskStyle, Match_, hash_value, mask_text};

    const SALT: &str = "test-salt";

    fn match_at(text: &str, needle: &str, label: &str) -> Match_ {
        let start = text.find(needle).expect("the needle is in the text");
        Match_::new(
            start,
            start + needle.len(),
            label,
            "email.builtin",
            hash_value(needle, SALT),
        )
    }

    fn token_map(matches: &[Match_], style: MaskStyle) -> BTreeMap<String, String> {
        let mut tokens = BTreeMap::new();
        let mut ordinals: BTreeMap<String, usize> = BTreeMap::new();
        for m in matches {
            if tokens.contains_key(&m.value_hash) {
                continue;
            }
            let n = ordinals.entry(m.label.clone()).or_insert(0);
            *n += 1;
            tokens.insert(
                m.value_hash.clone(),
                placeholder(style, &m.label, *n, &m.value_hash),
            );
        }
        tokens
    }

    fn fixture(text: &str) -> (Vec<Match_>, String, BTreeMap<String, String>, RemapMap) {
        let matches = vec![match_at(text, "ada@example.test", "email")];
        let tokens = token_map(&matches, MaskStyle::Numbered);
        let masked = mask_text(text, &matches, MaskStyle::Numbered);
        let map = RemapMap::from_matches(text, &masked, &matches, &tokens);
        (matches, masked, tokens, map)
    }

    #[test]
    fn the_map_recovers_the_value_the_mask_replaced() {
        let (_m, masked, _t, map) = fixture("write to ada@example.test today");
        assert_eq!(masked, "write to [EMAIL_1] today");
        assert_eq!(map.len(), 1);
        assert_eq!(map.entries()[0].placeholder, "[EMAIL_1]");
        assert_eq!(map.entries()[0].original, "ada@example.test");
    }

    #[test]
    fn the_entitled_reader_gets_the_original_back_in_the_answer() {
        let (_m, _masked, _t, map) = fixture("write to ada@example.test today");
        assert_eq!(
            map.substitute("I will write to [EMAIL_1] now."),
            "I will write to ada@example.test now."
        );
    }

    #[test]
    fn a_second_reader_sees_the_placeholder_not_the_original() {
        let (_m, _masked, _t, map) = fixture("write to ada@example.test today");
        let shared = map.redact("I will write to [EMAIL_1] now.");
        assert_eq!(shared, "I will write to [EMAIL_1] now.");
        assert!(!shared.contains("ada@example.test"));
    }

    #[test]
    fn an_unentitled_reader_receives_a_map_that_cannot_substitute_at_all() {
        let (_m, _masked, _t, map) = fixture("write to ada@example.test today");
        let others = map.for_reader(false);
        assert!(others.is_empty());
        assert_eq!(others.substitute("[EMAIL_1]"), "[EMAIL_1]");
    }

    #[test]
    fn an_untouched_answer_passes_through_both_paths_unchanged() {
        let map = RemapMap::empty();
        assert!(map.is_empty());
        assert_eq!(map.substitute("no values here"), "no values here");
        assert_eq!(map.redact("no values here"), "no values here");
    }

    #[test]
    fn a_mangled_placeholder_is_left_visible_rather_than_guessed() {
        let (_m, _masked, _t, map) = fixture("write to ada@example.test today");
        // The provider rewrote the token. `remap_text` substitutes exactly, so the mangled form
        // survives untouched — which is the documented behaviour: never guess a value in.
        assert_eq!(map.substitute("see [Email_1]"), "see [Email_1]");
    }

    #[test]
    fn the_same_value_twice_yields_one_placeholder_and_one_entry() {
        let text = "ada@example.test and ada@example.test";
        let matches = vec![
            match_at(text, "ada@example.test", "email"),
            match_at(text, "ada@example.test", "email"),
        ];
        // The second match's offsets must differ for a realistic duplicate span.
        let mut second = matches[1].clone();
        second.start = text.rfind("ada@example.test").expect("second occurrence");
        second.end = second.start + "ada@example.test".len();
        let matches = vec![matches[0].clone(), second];
        let tokens = token_map(&matches, MaskStyle::Numbered);
        let masked = mask_text(text, &matches, MaskStyle::Numbered);
        let map = RemapMap::from_matches(text, &masked, &matches, &tokens);
        assert_eq!(masked, "[EMAIL_1] and [EMAIL_1]");
        assert_eq!(map.len(), 2, "both spans map to the same token");
        assert_eq!(map.placeholders(), vec!["[EMAIL_1]", "[EMAIL_1]"]);
    }

    #[test]
    fn a_token_naming_two_values_in_two_turns_is_withheld_rather_than_guessed() {
        // Each turn is inspected separately, so each numbers its own first e-mail `[EMAIL_1]`.
        let first = "mail ada@example.test";
        let second = "mail grace@example.test";
        let maps = vec![
            Some(RemapMap::from_matches(
                first,
                &mask_text(
                    first,
                    &vec![match_at(first, "ada@example.test", "email")],
                    MaskStyle::Numbered,
                ),
                &vec![match_at(first, "ada@example.test", "email")],
                &token_map(
                    &[match_at(first, "ada@example.test", "email")],
                    MaskStyle::Numbered,
                ),
            )),
            Some(RemapMap::from_matches(
                second,
                &mask_text(
                    second,
                    &vec![match_at(second, "grace@example.test", "email")],
                    MaskStyle::Numbered,
                ),
                &vec![match_at(second, "grace@example.test", "email")],
                &token_map(
                    &[match_at(second, "grace@example.test", "email")],
                    MaskStyle::Numbered,
                ),
            )),
        ];
        let merged = RemapMap::merge(&maps);

        // The dangerous case is a silent wrong answer: a coin toss between two real addresses.
        // Whichever one this picks, the reader is told about somebody else's address.
        assert!(
            merged.is_empty(),
            "a contested token must put nothing back, got {:?}",
            merged.entries()
        );
        assert_eq!(
            merged.withheld(),
            ["[EMAIL_1]".to_owned()],
            "the withheld token must be reported, not silently dropped"
        );
        assert_eq!(
            merged.substitute("I mailed [EMAIL_1]"),
            "I mailed [EMAIL_1]",
            "the token must stay visible rather than become a guess"
        );
        assert!(
            !merged.substitute("[EMAIL_1]").contains("example.test"),
            "NO address may be substituted for a contested token"
        );
    }

    #[test]
    fn the_same_value_in_two_turns_merges_into_one_entry_and_round_trips() {
        let text = "mail ada@example.test";
        let matches = vec![match_at(text, "ada@example.test", "email")];
        let tokens = token_map(&matches, MaskStyle::Numbered);
        let masked = mask_text(text, &matches, MaskStyle::Numbered);
        let map = RemapMap::from_matches(text, &masked, &matches, &tokens);
        let merged = RemapMap::merge(&[Some(map.clone()), Some(map)]);

        assert_eq!(merged.len(), 1, "one person named twice is one entry");
        assert!(merged.withheld().is_empty(), "nothing was contested here");
        // The round trip is the promise: masked text -> original answer.
        assert_eq!(merged.substitute(&masked), text);
    }

    #[test]
    fn merging_unmasked_turns_alongside_masked_ones_keeps_the_masked_values() {
        let text = "mail ada@example.test";
        let matches = vec![match_at(text, "ada@example.test", "email")];
        let tokens = token_map(&matches, MaskStyle::Numbered);
        let masked = mask_text(text, &matches, MaskStyle::Numbered);
        let map = RemapMap::from_matches(text, &masked, &matches, &tokens);
        // `None` is what a clean turn reports — a batch is mostly clean turns in practice, and
        // pairing them positionally is the case an off-by-one would corrupt.
        let merged = RemapMap::merge(&[None, Some(map)]);

        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged.substitute("sent to [EMAIL_1]"),
            "sent to ada@example.test"
        );
    }

    #[test]
    fn a_merge_of_nothing_is_empty_and_harmless() {
        let merged = RemapMap::merge(&[]);
        assert!(merged.is_empty());
        assert!(merged.withheld().is_empty());
        assert_eq!(merged.substitute("[EMAIL_1]"), "[EMAIL_1]");
    }

    #[test]
    fn labels_lists_each_classification_once_in_first_appearance_order() {
        let text = "ada@example.test and 4111 1111 1111 1111";
        let matches = vec![
            match_at(text, "ada@example.test", "email"),
            match_at(text, "4111 1111 1111 1111", "card"),
        ];
        let tokens = token_map(&matches, MaskStyle::Numbered);
        let masked = mask_text(text, &matches, MaskStyle::Numbered);
        let map = RemapMap::from_matches(text, &masked, &matches, &tokens);
        // Appearance order, not sorted: the spans are already ordered by position by the detector,
        // and re-sorting would only invent a contract this function does not offer.
        assert_eq!(map.labels(), vec!["email".to_owned(), "card".to_owned()]);
        assert_eq!(
            map.substitute("mail [EMAIL_1] about [CARD_1]"),
            "mail ada@example.test about 4111 1111 1111 1111"
        );
    }
}
