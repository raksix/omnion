//! Folder and file grants: the narrowing layer over the permission catalogue (REQ-010, slice 4).
//!
//! A permission says *may this account touch the library*. A grant says *not this one, not
//! here* — and that asymmetry is the whole design, because the alternative is a second source
//! of truth beside the catalogue that anybody can hand capabilities out of, and the first thing
//! built on such a layer is a "grant a contractor read on the whole library" button that
//! quietly bypasses IAM.
//!
//! Every rule below follows from that, and each is a place the obvious implementation is wrong:
//!
//! * **Deny beats allow, at any depth.** Not "the nearest node wins" — a root allow is what an
//!   organization hands everybody without thinking, so a file deny overruled by it is a deny
//!   that does not deny. It is also the only ordering where a mistake in *either* direction
//!   fails closed.
//! * **Silence is the catalogue's answer.** A subject no row names keeps whatever the
//!   permission catalogue said, so [`resolve`] never invents a default. Returning "allow" from
//!   an empty table would turn a missing grant into a grant.
//! * **An allow may only grant, never re-grant.** The answer is "the union of the allows,
//!   minus the union of the denies" — not a fold in list order. A fold would let a file allow
//!   recorded after a folder deny undo it, and would let a root allow re-open a file whose own
//!   grant says no.
//! * **The bits are the narrow set** — read, write, delete, share — not the permission keys.
//!   `share` is separate because it is the capability that hands bytes to somebody who never
//!   signs in.

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::Result;
use crate::folders::Folder;

/// The three kinds of subject a grant may name (docs/07-IAM.md §8, §9, §14).
///
/// A string rather than an enum, because the row is a bare uuid with no foreign key: the three
/// subject tables belong to IAM, and a fourth reference would make a grant on a deleted group
/// un-deletable. Resolution treats an id that names nothing as inert, so a stale row can deny
/// nobody but can never grant anybody.
pub const SUBJECT_KINDS: [&str; 3] = ["user", "group", "role"];

/// What a grant's capability bits mean when they are being read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// May open the file, list its folder, download it.
    pub read: bool,
    /// May rename it, move it, edit its metadata, upload a new version.
    pub write: bool,
    /// May move it to the trash.
    pub delete: bool,
    /// May create a public share link over it.
    pub share: bool,
}

impl Default for Capabilities {
    fn default() -> Self {
        Capabilities::NONE
    }
}

impl Capabilities {
    /// Every capability: what a `deny` with all four bits removes.
    pub const ALL: Capabilities = Capabilities {
        read: true,
        write: true,
        delete: true,
        share: true,
    };

    /// Nothing at all: what an `allow` row with every bit `false` grants, and what the
    /// database refuses for a `deny` (a deny with no bit is a typo that would refuse nothing).
    pub const NONE: Capabilities = Capabilities {
        read: false,
        write: false,
        delete: false,
        share: false,
    };

    /// The bit set read out of a database row.
    #[must_use]
    pub fn from_row(can_read: bool, can_write: bool, can_delete: bool, can_share: bool) -> Self {
        Capabilities {
            read: can_read,
            write: can_write,
            delete: can_delete,
            share: can_share,
        }
    }

    /// The four bits as a row-ready tuple, in column order.
    #[must_use]
    pub fn as_row(self) -> (bool, bool, bool, bool) {
        (self.read, self.write, self.delete, self.share)
    }

    /// The bits this set has and `other` does not — what a narrower node took away.
    #[must_use]
    pub fn minus(self, other: Capabilities) -> Capabilities {
        Capabilities {
            read: self.read && !other.read,
            write: self.write && !other.write,
            delete: self.delete && !other.delete,
            share: self.share && !other.share,
        }
    }

