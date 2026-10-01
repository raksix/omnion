//! Anonymised exports: the column classification map and the builder that refuses to guess
//! (docs/requests/REQ-129, slice 4).
//!
//! ## The rule this module exists to make unrepresentable
//!
//! **An export must not contain a classified value, and the way to guarantee that is to refuse to
//! start without a classification.** Not "export it and warn", not "export it unless somebody
//! remembered" — refuse. Every other design has the same failure with different timing: a column
//! added by a migration nobody reviewed reaches a support dump, and the dump is emailed to a
//! vendor. That failure is invisible at the moment it happens and permanent afterwards, so the
//! only defensible default is the loud one.
//!
//! That is what [`plan_export`] is for. It is a pure function from "the tables asked for" plus "the
//! classification map" to either a [`PlannedExport`] or a [`ClassificationError`] — and it never
//! reaches a database, so the refusal can be unit-tested with no fixture, no pool and no clock.
//!
//! ## Why the check has to be about every selected COLUMN, not every selected TABLE
//!
//! Classifying a table is a lie of the useful kind: it is true of every column that exists today
//! and false of the first column somebody adds. So the map is keyed `(table, column)`, the
//! builder expands each requested table to its CURRENT columns, and a column with no row blocks
//! the export. Adding a column now *automatically* blocks the next export until it is classified —
//! which turns "somebody review the map when a migration adds personal data" from a habit into a
//! property. That is the difference between `remove|hash|synthetic` being a default and being a
//! guarantee.
//!
//! ## The other half: what "no classified value" has to mean
//!
//! A column classified `secret` is not hashed or synthesised, it is **removed** — always, and the
//! caller cannot override it. A hash of a credential is a credential with a slow guess in front of
//! it, so an overridable action here would be a data leak with a config screen in front of it.
//! That refusal lives in [`resolve_action`], which is deliberately separate from
//! [`is_overridable`] so the one place that decides cannot be skipped by a caller that checked the
//! wrong predicate.
//!
//! ## Hashed values are consistent within one export and across tables
//!
//! The requirement is that joins still work: a hashed `users.id` must equal the same hashed
//! `orders.user_id`, or every join in the dump dies. That is a property of the *salt* being one
//! value per export, not of the hashing function, so [`anonymize_value`] takes the salt as an
//! argument and the builder generates it once ([`Salt`]). The salt is deliberately NOT returned by
//! [`plan_export`] and never stored beside the file — see the migration header for why storing it
//! would make the hashing pointless.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// What a column holds. The vocabulary is closed by the migration's CHECK, and this list is the
/// code's copy of it — kept in step deliberately, because the two must not disagree.
pub const CLASSES: [&str; 4] = ["personal", "secret", "identifier", "safe"];

/// What an export does with a column.
pub const ACTIONS: [&str; 4] = ["remove", "hash", "synthetic", "keep"];

/// The states an export passes through. Mirrors the migration's CHECK on `status`.
pub const EXPORT_STATES: [&str; 6] = ["queued", "running", "ready", "failed", "revoked", "expired"];

/// One classified column.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Classification {
    /// The table the column belongs to.
    pub table: String,
    /// The column name.
    pub column: String,
    /// What the data is (`personal`, `secret`, `identifier`, `safe`).
    pub class: String,
    /// What an export does with it when the caller says nothing.
    pub default_action: String,
    /// Why it is classified this way.
    pub notes: String,
}

impl Classification {
    /// Build a classification row.
    pub fn new(
        table: impl Into<String>,
        column: impl Into<String>,
        class: impl Into<String>,
        default_action: impl Into<String>,
    ) -> Self {
        Self {
            table: table.into(),
            column: column.into(),
            class: class.into(),
            default_action: default_action.into(),
            notes: String::new(),
        }
    }

    /// Attach the reviewer's reason.
    pub fn with_notes(mut self, notes: impl Into<String>) -> Self {
        self.notes = notes.into();
        self
    }
}

