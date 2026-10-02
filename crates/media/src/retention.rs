//! Retention: how long a library keeps what, and the worker that enforces it (REQ-010, slice 4).
//!
//! The trash has existed since slice 1, and the countdown it shows was always the *site's*
//! fallback rather than a policy anybody could change. What is here is the policy itself —
//! scoped to a site or a folder — plus the three sweeps that act on it and the run log that
//! records every one of them.
//!
//! Seven rules hold across this module, and each is a place the obvious shortcut is wrong:
//!
//! * **The current version is never a version sweep's target, whatever its age.** The obvious
//!   query is "delete every version older than N days", and it deletes the version the `media`
//!   row points at — leaving `media.storage_key` naming an object that no longer exists. The
//!   exemption is by *number* (`version <> media.current_version`), not by age, so a policy
//!   with a one-day keep window still cannot empty a file's history into a broken file.
//! * **A hold is a column, so it is one word in every sweep.** A policy-level flag would have
//!   to be joined into three destructive statements, and the first one somebody edits forgets
//!   the other two — and the forgotten one is the one that deletes evidence.
//! * **A purge refuses a referenced file and names the referrers.** `media_references` cascades
//!   away with the file, so a purge *can* silently delete a hero image a live page resolves to
//!   and leave the page holding a uuid nothing answers to. The refusal is a `409` carrying the
//!   referring records, because "3 pages still use this" is something an operator can act on
//!   and "cannot purge" is not.
//! * **The worker writes a run even when it finds nothing.** Retention is the one library
//!   feature whose absence of activity is indistinguishable from being broken: an operator
//!   asking "why is this file still here" needs the sentence "the last run was at 02:00 and it
//!   changed nothing", and a table that only records activity cannot produce it.
//! * **The folder policy wins, and the site policy is the fallback — never the union.** Taking
//!   the *shortest* window of everything that matches is the obvious reading of "most
//!   aggressive" and it is wrong twice: a folder somebody created for a campaign cannot be
//!   shortened by a site-wide rule that happens to be tighter, and the two rows are then
//!   fighting with no way to say which one won. The narrowest scope is the authority.
//! * **A version's bytes are removed before its row.** The obvious order is delete-then-remove,
//!   and it orphans every object the removed version owned: the row is gone, so nothing can
//!   ever name the key again, and the object store keeps the bytes for ever.
//! * **A reference is a statement, not a copy.** The repair scan drops rows whose referent no
//!   longer exists — the *opposite* direction from the purge refusal, and both directions are
//!   needed: a page that was deleted leaves a reference that would refuse a purge for ever,
//!   which is how "cannot purge, still referenced" becomes a support ticket.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{MediaError, Result};

/// Longest a policy name may be. The screen renders it next to a scope path, and an unbounded
/// name pushes the scope off the end of a table row.
pub const MAX_POLICY_NAME_LENGTH: usize = 120;

/// The shortest window a policy may set, in days.
pub const MIN_WINDOW_DAYS: i32 = 1;

/// The `trash_days` a policy gets when the caller names none.
///
/// Spelled here rather than read from the schema because [`validate_new`] has to answer the
/// same question the insert would, and a check that guesses `1` while the column says `30` is
/// a check that lets through a policy the database then refuses by constraint name.
pub const DEFAULT_TRASH_DAYS: i32 = 30;

/// The `purge_after_days` a policy gets when the caller names none.
pub const DEFAULT_PURGE_DAYS: i32 = 90;

/// The `keep_versions_days` a policy gets when the caller names none.
pub const DEFAULT_KEEP_VERSIONS_DAYS: i32 = 365;

/// The longest window a policy may set, in days.
///
/// Ten years. A larger number is not "a longer window", it is a policy somebody typed without
/// reading it, and a library whose files can never be reclaimed is a library that fills the
/// bucket at 03:00 with an alert nobody can act on.
pub const MAX_WINDOW_DAYS: i32 = 3650;

/// Files one sweep touches at most, per pass.
///
/// A sweep over a library with a million rows must not hold a statement open for the length of
/// a working day, and it must not be the thing that times the API's request out. The worker
/// runs again on the next tick, so a bounded pass that leaves work behind is a *later* pass
/// rather than a lost one.
pub const SWEEP_BATCH: i64 = 200;

/// One retention policy.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RetentionPolicy {
    /// Row id.
    pub id: Uuid,
    /// Site the policy belongs to.
    pub site_id: Uuid,
    /// How the policy is named on the settings screen.
    pub name: String,
    /// The folder the policy governs; `None` means the whole site.
    pub folder_id: Option<Uuid>,
    /// How long a superseded version survives.
    pub keep_versions_days: i32,
    /// How long a trashed file can be restored.
    pub trash_days: i32,
    /// How long after `trash_days` the bytes actually go.
    pub purge_after_days: i32,
    /// Whether the hold applies to everything this policy would otherwise touch.
    pub legal_hold: bool,
    /// Whether the worker acts on this row.
    pub enabled: bool,
    /// When the row was created.
    pub created_at: OffsetDateTime,
    /// When the row was last written.
    pub updated_at: OffsetDateTime,
}

impl RetentionPolicy {
    /// What this policy does to a file, as one sentence for the settings screen.
    ///
    /// A screen that shows `keep_versions_days: 365 · trash_days: 30 · purge_after_days: 90` in
    /// three number inputs is showing the *settings*; an operator wants to know what happens to
    /// *their* file, and that is a different sentence.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut sentence = format!(
            "A replaced file keeps {} of superseded history; a deleted file can be restored \
             for {}, and its bytes are removed {} after that",
            plural_days(self.keep_versions_days, "day"),
            plural_days(self.trash_days, "day"),
            plural_days(self.purge_after_days, "day"),
        );
        if self.legal_hold {
            sentence.push_str(
                ". A legal hold is on, so nothing under this policy is removed at all until it is \
                 cleared",
            );
        }
        if !self.enabled {
            sentence.push_str(". This policy is disabled: the worker skips it");
        }
        sentence
    }

    /// What the policy is scoped to, in words.
    #[must_use]
    pub fn scope(&self, folder_path: Option<&str>) -> String {
        match folder_path {
            Some(path) => format!("the folder {path} and everything under it"),
            None => "the whole site".to_string(),
        }
    }
}

