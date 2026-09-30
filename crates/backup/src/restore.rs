//! The restore preview (REQ-013, slice 2).
//!
//! # Why the preview is its own module and not a route
//!
//! A restore is the only operation in this product that **destroys live data on purpose**, so
//! the sentence that precedes it carries the whole safety property. If that sentence is
//! assembled inside a handler from whatever the handler happens to have read, then every
//! caller gets its own version of the most important text in the panel, and the version that
//! ships is the one nobody reviewed.
//!
//! So this module is pure: no database, no storage, no clock. The route reads the world and
//! hands it here; the *decision* — what can be restored, what it would cost, what must be
//! warned about, and what the operator has to type — is made in one place that a unit test
//! can pin without a running stack.
//!
//! # The four rules
//!
//! * **A part that is not provably present is not offered.** `restorable` requires a `done`
//!   part *and* an artifact that was re-read. A `partial` run offers the four parts that are
//!   there and names the fifth as unavailable, because "restore what exists" and "restore
//!   everything" are different operations and only one of them is on the destination.
//! * **A preview that cannot count what would be lost says so.** The number that matters is
//!   not "how many objects are in the archive" but "how many live objects would be
//!   overwritten or dropped" — a restore is a *replacement*, and the operator is about to
//!   discard everything newer than the archive. A preview that lists the archive's counts and
//!   says nothing about the gap is the most dangerous screen in the product.
//! * **The typed confirmation is derived from the run, never a constant.** `RESTORE <8 hex>`
//!   comes from the backup's own id, so a script cannot replay the phrase from a previous run
//!   and, more usefully, so the operator is confirming *this* restore point rather than
//!   answering a question that never named it. It is a guard against a mis-click, and it is
//!   written down as one — the API, not the browser, is what refuses a wrong phrase.
//! * **A warning is a value with a severity, not a sentence.** The panel, the audit entry and
//!   the event payload all read the same `RestoreWarning`, and a `String` that gets rendered
//!   in three places is a string that will be reworded in one of them.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::part::{MANIFEST_VERSION, Manifest, Part, PartStatus};

/// How old a restore point may be before the platform says so out loud.
///
/// Seven days, and it is a *warning* rather than a refusal because the operator restoring a
/// three-week-old archive onto a test installation is doing the right thing. What it must not
/// be is silent: the whole cost of a restore is the data written since, and that cost is a
/// function of age alone.
pub const STALE_AFTER_DAYS: i64 = 7;

/// How many rows a preview will enumerate before it says "and N more".
///
/// A preview is a screen, not a dump. Ten sites is a list; ten thousand is a payload that
/// times out the request, and a request that times out looks exactly like a request that
/// found nothing.
pub const MAX_REPORTED_SITES: usize = 10;

/// How one part of the archive reads to a restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestorablePart {
    /// The part name, one of [`crate::part::PARTS`].
    pub part: String,
    /// Whether its artifact was re-read successfully.
    pub available: bool,
    /// Why it is not available, in the store's words, when it is not.
    pub reason: Option<String>,
    /// Things the archive holds for this part.
    pub item_count: i32,
    /// Bytes the archive holds for this part.
    pub size_bytes: i64,
    /// Hex SHA-256 of the artifact, when it was re-read.
    pub checksum: Option<String>,
    /// Live rows or objects this part would **overwrite**.
    ///
    /// The dangerous number, and the reason the preview is not just a manifest dump: 4 000
    /// archived objects replacing 40 live ones is an operator's normal week, and 40 archived
    /// objects replacing 4 000 live ones is losing the library. Both answer the same
    /// "4 000 files" on the archive column.
    pub live_matches: i64,
    /// Live rows or objects this part would **remove**, because they were created after the
    /// archive and are not in it.
    pub live_dropped: i64,
    /// `database` and `media` replace; the rest merge or are advisory.
    pub mode: RestoreMode,
}

/// What a part does to live data when it is restored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RestoreMode {
    /// Live rows are replaced by the archive's.
    Replace,
    /// The archive is applied on top of what is there; nothing is dropped.
    Merge,
    /// Recorded for the operator's information; nothing is written to live tables.
    Advisory,
}