/// The classification map, indexed for the builder's lookup.
#[derive(Debug, Clone, Default)]
pub struct ClassificationMap {
    by_column: BTreeMap<(String, String), Classification>,
}

impl ClassificationMap {
    /// An empty map — which fails every export, which is the correct starting state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from rows, last write per `(table, column)` winning.
    ///
    /// Last-write-wins rather than first-write-wins because the caller is reading them out of a
    /// database whose primary key already made that choice; re-deciding it here would be a second
    /// answer to "which row won".
    pub fn from_rows(rows: impl IntoIterator<Item = Classification>) -> Self {
        let mut map = Self::new();
        for row in rows {
            map.insert(row);
        }
        map
    }

    /// Add or replace one column's classification.
    pub fn insert(&mut self, row: Classification) {
        self.by_column
            .insert((row.table.clone(), row.column.clone()), row);
    }

    /// The row for one column, if it is classified.
    pub fn get(&self, table: &str, column: &str) -> Option<&Classification> {
        self.by_column.get(&(table.to_string(), column.to_string()))
    }

    /// How many columns are classified.
    pub fn len(&self) -> usize {
        self.by_column.len()
    }

    /// Whether the map classifies nothing at all.
    pub fn is_empty(&self) -> bool {
        self.by_column.is_empty()
    }

    /// Every row, in `(table, column)` order so a rendered list is stable.
    pub fn rows(&self) -> Vec<&Classification> {
        self.by_column.values().collect()
    }
}

/// Why an export cannot run.
///
/// Every variant names the *columns*, not the table, because the operator's next action is to
/// classify specific columns and a message that only said "`users` is unclassified" would be
/// answered by classifying the wrong ones.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClassificationError {
    /// Some selected column has no row in the map.
    #[error(
        "{count} selected column(s) have no classification, so the export is refused: {columns}. \
         Classify them first — an unclassified column is never exported."
    )]
    Unclassified {
        /// How many columns are missing.
        count: usize,
        /// The missing `table.column` pairs, comma-separated.
        columns: String,
    },
    /// The caller asked for an action a column's class does not permit.
    #[error("{table}.{column} is `{class}`, so it cannot be `{requested}`: {reason}")]
    Refused {
        /// The table.
        table: String,
        /// The column.
        column: String,
        /// Its class.
        class: String,
        /// The action that was asked for.
        requested: String,
        /// Why that pair is refused.
        reason: String,
    },
    /// The request names an action that is not in the vocabulary at all.
    #[error("{action:?} is not an export action: use one of {}", ACTIONS.join(", "))]
    UnknownAction {
        /// What the caller sent.
        action: String,
    },
    /// The request asked for no tables, or only empty names.
    #[error("an export must name at least one table")]
    NoTables,
    /// A requested table does not exist in this installation.
    #[error("no table named {name:?} in this installation")]
    UnknownTable {
        /// What the caller sent.
        name: String,
    },
}

/// One table's columns as the database reports them today.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TableColumns {
    /// The table name.
    pub table: String,
    /// Its columns, in ordinal order.
    pub columns: Vec<String>,
}

/// One column's decided treatment in a planned export.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ColumnPlan {
    /// The table.
    pub table: String,
    /// The column.
    pub column: String,
    /// Its class.
    pub class: String,
    /// The decided action.
    pub action: String,
    /// Whether the caller's request overrode the map (recorded for the audit trail).
    pub overridden: bool,
}

/// A whole export, resolved and ready to run.
///
/// Everything here was decided BEFORE any row was read, which is what makes the refusal cheap and
/// the audit trail honest: the plan records what the export was going to do, not what it managed to
/// do after the fact.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlannedExport {
    /// Tables in the order requested, deduplicated.
    pub tables: Vec<String>,
    /// Every column of every requested table, with its decided action.
    pub columns: Vec<ColumnPlan>,
    /// How many columns are removed — the number a reviewer reads first.
    pub removed: usize,
    /// How many are hashed.
    pub hashed: usize,
    /// How many are replaced with synthetic values.
    pub synthetic: usize,
    /// How many are kept as they are.
    pub kept: usize,
}