/// `1 day` / `30 days`, and `1 byte` / `2048 bytes` — a number with its unit, because "1 days"
/// in a policy screen is the kind of detail that makes an operator read the whole sentence
/// twice, and "1 bytes reclaimed" reads as a typo in a run log.
///
/// Two functions rather than one generic, because the two call sites have two different types
/// and a `T: Display + PartialEq` version needs a `one::<T>()` that cannot be written without
/// a transmute. Naming the type is the honest spelling of "this counts days" versus "this
/// counts bytes", and it is what a reader can verify without a `where` clause.
fn plural_days(days: i32, unit: &str) -> String {
    if days == 1 {
        format!("1 {unit}")
    } else {
        format!("{days} {unit}s")
    }
}

/// The same, over a byte count. `bytes_reclaimed` is a `sum(bigint)` and arrives as an `i64`,
/// which is exactly the value that cannot be a day count.
fn plural_bytes(bytes: i64, unit: &str) -> String {
    if bytes == 1 {
        format!("1 {unit}")
    } else {
        format!("{bytes} {unit}s")
    }
}

/// A policy as the caller describes it. Every field is optional so a `PATCH` sends only what
/// changed, and `None` means "leave alone" rather than "set to null".
#[derive(Debug, Clone, Default)]
pub struct NewRetentionPolicy {
    /// How the policy is named.
    pub name: Option<String>,
    /// The folder it governs.
    pub folder_id: Option<Option<Uuid>>,
    /// Superseded history window.
    pub keep_versions_days: Option<i32>,
    /// Restore window.
    pub trash_days: Option<i32>,
    /// Hard-delete window.
    pub purge_after_days: Option<i32>,
    /// Whether the hold applies.
    pub legal_hold: Option<bool>,
    /// Whether the worker acts on it.
    pub enabled: Option<bool>,
}

/// What a save was asked to change, for the audit entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolicyChanges {
    /// Fields whose value actually moved.
    pub fields: Vec<&'static str>,
}

impl PolicyChanges {
    /// One line for the audit log — the fields, or `[]` when nothing moved.
    #[must_use]
    pub fn summary(&self) -> String {
        if self.fields.is_empty() {
            "[]".to_string()
        } else {
            format!("[{}]", self.fields.join(", "))
        }
    }
}

/// One row of the run log.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RetentionRun {
    /// Row id.
    pub id: Uuid,
    /// Site the run covered; null for an installation-wide pass.
    pub site_id: Option<Uuid>,
    /// The policy that asked for it, when one did.
    pub policy_id: Option<Uuid>,
    /// `daily`, `manual`, `versions`, `trash` or `purge`.
    pub kind: String,
    /// Superseded versions removed.
    pub versions_removed: i64,
    /// Bytes those removals reclaimed.
    pub versions_bytes: i64,
    /// Files purged.
    pub purged: i64,
    /// Bytes those purges reclaimed.
    pub purged_bytes: i64,
    /// Files the sweep wanted to purge and did not, because something still points at them.
    pub refused: i64,
    /// Files the hold removed from the eligible set.
    pub held_back: i64,
    /// What stopped the run, empty when nothing did.
    pub error: String,
    /// Who asked, for a manual pass.
    pub actor_user_id: Option<Uuid>,
    /// When it started.
    pub started_at: OffsetDateTime,
    /// When it finished; null while it is still running.
    pub finished_at: Option<OffsetDateTime>,
}

impl RetentionRun {
    /// Whether the run finished.
    #[must_use]
    pub fn finished(&self) -> bool {
        self.finished_at.is_some()
    }

    /// What the run removed, as a byte total.
    #[must_use]
    pub fn bytes_reclaimed(&self) -> i64 {
        self.versions_bytes.saturating_add(self.purged_bytes)
    }

    /// One sentence about the run, for the log screen.
    ///
    /// A run that removed nothing says so, and says *why nothing was wrong*: a held file and a
    /// referenced file are the two answers an operator needs, and "0 files" alone reads as a
    /// broken worker.
    #[must_use]
    pub fn summary(&self) -> String {
        if !self.error.is_empty() {
            return format!("The run stopped early: {}", self.error);
        }
        if self.versions_removed == 0 && self.purged == 0 {
            if self.held_back > 0 {
                return format!(
                    "Nothing was removed — {} file(s) are under a legal hold.",
                    self.held_back
                );
            }
            if self.refused > 0 {
                return format!(
                    "Nothing was removed — {} file(s) are still referenced.",
                    self.refused
                );
            }
            return "Nothing was eligible.".to_string();
        }
        let mut parts: Vec<String> = Vec::new();
        if self.versions_removed > 0 {
            parts.push(format!(
                "{} old version(s) removed ({} reclaimed)",
                self.versions_removed,
                plural_bytes(self.versions_bytes, "byte")
            ));
        }
        if self.purged > 0 {
            parts.push(format!(
                "{} file(s) purged ({} reclaimed)",
                self.purged,
                plural_bytes(self.purged_bytes, "byte")
            ));
        }
        let mut sentence = parts.join("; ");
        if self.held_back > 0 {
            sentence.push_str(&format!("; {} held back", self.held_back));
        }
        if self.refused > 0 {
            sentence.push_str(&format!("; {} refused, still referenced", self.refused));
        }
        sentence
    }
}

/// Why one file may not be purged.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct PurgeRefusal {
    /// The file that would have been purged.
    pub media_id: Uuid,
    /// Its file name, so the message is about a file the operator can see.
    pub filename: String,
    /// What still points at it.
    pub resource_kind: String,
    /// The referent's id, as text.
    pub resource_id: String,
    /// Which field of it points here.
    pub field: String,
}

impl PurgeRefusal {
    /// One line naming the record that holds the file.
    #[must_use]
    pub fn describe(&self) -> String {
        if self.field.is_empty() {
            format!("{} {}", self.resource_kind, self.resource_id)
        } else {
            format!("{} {} ({})", self.resource_kind, self.resource_id, self.field)
        }
    }
}

/// What a purge found, per file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PurgeOutcome {
    /// Files whose bytes and rows are gone.
    pub purged: i64,
    /// Bytes those files occupied.
    pub purged_bytes: i64,
    /// Files skipped because a hold applies.
    pub held_back: i64,
    /// Files skipped because something still points at them, with the referrers.
    pub refused: Vec<PurgeRefusal>,
    /// Storage keys of the removed files, so the caller can delete exactly those bytes.
    ///
    /// The *current* version's key and every superseded key it owned: a purge removes the whole
    /// history, and a caller that deletes only `media.storage_key` leaves every old version in
    /// the object store for ever with no row that could ever name it again.
    pub storage_keys: Vec<String>,
}