    /// Whether any bit differs, so a caller can skip a write that would change nothing.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self == Capabilities::NONE
    }

    /// The names of the capabilities this set carries, for the summary line on the screen.
    #[must_use]
    pub fn labels(self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.read {
            out.push("read");
        }
        if self.write {
            out.push("write");
        }
        if self.delete {
            out.push("delete");
        }
        if self.share {
            out.push("share");
        }
        out
    }
}

/// Which node a grant attaches to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantTarget {
    /// A folder, and every file beneath it.
    Folder(Uuid),
    /// One file, whatever folder it sits in.
    File(Uuid),
}

impl GrantTarget {
    /// The `(folder_id, media_id)` pair the table's XOR constraint expects.
    #[must_use]
    pub fn columns(self) -> (Option<Uuid>, Option<Uuid>) {
        match self {
            GrantTarget::Folder(id) => (Some(id), None),
            GrantTarget::File(id) => (None, Some(id)),
        }
    }
}

/// What a grant says.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Grant {
    /// The row's own id.
    pub id: Uuid,
    /// The folder it attaches to.
    pub folder_id: Option<Uuid>,
    /// The file it attaches to, when it is a file grant.
    pub media_id: Option<Uuid>,
    /// Which kind of subject it names.
    pub subject_kind: String,
    /// The subject itself.
    pub subject_id: Uuid,
    /// Read bit.
    pub can_read: bool,
    /// Write bit.
    pub can_write: bool,
    /// Delete bit.
    pub can_delete: bool,
    /// Share bit.
    pub can_share: bool,
    /// `allow` or `deny`.
    pub effect: String,
    /// Who wrote it.
    pub created_by: Option<Uuid>,
    /// When it was written.
    pub created_at: OffsetDateTime,
}

impl Grant {
    /// The capability bits, in the same order the columns hold them.
    #[must_use]
    pub fn capabilities(&self) -> Capabilities {
        Capabilities::from_row(
            self.can_read,
            self.can_write,
            self.can_delete,
            self.can_share,
        )
    }

    /// Whether this row removes capabilities.
    #[must_use]
    pub fn is_deny(&self) -> bool {
        self.effect == "deny"
    }

    /// Where the row sits, for the screen and the audit entry.
    #[must_use]
    pub fn target(&self) -> Option<GrantTarget> {
        match (self.folder_id, self.media_id) {
            (Some(id), None) => Some(GrantTarget::Folder(id)),
            (None, Some(id)) => Some(GrantTarget::File(id)),
            // The XOR constraint makes this unreachable; a row that somehow is both is not
            // resolved as a grant at all rather than resolved as a folder by accident.
            _ => None,
        }
    }
}

/// What a grant is written with.
#[derive(Debug, Clone)]
pub struct NewGrant {
    /// The node.
    pub target: GrantTarget,
    /// Which kind of subject.
    pub subject_kind: String,
    /// The subject.
    pub subject_id: Uuid,
    /// The bits.
    pub capabilities: Capabilities,
    /// `allow` or `deny`; an empty string is read as `allow`.
    pub effect: String,
    /// Who is writing it.
    pub created_by: Option<Uuid>,
}

/// The columns of a grant row, in the order [`Grant`] reads them.
const GRANT_COLUMNS: &str = "id, folder_id, media_id, subject_kind, subject_id, can_read, \
     can_write, can_delete, can_share, effect, created_by, created_at";

/// The `(node, subject)` expression the unique index is built on.
///
/// Written here rather than repeated in the query so the index and the `on conflict` clause
/// cannot drift: a conflict target that does not match the index it names is a statement
/// PostgreSQL refuses, and the two being written out separately is exactly how that happens.
///
/// Public so the API crate's test can pin that this string and the migration's index agree —
/// the two live in different files and different languages, which is why the drift is real.
pub const GRANT_CONFLICT: &str = "( \
     coalesce(folder_id, '00000000-0000-0000-0000-000000000000'::uuid), \
     coalesce(media_id, '00000000-0000-0000-0000-000000000000'::uuid), \
     subject_kind, subject_id \
 )";