impl PlannedExport {
    /// True when the plan removes at least one column.
    pub fn removes_anything(&self) -> bool {
        self.removed > 0
    }
}

/// Decide every column's action, or refuse.
///
/// `override_actions` is keyed `table.column` and holds the caller's per-column choice; `None`
/// means "use the map's default".
pub fn plan_export(
    requested_tables: &[String],
    available: &[TableColumns],
    map: &ClassificationMap,
    override_actions: &BTreeMap<String, String>,
) -> std::result::Result<PlannedExport, ClassificationError> {
    // Deduplicate while preserving request order: an export naming the same table twice is a
    // client quirk, and answering it by exporting it twice would double every count below.
    let mut tables: Vec<String> = Vec::new();
    for name in requested_tables {
        let trimmed = name.trim();
        if trimmed.is_empty() || tables.iter().any(|t| t == trimmed) {
            continue;
        }
        tables.push(trimmed.to_string());
    }
    if tables.is_empty() {
        return Err(ClassificationError::NoTables);
    }

    // Resolve every table's CURRENT columns first. The map is keyed by column, so the builder
    // cannot decide anything until it knows what exists — and "what exists" is the thing a new
    // migration changes, which is exactly why the classification has to be re-read per export
    // rather than captured once at startup.
    let mut resolved: Vec<(&TableColumns, Vec<String>)> = Vec::new();
    for table in &tables {
        let found = available
            .iter()
            .find(|t| &t.table == table)
            .ok_or_else(|| ClassificationError::UnknownTable {
                name: table.clone(),
            })?;
        resolved.push((found, found.columns.clone()));
    }

    // Collect the unclassified columns across every table BEFORE deciding anything, so the
    // refusal lists all of them at once. Refusing on the first one would make an operator with a
    // forty-column table click forty times to find out how many they owe.
    let mut missing: Vec<String> = Vec::new();
    for (table, columns) in &resolved {
        for column in columns {
            if map.get(&table.table, column).is_none() {
                missing.push(format!("{}.{}", table.table, column));
            }
        }
    }
    if !missing.is_empty() {
        return Err(ClassificationError::Unclassified {
            count: missing.len(),
            columns: missing.join(", "),
        });
    }

    // Every column is classified, so the decision itself cannot fail for want of a row.
    let mut columns = Vec::new();
    for (table, cols) in &resolved {
        for column in cols {
            let row = map
                .get(&table.table, column)
                .expect("every column was checked above; a missing row cannot reach here");
            let key = format!("{}.{}", table.table, column);
            let requested = override_actions.get(&key).map(String::as_str);
            let (action, overridden) =
                resolve_action(row, requested).map_err(|refusal| match refusal {
                    ActionRefusal::Unknown(action) => ClassificationError::UnknownAction { action },
                    ActionRefusal::Forbidden(requested) => ClassificationError::Refused {
                        table: table.table.clone(),
                        column: column.clone(),
                        class: row.class.clone(),
                        requested,
                        reason: refusal_reason(row),
                    },
                })?;
            columns.push(ColumnPlan {
                table: table.table.clone(),
                column: column.clone(),
                class: row.class.clone(),
                action,
                overridden,
            });
        }
    }

    Ok(PlannedExport {
        tables,
        removed: count_action(&columns, "remove"),
        hashed: count_action(&columns, "hash"),
        synthetic: count_action(&columns, "synthetic"),
        kept: count_action(&columns, "keep"),
        columns,
    })
}

/// Why a class cannot take the requested action. Shared by the refusal message and the tests.
fn refusal_reason(row: &Classification) -> String {
    match row.class.as_str() {
        "secret" => {
            "a credential is removed, never exported — a hash of it is still it".to_string()
        }
        "personal" => {
            "personal data may be removed, hashed or replaced, but not kept as it is".to_string()
        }
        "identifier" => {
            "an identifier may be removed, hashed or replaced, but not kept as it is".to_string()
        }
        other => format!("`{other}` columns may take any action"),
    }
}