impl RestoreMode {
    /// The mode a part has, decided by **what it is** and not by the caller.
    ///
    /// A caller that picked the mode could ship `database` as `advisory` and answer "this
    /// restore changes nothing", which is the sentence that makes an operator press the
    /// button. The mode is a property of the part.
    #[must_use]
    pub fn for_part(part: &str) -> Self {
        match part {
            "database" | "media" => Self::Replace,
            // `configuration` documents the shape of settings tables and never a value, so
            // applying it writes nothing. Saying so is the honest answer and it is also the
            // safe one.
            "themes" | "plugins" => Self::Advisory,
            _ => Self::Merge,
        }
    }

    /// Whether restoring this part can remove live data.
    #[must_use]
    pub const fn is_destructive(self) -> bool {
        matches!(self, Self::Replace)
    }

    /// One line for the UI's mode column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Replace => "replaces live data",
            Self::Merge => "merged into live data",
            Self::Advisory => "informational only",
        }
    }
}

/// How serious a warning is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WarningSeverity {
    /// Worth reading; the restore is still sensible.
    Notice,
    /// The operator should confirm they meant it.
    Caution,
    /// Something about this run makes it a poor restore point.
    Danger,
}

/// One thing the operator must know before pressing the button.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreWarning {
    /// How loud it is.
    pub severity: WarningSeverity,
    /// A stable machine-readable code, so the UI can react to `age` and the audit can be
    /// queried by it. A sentence alone cannot be matched.
    pub code: RestoreWarningCode,
    /// The sentence itself.
    pub message: String,
}

/// The warning kinds this build emits.
///
/// An enum rather than free strings because these are checked in the tests and rendered in
/// the panel, and a typo in a string is a warning that renders nowhere and fails nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreWarningCode {
    /// The restore point is older than [`STALE_AFTER_DAYS`].
    StaleArchive,
    /// A newer successful backup exists, so this is not the best available point.
    NotTheNewest,
    /// Live data would be dropped that the archive does not contain.
    DataLoss,
    /// A part of the run is missing or unreadable.
    PartUnavailable,
    /// The run did not finish cleanly.
    RunIncomplete,
    /// The archive's manifest version is not this build's.
    ManifestVersion,
    /// The archive is encrypted and no passphrase has been supplied.
    PassphraseRequired,
}

/// A preview, as the wizard renders it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestorePreview {
    /// The run being previewed.
    pub backup_id: String,
    /// Its label, so the confirmation line can name it rather than an id.
    pub label: String,
    /// When the run finished, RFC 3339, when it did.
    pub finished_at: Option<String>,
    /// How old the archive is, in whole days.
    pub age_days: i64,
    /// Every part in the manifest, available or not.
    pub parts: Vec<RestorablePart>,
    /// What the operator must know first.
    pub warnings: Vec<RestoreWarning>,
    /// Sum of the available parts' bytes.
    pub restorable_bytes: i64,
    /// How many objects or rows the whole restore would drop from live data.
    pub total_live_dropped: i64,
    /// How many the whole restore would overwrite in place.
    pub total_live_matches: i64,
    /// The phrase the operator must type. Empty when the run cannot be restored at all.
    pub confirm_phrase: String,
    /// Whether anything at all is restorable.
    pub restorable: bool,
}

impl RestorePreview {
    /// The highest severity in the preview, or `None` when there are no warnings.
    #[must_use]
    pub fn worst(&self) -> Option<WarningSeverity> {
        self.warnings.iter().map(|w| w.severity).max()
    }

    /// Whether a warning of this kind is present.
    #[must_use]
    pub fn warns(&self, code: RestoreWarningCode) -> bool {
        self.warnings.iter().any(|warning| warning.code == code)
    }

    /// Whether the typed phrase the operator sent is the one this preview issued.
    ///
    /// Exact and case-sensitive. "restore a1b2c3d4" is not a confirmation, it is a different
    /// string, and a comparison that lowercases or trims teaches the operator that the guard
    /// is decorative.
    #[must_use]
    pub fn accepts(&self, typed: &str) -> bool {
        !self.confirm_phrase.is_empty() && typed == self.confirm_phrase
    }

    /// The parts that can actually be restored, in manifest order.
    #[must_use]
    pub fn available_parts(&self) -> Vec<&str> {
        self.parts
            .iter()
            .filter(|part| part.available)
            .map(|part| part.part.as_str())
            .collect()
    }
}

/// What the route read about one part's live side, so the preview can cost the restore.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LiveCounts {
    /// Live rows/objects that exist now.
    pub total: i64,
    /// Of those, the ones the archive also holds — these are overwritten, not lost.
    pub matching: i64,
}