/// Every grant on one node, oldest first.
///
/// A whole node rather than a filtered one: the resolution walk needs a deny for the subject
/// the caller *is*, and a "grants for this subject" read would have to know the subject before
/// it could find a reason to refuse — which is fine for a decision about one person and useless
/// for the screen, which has to show the other rows too.
pub async fn list_grants(pool: &PgPool, target: GrantTarget) -> Result<Vec<Grant>> {
    let (folder_id, media_id) = target.columns();
    let query = format!(
        "select {GRANT_COLUMNS} from media_grants \
         where folder_id is not distinct from $1 and media_id is not distinct from $2 \
         order by created_at, id"
    );
    sqlx::query_as::<_, Grant>(&query)
        .bind(folder_id)
        .bind(media_id)
        .fetch_all(pool)
        .await
        .map_err(Into::into)
}

/// Write a grant, or rewrite the one already there for the same (node, subject).
///
/// An upsert rather than an insert that fails, because "add this person" pressed twice is a
/// person who ends up with two rows that disagree about what they may do — and the disagreement
/// is invisible until somebody is refused for a reason nobody can find.
pub async fn put_grant(pool: &PgPool, new: &NewGrant) -> Result<Grant> {
    let (folder_id, media_id) = new.target.columns();
    let effect = if new.effect.is_empty() {
        "allow"
    } else {
        new.effect.as_str()
    };
    let (can_read, can_write, can_delete, can_share) = new.capabilities.as_row();
    let query = format!(
        "insert into media_grants ( \
             folder_id, media_id, subject_kind, subject_id, \
             can_read, can_write, can_delete, can_share, effect, created_by \
         ) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
         on conflict {GRANT_CONFLICT} do update set \
             can_read = excluded.can_read, \
             can_write = excluded.can_write, \
             can_delete = excluded.can_delete, \
             can_share = excluded.can_share, \
             effect = excluded.effect, \
             created_by = excluded.created_by, \
             updated_at = now() \
         returning {GRANT_COLUMNS}"
    );
    sqlx::query_as::<_, Grant>(&query)
        .bind(folder_id)
        .bind(media_id)
        .bind(&new.subject_kind)
        .bind(new.subject_id)
        .bind(can_read)
        .bind(can_write)
        .bind(can_delete)
        .bind(can_share)
        .bind(effect)
        .bind(new.created_by)
        .fetch_one(pool)
        .await
        .map_err(Into::into)
}