/// Whether a class may take an action at all.
///
/// One predicate rather than an `if` inside [`resolve_action`], because the rule has to hold for
/// the caller's override AND the map's own default, and two copies of a rule about leaking
/// personal data is one copy too many.
///
/// Three bands, not two, and the middle one is the whole point:
///
/// * `secret` — only `remove`. A hash of a credential is the credential with a dictionary
///   attack in front of it, so `hash` is refused here even though it is "safe" by the general
///   rule below. `synthetic` is refused for the same reason: a synthetic value for a token is
///   not a credential, but the column exists only to hold one and nothing downstream can tell the
///   difference between "replaced" and "leaked with extra steps".
/// * `safe` — anything, `keep` included.
/// * everything else (`personal`, `identifier`, and any class added later) — `remove`, `hash` or
///   `synthetic`, but never `keep`.
fn action_is_permitted(row: &Classification, action: &str) -> bool {
    // `remove` is always permitted: it is the one action that cannot leak, so no class is ever
    // blocked from the safe option.
    if action == "remove" {
        return true;
    }
    match row.class.as_str() {
        "secret" => false,
        "safe" => true,
        _ => action != "keep",
    }
}

/// Decide one column's action, honouring a caller override where the class permits one.
///
/// Returns the action and whether the override won. This is the ONLY place an action is decided,
/// which is what makes the secret rule unskippable: a caller that checks [`is_overridable`]
/// elsewhere still cannot bypass the `secret` branch here.
///
/// Two refusal kinds, deliberately not one. A name outside the vocabulary is the CALLER's
/// mistake and must not be reported as "this column is classified secret, you may not hash it" —
/// that message sends an operator to re-review a column whose classification was fine, when the
/// actual fix is one character in a request body.
fn resolve_action(
    row: &Classification,
    requested: Option<&str>,
) -> std::result::Result<(String, bool), ActionRefusal> {
    let action = match requested {
        Some(action) => {
            if !ACTIONS.contains(&action) {
                return Err(ActionRefusal::Unknown(action.to_string()));
            }
            if !action_is_permitted(row, action) {
                return Err(ActionRefusal::Forbidden(action.to_string()));
            }
            action
        }
        // The MAP's own default is checked too, not just the caller's override. The migration's
        // CHECK blocks `secret` + `keep`, but nothing stopped a hand-written or imported row from
        // saying `personal` + `keep` — and this function is the one every export goes through, so
        // the rule belongs here rather than in the schema alone.
        None if !action_is_permitted(row, row.default_action.as_str()) => {
            return Err(ActionRefusal::Forbidden(row.default_action.clone()));
        }
        None => row.default_action.as_str(),
    };
    // An override that happens to EQUAL the map's default is not an override: the audit trail
    // answers "did a human change this column's treatment?", and recording a review that changed
    // nothing would put a false entry in a trail read during an incident.
    let overridden = requested.is_some_and(|a| a != row.default_action);
    Ok((action.to_string(), overridden))
}

/// Why one column's action could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ActionRefusal {
    /// The action is not in [`ACTIONS`].
    Unknown(String),
    /// The action is real but this column's class does not permit it.
    Forbidden(String),
}

/// Whether a caller may override this class's action at all.
///
/// Present so a screen can disable the control with the reason, NOT as the enforcement point —
/// enforcement is [`resolve_action`], which a client cannot reach past.
pub fn is_overridable(class: &str) -> bool {
    class != "secret"
}

/// The action a class takes when the caller says nothing and the map row was not read.
///
/// Only used by a screen rendering a "what will happen" preview before the map is loaded; the
/// builder always reads the row.
///
/// **An unrecognised class hashes.** The `safe` arm has to be spelled out rather than used as
/// the catch-all, because this function is what a screen shows an operator BEFORE the map has
/// loaded — and a preview that says "keep" for a class nobody has heard of is how a column
/// reaches a support dump. The default has to be the one that cannot leak.
pub fn class_default_action(class: &str) -> &'static str {
    match class {
        "safe" => "keep",
        "secret" => "remove",
        _ => "hash",
    }
}