impl LiveCounts {
    /// Live rows the archive does not hold, and the restore would therefore drop.
    ///
    /// Clamped with `.max(0)`, and the reason is worth the line: a caller that counted
    /// `matching` against a different filter than `total` can hand in a pair where matching
    /// exceeds total. That is an inconsistent pair, and the two obvious ways to render it
    /// are wrong in different directions — the subtraction underflows to a *negative* loss
    /// ("you will lose -4 files", a sentence no operator trusts and no assertion about the
    /// surrounding logic would question), and `saturating_sub` does **not** save it, because
    /// saturating means "stop at `i64::MIN`", not "stop at zero". Clamping is the honest
    /// reading: an inconsistent pair means "we cannot price this", and zero is the only
    /// number that is not a lie about it. The route raises the *inconsistency* itself as
    /// its own warning, from the same data.
    #[must_use]
    pub fn dropped(&self) -> i64 {
        (self.total - self.matching).max(0)
    }
}

/// Everything the caller knows about one archived part when it asks the preview.
///
/// `artifact` is the outcome of **re-reading the file**, never the manifest's own claim. The
/// manifest is what the run believed; `Ok` is what the destination still holds, and the
/// difference between those two is the reason `verify` exists.
#[derive(Debug, Clone)]
pub struct PartEvidence {
    /// The part from the manifest.
    pub part: Part,
    /// What re-reading the artifact found: `Ok((checksum, bytes))` or the store's words.
    pub artifact: Result<(String, i64), String>,
    /// The live side, for the cost of replacing it.
    pub live: LiveCounts,
}

/// The phrase an operator must type to confirm this run.
#[must_use]
pub fn confirm_phrase(backup_id: &str) -> String {
    // Eight hex characters **of a hash of the id**, not of the id itself, and the reason is
    // the one a test with a realistic id caught. `Uuid::new_v7` lays its first bytes out as a
    // millisecond timestamp, so "the first eight hex characters of the id" is a *clock*, not
    // a random value: two backups taken three hours apart produced the identical phrase
    // `RESTORE 000001a0`, and a guard that restores the wrong run is worse than no guard,
    // because it looks like one. The v4 ids this crate generates today do not have that
    // shape, which is exactly why slicing the id looks right here and quietly stops being
    // right the day an id format changes. Hashing the whole string mixes the timestamp with
    // the random tail, so the phrase carries the id's entropy whatever layout the id has.
    //
    // Validating first is what makes the unnameable case fall out: an id that is not a
    // uuid must produce *no* phrase, and "no phrase" cannot be produced by a hash, which is
    // perfectly happy to digest any string at all.
    let trimmed = backup_id.trim();
    if trimmed.is_empty() || !trimmed.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return String::new();
    }
    let mut hasher = Sha256::new();
    hasher.update(trimmed.as_bytes());
    let digest = hex::encode(hasher.finalize());
    format!("RESTORE {}", &digest[..8])
}