/// Remove one grant by id, refusing a row that is not there.
///
/// A boolean rather than a count: the screen has to be able to say "that grant is gone", and
/// cannot say it from a silent zero.
pub async fn delete_grant(pool: &PgPool, id: Uuid) -> Result<bool> {
    let removed = sqlx::query("delete from media_grants where id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(removed.rows_affected() > 0)
}

/// The ids of a user's groups, so a grant given to a team reaches its members.
///
/// Read per request rather than cached: a person added to a team must pick up the team's grant
/// on the next request, which is the same reason membership resolution never caches in
/// `crates/permissions` either.
pub async fn group_ids_of(pool: &PgPool, user_id: Uuid) -> Result<Vec<Uuid>> {
    sqlx::query_scalar("select group_id from group_members where user_id = $1")
        .bind(user_id)
        .fetch_all(pool)
        .await
        .map_err(Into::into)
}

// ---------------------------------------------------------------------------------------------
// Chain assembly
// ---------------------------------------------------------------------------------------------

/// One folder on a file's chain, with the grants recorded against it.
#[derive(Debug, Clone)]
pub struct ChainNode {
    /// The folder.
    pub folder: Folder,
    /// Its grants.
    pub grants: Vec<Grant>,
}

/// Everything resolution reads: the file's own grants and its folder chain.
///
/// The folder chain is **nearest ancestor first**, and the order is the reason this is a
/// documented struct rather than a `Vec` a call site assembles: root-first would read as
/// "nearest wins", which is the ordering that does not deny.
#[derive(Debug, Clone, Default)]
pub struct Chain {
    /// Grants recorded against the file itself. A deny here is final.
    pub file: Vec<Grant>,
    /// The file's folder, then its parent, then its grandparent, to the root.
    pub nodes: Vec<ChainNode>,
}

/// The deepest a chain is walked before it is declared corrupt.
///
/// The folder move refuses a cycle, so this cannot be reached by an honest tree; it is here so
/// a corrupt `parent_id` reads as "a folder row points at itself" rather than as a request
/// that hangs until the statement timeout and looks like a slow page.
pub const MAX_CHAIN_DEPTH: usize = 64;

/// Read a file's whole grant chain from the database.
///
/// Two statements and one folder read per level: the chain is two or three deep in practice, so
/// this is cheap, and it is one code path rather than one per call site — the two would drift,
/// and a chain built one level by one caller and two by another is a deny that applies on one
/// screen and not another.
pub async fn load_chain(pool: &PgPool, media_id: Uuid, folder_id: Option<Uuid>) -> Result<Chain> {
    let file = list_grants(pool, GrantTarget::File(media_id)).await?;
    let mut nodes = Vec::new();
    let mut cursor = folder_id;
    let mut hops = 0usize;
    while let Some(id) = cursor {
        if hops >= MAX_CHAIN_DEPTH {
            break;
        }
        hops += 1;
        let Some(folder) = crate::folder_store::find_folder(pool, id).await? else {
            break;
        };
        let grants = list_grants(pool, GrantTarget::Folder(folder.id)).await?;
        cursor = folder.parent_id;
        nodes.push(ChainNode { folder, grants });
    }
    Ok(Chain { file, nodes })
}

// ---------------------------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------------------------

/// What resolution concluded for one subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// The capabilities the subject has *after* every grant on the chain.
    pub effective: Capabilities,
    /// The grants that were read, in the order they were read.
    pub applied: Vec<Uuid>,
    /// A sentence saying where the answer came from, for the audit entry and the screen.
    pub reason: String,
    /// Whether any grant named this subject at all.
    ///
    /// The caller needs this separately from `applied.is_empty()` only in one case — a
    /// subject named *solely* by a grant whose effect is deny still has `applied` filled — so
    /// it is a named question rather than something each caller re-derives differently.
    pub touched: bool,
}

impl Decision {
    /// A decision that touched nothing, so the caller keeps the catalogue's answer.
    #[must_use]
    pub fn untouched() -> Self {
        Decision {
            effective: Capabilities::NONE,
            applied: Vec::new(),
            reason: "no grant names this subject on this file's chain".to_owned(),
            touched: false,
        }
    }
}

/// Where on a chain a grant was found.
///
/// Only used for the sentence, but the sentence is what makes a denial debuggable — "deny
/// wins" without a place to look is folklore, and the operator who cannot find the row that
/// refused a colleague stops trusting the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Depth {
    /// The file itself.
    File,
    /// The file's own folder.
    OwnFolder,
    /// Somewhere further up.
    Ancestor,
}

impl Depth {
    fn label(self) -> &'static str {
        match self {
            Depth::File => "the file",
            Depth::OwnFolder => "its folder",
            Depth::Ancestor => "an ancestor folder",
        }
    }
}