fn count_action(columns: &[ColumnPlan], action: &str) -> usize {
    columns.iter().filter(|c| c.action == action).count()
}

/// A per-export salt.
///
/// Deliberately not `Clone`+`Debug`-printable in a way that could reach a log, and deliberately not
/// stored: `fingerprint()` is what the audit trail records, and the salt exists only for the life
/// of the export.
pub struct Salt([u8; 32]);

impl std::fmt::Debug for Salt {
    /// Prints the fingerprint, never the bytes: a salt in a log line is a hash-cracking oracle.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Salt(fingerprint={})", self.fingerprint())
    }
}

impl Salt {
    /// Derive a salt deterministically from bytes the caller generated.
    ///
    /// Public constructor rather than a random one so a test can assert consistency across tables
    /// with a salt it chose; production passes 32 bytes from the process's CSPRNG.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// SHA-256 of the salt. Two exports with the same fingerprint used the same salt, which is
    /// what an auditor comparing two dumps is asking.
    pub fn fingerprint(&self) -> String {
        hex(&Sha256::digest(self.0))
    }

    /// The raw salt, for hashing a value. Private on purpose: it leaves through
    /// [`anonymize_value`] and nowhere else.
    fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Replace a value according to its action, consistently within one salt.
///
/// * `remove` — the column is not selected at all, so this returns `None`.
/// * `hash` — `sha256(salt || value)` as lowercase hex. The SAME input under the SAME salt gives
///   the SAME output, in any table, which is what keeps joins working.
/// * `synthetic` — a stable, obviously-synthetic placeholder (`anonymized-<hash prefix>`) so a
///   reader can tell a replaced value from a real one by eye, and a length hint survives.
/// * `keep` — the value untouched.
pub fn anonymize_value<'a>(
    action: &str,
    salt: &Salt,
    column: &str,
    value: Option<&'a str>,
) -> Option<Option<String>> {
    let Some(value) = value else {
        // A NULL stays NULL whatever the action. Turning NULL into a hash of the empty string
        // would invent data that was never there.
        return Some(None);
    };
    match action {
        "remove" => None,
        "hash" => Some(Some(hash_value(salt, column, value))),
        "synthetic" => {
            let digest = hash_digest(salt, column, value);
            Some(Some(format!(
                "anonymized-{}",
                hex(&digest)[..12].to_string()
            )))
        }
        _ => Some(Some(value.to_string())),
    }
}

/// The digest behind one anonymised value.
///
/// A named function because a chained `Sha256::new().update(..)` does not compile: `Sha256::new()`
/// returns a concrete hasher, but writing it as a single expression makes the temporary's type
/// ambiguous at the `.update` call. Building it in a `let` and returning the finished digest also
/// keeps the salt and the domain separator in ONE place, which is the property the cross-table
/// join test depends on.
fn hash_digest(salt: &Salt, column: &str, value: &str) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(salt.bytes().as_slice());
    // \x1f (unit separator) between parts: a value that happens to contain the column name cannot
    // be shifted across the boundary, so `("ab", "c")` and `("a", "bc")` cannot collide.
    hasher.update(b"\x1f");
    hasher.update(column.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(value.as_bytes());
    hasher.finalize().to_vec()
}

/// One hashed value as lowercase hex.
fn hash_value(salt: &Salt, column: &str, value: &str) -> String {
    hex(&hash_digest(salt, column, value))
}

/// Hash a value with the export's salt — the shape the builder applies to a column.
///
/// Split out because the DOMAIN SEPARATOR matters: hashing `salt || column || value` means a value
/// that appears in two columns under different actions cannot produce the same digest, so a
/// `keep`-ed e-mail and a `hash`-ed copy of it are visibly different rather than silently equal.
pub fn hash_with_salt(salt: &Salt, column: &str, value: &str) -> String {
    anonymize_value("hash", salt, column, Some(value))
        .expect("\"hash\" is never the remove action")
        .expect("a Some value is never None")
}