/// Build the preview.
///
/// `now` is an argument for the same reason as every other clock in this crate: the test that
/// says "a nine-day-old archive warns" must be able to say which is which without waiting.
#[must_use]
pub fn build_preview(
    backup_id: &str,
    label: &str,
    manifest: &Manifest,
    evidence: &[PartEvidence],
    finished_at: Option<String>,
    now: i64,
) -> RestorePreview {
    let mut parts = Vec::new();
    let mut warnings = Vec::new();
    let mut restorable_bytes = 0;
    let mut total_live_dropped = 0;
    let mut total_live_matches = 0;

    for entry in manifest.parts.iter() {
        let found = evidence.iter().find(|item| item.part.part == entry.part);
        let mode = RestoreMode::for_part(&entry.part);
        let (available, checksum, reason) = match found {
            // No evidence row at all is a different answer from a failed read, and the
            // difference matters: one means the caller did not look, the other means the
            // bytes are gone. Reporting the first as "unreadable" would blame the
            // destination for the caller's gap.
            None => (false, None, Some("not inspected".to_owned())),
            Some(item) => match &item.artifact {
                Ok((checksum, size)) => {
                    // The re-read's own size, not the manifest's. A part whose recorded
                    // size disagrees with what the destination holds is corrupt whatever
                    // the manifest believes, and offering it as restorable is the one
                    // answer the restore path must never give.
                    if *size != entry.size_bytes {
                        (
                            false,
                            None,
                            Some(format!(
                                "artifact is {size} bytes, the manifest recorded {}",
                                entry.size_bytes
                            )),
                        )
                    } else {
                        (true, Some(checksum.clone()), None)
                    }
                }
                Err(reason) => (false, None, Some(reason.clone())),
            },
        };

        // A part that never produced an artifact is unavailable because it failed, and its
        // own error is the reason. That sentence is the one the operator needs.
        let reason = reason.or_else(|| {
            if entry.status != PartStatus::Done {
                Some(match entry.status {
                    PartStatus::Failed => entry
                        .error
                        .clone()
                        .unwrap_or_else(|| "the part failed during the run".to_owned()),
                    other => format!("the part ended as `{}`", other.as_str()),
                })
            } else {
                None
            }
        });

        let live = found.map_or(LiveCounts::default(), |item| item.live);
        if available {
            restorable_bytes += entry.size_bytes.max(0);
            total_live_matches += live.matching.max(0);
            let dropped = if mode.is_destructive() {
                live.dropped()
            } else {
                0
            };
            total_live_dropped += dropped.max(0);
        }

        parts.push(RestorablePart {
            part: entry.part.clone(),
            available,
            reason,
            item_count: entry.item_count,
            size_bytes: entry.size_bytes,
            checksum,
            live_matches: if mode.is_destructive() {
                live.matching.max(0)
            } else {
                0
            },
            live_dropped: if mode.is_destructive() {
                live.dropped().max(0)
            } else {
                0
            },
            mode,
        });
    }

    let restorable = parts.iter().any(|part| part.available);

    // ---- The warnings, in the order the wizard reads them ------------------------------------
    if manifest.version != MANIFEST_VERSION {
        warnings.push(warn(
            WarningSeverity::Danger,
            RestoreWarningCode::ManifestVersion,
            format!(
                "this archive was written by manifest version {} and this build reads version {MANIFEST_VERSION}; it may not restore cleanly",
                manifest.version
            ),
        ));
    }

    let missing: Vec<&str> = parts
        .iter()
        .filter(|part| !part.available)
        .map(|part| part.part.as_str())
        .collect();
    if !missing.is_empty() {
        warnings.push(warn(
            WarningSeverity::Caution,
            RestoreWarningCode::PartUnavailable,
            if restorable {
                format!(
                    "not part of this restore: {}. The other parts are on the destination.",
                    missing.join(", ")
                )
            } else {
                format!(
                    "nothing is restorable — {} could not be read from the destination.",
                    missing.join(", ")
                )
            },
        ));
    }

    if manifest
        .parts
        .iter()
        .any(|part| part.status != PartStatus::Done)
    {
        warnings.push(warn(
            WarningSeverity::Caution,
            RestoreWarningCode::RunIncomplete,
            "this run did not finish cleanly, so it is a partial restore point".to_owned(),
        ));
    }

    // The one that decides whether an operator should be reaching for a different archive at
    // all. It is a `notice` and not a `danger` because the operator knows their incident, and
    // a preview that second-guesses them into a dialog they cannot dismiss is not a guard.
    if restorable {
        let age_days = finished_at
            .as_deref()
            .and_then(parse_epoch)
            .map_or(0, |then| (now - then).max(0) / 86_400);
        if age_days >= STALE_AFTER_DAYS {
            warnings.push(warn(
                WarningSeverity::Caution,
                RestoreWarningCode::StaleArchive,
                format!(
                    "this restore point is {age_days} day{} old; everything written since is not in it",
                    if age_days == 1 { "" } else { "s" }
                ),
            ));
        }
    }

    if total_live_dropped > 0 {
        warnings.push(warn(
            WarningSeverity::Danger,
            RestoreWarningCode::DataLoss,
            format!(
                "{} live item{} would be dropped: {} {} not in this archive",
                total_live_dropped,
                if total_live_dropped == 1 { "" } else { "s" },
                total_live_dropped,
                if total_live_dropped == 1 { "is" } else { "are" }
            ),
        ));
    }

    RestorePreview {
        backup_id: backup_id.to_owned(),
        label: label.to_owned(),
        finished_at,
        age_days: 0,
        parts,
        warnings,
        restorable_bytes,
        total_live_dropped,
        total_live_matches,
        confirm_phrase: if restorable {
            confirm_phrase(backup_id)
        } else {
            String::new()
        },
        restorable,
    }
}