/// Resolve one subject against a file's chain.
///
/// Four steps, and the order *is* the rule:
///
/// 1. **Collect** every grant naming the subject, matched by identity or by group membership
///    — a grant given to a team is a grant to its members, and the team table is the only place
///    that is knowable.
/// 2. **A `deny` anywhere wins**, at any depth, and its bits are subtracted. This is not a fold
///    in list order: a file allow recorded *after* a folder deny must not undo it.
/// 3. **An `allow` contributes** its bits to a separate set, and the answer is
///    `granted − denied` — so a root allow cannot reach past a deny on any folder below it, and
///    a subject named *only* by denies gets nothing rather than "everything except what was
///    denied".
/// 4. **A chain that names the subject not at all returns [`Decision::untouched`]**, and the
///    caller keeps whatever the permission catalogue said.
pub fn resolve(
    chain: &Chain,
    subject_kind: &str,
    subject_id: Uuid,
    group_ids: &[Uuid],
) -> Decision {
    let named = |grant: &Grant| {
        (grant.subject_kind == subject_kind && grant.subject_id == subject_id)
            || (grant.subject_kind == "group" && group_ids.contains(&grant.subject_id))
    };

    let mut collected: Vec<(&Grant, Depth)> = Vec::new();
    for grant in &chain.file {
        if named(grant) {
            collected.push((grant, Depth::File));
        }
    }
    for (index, node) in chain.nodes.iter().enumerate() {
        let depth = if index == 0 {
            Depth::OwnFolder
        } else {
            Depth::Ancestor
        };
        for grant in &node.grants {
            if named(grant) {
                collected.push((grant, depth));
            }
        }
    }

    if collected.is_empty() {
        return Decision::untouched();
    }

    let mut denied = Capabilities::NONE;
    let mut granted = Capabilities::NONE;
    let mut applied = Vec::new();
    let mut denied_at: Vec<&'static str> = Vec::new();
    let mut allow_count = 0usize;

    for (grant, depth) in &collected {
        applied.push(grant.id);
        let bits = grant.capabilities();
        if grant.is_deny() {
            // A duplicate bit is recorded once in the sentence but subtracted twice, which is
            // harmless — `x & !x` is idempotent — while the sentence would otherwise read
            // "the file, and the file".
            denied.read |= bits.read;
            denied.write |= bits.write;
            denied.delete |= bits.delete;
            denied.share |= bits.share;
            if !bits.is_empty() && !denied_at.contains(&depth.label()) {
                denied_at.push(depth.label());
            }
        } else {
            allow_count += 1;
            granted.read |= bits.read;
            granted.write |= bits.write;
            granted.delete |= bits.delete;
            granted.share |= bits.share;
        }
    }

    if allow_count == 0 {
        // Named only by denies. Returning "everything except the denied bits" here would hand
        // a subject that was explicitly refused the three capabilities nobody denied, and the
        // only reason it looks reasonable is that an allow row is usually also there.
        return Decision {
            effective: Capabilities::NONE,
            applied,
            reason: format!(
                "a deny on {} removes every capability this subject had here",
                phrase(&denied_at)
            ),
            touched: true,
        };
    }

    let effective = granted.minus(denied);
    let reason = if denied_at.is_empty() {
        format!(
            "{allow_count} allow{} naming this subject on the chain",
            if allow_count == 1 { "" } else { "s" }
        )
    } else {
        format!(
            "a deny on {} overrides the allow{} on this chain",
            phrase(&denied_at),
            if allow_count == 1 { "" } else { "s" }
        )
    };

    Decision {
        effective,
        applied,
        reason,
        touched: true,
    }
}