/// The watermark stamped into a produced file.
pub fn watermark(reason: &str, fingerprint: &str, created_at: &str) -> String {
    format!(
        "Omnion anonymised export · reason: {reason} · salt {fingerprint} · produced {created_at} · \
         classified columns removed, hashed or replaced — this file is single-use and expires."
    )
}

/// Lowercase hex.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tables() -> Vec<TableColumns> {
        vec![
            TableColumns {
                table: "users".into(),
                columns: vec!["id".into(), "email".into(), "full_name".into()],
            },
            TableColumns {
                table: "orders".into(),
                columns: vec!["id".into(), "user_id".into(), "total".into()],
            },
        ]
    }

    fn full_map() -> ClassificationMap {
        ClassificationMap::from_rows([
            Classification::new("users", "id", "identifier", "hash"),
            Classification::new("users", "email", "personal", "hash"),
            Classification::new("users", "full_name", "personal", "remove"),
            Classification::new("orders", "id", "identifier", "hash"),
            Classification::new("orders", "user_id", "identifier", "hash"),
            Classification::new("orders", "total", "safe", "keep"),
        ])
    }

    #[test]
    fn an_unclassified_column_refuses_and_names_the_columns() {
        // The map covers `users` but not `orders.total` — the shape a migration that adds one
        // column leaves behind.
        let map = ClassificationMap::from_rows([
            Classification::new("users", "id", "identifier", "hash"),
            Classification::new("users", "email", "personal", "hash"),
            Classification::new("users", "full_name", "personal", "remove"),
        ]);
        let err = plan_export(
            &["users".into(), "orders".into()],
            &tables(),
            &map,
            &BTreeMap::new(),
        )
        .expect_err("orders.total has no row");
        match err {
            ClassificationError::Unclassified { count, columns } => {
                assert_eq!(count, 3, "all three orders columns are missing");
                assert_eq!(
                    columns, "orders.id, orders.user_id, orders.total",
                    "the refusal names the columns, not just the table — the operator's next \
                     action is to classify columns"
                );
            }
            other => panic!("expected an unclassified refusal, got {other}"),
        }
    }

    #[test]
    fn a_single_column_short_of_a_full_map_blocks_the_whole_export() {
        let mut map = full_map();
        map.by_column.remove(&("orders".into(), "total".into()));
        let err = plan_export(&["orders".into()], &tables(), &map, &BTreeMap::new())
            .expect_err("one missing column is enough");
        assert!(matches!(
            err,
            ClassificationError::Unclassified { count: 1, .. }
        ));
    }

    #[test]
    fn every_missing_column_is_listed_at_once_not_one_per_click() {
        let err = plan_export(
            &["users".into(), "orders".into()],
            &tables(),
            &ClassificationMap::new(),
            &BTreeMap::new(),
        )
        .expect_err("an empty map classifies nothing");
        match err {
            ClassificationError::Unclassified { count, columns } => {
                assert_eq!(count, 6, "both tables, every column, in one refusal");
                assert!(columns.contains("users.email") && columns.contains("orders.total"));
            }
            other => panic!("expected one refusal listing everything, got {other}"),
        }
    }

    #[test]
    fn a_secret_column_is_removed_and_the_caller_cannot_override_it() {
        let map = ClassificationMap::from_rows([
            Classification::new("api_keys", "id", "identifier", "hash"),
            Classification::new("api_keys", "token_hash", "secret", "remove"),
        ]);
        let available = vec![TableColumns {
            table: "api_keys".into(),
            columns: vec!["id".into(), "token_hash".into()],
        }];

        // The default is safe.
        let plan = plan_export(&["api_keys".into()], &available, &map, &BTreeMap::new())
            .expect("the default removes the secret");
        assert_eq!(plan.removed, 1);

        // Asking for `hash` — the "safer looking" option that is actually still a credential.
        let mut override_actions = BTreeMap::new();
        override_actions.insert("api_keys.token_hash".to_string(), "hash".to_string());
        let err = plan_export(&["api_keys".into()], &available, &map, &override_actions)
            .expect_err("a credential cannot be exported under any action but remove");
        match err {
            ClassificationError::Refused {
                column,
                class,
                requested,
                ..
            } => {
                assert_eq!(column, "token_hash");
                assert_eq!(class, "secret");
                assert_eq!(requested, "hash");
            }
            other => panic!("expected a refused override, got {other}"),
        }

        // `keep` too — the override a careless UI would offer.
        let mut override_actions = BTreeMap::new();
        override_actions.insert("api_keys.token_hash".to_string(), "keep".to_string());
        assert!(matches!(
            plan_export(&["api_keys".into()], &available, &map, &override_actions),
            Err(ClassificationError::Refused { .. })
        ));

        // And the screen can be told not to offer the control at all.
        assert!(!is_overridable("secret"));
        assert!(is_overridable("personal"));
    }

    #[test]
    fn a_personal_column_may_be_overridden_but_not_kept() {
        let map = ClassificationMap::from_rows([Classification::new(
            "users", "email", "personal", "hash",
        )]);
        let available = vec![TableColumns {
            table: "users".into(),
            columns: vec!["email".into()],
        }];
        // Only the two actions that DIFFER from the map's default count as an override. Asking for
        // `hash` on a column already classified `hash` is the map being obeyed, not a human
        // changing it — and the audit trail is read during an incident, so an entry that claims a
        // review that changed nothing is worse than a missing one.
        for action in ["remove", "hash", "synthetic"] {
            let mut override_actions = BTreeMap::new();
            override_actions.insert("users.email".to_string(), action.to_string());
            let plan = plan_export(&["users".into()], &available, &map, &override_actions)
                .unwrap_or_else(|e| panic!("{action} is a legitimate action for personal data"));
            assert_eq!(plan.columns[0].action, action);
            assert_eq!(
                plan.columns[0].overridden,
                action != "hash",
                "{action} against a `hash` default: only a CHANGE is an override"
            );
        }
        let mut override_actions = BTreeMap::new();
        override_actions.insert("users.email".to_string(), "keep".to_string());
        assert!(matches!(
            plan_export(&["users".into()], &available, &map, &override_actions),
            Err(ClassificationError::Refused { .. })
        ));
    }

    #[test]
    fn an_action_outside_the_vocabulary_is_refused_by_name() {
        let map = ClassificationMap::from_rows([Classification::new(
            "users", "email", "personal", "hash",
        )]);
        let available = vec![TableColumns {
            table: "users".into(),
            columns: vec!["email".into()],
        }];
        let mut override_actions = BTreeMap::new();
        override_actions.insert("users.email".to_string(), "truncate".to_string());
        let err = plan_export(&["users".into()], &available, &map, &override_actions)
            .expect_err("truncate is not an action");
        assert!(matches!(err, ClassificationError::UnknownAction { .. }));
    }

    #[test]
    fn a_plan_counts_what_it_decided_and_counts_each_column_once() {
        let plan = plan_export(
            &["users".into(), "orders".into(), "users".into()],
            &tables(),
            &full_map(),
            &BTreeMap::new(),
        )
        .expect("the map is complete");
        assert_eq!(
            plan.tables,
            vec!["users", "orders"],
            "a repeated table is one table"
        );
        assert_eq!(
            plan.columns.len(),
            6,
            "six columns, not eight: the repeat is deduplicated"
        );
        assert_eq!(plan.removed, 1);
        assert_eq!(plan.hashed, 4);
        assert_eq!(plan.kept, 1);
        assert_eq!(
            plan.removed + plan.hashed + plan.kept + plan.synthetic,
            plan.columns.len()
        );
    }

    #[test]
    fn an_empty_request_names_no_tables() {
        assert!(matches!(
            plan_export(&[], &tables(), &full_map(), &BTreeMap::new()),
            Err(ClassificationError::NoTables)
        ));
        assert!(matches!(
            plan_export(&["  ".into()], &tables(), &full_map(), &BTreeMap::new()),
            Err(ClassificationError::NoTables)
        ));
    }

    #[test]
    fn a_table_that_does_not_exist_is_refused_rather_than_exported_as_empty() {
        let err = plan_export(
            &["not_a_table".into()],
            &tables(),
            &full_map(),
            &BTreeMap::new(),
        )
        .expect_err("a typo in a table name must not produce an empty export");
        assert!(matches!(err, ClassificationError::UnknownTable { .. }));
    }

    #[test]
    fn the_same_value_hashes_the_same_way_across_tables_so_joins_survive() {
        let salt = Salt::from_bytes([7u8; 32]);
        let from_users = hash_with_salt(&salt, "users.id", "42");
        let from_orders = hash_with_salt(&salt, "users.id", "42");
        assert_eq!(
            from_users, from_orders,
            "the join is the requirement: orders.user_id must equal users.id after hashing"
        );
        assert_ne!(
            from_users,
            hash_with_salt(&salt, "orders.user_id", "42"),
            "the column is part of the digest, so a value in two columns under two actions cannot \
             silently compare equal"
        );
    }

    #[test]
    fn two_exports_with_different_salt_hash_the_same_value_differently() {
        let one = hash_with_salt(&Salt::from_bytes([1u8; 32]), "users.id", "42");
        let two = hash_with_salt(&Salt::from_bytes([2u8; 32]), "users.id", "42");
        assert_ne!(
            one, two,
            "a shared salt would let a vendor join one support ticket to the next"
        );
    }

    #[test]
    fn a_null_stays_null_under_every_action() {
        let salt = Salt::from_bytes([3u8; 32]);
        for action in ["hash", "synthetic", "keep"] {
            assert_eq!(
                anonymize_value(action, &salt, "users.email", None),
                Some(None),
                "{action} must not invent a value for a NULL"
            );
        }
        assert_eq!(
            anonymize_value("remove", &salt, "users.email", None),
            Some(None)
        );
    }

    #[test]
    fn remove_yields_no_column_at_all_and_synthetic_is_visibly_synthetic() {
        let salt = Salt::from_bytes([4u8; 32]);
        assert_eq!(
            anonymize_value("remove", &salt, "users.token", Some("abc")),
            None,
            "removed means the column is not in the output, not that it is blank"
        );
        let synthetic = anonymize_value("synthetic", &salt, "users.email", Some("a@b.com"))
            .expect("synthetic is not remove")
            .expect("the value was present");
        assert!(synthetic.starts_with("anonymized-"), "{synthetic}");
        assert!(
            !synthetic.contains("@"),
            "a synthetic value must not look like the real one — a reader eyeballing the dump \
             needs to see which columns were replaced"
        );
    }

    #[test]
    fn the_salt_never_prints_its_bytes() {
        let rendered = format!("{:?}", Salt::from_bytes([9u8; 32]));
        assert!(
            !rendered.contains("0909"),
            "a salt in a log line is a hash-cracking oracle: {rendered}"
        );
        assert!(rendered.contains(&Salt::from_bytes([9u8; 32]).fingerprint()));
    }

    #[test]
    fn the_class_defaults_are_the_conservative_ones() {
        // Anything not explicitly safe is treated as something to change, never to keep. This is
        // what a screen renders before the map is loaded, so its answer has to be the safe one.
        assert_eq!(class_default_action("secret"), "remove");
        assert_eq!(class_default_action("personal"), "hash");
        assert_eq!(class_default_action("identifier"), "hash");
        assert_eq!(class_default_action("safe"), "keep");
        assert_eq!(class_default_action("whatever-new"), "hash");
    }
}