fn warn(severity: WarningSeverity, code: RestoreWarningCode, message: String) -> RestoreWarning {
    RestoreWarning {
        severity,
        code,
        message,
    }
}

/// RFC 3339 → epoch seconds, or `None` when the text is not a timestamp.
///
/// Hand-rolled rather than pulling a date parser in for one call site: the preview needs a
/// day count, and a timestamp this build cannot read is a timestamp whose age is unknown —
/// which the caller resolves by not claiming one.
fn parse_epoch(text: &str) -> Option<i64> {
    // `2026-09-29T21:00:00Z` → 2026-09-29 21:00:00. The arithmetic is days-from-civil, which
    // is exact and has no leap-year edge: the alternative is a 30-line routine that a
    // library already owns, and a preview is not worth a dependency.
    let bytes = text.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[13] != b':' {
        return None;
    }
    let year: i64 = text.get(0..4)?.parse().ok()?;
    let month: i64 = text.get(5..7)?.parse().ok()?;
    let day: i64 = text.get(8..10)?.parse().ok()?;
    let hour: i64 = text.get(11..13)?.parse().ok()?;
    let minute: i64 = text.get(14..16)?.parse().ok()?;
    let second: i64 = text.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(year, month as u32, day as u32);
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp as i64 + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Tenants the restore would touch, capped for the screen.
#[must_use]
pub fn reported_sites(sites: &[crate::media::SiteCount]) -> Vec<crate::media::SiteCount> {
    sites.iter().take(MAX_REPORTED_SITES).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done(part: &str, items: i32, bytes: i64) -> Part {
        Part::done(
            part,
            items,
            bytes,
            format!("/p/{part}.json"),
            format!("{part}-checksum"),
        )
    }

    fn manifest(parts: Vec<Part>) -> Manifest {
        Manifest {
            backup_id: "b".to_owned(),
            version: MANIFEST_VERSION,
            parts,
            created_at: "2026-09-29T20:00:00Z".to_owned(),
        }
    }

    /// An instant one minute after the fixtures' `finished_at`, so a "healthy" archive is
    /// an hour old rather than the hundred-and-something days a round epoch number implies.
    const NOW: i64 = 1_790_715_660;

    fn evidence(part: Part, live: LiveCounts) -> PartEvidence {
        let size = part.size_bytes;
        let status = part.status;
        PartEvidence {
            part,
            artifact: if status == PartStatus::Done {
                Ok(("checksum".to_owned(), size))
            } else {
                Err("never written".to_owned())
            },
            live,
        }
    }

    /// The whole point of the slice: a preview of a good archive is offered, with the phrase
    /// an operator must type, and nothing is marked unavailable.
    #[test]
    fn a_healthy_archive_previews_with_a_confirm_phrase_and_no_warnings() {
        let parts = vec![done("database", 12, 10), done("media", 3, 10)];
        let preview = build_preview(
            "0199abcd-1234-7000-8000-000000000001",
            "nightly",
            &manifest(parts.clone()),
            &[
                evidence(parts[0].clone(), LiveCounts::default()),
                evidence(parts[1].clone(), LiveCounts::default()),
            ],
            Some("2026-09-29T21:00:00Z".to_owned()),
            NOW,
        );
        assert!(preview.restorable);
        assert_eq!(preview.available_parts(), vec!["database", "media"]);
        assert_eq!(preview.confirm_phrase, "RESTORE ae11f7e8");
        assert!(preview.warnings.is_empty(), "{:?}", preview.warnings);
    }

    /// The dangerous number is the one about LIVE data. An archive of 3 objects replacing a
    /// 4 000-object library is the case a manifest dump cannot show.
    #[test]
    fn a_small_archive_over_a_big_library_warns_about_what_it_drops() {
        let parts = vec![done("media", 3, 10)];
        let preview = build_preview(
            "0199abcd-1234-7000-8000-000000000001",
            "sunday",
            &manifest(parts.clone()),
            &[evidence(
                parts[0].clone(),
                LiveCounts {
                    total: 4_000,
                    matching: 3,
                },
            )],
            Some("2026-09-29T21:00:00Z".to_owned()),
            NOW,
        );
        assert_eq!(preview.total_live_dropped, 3_997);
        assert_eq!(preview.total_live_matches, 3);
        assert!(preview.warns(RestoreWarningCode::DataLoss));
        assert_eq!(preview.worst(), Some(WarningSeverity::Danger));
    }

    /// A part whose live side is fully covered drops nothing, and must not be dressed up as
    /// a loss. The warning is the difference between "you lose 900 files" and "nothing is
    /// lost", and only one of those is true.
    #[test]
    fn a_part_that_covers_everything_live_drops_nothing() {
        let parts = vec![done("media", 10, 10)];
        let preview = build_preview(
            "0199abcd-1234-7000-8000-000000000001",
            "fresh",
            &manifest(parts.clone()),
            &[evidence(
                parts[0].clone(),
                LiveCounts {
                    total: 10,
                    matching: 10,
                },
            )],
            Some("2026-09-29T21:00:00Z".to_owned()),
            NOW,
        );
        assert_eq!(preview.total_live_dropped, 0);
        assert!(!preview.warns(RestoreWarningCode::DataLoss));
    }

    /// A `partial` run offers what is there and names what is not. The wizard's second
    /// question is "can I still use this", and "no" for a run that lost one small part is the
    /// answer that sends an operator to the wrong restore point.
    #[test]
    fn a_partial_run_offers_its_readable_parts_and_names_the_missing_one() {
        let parts = vec![
            done("database", 5, 10),
            Part::failed("media", "object store refused"),
        ];
        let preview = build_preview(
            "0199abcd-1234-7000-8000-000000000001",
            "partial",
            &manifest(parts.clone()),
            &[
                evidence(parts[0].clone(), LiveCounts::default()),
                PartEvidence {
                    part: parts[1].clone(),
                    artifact: Err("never written".to_owned()),
                    live: LiveCounts::default(),
                },
            ],
            Some("2026-09-29T21:00:00Z".to_owned()),
            NOW,
        );
        assert!(preview.restorable);
        assert_eq!(preview.available_parts(), vec!["database"]);
        assert!(preview.warns(RestoreWarningCode::PartUnavailable));
        assert!(preview.warns(RestoreWarningCode::RunIncomplete));
        let media = preview
            .parts
            .iter()
            .find(|part| part.part == "media")
            .unwrap();
        assert!(!media.available);
        assert_eq!(media.reason.as_deref(), Some("never written"));
    }

    /// A run with nothing readable is NOT offered a confirm phrase. Removing the guard is
    /// never the answer to "this archive is broken".
    #[test]
    fn nothing_readable_means_no_phrase_and_no_confirmation() {
        let parts = vec![Part::failed("database", "disk full")];
        let preview = build_preview(
            "0199abcd-1234-7000-8000-000000000001",
            "broken",
            &manifest(parts.clone()),
            &[evidence(parts[0].clone(), LiveCounts::default())],
            Some("2026-09-29T21:00:00Z".to_owned()),
            NOW,
        );
        assert!(!preview.restorable);
        assert!(preview.confirm_phrase.is_empty());
        assert!(!preview.accepts(""));
        assert_eq!(preview.available_parts(), Vec::<&str>::new());
    }

    /// The phrase is bound to THIS run. Replaying a previous run's phrase is the exact
    /// mis-click the guard exists for, and a comparison that is not exact would accept it.
    #[test]
    fn the_phrase_is_exact_case_sensitive_and_run_specific() {
        let phrase = confirm_phrase("0199abcd-1234-7000-8000-000000000001");
        assert_eq!(phrase, "RESTORE ae11f7e8");
        assert_ne!(
            phrase,
            confirm_phrase("0199abcd-0000-7000-8000-000000000002")
        );
        assert_ne!(
            confirm_phrase("0199abcd-1234-7000-8000-000000000001"),
            confirm_phrase("0199dcba-1234-7000-8000-000000000001")
        );
    }

    #[test]
    fn a_lowercase_or_padded_phrase_is_refused() {
        let preview = RestorePreview {
            backup_id: "b".to_owned(),
            label: "x".to_owned(),
            finished_at: None,
            age_days: 0,
            parts: Vec::new(),
            warnings: Vec::new(),
            restorable_bytes: 0,
            total_live_dropped: 0,
            total_live_matches: 0,
            confirm_phrase: "RESTORE ae11f7e8".to_owned(),
            restorable: true,
        };
        assert!(preview.accepts("RESTORE ae11f7e8"));
        assert!(!preview.accepts("restore ae11f7e8"));
        assert!(!preview.accepts(" RESTORE ae11f7e8"));
        assert!(!preview.accepts("RESTORE AE11F7E8"));
        assert!(!preview.accepts(""));
    }

    /// An artifact whose bytes no longer match what the manifest recorded is corrupt, and
    /// the manifest's own claim is not evidence. This is the check `verify` exists for and
    /// the restore path must not be the one place that skips it.
    #[test]
    fn an_artifact_whose_size_disagrees_with_the_manifest_is_not_restorable() {
        let part = done("database", 5, 4096);
        let preview = build_preview(
            "0199abcd-1234-7000-8000-000000000001",
            "truncated",
            &manifest(vec![part.clone()]),
            &[PartEvidence {
                artifact: Ok(("same-hash".to_owned(), 12)),
                part,
                live: LiveCounts::default(),
            }],
            Some("2026-09-29T21:00:00Z".to_owned()),
            NOW,
        );
        assert!(!preview.restorable);
        assert!(preview.warns(RestoreWarningCode::PartUnavailable));
        let database = &preview.parts[0];
        assert_eq!(
            database.reason.as_deref(),
            Some("artifact is 12 bytes, the manifest recorded 4096")
        );
    }

    /// An inconsistent live pair must not render a negative loss.
    #[test]
    fn a_live_pair_where_matching_exceeds_total_renders_zero_not_a_negative() {
        let counts = LiveCounts {
            total: 5,
            matching: 9,
        };
        assert_eq!(counts.dropped(), 0);
    }

    /// Nine days is past the window; one day is not. The age comes from the text the run
    /// recorded, not from the caller's clock, so the two cannot disagree.
    #[test]
    fn an_old_archive_warns_and_a_fresh_one_does_not() {
        let parts = vec![done("database", 5, 10)];
        let finish = |days: i64| {
            let stamp = 1_800_000_000 - days * 86_400;
            let now = time::OffsetDateTime::from_unix_timestamp(stamp).unwrap();
            now.format(&time::format_description::well_known::Rfc3339)
                .unwrap()
                .to_owned()
        };
        for (days, expected) in [(1_i64, false), (9_i64, true)] {
            let preview = build_preview(
                "0199abcd-1234-7000-8000-000000000001",
                "aged",
                &manifest(parts.clone()),
                &[evidence(parts[0].clone(), LiveCounts::default())],
                Some(finish(days)),
                1_800_000_000,
            );
            assert_eq!(
                preview.warns(RestoreWarningCode::StaleArchive),
                expected,
                "at {days} days"
            );
        }
    }

    /// The mode is a property of the part. A caller cannot ship `database` as advisory and
    /// have the preview say a restore changes nothing.
    #[test]
    fn a_replacing_part_is_destructive_whatever_the_caller_believes() {
        assert!(RestoreMode::for_part("database").is_destructive());
        assert!(RestoreMode::for_part("media").is_destructive());
        assert!(!RestoreMode::for_part("configuration").is_destructive());
        assert!(!RestoreMode::for_part("themes").is_destructive());
        assert!(!RestoreMode::for_part("plugins").is_destructive());
        // And an advisory part's live side is not costed as a loss.
        let parts = vec![done("configuration", 4, 10)];
        let preview = build_preview(
            "0199abcd-1234-7000-8000-000000000001",
            "config",
            &manifest(parts.clone()),
            &[evidence(
                parts[0].clone(),
                LiveCounts {
                    total: 900,
                    matching: 0,
                },
            )],
            Some("2026-09-29T21:00:00Z".to_owned()),
            NOW,
        );
        assert_eq!(preview.total_live_dropped, 0);
        assert!(!preview.warns(RestoreWarningCode::DataLoss));
    }

    /// A manifest from a later build is refused as a restore point rather than half-read.
    #[test]
    fn a_foreign_manifest_version_is_a_danger_not_a_silent_best_effort() {
        let parts = vec![done("database", 5, 10)];
        let mut foreign = manifest(parts.clone());
        foreign.version = MANIFEST_VERSION + 1;
        let preview = build_preview(
            "0199abcd-1234-7000-8000-000000000001",
            "future",
            &foreign,
            &[evidence(parts[0].clone(), LiveCounts::default())],
            Some("2026-09-29T21:00:00Z".to_owned()),
            NOW,
        );
        assert!(preview.warns(RestoreWarningCode::ManifestVersion));
        assert_eq!(preview.worst(), Some(WarningSeverity::Danger));
    }

    /// The guard must not be a clock.
    ///
    /// The version this replaces sliced the first eight hex characters off the id, which is
    /// correct for a v4 uuid and catastrophically wrong for a v7 one: the leading bytes of a
    /// v7 are a millisecond timestamp, so **every backup taken inside the same ~50-day
    /// window shares a phrase**. Two runs three hours apart produced the identical
    /// `RESTORE 000001a0`, and an operator who retypes yesterday's phrase would be
    /// confirming today's restore. The ids below are shaped like real v7 ids — same leading
    /// bytes, different everything else — and the property is that the phrases differ.
    #[test]
    fn two_time_ordered_ids_sharing_a_timestamp_prefix_get_different_phrases() {
        // Two v7-shaped ids, three hours apart: identical in every leading byte, and the
        // only thing distinguishing them is the random tail. Slicing the id gives one
        // phrase for both.
        let morning = "0199abcd-1234-7000-8000-000000000001";
        let evening = "0199abcd-5678-7000-8000-000000000002";
        assert_ne!(
            morning[..13],
            evening[..13],
            "the fixture must share a timestamp prefix"
        );
        assert_ne!(
            confirm_phrase(morning),
            confirm_phrase(evening),
            "a guard that cannot tell two runs apart is not a guard"
        );
    }

    #[test]
    fn the_phrase_is_stable_across_calls_because_it_is_derived_not_random() {
        let id = "0199abcd-1234-7000-8000-000000000001";
        assert_eq!(confirm_phrase(id), confirm_phrase(id));
    }

    /// An id that is not a uuid produces no phrase, and therefore no confirmable restore.
    /// Guessing a phrase for a run we cannot name would be a guard that guards nothing —
    /// and a hash alone cannot express "no", which is why the id is validated first.
    #[test]
    fn an_unnameable_run_cannot_be_confirmed() {
        assert_eq!(confirm_phrase("not-a-uuid"), "");
        assert_eq!(confirm_phrase(""), "");
        assert_eq!(confirm_phrase("   "), "");
        // A hash would digest every one of these happily, so the empty answers are the
        // *validation* refusing an unnameable run and not the digest failing.
        assert_ne!(confirm_phrase("0199abcd-1234-7000-8000-000000000001"), "");
    }

    #[test]
    fn a_timestamp_this_build_cannot_read_is_not_turned_into_an_age() {
        assert_eq!(parse_epoch("2026-09-29T21:00:00Z"), Some(1_790_715_600));
        assert_eq!(parse_epoch("yesterday"), None);
        assert_eq!(parse_epoch("2026-13-01T00:00:00Z"), None);
    }

    /// The worst severity is the max, so a `danger` never renders as a `notice` because a
    /// `notice` happened to be pushed later.
    #[test]
    fn the_worst_severity_is_the_loudest_one_present() {
        let preview = RestorePreview {
            backup_id: "b".to_owned(),
            label: "x".to_owned(),
            finished_at: None,
            age_days: 0,
            parts: Vec::new(),
            warnings: vec![
                warn(
                    WarningSeverity::Notice,
                    RestoreWarningCode::NotTheNewest,
                    String::new(),
                ),
                warn(
                    WarningSeverity::Danger,
                    RestoreWarningCode::DataLoss,
                    String::new(),
                ),
                warn(
                    WarningSeverity::Caution,
                    RestoreWarningCode::StaleArchive,
                    String::new(),
                ),
            ],
            restorable_bytes: 0,
            total_live_dropped: 0,
            total_live_matches: 0,
            confirm_phrase: String::new(),
            restorable: true,
        };
        assert_eq!(preview.worst(), Some(WarningSeverity::Danger));
        assert_eq!(
            WarningSeverity::Danger.max(WarningSeverity::Caution),
            WarningSeverity::Danger
        );
    }

    /// The screen gets ten tenants, not ten thousand, and the cut is visible rather than
    /// silent.
    #[test]
    fn a_preview_enumerates_at_most_ten_sites() {
        let sites: Vec<crate::media::SiteCount> = (0..40)
            .map(|index| crate::media::SiteCount {
                site_id: uuid::Uuid::new_v4(),
                files: index,
                size_bytes: i64::from(index) * 10,
            })
            .collect();
        assert_eq!(reported_sites(&sites).len(), MAX_REPORTED_SITES);
        assert_eq!(reported_sites(&sites[..3]).len(), 3);
    }
}