/// Join the places a deny was found into one readable phrase.
fn phrase(places: &[&'static str]) -> String {
    match places {
        [] => "this file's chain".to_owned(),
        [one] => (*one).to_owned(),
        [one, rest @ ..] => format!("{one} and {}", phrase(rest)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(effect: &str, read: bool, write: bool, del: bool, share: bool) -> Grant {
        Grant {
            id: Uuid::new_v4(),
            folder_id: None,
            media_id: None,
            subject_kind: "user".to_owned(),
            subject_id: Uuid::nil(),
            can_read: read,
            can_write: write,
            can_delete: del,
            can_share: share,
            effect: effect.to_owned(),
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn folder_node(grants: Vec<Grant>) -> ChainNode {
        ChainNode {
            folder: Folder {
                id: Uuid::new_v4(),
                site_id: Uuid::nil(),
                parent_id: None,
                name: "node".to_owned(),
                path: "/node".to_owned(),
                created_by: None,
                created_at: OffsetDateTime::UNIX_EPOCH,
                // Not `Option`: the column has no default of `null` on this row shape, and a
                // test fixture that guesses the type differently from the model compiles into a
                // test that never ran.
                updated_at: OffsetDateTime::UNIX_EPOCH,
            },
            grants,
        }
    }

    /// A chain with one optional file grant and `depth` folders, nearest first.
    fn chain(file: Option<Grant>, folders: Vec<Grant>) -> Chain {
        Chain {
            file: file.into_iter().collect(),
            nodes: folders.into_iter().map(|g| folder_node(vec![g])).collect(),
        }
    }

    const SUBJECT: Uuid = Uuid::nil();

    #[test]
    fn a_chain_that_names_nobody_keeps_the_catalogues_answer() {
        let decision = resolve(&chain(None, vec![]), "user", SUBJECT, &[]);
        assert_eq!(decision.effective, Capabilities::NONE);
        assert!(!decision.touched);
        assert!(decision.applied.is_empty());
        assert!(decision.reason.contains("no grant names this subject"));
    }

    #[test]
    fn an_allow_on_the_file_is_the_answer() {
        let decision = resolve(
            &chain(Some(grant("allow", true, true, false, false)), vec![]),
            "user",
            SUBJECT,
            &[],
        );
        assert!(decision.effective.read);
        assert!(decision.effective.write);
        assert!(!decision.effective.delete);
    }

    #[test]
    fn a_file_deny_beats_an_inherited_allow_from_every_depth() {
        // The whole point: the root allow is what an organization hands everybody without
        // thinking, so a deny on the file has to survive it at every depth.
        for depth in 1..=5 {
            let ancestors = vec![grant("allow", true, false, false, false); depth];
            let decision = resolve(
                &chain(
                    Some(grant("deny", true, false, false, false)),
                    ancestors,
                ),
                "user",
                SUBJECT,
                &[],
            );
            assert!(
                !decision.effective.read,
                "a file deny must survive an allow {depth} level(s) up"
            );
            assert!(decision.touched);
        }
    }

    #[test]
    fn a_deny_on_an_ancestor_beats_the_own_folders_allow() {
        // Nearest-first chain: the file's own folder allows, the folder above it denies. The
        // "nearer wins" answer would be `read`, and the sentence has to name *where* the deny
        // was found — an operator who cannot point at the row that refused a colleague stops
        // trusting the answer.
        let decision = resolve(
            &chain(
                None,
                vec![
                    grant("allow", true, false, false, false),
                    grant("deny", true, false, false, false),
                ],
            ),
            "user",
            SUBJECT,
            &[],
        );
        assert!(!decision.effective.read, "the deny must win over the nearer allow");
        assert!(
            decision.reason.contains("an ancestor folder"),
            "the sentence must name where the deny was found: {}",
            decision.reason
        );
    }

    #[test]
    fn a_deny_on_the_files_own_folder_is_named_as_such() {
        let decision = resolve(
            &chain(
                Some(grant("allow", true, false, false, false)),
                vec![grant("deny", true, false, false, false)],
            ),
            "user",
            SUBJECT,
            &[],
        );
        assert!(!decision.effective.read);
        assert!(decision.reason.contains("its folder"), "{}", decision.reason);
    }

    #[test]
    fn a_deny_removed_one_bit_does_not_remove_the_others() {
        let decision = resolve(
            &chain(
                Some(grant("allow", true, true, true, true)),
                vec![grant("deny", true, false, false, false)],
            ),
            "user",
            SUBJECT,
            &[],
        );
        assert!(!decision.effective.read);
        assert!(decision.effective.write);
        assert!(decision.effective.delete);
        assert!(decision.effective.share);
    }

    #[test]
    fn a_file_allow_cannot_reopen_what_a_folder_deny_took() {
        let decision = resolve(
            &chain(
                Some(grant("allow", true, false, false, false)),
                vec![grant("deny", true, false, false, false)],
            ),
            "user",
            SUBJECT,
            &[],
        );
        assert!(
            !decision.effective.read,
            "an allow must not union past a deny"
        );
    }

    #[test]
    fn a_group_grant_reaches_a_member_and_nobody_else() {
        let mut member = grant("allow", true, false, false, false);
        member.subject_kind = "group".to_owned();
        member.subject_id = Uuid::from_u128(7);
        let chain = chain(Some(member), vec![]);

        let inside = resolve(&chain, "user", SUBJECT, &[Uuid::from_u128(7)]);
        assert!(inside.effective.read, "a member of the group is covered");

        let outside = resolve(&chain, "user", SUBJECT, &[Uuid::from_u128(8)]);
        assert!(
            !outside.touched,
            "somebody outside the group is named by nothing"
        );
    }

    #[test]
    fn a_group_deny_applies_to_every_member() {
        let mut deny = grant("deny", true, false, false, false);
        deny.subject_kind = "group".to_owned();
        deny.subject_id = Uuid::from_u128(7);
        let decision = resolve(&chain(Some(deny), vec![]), "user", SUBJECT, &[Uuid::from_u128(7)]);
        assert!(!decision.effective.read, "the group's deny covers its members");
    }

    #[test]
    fn a_deny_row_alone_is_never_an_allow_row() {
        let decision = resolve(
            &chain(Some(grant("deny", true, false, false, false)), vec![]),
            "user",
            SUBJECT,
            &[],
        );
        assert_eq!(decision.effective, Capabilities::NONE);
        assert!(decision.touched, "the subject was named, and refused");
        assert!(decision.reason.contains("removes every capability"));
    }

    #[test]
    fn a_deny_that_names_a_different_subject_changes_nothing() {
        let mut other = grant("deny", true, false, false, false);
        other.subject_id = Uuid::from_u128(42);
        let decision = resolve(&chain(Some(other), vec![]), "user", SUBJECT, &[]);
        assert!(!decision.touched);
    }

    #[test]
    fn two_denies_at_the_same_depth_are_named_once() {
        let decision = resolve(
            &chain(
                Some(grant("allow", true, true, false, false)),
                vec![
                    grant("deny", true, false, false, false),
                    grant("deny", true, false, false, false),
                ],
            ),
            "user",
            SUBJECT,
            &[],
        );
        assert!(!decision.effective.read);
        assert!(decision.effective.write);
        assert_eq!(
            decision.reason.matches("its folder").count(),
            1,
            "the sentence should name the place, not the rows: {}",
            decision.reason
        );
    }

    #[test]
    fn a_grant_target_names_exactly_one_node() {
        assert_eq!(
            GrantTarget::Folder(Uuid::nil()).columns(),
            (Some(Uuid::nil()), None)
        );
        assert_eq!(
            GrantTarget::File(Uuid::nil()).columns(),
            (None, Some(Uuid::nil()))
        );
    }

    #[test]
    fn a_row_naming_neither_node_resolves_as_no_target() {
        let mut row = grant("allow", true, false, false, false);
        row.folder_id = Some(Uuid::new_v4());
        row.media_id = Some(Uuid::new_v4());
        assert_eq!(row.target(), None);
    }

    #[test]
    fn minus_reports_what_the_other_set_took() {
        let taken = Capabilities {
            read: true,
            ..Capabilities::NONE
        };
        assert!(!taken.minus(Capabilities::ALL).read);
        assert!(taken.minus(Capabilities::NONE).read);
        assert!(Capabilities::ALL.minus(Capabilities::ALL).is_empty());
    }

    #[test]
    fn labels_name_the_bits_that_are_set() {
        assert_eq!(
            Capabilities::from_row(true, false, true, false).labels(),
            vec!["read", "delete"]
        );
        assert!(Capabilities::NONE.labels().is_empty());
    }
}