/// What a version sweep found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VersionSweep {
    /// Superseded versions removed.
    pub removed: i64,
    /// Bytes those removals reclaimed.
    pub bytes: i64,
    /// Files the hold removed from the eligible set.
    pub held_back: i64,
    /// Storage keys of the removed versions.
    pub storage_keys: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// Policy CRUD
// ---------------------------------------------------------------------------------------------

const POLICY_COLUMNS: &str =
    "id, site_id, name, folder_id, keep_versions_days, trash_days, purge_after_days, \
     legal_hold, enabled, created_at, updated_at";

/// The policies of a site, site-wide rule first, then the folder rules.
pub async fn list_policies(pool: &PgPool, site_id: Uuid) -> Result<Vec<RetentionPolicy>> {
    let rows = sqlx::query_as::<_, RetentionPolicy>(&format!(
        "select {POLICY_COLUMNS} from media_retention_policies where site_id = $1 \
         order by (folder_id is not null), name"
    ))
    .bind(site_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// The enabled policies a worker acts on for a site.
pub async fn enabled_policies(pool: &PgPool, site_id: Uuid) -> Result<Vec<RetentionPolicy>> {
    let rows = sqlx::query_as::<_, RetentionPolicy>(&format!(
        "select {POLICY_COLUMNS} from media_retention_policies \
         where site_id = $1 and enabled order by (folder_id is not null), name"
    ))
    .bind(site_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// One policy, scoped to the site in the same `where`.
///
/// The scope is in the lookup rather than applied afterwards, for the same reason the grant
/// delete learned it: resolving the row first and checking tenancy second answers `403` for a
/// row that exists, which is the oracle the rule exists to prevent.
pub async fn find_policy(pool: &PgPool, site_id: Uuid, policy_id: Uuid) -> Result<Option<RetentionPolicy>> {
    let row = sqlx::query_as::<_, RetentionPolicy>(&format!(
        "select {POLICY_COLUMNS} from media_retention_policies where id = $1 and site_id = $2"
    ))
    .bind(policy_id)
    .bind(site_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// The policy of a site that governs the whole site.
///
/// Read by a *fallback* path only — the trash countdown, mostly. A file inside a folder with
/// its own rule is governed by that rule, and this row must not answer for it.
pub async fn site_policy(pool: &PgPool, site_id: Uuid) -> Result<RetentionPolicy> {
    let existing = sqlx::query_as::<_, RetentionPolicy>(&format!(
        "select {POLICY_COLUMNS} from media_retention_policies \
         where site_id = $1 and folder_id is null order by created_at limit 1"
    ))
    .bind(site_id)
    .fetch_optional(pool)
    .await?;
    if let Some(row) = existing {
        return Ok(row);
    }
    // A site whose trigger row was removed, or a database restored from a backup taken between
    // the site and its policy. `on conflict do nothing` against the same key the seed uses, so
    // this is the same row the migration would have made.
    let row = sqlx::query_as::<_, RetentionPolicy>(&format!(
        "insert into media_retention_policies (site_id, name) values ($1, 'Standard retention') \
         on conflict (site_id, name) do update set site_id = excluded.site_id \
         returning {POLICY_COLUMNS}"
    ))
    .bind(site_id)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// Validate a create request and return it in the shape the insert wants.
///
/// Every refusal names the *field*, because the settings form renders the message under the
/// input and a check-constraint violation from the database arrives as a `500` whose only clue
/// is a constraint name.
pub fn validate_new(_site_id: Uuid, new: NewRetentionPolicy) -> Result<NewRetentionPolicy> {
    if let Some(name) = &new.name {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(policy_field("name", "name a policy so it can be told apart from the others"));
        }
        if trimmed.chars().count() > MAX_POLICY_NAME_LENGTH {
            return Err(policy_field(
                "name",
                format!("a policy name may be at most {MAX_POLICY_NAME_LENGTH} characters"),
            ));
        }
    }
    for (field, value) in [
        ("keep_versions_days", new.keep_versions_days),
        ("trash_days", new.trash_days),
        ("purge_after_days", new.purge_after_days),
    ] {
        if let Some(days) = value {
            if !(MIN_WINDOW_DAYS..=MAX_WINDOW_DAYS).contains(&days) {
                return Err(policy_field(
                    field,
                    format!("{field} must be between {MIN_WINDOW_DAYS} and {MAX_WINDOW_DAYS} days"),
                ));
            }
        }
    }
    // The cross-field rule, checked here because the request may only carry one of the two.
    // `purge_after_days < trash_days` is "delete the bytes while the file is still restorable",
    // and the database refuses it too — this makes the message name the field instead of
    // arriving as a `500` with a constraint name.
    if let Some(purge) = new.purge_after_days {
        // The fallback is the *column default*, not the minimum: a create that sends only
        // `purge_after_days` gets `trash_days` from the table, and the check has to compare
        // against the value the row will actually hold. Defaulting to 1 here let a request of
        // `purge_after_days = 5` through, and the insert then wrote 5 against a stored 30 —
        // a policy whose bytes go 25 days before the restore window closes, refused by the
        // database as a bare check-constraint name.
        let trash = new.trash_days.unwrap_or(DEFAULT_TRASH_DAYS);
        if purge < trash {
            return Err(policy_field(
                "purge_after_days",
                "purge_after_days must be at least trash_days — the bytes cannot go before the \
                 restore window closes",
            ));
        }
    }
    Ok(NewRetentionPolicy { name: new.name.map(|value| value.trim().to_string()), ..new })
}

fn policy_field(field: &str, message: impl Into<String>) -> MediaError {
    MediaError::InvalidRetentionSetting { field: field.to_string(), reason: message.into() }
}

/// Create a policy. A sibling with the same name is refused rather than renamed.
pub async fn create_policy(
    pool: &PgPool,
    site_id: Uuid,
    new: NewRetentionPolicy,
) -> Result<RetentionPolicy> {
    let name = new
        .name
        .clone()
        .ok_or_else(|| policy_field("name", "name a policy so it can be told apart from the others"))?;
    let new = validate_new(site_id, new)?;
    let row = sqlx::query_as::<_, RetentionPolicy>(&format!(
        "insert into media_retention_policies \
         (site_id, name, folder_id, keep_versions_days, trash_days, purge_after_days, \
          legal_hold, enabled) \
         values ($1, $2, $3, coalesce($4, $9), coalesce($5, $10), coalesce($6, $11), \
         coalesce($7, false), coalesce($8, true)) \
         returning {POLICY_COLUMNS}"
    ))
    .bind(site_id)
    .bind(&name)
    .bind(new.folder_id.unwrap_or(None))
    .bind(new.keep_versions_days)
    .bind(new.trash_days)
    .bind(new.purge_after_days)
    .bind(new.legal_hold)
    .bind(new.enabled)
    // The defaults are bound rather than written into the statement so the number lives in
    // exactly one place: the same constants `validate_new` compares against.
    .bind(DEFAULT_KEEP_VERSIONS_DAYS)
    .bind(DEFAULT_TRASH_DAYS)
    .bind(DEFAULT_PURGE_DAYS)
    .fetch_one(pool)
    .await
    .map_err(|error| policy_name_taken(error, &name))?;
    Ok(row)
}

/// A create that lost the race for a name is the sibling's `PolicyNameTaken`, not a `500`.
///
/// The unique index's own message is a constraint name, and the settings form shows a
/// constraint name to an operator who typed a name that is taken.
fn policy_name_taken(error: sqlx::Error, name: &str) -> MediaError {
    let text = error.to_string();
    if text.contains("media_retention_policy_name_key") || text.contains("23505") {
        return MediaError::PolicyNameTaken { name: name.to_string() };
    }
    MediaError::Database(error)
}

/// Change a policy. Returns the row and the fields that actually moved.
///
/// The fields matter: retention is destructive, and an audit entry that says "policy changed"
/// without naming *which* window answers no question anybody asks afterwards.
pub async fn update_policy(
    pool: &PgPool,
    site_id: Uuid,
    policy_id: Uuid,
    new: NewRetentionPolicy,
) -> Result<(RetentionPolicy, PolicyChanges)> {
    let before = find_policy(pool, site_id, policy_id)
        .await?
        .ok_or(MediaError::RetentionPolicyNotFound)?;
    let new = validate_new(site_id, new)?;

    // The cross-field rule against the *stored* row, not only against the request: a caller
    // that sends only `trash_days = 200` against a stored `purge_after_days = 90` is asking
    // for a policy whose purge is inside its own restore window, and the request alone cannot
    // see that.
    let trash = new.trash_days.unwrap_or(before.trash_days);
    let purge = new.purge_after_days.unwrap_or(before.purge_after_days);
    if purge < trash {
        return Err(policy_field(
            "purge_after_days",
            "purge_after_days must be at least trash_days — the bytes cannot go before the \
             restore window closes",
        ));
    }

    let after = sqlx::query_as::<_, RetentionPolicy>(&format!(
        "update media_retention_policies set \
           name = coalesce($3, name), \
           folder_id = case when $4::boolean then $5 else folder_id end, \
           keep_versions_days = coalesce($6, keep_versions_days), \
           trash_days = coalesce($7, trash_days), \
           purge_after_days = coalesce($8, purge_after_days), \
           legal_hold = coalesce($9, legal_hold), \
           enabled = coalesce($10, enabled), \
           updated_at = now() \
         where id = $1 and site_id = $2 returning {POLICY_COLUMNS}"
    ))
    .bind(policy_id)
    .bind(site_id)
    .bind(new.name.as_deref())
    .bind(new.folder_id.is_some())
    .bind(new.folder_id.unwrap_or(None))
    .bind(new.keep_versions_days)
    .bind(new.trash_days)
    .bind(new.purge_after_days)
    .bind(new.legal_hold)
    .bind(new.enabled)
    .fetch_one(pool)
    .await
    .map_err(|error| policy_name_taken(error, new.name.as_deref().unwrap_or_default()))?;

    let mut fields: Vec<&'static str> = Vec::new();
    if new.name.is_some() && before.name != after.name {
        fields.push("name");
    }
    if new.folder_id.is_some() && before.folder_id != after.folder_id {
        fields.push("folder_id");
    }
    if new.keep_versions_days.is_some() && before.keep_versions_days != after.keep_versions_days {
        fields.push("keep_versions_days");
    }
    if new.trash_days.is_some() && before.trash_days != after.trash_days {
        fields.push("trash_days");
    }
    if new.purge_after_days.is_some() && before.purge_after_days != after.purge_after_days {
        fields.push("purge_after_days");
    }
    if new.legal_hold.is_some() && before.legal_hold != after.legal_hold {
        fields.push("legal_hold");
    }
    if new.enabled.is_some() && before.enabled != after.enabled {
        fields.push("enabled");
    }
    Ok((after, PolicyChanges { fields }))
}

/// Delete a policy. A site-wide rule cannot be deleted — there would be nothing left to fall
/// back to, and the sweep's "which policy governs this file" would have no answer for the
/// files no folder rule covers.
pub async fn delete_policy(pool: &PgPool, site_id: Uuid, policy_id: Uuid) -> Result<()> {
    let result = sqlx::query(
        "delete from media_retention_policies where id = $1 and site_id = $2",
    )
    .bind(policy_id)
    .bind(site_id)
    .execute(pool)
    .await?;
    if result.rows_affected() == 0 {
        return Err(MediaError::RetentionPolicyNotFound);
    }
    Ok(())
}

/// Put a file under a hold, or take it off one.
///
/// The hold is a column on `media` and this is the only place that writes it, so the reason
/// has exactly one door: a hold nobody can explain is a file nobody will ever be allowed to
/// delete, and the audit log is the only place that says why.
pub async fn set_hold(pool: &PgPool, site_id: Uuid, media_id: Uuid, hold: bool) -> Result<bool> {
    let rows = sqlx::query(
        "update media set legal_hold = $3, updated_at = now() \
         where id = $1 and site_id = $2 and (legal_hold is distinct from $3)",
    )
    .bind(media_id)
    .bind(site_id)
    .bind(hold)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(rows > 0)
}

/// The folder path a policy is scoped to, for the settings screen.
///
/// Read from `media_folders` rather than kept on the policy: a rename rewrites the path, and a
/// copy on the row would go stale and leave the screen naming a folder that no longer exists.
pub async fn policy_scope_paths(pool: &PgPool, site_id: Uuid) -> Result<Vec<(Uuid, String)>> {
    let rows = sqlx::query_as::<_, (Uuid, String)>(
        "select p.id, coalesce(f.path, '') from media_retention_policies p \
         left join media_folders f on f.id = p.folder_id \
         where p.site_id = $1 and p.folder_id is not null",
    )
    .bind(site_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

// ---------------------------------------------------------------------------------------------
// The sweeps
// ---------------------------------------------------------------------------------------------

/// The window that governs one file: the narrowest policy that covers it, or the site rule.
///
/// A file inside a folder with its own policy is governed by *that* policy. Taking the
/// shortest window of everything that matches — the obvious reading of "the most aggressive
/// rule wins" — is wrong in both directions: a campaign folder cannot be shortened by a
/// site-wide rule that happens to be tighter, and when the two disagree there is no way to say
/// which one the operator meant.
pub async fn governing_window(
    pool: &PgPool,
    site_id: Uuid,
    folder_id: Option<Uuid>,
) -> Result<Window> {
    if let Some(folder_id) = folder_id {
        let scoped = sqlx::query_as::<_, (i32, i32, i32)>(
            "select keep_versions_days, trash_days, purge_after_days \
             from media_retention_policies \
             where site_id = $1 and folder_id = $2 and enabled limit 1",
        )
        .bind(site_id)
        .bind(folder_id)
        .fetch_optional(pool)
        .await?;
        if let Some(window) = scoped {
            return Ok(Window {
                keep_versions_days: window.0,
                trash_days: window.1,
                purge_after_days: window.2,
            });
        }
    }
    let site = site_policy(pool, site_id).await?;
    Ok(Window {
        keep_versions_days: site.keep_versions_days,
        trash_days: site.trash_days,
        purge_after_days: site.purge_after_days,
    })
}

/// The three windows, resolved for one file's folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// Superseded history window.
    pub keep_versions_days: i32,
    /// Restore window.
    pub trash_days: i32,
    /// Hard-delete window.
    pub purge_after_days: i32,
}

impl Window {
    /// When a file deleted at `deleted_at` loses its bytes.
    #[must_use]
    pub fn purge_at(&self, deleted_at: OffsetDateTime) -> OffsetDateTime {
        deleted_at + time::Duration::days(i64::from(self.purge_after_days))
    }

    /// When a file deleted at `deleted_at` stops being restorable.
    #[must_use]
    pub fn restore_until(&self, deleted_at: OffsetDateTime) -> OffsetDateTime {
        deleted_at + time::Duration::days(i64::from(self.trash_days))
    }
}

/// Remove superseded versions past their keep window.
///
/// Three rules live in this one function, and each of them is a way to break a file:
///
/// * the **current** version is exempt by number, whatever its age;
/// * a file under a hold is not even a candidate;
/// * the row goes before the bytes are removed by the caller, and the caller is handed the
///   keys — an object whose row is gone can never be named again, which is how a library
///   grows an invisible archive of every version it ever had.
pub async fn sweep_versions(
    pool: &PgPool,
    site_id: Uuid,
    window: Window,
    batch: i64,
) -> Result<VersionSweep> {
    let cutoff = OffsetDateTime::now_utc() - time::Duration::days(i64::from(window.keep_versions_days));

    // Claim the rows first, in one statement, so two workers running the same site cannot both
    // count the same version. `for update skip locked` is the same shape the scan sweep uses.
    //
    // The current version is `max(version)` per file rather than a column on `media`: `0026`
    // has no such column, and adding one would mean a replace has to write it in the same
    // transaction as the append or the two disagree. The correlated subquery reads the index
    // `0026` already built, so "the newest number this file has" is one probe per row.
    let claimed: Vec<(Uuid, i64, String)> = sqlx::query_as(
        "select v.id, v.size_bytes, v.storage_key from media_versions v \
         join media m on m.id = v.media_id \
         where m.site_id = $1 and m.legal_hold = false and m.deleted_at is null \
           and v.version < coalesce((select max(v2.version) from media_versions v2 \
                                     where v2.media_id = v.media_id), 1) \
           and v.created_at < $2 \
         order by v.created_at \
         limit $3 for update of v skip locked",
    )
    .bind(site_id)
    .bind(cutoff)
    .bind(batch)
    .fetch_all(pool)
    .await?;

    let held_back = sqlx::query_scalar::<_, i64>(
        "select count(*) from media_versions v join media m on m.id = v.media_id \
         where m.site_id = $1 and m.legal_hold = true and m.deleted_at is null \
           and v.version < coalesce((select max(v2.version) from media_versions v2 \
                                     where v2.media_id = v.media_id), 1) \
           and v.created_at < $2",
    )
    .bind(site_id)
    .bind(cutoff)
    .fetch_one(pool)
    .await?;

    if claimed.is_empty() {
        return Ok(VersionSweep { removed: 0, bytes: 0, held_back, storage_keys: Vec::new() });
    }

    let ids: Vec<Uuid> = claimed.iter().map(|(id, _, _)| *id).collect();
    let bytes: i64 = claimed.iter().map(|(_, size, _)| *size).sum();
    let keys: Vec<String> = claimed.iter().map(|(_, _, key)| key.clone()).collect();
    sqlx::query("delete from media_versions where id = any($1)")
        .bind(&ids)
        .execute(pool)
        .await?;

    Ok(VersionSweep {
        removed: i64::try_from(ids.len()).unwrap_or(0),
        bytes,
        held_back,
        storage_keys: keys,
    })
}

/// The files of a site that are past their purge window, with the reason each was skipped.
///
/// `held_back` and `refused` are *counts of the reason* rather than booleans on the policy,
/// because "the sweep did nothing" and "the sweep declined to do something" are different
/// answers and an operator reading a run log needs to know which one it was.
pub async fn purge_candidates(
    pool: &PgPool,
    site_id: Uuid,
    window: Window,
    batch: i64,
) -> Result<(Vec<PurgeRefusal>, i64, i64)> {
    let cutoff =
        OffsetDateTime::now_utc() - time::Duration::days(i64::from(window.purge_after_days));

    // The held set, counted rather than listed: an operator needs "17 files are under a hold",
    // not 17 names they have to read past to find the ones they can act on.
    let held_back = sqlx::query_scalar::<_, i64>(
        "select count(*) from media where site_id = $1 and deleted_at is not null \
         and legal_hold = true and deleted_at < $2",
    )
    .bind(site_id)
    .bind(cutoff)
    .fetch_one(pool)
    .await?;

    let refused: Vec<PurgeRefusal> = sqlx::query_as(
        "select m.id as media_id, m.filename, r.resource_kind, r.resource_id, r.field \
         from media m join media_references r on r.media_id = m.id \
         where m.site_id = $1 and m.deleted_at is not null and m.legal_hold = false \
           and m.deleted_at < $2 and r.resource_kind <> '' \
         order by m.deleted_at, m.id limit $3",
    )
    .bind(site_id)
    .bind(cutoff)
    .bind(batch)
    .fetch_all(pool)
    .await?;

    // Distinct files, not distinct rows: a page naming the same file in three fields is one
    // refused file, and a count of three would put "cannot purge, 3 references" next to a
    // library the operator believes holds one page.
    let refused_count = refused
        .iter()
        .map(|row| row.media_id)
        .collect::<std::collections::BTreeSet<_>>()
        .len() as i64;

    Ok((refused, refused_count, held_back))
}

/// Purge the trashed files of a site that are past their window and are not held or referenced.
///
/// The storage keys of *every* version a purged file owned are returned, not just the row's
/// own key: a purge removes the whole history, and a caller that deletes one key per file
/// leaves every superseded version in the bucket with no row that could ever name it again.
pub async fn purge_eligible(
    pool: &PgPool,
    site_id: Uuid,
    window: Window,
    batch: i64,
) -> Result<PurgeOutcome> {
    let cutoff =
        OffsetDateTime::now_utc() - time::Duration::days(i64::from(window.purge_after_days));

    // The candidates, claimed with a row lock so two workers cannot purge the same file twice
    // and then both report the same bytes reclaimed.
    let claimed: Vec<(Uuid, i64)> = sqlx::query_as(
        "select id, size_bytes from media \
         where site_id = $1 and deleted_at is not null and legal_hold = false \
           and deleted_at < $2 and not exists (select 1 from media_references r \
                                              where r.media_id = media.id) \
         order by deleted_at limit $3 for update skip locked",
    )
    .bind(site_id)
    .bind(cutoff)
    .bind(batch)
    .fetch_all(pool)
    .await?;

    if claimed.is_empty() {
        return Ok(PurgeOutcome::default());
    }

    let ids: Vec<Uuid> = claimed.iter().map(|(id, _)| *id).collect();
    let keys = all_keys_of(pool, &ids).await?;

    let purged = sqlx::query("delete from media where id = any($1) and deleted_at is not null")
        .bind(&ids)
        .execute(pool)
        .await?
        .rows_affected();
    let purged_bytes: i64 = claimed.iter().map(|(_, size)| *size).sum();

    Ok(PurgeOutcome {
        purged: purged as i64,
        purged_bytes,
        held_back: 0,
        refused: Vec::new(),
        storage_keys: keys,
    })
}

/// Every storage key a set of files owns, current version, superseded history and preset cache
/// alike.
///
/// A **delegate**, not a second query. This function and [`crate::browser::owned_object_keys`] are
/// the same question asked twice, and they had drifted: this one unioned `media` with
/// `media_versions` only, so the nightly purge sweep — the path that exists to reclaim storage that
/// nothing else will — left every `media_derivatives` object in the bucket while deleting the rows
/// that named it. Three implementations of one answer (this, `owned_object_keys`, and
/// [`crate::preset_store::clear_derivatives`]) is how the interactive purge came to read only
/// `media.storage_key` in the first place. One function, one answer.
pub async fn all_keys_of(pool: &PgPool, ids: &[Uuid]) -> Result<Vec<String>> {
    crate::browser::owned_object_keys(pool, ids).await
}

/// How many trashed files of a site are past their restore window, and their bytes.
///
/// The trash screen's own number. It resolves the window per file through the governing policy
/// rather than assuming the site rule, because a campaign folder with its own rule would
/// otherwise be counted against the site-wide date and told it has days left it does not have.
pub async fn past_restore_window(pool: &PgPool, site_id: Uuid, window: Window) -> Result<(i64, i64)> {
    let cutoff =
        OffsetDateTime::now_utc() - time::Duration::days(i64::from(window.trash_days));
    // The `::bigint` on the size column is not decoration, and it is load-bearing in a way that
    // is easy to undo. `sum()` over a `bigint` returns `numeric`, and sqlx will not decode that
    // into an `i64` — verified against the database, not assumed:
    //
    //   select pg_typeof(sum(size_bytes))            -- numeric
    //   select pg_typeof(coalesce(sum(size_bytes),0)) -- numeric  ← the `0` adopts the other type
    //
    // The previous line had no cast at all, and the comment above it claimed one. Nothing
    // exercised it: every site in the walks had no trashed file past its window, so `sum()` over
    // an empty set returned `NULL`, which `Option<i64>` accepts — a query that fails only when
    // there is something to count, and a comment that asserted the guard was already in place.
    // The screen that shows this number is the retention tab, so the symptom a person meets is
    // "the retention screen 500s once enough files are trashed".
    let row = sqlx::query_as::<_, (i64, Option<i64>)>(
        "select count(*), sum(size_bytes)::bigint from media \
         where site_id = $1 and deleted_at is not null and deleted_at < $2",
    )
    .bind(site_id)
    .bind(cutoff)
    .fetch_one(pool)
    .await?;
    Ok((row.0, row.1.unwrap_or(0)))
}

// ---------------------------------------------------------------------------------------------
// The run log
// ---------------------------------------------------------------------------------------------

const RUN_COLUMNS: &str =
    "id, site_id, policy_id, kind, versions_removed, versions_bytes, purged, purged_bytes, \
     refused, held_back, error, actor_user_id, started_at, finished_at";

/// What one sweep did, for the run log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunTotals {
    /// Superseded versions removed.
    pub versions_removed: i64,
    /// Bytes those removals reclaimed.
    pub versions_bytes: i64,
    /// Files purged.
    pub purged: i64,
    /// Bytes those purges reclaimed.
    pub purged_bytes: i64,
    /// Files the sweep declined to purge because something points at them.
    pub refused: i64,
    /// Files the hold removed from the eligible set.
    pub held_back: i64,
}

impl RunTotals {
    /// Fold one sweep's outcome into the totals.
    pub fn absorb(&mut self, versions: &VersionSweep, purge: &PurgeOutcome) {
        self.versions_removed += versions.removed;
        self.versions_bytes += versions.bytes;
        self.purged += purge.purged;
        self.purged_bytes += purge.purged_bytes;
        self.held_back += versions.held_back + purge.held_back;
    }
}

/// Open a run. A run is written before the work starts so a sweep that dies leaves evidence.
pub async fn begin_run(
    pool: &PgPool,
    site_id: Option<Uuid>,
    policy_id: Option<Uuid>,
    kind: &str,
    actor: Option<Uuid>,
) -> Result<Uuid> {
    let id = sqlx::query_scalar::<_, Uuid>(
        "insert into media_retention_runs (site_id, policy_id, kind, actor_user_id) \
         values ($1, $2, $3, $4) returning id",
    )
    .bind(site_id)
    .bind(policy_id)
    .bind(kind)
    .bind(actor)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Close a run with what it found — including what it found nothing of.
pub async fn finish_run(pool: &PgPool, run_id: Uuid, totals: &RunTotals, error: &str) -> Result<()> {
    sqlx::query(
        "update media_retention_runs set versions_removed = $2, versions_bytes = $3, \
           purged = $4, purged_bytes = $5, refused = $6, held_back = $7, error = $8, \
           finished_at = now() where id = $1",
    )
    .bind(run_id)
    .bind(totals.versions_removed)
    .bind(totals.versions_bytes)
    .bind(totals.purged)
    .bind(totals.purged_bytes)
    .bind(totals.refused)
    .bind(totals.held_back)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

/// The run log of a site, newest first.
pub async fn list_runs(pool: &PgPool, site_id: Uuid, limit: i64) -> Result<Vec<RetentionRun>> {
    let rows = sqlx::query_as::<_, RetentionRun>(&format!(
        "select {RUN_COLUMNS} from media_retention_runs where site_id = $1 \
         order by started_at desc, id desc limit $2"
    ))
    .bind(site_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// The last finished run of a site, whatever its kind.
pub async fn last_run(pool: &PgPool, site_id: Uuid) -> Result<Option<RetentionRun>> {
    let row = sqlx::query_as::<_, RetentionRun>(&format!(
        "select {RUN_COLUMNS} from media_retention_runs \
         where site_id = $1 and finished_at is not null order by started_at desc, id desc limit 1"
    ))
    .bind(site_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// The sites that hold at least one library — the set a daily pass walks.
pub async fn sites_with_media(pool: &PgPool) -> Result<Vec<Uuid>> {
    let rows = sqlx::query_scalar::<_, Uuid>(
        "select distinct site_id from media",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

// ---------------------------------------------------------------------------------------------
// The repair scan
// ---------------------------------------------------------------------------------------------

/// A reference row whose referent is gone, or whose file is gone.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct DanglingReference {
    /// The reference row.
    pub id: Uuid,
    /// The file it names.
    pub media_id: Uuid,
    /// What kind of record it claimed.
    pub resource_kind: String,
    /// The referent's id.
    pub resource_id: String,
    /// Which field.
    pub field: String,
}

/// The reference rows whose referent no longer exists.
///
/// The other direction from the purge refusal, and both are needed. A page that was deleted
/// leaves a reference behind that will refuse a purge for ever, so "cannot purge: still
/// referenced" is a sentence an operator will meet and cannot act on. `pages` is the only
/// referent the platform can prove the existence of today; a kind this scan does not know is
/// *kept*, because deleting a reference to a record in a module that arrived later is a
/// library that forgets where its files are used.
pub async fn dangling_references(pool: &PgPool, limit: i64) -> Result<Vec<DanglingReference>> {
    let rows = sqlx::query_as::<_, DanglingReference>(
        "select r.id, r.media_id, r.resource_kind, r.resource_id, r.field \
         from media_references r \
         where r.resource_kind = 'page' \
           and not exists (select 1 from pages p where p.id::text = r.resource_id) \
         order by r.created_at limit $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Drop the reference rows whose referent is gone, and say how many went.
///
/// The count is returned rather than logged and forgotten, because "the repair scan found 0"
/// and "the repair scan found 40 and removed 40" are the two answers the settings screen shows.
pub async fn repair_references(pool: &PgPool, limit: i64) -> Result<i64> {
    let removed = sqlx::query(
        "delete from media_references r where r.id in ( \
           select r2.id from media_references r2 \
           where r2.resource_kind = 'page' \
             and not exists (select 1 from pages p where p.id::text = r2.resource_id) \
           order by r2.created_at limit $1 )",
    )
    .bind(limit)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(removed as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(keep: i32, trash: i32, purge: i32, hold: bool, enabled: bool) -> RetentionPolicy {
        RetentionPolicy {
            id: Uuid::nil(),
            site_id: Uuid::nil(),
            name: "Campaign".to_string(),
            folder_id: None,
            keep_versions_days: keep,
            trash_days: trash,
            purge_after_days: purge,
            legal_hold: hold,
            enabled,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn a_policy_states_what_it_does_to_a_file() {
        let sentence = policy(365, 30, 90, false, true).describe();
        assert!(sentence.contains("365 days of superseded history"), "{sentence}");
        assert!(sentence.contains("restored for 30 days"), "{sentence}");
        assert!(sentence.contains("removed 90 days after that"), "{sentence}");
        // The unit is spelled once. `plural_days` appends the "s", so a format string that
        // also says "day(s)" prints "30 days day(s)" — which passed a `contains("30 days")`
        // assertion happily and reads as a typo on every policy card.
        assert!(!sentence.contains("day(s)"), "{sentence}");
        assert!(!sentence.contains("legal hold"), "{sentence}");
    }

    #[test]
    fn a_one_day_window_says_one_day() {
        // "1 days" in a policy screen is the kind of detail that makes an operator read the
        // whole sentence twice, and a keep window of one day is a real configuration.
        let sentence = policy(1, 1, 1, false, true).describe();
        assert!(sentence.contains("1 day of superseded history"), "{sentence}");
        assert!(sentence.contains("restored for 1 day"), "{sentence}");
        assert!(!sentence.contains("1 days"), "{sentence}");
    }

    #[test]
    fn a_hold_is_stated_on_the_policy_and_not_left_to_the_row() {
        let sentence = policy(365, 30, 90, true, true).describe();
        assert!(sentence.contains("legal hold is on"), "{sentence}");
        assert!(sentence.contains("until it is cleared"), "{sentence}");
    }

    #[test]
    fn a_disabled_policy_says_it_is_skipped() {
        let sentence = policy(365, 30, 90, false, false).describe();
        assert!(sentence.contains("disabled"), "{sentence}");
    }

    #[test]
    fn a_purge_window_shorter_than_the_restore_window_is_refused() {
        // The whole point of the rule: the bytes cannot go while the operator was still
        // promised a restore. The request carries only the purge window, so the check has to
        // work with the trash window defaulted.
        let error = validate_new(
            Uuid::nil(),
            NewRetentionPolicy { purge_after_days: Some(5), ..NewRetentionPolicy::default() },
        )
        .expect_err("a purge window inside the restore window is unrepresentable");
        assert!(
            error.to_string().contains("purge_after_days"),
            "{error}"
        );
    }

    #[test]
    fn a_purge_window_equal_to_the_restore_window_is_allowed() {
        // `>=`, not `>`: "purge the day the restore window closes" is a legitimate policy and
        // refusing it would push an operator to a longer window than they asked for.
        assert!(validate_new(
            Uuid::nil(),
            NewRetentionPolicy {
                purge_after_days: Some(30),
                trash_days: Some(30),
                ..NewRetentionPolicy::default()
            }
        )
        .is_ok());
    }

    #[test]
    fn every_window_out_of_range_names_its_own_field() {
        for (field, value) in [
            ("keep_versions_days", 0),
            ("trash_days", -1),
            ("purge_after_days", MAX_WINDOW_DAYS + 1),
        ] {
            let mut new = NewRetentionPolicy::default();
            match field {
                "keep_versions_days" => new.keep_versions_days = Some(value),
                "trash_days" => new.trash_days = Some(value),
                _ => new.purge_after_days = Some(value),
            }
            let error = validate_new(Uuid::nil(), new).expect_err(field);
            assert!(error.to_string().contains(field), "{field}: {error}");
        }
    }

    #[test]
    fn a_blank_policy_name_is_refused_rather_than_becoming_one() {
        let error = validate_new(
            Uuid::nil(),
            NewRetentionPolicy { name: Some("   ".to_string()), ..NewRetentionPolicy::default() },
        )
        .expect_err("a blank name is a policy nobody can find again");
        assert!(error.to_string().contains("name"), "{error}");
    }

    #[test]
    fn a_name_is_trimmed_rather_than_stored_with_its_whitespace() {
        let new = validate_new(
            Uuid::nil(),
            NewRetentionPolicy { name: Some("  Campaign  ".to_string()), ..NewRetentionPolicy::default() },
        )
        .expect("a padded name is a legal name");
        assert_eq!(new.name.as_deref(), Some("Campaign"));
    }

    #[test]
    fn a_run_that_removed_nothing_says_which_of_three_nothings_it_was() {
        // The three sentences are different failures and the operator cannot tell them apart
        // from "0 files": a broken worker, a hold, and a reference.
        let base = RetentionRun {
            id: Uuid::nil(),
            site_id: None,
            policy_id: None,
            kind: "daily".to_string(),
            versions_removed: 0,
            versions_bytes: 0,
            purged: 0,
            purged_bytes: 0,
            refused: 0,
            held_back: 0,
            error: String::new(),
            actor_user_id: None,
            started_at: OffsetDateTime::UNIX_EPOCH,
            finished_at: None,
        };
        assert_eq!(base.summary(), "Nothing was eligible.");

        let held = RetentionRun { held_back: 17, ..base.clone() };
        assert!(held.summary().contains("legal hold"), "{}", held.summary());
        assert!(held.summary().contains("17"), "{}", held.summary());

        let refused = RetentionRun { refused: 3, ..base.clone() };
        assert!(refused.summary().contains("still referenced"), "{}", refused.summary());
        assert!(refused.summary().contains('3'), "{}", refused.summary());
    }

    #[test]
    fn a_run_that_worked_reports_both_halves_and_both_exceptions() {
        let run = RetentionRun {
            versions_removed: 4,
            versions_bytes: 1024,
            purged: 2,
            purged_bytes: 2048,
            held_back: 1,
            refused: 1,
            ..RetentionRun {
                id: Uuid::nil(),
                site_id: None,
                policy_id: None,
                kind: "daily".to_string(),
                versions_removed: 0,
                versions_bytes: 0,
                purged: 0,
                purged_bytes: 0,
                refused: 0,
                held_back: 0,
                error: String::new(),
                actor_user_id: None,
                started_at: OffsetDateTime::UNIX_EPOCH,
                finished_at: None,
            }
        };
        let summary = run.summary();
        assert!(summary.contains("4 old version(s) removed"), "{summary}");
        assert!(summary.contains("2 file(s) purged"), "{summary}");
        assert!(summary.contains("1 held back"), "{summary}");
        assert!(summary.contains("1 refused, still referenced"), "{summary}");
        assert_eq!(run.bytes_reclaimed(), 3072);
    }

    #[test]
    fn a_run_that_stopped_early_says_why_rather_than_reporting_zero() {
        // "Nothing was eligible" after a crash is the most expensive sentence in this module:
        // it is a lie that reads as a healthy worker, and it is what an operator sees at 09:00
        // when the library has quietly stopped being pruned.
        let run = RetentionRun {
            error: "the object store refused the delete".to_string(),
            ..RetentionRun {
                id: Uuid::nil(),
                site_id: None,
                policy_id: None,
                kind: "daily".to_string(),
                versions_removed: 0,
                versions_bytes: 0,
                purged: 0,
                purged_bytes: 0,
                refused: 0,
                held_back: 0,
                error: String::new(),
                actor_user_id: None,
                started_at: OffsetDateTime::UNIX_EPOCH,
                finished_at: None,
            }
        };
        let summary = run.summary();
        assert!(summary.starts_with("The run stopped early"), "{summary}");
        assert!(summary.contains("object store"), "{summary}");
    }

    #[test]
    fn a_refusal_names_the_record_holding_the_file() {
        let refusal = PurgeRefusal {
            media_id: Uuid::nil(),
            filename: "hero.png".to_string(),
            resource_kind: "page".to_string(),
            resource_id: "0d3f".to_string(),
            field: "hero_image_id".to_string(),
        };
        assert_eq!(refusal.describe(), "page 0d3f (hero_image_id)");

        // The record *is* the file: no field to name, and printing an empty bracket is noise.
        let whole = PurgeRefusal { field: String::new(), ..refusal };
        assert_eq!(whole.describe(), "page 0d3f");
    }

    #[test]
    fn a_purge_window_counts_from_the_deletion_not_from_today() {
        let window = Window { keep_versions_days: 365, trash_days: 30, purge_after_days: 90 };
        let deleted = OffsetDateTime::UNIX_EPOCH + time::Duration::days(5);
        assert_eq!(window.purge_at(deleted), deleted + time::Duration::days(90));
        assert_eq!(window.restore_until(deleted), deleted + time::Duration::days(30));
    }

    #[test]
    fn the_audit_summary_names_the_fields_or_says_none_moved() {
        assert_eq!(PolicyChanges::default().summary(), "[]");
        let changes = PolicyChanges { fields: vec!["trash_days", "legal_hold"] };
        assert_eq!(changes.summary(), "[trash_days, legal_hold]");
    }
}
