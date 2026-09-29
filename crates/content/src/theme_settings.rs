//! Theme settings: draft, publish, history and the contrast check (REQ-062, slice 2).
//!
//! Slice 1 made a theme a thing a site can *have*. This is the thing a site can *look like*
//! while having it, and the whole module exists because of one awkward fact: a save is not a
//! publish. Every design below is downstream of that.
//!
//! * **Three rows, three states, and they are not collapsible.** A site may have a published
//!   revision, a draft revision, both, or neither. Collapsing them into one mutable
//!   `theme_settings` row is the obvious move and it destroys two things at once: a save
//!   becomes the live site the moment somebody types, and there is no history to restore. So
//!   `theme_settings_revisions` is append-only, `theme_settings_published` is the pointer a
//!   visitor renders from, and `theme_settings_draft` is the pointer the panel edits.
//!
//! * **A restore writes a revision.** `restore_revision` copies revision N into revision N+1
//!   and records where it came from. The alternative — moving the published pointer backwards
//!   — is a history screen listing revisions in an order the site never had, and it makes
//!   "restore twice in a row" delete the first restore.
//!
//! * **The contrast check is computed here, not in the browser.** The customize screen shows a
//!   badge, but a badge a client computes is a badge that disappears on reload, and the
//!   publish guard has to work for `curl` too. [`contrast_report`] is the one function both
//!   the badge and the refusal call, so a warning the operator saw and a warning the server
//!   enforced are the same warning.
//!
//! * **Publishing does not silently discard the draft.** If there is a draft newer than what
//!   is being published, the publish is refused with the number the site is on and the number
//!   that would be lost. Publishing "whatever is in the panel" by accident is how a
//!   half-typed colour ends up on the public site.

use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{ContentError, Result};
use crate::themes::DEFAULT_THEME_KEY;

/// Longest accepted `default_mode`-adjacent free text (a font stack, a header variant name).
pub const MAX_VALUE_LENGTH: usize = 500;

/// The colour modes a site can default to.
pub const MODES: [&str; 3] = ["light", "dark", "system"];

/// Lowest contrast ratio that passes WCAG AA for body text.
pub const AA_BODY: f64 = 4.5;

/// Lowest contrast ratio that passes WCAG AA for large text.
pub const AA_LARGE: f64 = 3.0;

// ---------------------------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------------------------

/// One settings revision, exactly as it was written.
///
/// Derives `Serialize` because the history screen's diff is computed against the *previous*
/// row, and a diff computed in the browser against a field list that happens to match today is
/// a diff that silently stops covering a field the REQ added in slice 3.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct SettingsRevision {
    /// Primary key.
    pub id: Uuid,
    /// Site the revision belongs to.
    pub site_id: Uuid,
    /// `1`, `2`, `3`, … per site.
    pub revision_no: i32,
    /// The theme this revision customizes.
    pub theme_key: String,
    /// Colour tokens, `{ "surface": { "light": "#fff", "dark": "#111" } }`.
    pub tokens: Value,
    /// Typography: base size, scale ratio, font stacks, heading weight.
    pub typography: Value,
    /// Layout: container width, radius, spacing scale.
    pub layout: Value,
    /// Branding: logo, dark logo, favicon.
    pub branding: Value,
    /// Header/footer variant names.
    pub header_footer: Value,
    /// `light`, `dark` or `system`.
    pub default_mode: String,
    /// Who saved it.
    pub created_by: Option<Uuid>,
    /// When.
    pub created_at: OffsetDateTime,
    /// When it went live, if it ever did.
    pub published_at: Option<OffsetDateTime>,
    /// The revision this one restores, if it is itself a restore.
    pub restored_from_id: Option<Uuid>,
}

/// The `SETTINGS_COLUMNS` list, aliased for the self-join that fills a revision's author name.
const REVISION_COLUMNS: &str = "id, site_id, revision_no, theme_key, tokens, typography, \
     layout, branding, header_footer, default_mode, created_by, created_at, published_at, \
     restored_from_id";

/// What the customize screen loads: the draft it is editing, what is live, and what the
/// history holds.
///
/// The three are separate fields and not one "current" object, because the screen has to be
/// able to say all four of: "you are editing a draft that is not live", "you have unsaved
/// changes", "nothing is published yet, so the theme's own defaults are what visitors see",
/// and "this draft is older than what is live" (a restore made the published revision
/// newer). A single `settings` field can express one of those and lies about the rest.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsView {
    /// Site the view is for.
    pub site_id: Uuid,
    /// The theme the site renders with right now.
    pub theme_key: String,
    /// The draft the panel is editing, or `None` for a site that has never saved.
    pub draft: Option<SettingsRevision>,
    /// The live revision, or `None` when nothing has been published.
    pub published: Option<SettingsRevision>,
    /// Newest first, for the history screen.
    pub revisions: Vec<RevisionSummary>,
    /// Contrast findings for the values being edited, computed by the server.
    pub contrast: Vec<ContrastFinding>,
    /// Token names the theme itself declares, so the panel can offer a reset per token.
    pub default_tokens: Value,
}

/// A row in the history list: enough to render, not the whole payload.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct RevisionSummary {
    /// Primary key.
    pub id: Uuid,
    /// The number an author sees.
    pub revision_no: i32,
    /// Theme it customizes.
    pub theme_key: String,
    /// When it was saved.
    pub created_at: OffsetDateTime,
    /// Who saved it.
    pub created_by: Option<Uuid>,
    /// Display name of the author, resolved by the route.
    pub created_by_name: Option<String>,
    /// Whether this is the live revision.
    pub is_published: bool,
    /// Whether it is the draft being edited.
    pub is_draft: bool,
    /// Whether it is itself a restore, and of what.
    pub restored_from_no: Option<i32>,
}

/// One contrast finding: a token pair that is below the AA threshold, or not colour at all.
///
/// `PartialEq` and not `Eq` because `ratio` is a `f64`. It is rounded to two decimals before
/// it is stored here precisely so that the value a caller compares twice is the value it saw
/// on screen, and `Eq` would be a claim the type cannot keep.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContrastFinding {
    /// The token that failed, e.g. `text` on `surface`.
    pub foreground: String,
    /// The token it is read against.
    pub background: String,
    /// Which mode the values came from.
    pub mode: String,
    /// The measured ratio, to two decimals.
    pub ratio: f64,
    /// The threshold that applies: AA for body text, AA-large for large text.
    pub required: f64,
    /// A sentence naming both tokens and the number.
    pub message: String,
}

/// The pairs the platform checks, and how large their text is.
///
/// A hard-coded table rather than a manifest field, because the manifest is data an uploaded
/// package controls and a package that declared "my contrast requirement is zero" would be
/// checking itself. The pairs below are the ones a readable page needs whatever the theme
/// calls its tokens: body on surface, muted text on surface, links on surface, and the two
/// surfaces a card sits on.
const CONTRAST_PAIRS: [(&str, &str, f64); 4] = [
    ("text", "surface", AA_BODY),
    ("textMuted", "surface", AA_BODY),
    ("accent", "surface", AA_BODY),
    ("text", "surfaceRaised", AA_BODY),
];

/// A value the panel is about to save. Not a `SettingsRevision`: a save has no number yet.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct SettingsInput {
    /// Theme these settings belong to. Must be the site's active theme (or a theme the site
    /// may switch to), so a settings row cannot describe a site rendering something else.
    #[serde(default)]
    pub theme_key: String,
    /// Colour tokens.
    #[serde(default)]
    pub tokens: Value,
    /// Typography.
    #[serde(default)]
    pub typography: Value,
    /// Layout.
    #[serde(default)]
    pub layout: Value,
    /// Branding.
    #[serde(default)]
    pub branding: Value,
    /// Header/footer variants.
    #[serde(default)]
    pub header_footer: Value,
    /// `light`, `dark` or `system`.
    #[serde(default)]
    pub default_mode: String,
}

/// What a save, a publish or a restore changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsChange {
    /// A new draft revision was written.
    Draft {
        /// The revision that now holds the draft.
        revision: SettingsRevision,
    },
    /// A revision went live.
    Published {
        /// The revision that is now the live one.
        revision_no: i32,
        /// The revision it replaced, if any.
        previous_revision_no: Option<i32>,
    },
    /// A revision was restored, which wrote a new one.
    Restored {
        /// The new revision carrying the restored content.
        revision: SettingsRevision,
    },
}

// ---------------------------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------------------------

/// The customize screen's whole payload for one site.
pub async fn settings_view(
    pool: &PgPool,
    site_id: Uuid,
    active_theme_key: &str,
    default_tokens: Value,
) -> Result<SettingsView> {
    let (draft, published) = pointers(pool, site_id).await?;
    let revisions = list_revisions(pool, site_id).await?;

    // The contrast report is computed on whatever the panel is EDITING — the draft when there
    // is one, the live revision when there is not, and the theme's own defaults when the site
    // has never saved. Any other choice is a badge that describes a different set of colours
    // than the ones the operator is looking at next to it.
    let subject = draft
        .as_ref()
        .or(published.as_ref())
        .map(|revision| (revision.tokens.clone(), revision.theme_key.clone()));
    let tokens = match &subject {
        Some((tokens, _)) => merge_over(default_tokens.clone(), tokens.clone()),
        None => default_tokens.clone(),
    };
    let key = subject
        .as_ref()
        .map_or_else(|| active_theme_key.to_owned(), |(_, key)| key.clone());

    Ok(SettingsView {
        site_id,
        theme_key: key,
        draft,
        published,
        revisions,
        contrast: contrast_report(&tokens),
        default_tokens,
    })
}

/// The draft and published pointers of a site, or `None` for each.
async fn pointers(
    pool: &PgPool,
    site_id: Uuid,
) -> Result<(Option<SettingsRevision>, Option<SettingsRevision>)> {
    let draft_id: Option<Uuid> =
        sqlx::query_scalar("select revision_id from theme_settings_draft where site_id = $1")
            .bind(site_id)
            .fetch_optional(pool)
            .await?;
    let published_id: Option<Uuid> = sqlx::query_scalar(
        "select revision_id from theme_settings_published where site_id = $1",
    )
    .bind(site_id)
    .fetch_optional(pool)
    .await?;

    Ok((
        read_revision(pool, draft_id).await?,
        read_revision(pool, published_id).await?,
    ))
}

/// One revision by id, or `None` for a site that has none.
async fn read_revision(
    pool: &PgPool,
    id: Option<Uuid>,
) -> Result<Option<SettingsRevision>> {
    let Some(id) = id else { return Ok(None) };
    let revision = sqlx::query_as::<_, SettingsRevision>(&format!(
        "select {REVISION_COLUMNS} from theme_settings_revisions where id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(revision)
}

/// One revision by its number, for the history screen's detail route.
pub async fn revision(
    pool: &PgPool,
    site_id: Uuid,
    revision_no: i32,
) -> Result<SettingsRevision> {
    sqlx::query_as::<_, SettingsRevision>(&format!(
        "select {REVISION_COLUMNS} from theme_settings_revisions \
         where site_id = $1 and revision_no = $2"
    ))
    .bind(site_id)
    .bind(revision_no)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| {
        ContentError::ThemeSettingsRevisionNotFound(revision_no)
    })
}

/// One revision by its id, for a caller that resolved the id itself (the publish guard reads
/// the draft pointer and needs its payload, not its number).
///
/// A row id is a `uuid`, so this cannot be a "not found" the caller shows to a person — the
/// publish route looks the id up from a pointer that a concurrent delete could have removed, so
/// `None` is a real answer rather than a programming error.
pub async fn revision_by_id(pool: &PgPool, id: Uuid) -> Result<Option<SettingsRevision>> {
    read_revision(pool, Some(id)).await
}

/// The history, newest first, with the two pointers resolved.
///
/// A walk with `--test-threads` is not the only reader: an operator opens this screen in the
/// browser while a colleague publishes, and a history that cannot say which row is live is a
/// history that shows the wrong "current" marker.
pub async fn list_revisions(pool: &PgPool, site_id: Uuid) -> Result<Vec<RevisionSummary>> {
    let draft_id: Option<Uuid> =
        sqlx::query_scalar("select revision_id from theme_settings_draft where site_id = $1")
            .bind(site_id)
            .fetch_optional(pool)
            .await?;
    let published_id: Option<Uuid> = sqlx::query_scalar(
        "select revision_id from theme_settings_published where site_id = $1",
    )
    .bind(site_id)
    .fetch_optional(pool)
    .await?;

    let rows = sqlx::query_as::<_, RevisionRow>(
        "select r.id, r.revision_no, r.theme_key, r.created_at, r.created_by, \
                u.display_name as created_by_name, \
                (r.id = coalesce($2::uuid, '00000000-0000-0000-0000-000000000000'::uuid)) as is_published, \
                (r.id = coalesce($3::uuid, '00000000-0000-0000-0000-000000000000'::uuid)) as is_draft, \
                (select source.revision_no from theme_settings_revisions source \
                 where source.id = r.restored_from_id) as restored_from_no \
         from theme_settings_revisions r \
         left join users u on u.id = r.created_by \
         where r.site_id = $1 \
         order by r.revision_no desc",
    )
    .bind(site_id)
    .bind(published_id)
    .bind(draft_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| RevisionSummary {
            id: row.id,
            revision_no: row.revision_no,
            theme_key: row.theme_key,
            created_at: row.created_at,
            created_by: row.created_by,
            created_by_name: row.created_by_name,
            is_published: row.is_published,
            is_draft: row.is_draft,
            restored_from_no: row.restored_from_no,
        })
        .collect())
}

/// The row the history query returns, including the sub-select alias.
#[derive(sqlx::FromRow)]
struct RevisionRow {
    id: Uuid,
    revision_no: i32,
    theme_key: String,
    created_at: OffsetDateTime,
    created_by: Option<Uuid>,
    created_by_name: Option<String>,
    is_published: bool,
    is_draft: bool,
    restored_from_no: Option<i32>,
}

// ---------------------------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------------------------

/// Save a draft. Never touches what visitors see.
pub async fn save_draft(
    pool: &PgPool,
    site_id: Uuid,
    input: &SettingsInput,
    user_id: Option<Uuid>,
) -> Result<SettingsRevision> {
    let input = validate(input)?;
    let mut transaction = pool.begin().await?;

    // `max(revision_no) + 1` under the same transaction that inserts is what makes two
    // concurrent saves produce 3 and 4 rather than two rows both numbered 3 (the unique index
    // would reject the second — a 500 the panel cannot explain, and an operator who pressed
    // save twice should not get an error).
    let next: i32 =
        sqlx::query_scalar("select coalesce(max(revision_no), 0) + 1 from theme_settings_revisions where site_id = $1")
            .bind(site_id)
            .fetch_one(&mut *transaction)
            .await?;

    let revision = insert_revision(
        &mut transaction,
        site_id,
        next,
        &input,
        user_id,
        None,
    )
    .await?;

    // The draft pointer is an UPSERT, not an insert: saving twice must move the pointer, not
    // fail on a duplicate site.
    sqlx::query(
        "insert into theme_settings_draft (site_id, revision_id, updated_by, updated_at) \
         values ($1, $2, $3, now()) \
         on conflict (site_id) do update set revision_id = excluded.revision_id, \
             updated_by = excluded.updated_by, updated_at = excluded.updated_at",
    )
    .bind(site_id)
    .bind(revision.id)
    .bind(user_id)
    .execute(&mut *transaction)
    .await?;

    transaction.commit().await?;
    Ok(revision)
}

/// Publish the draft, refusing when it is not the newest thing the panel knows about.
pub async fn publish(
    pool: &PgPool,
    site_id: Uuid,
    user_id: Option<Uuid>,
) -> Result<SettingsChange> {
    let mut transaction = pool.begin().await?;

    // `for update` on the pointer row: two administrators publishing at once must not both
    // read "the previous published revision is 4" and both write "5 replaced 4".
    let current: Option<(Uuid, i32)> = sqlx::query_as(
        "select p.revision_id, r.revision_no from theme_settings_published p \
         join theme_settings_revisions r on r.id = p.revision_id \
         where p.site_id = $1 for update of p",
    )
    .bind(site_id)
    .fetch_optional(&mut *transaction)
    .await?;

    let draft_id: Uuid = sqlx::query_scalar(
        "select revision_id from theme_settings_draft where site_id = $1",
    )
    .bind(site_id)
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or(ContentError::ThemeSettingsNothingToPublish)?;

    let draft_no: i32 = sqlx::query_scalar(
        "select revision_no from theme_settings_revisions where id = $1",
    )
    .bind(draft_id)
    .fetch_one(&mut *transaction)
    .await?;

    // The draft pointer is authoritative, so this can only happen if somebody restored a
    // revision (which publishes) and then an older panel tab pressed Publish. Refusing with
    // both numbers is the difference between a person understanding what was lost and a person
    // clicking through a warning twice.
    if let Some((published_id, published_no)) = &current
        && *published_id != draft_id
        && *published_no > draft_no
    {
        return Err(ContentError::ThemeSettingsDraftStale {
            draft_no,
            published_no: *published_no,
        });
    }

    // Stamping `published_at` on the revision is what the history screen reads for "when did
    // this go live", and it is set only the first time: republishing the same revision twice
    // must not rewrite the date the site first showed it.
    sqlx::query("update theme_settings_revisions set published_at = now() where id = $1 and published_at is null")
        .bind(draft_id)
        .execute(&mut *transaction)
        .await?;

    sqlx::query(
        "insert into theme_settings_published (site_id, revision_id, published_by, published_at) \
         values ($1, $2, $3, now()) \
         on conflict (site_id) do update set revision_id = excluded.revision_id, \
             published_by = excluded.published_by, published_at = excluded.published_at",
    )
    .bind(site_id)
    .bind(draft_id)
    .bind(user_id)
    .execute(&mut *transaction)
    .await?;

    transaction.commit().await?;
    Ok(SettingsChange::Published {
        revision_no: draft_no,
        previous_revision_no: current.map(|(_, number)| number),
    })
}

/// Restore an earlier revision by writing its content as a new one.
///
/// Returns the new revision rather than moving a pointer, and that is the entire point: a
/// restore is itself something the site did, so it gets a number, an author and a place in the
/// history. The restored content also goes live immediately, because a restore that only
/// updates the draft would leave the panel looking right and the site unchanged.
pub async fn restore_revision(
    pool: &PgPool,
    site_id: Uuid,
    revision_no: i32,
    user_id: Option<Uuid>,
) -> Result<SettingsChange> {
    let source = revision(pool, site_id, revision_no).await?;

    let mut transaction = pool.begin().await?;
    let next: i32 = sqlx::query_scalar(
        "select coalesce(max(revision_no), 0) + 1 from theme_settings_revisions where site_id = $1",
    )
    .bind(site_id)
    .fetch_one(&mut *transaction)
    .await?;

    // Restoring "revision 1" while the draft is revision 4 must not throw revision 4 away:
    // the draft pointer moves to the new revision, and the number an operator saw on the
    // draft before is still in the history.
    let input = SettingsInput {
        theme_key: source.theme_key.clone(),
        tokens: source.tokens.clone(),
        typography: source.typography.clone(),
        layout: source.layout.clone(),
        branding: source.branding.clone(),
        header_footer: source.header_footer.clone(),
        default_mode: source.default_mode.clone(),
    };
    let restored = insert_revision(
        &mut transaction,
        site_id,
        next,
        &input,
        user_id,
        Some(source.id),
    )
    .await?;

    for (table, by) in [("theme_settings_draft", "updated_by"), ("theme_settings_published", "published_by")] {
        sqlx::query(&format!(
            "insert into {table} (site_id, revision_id, {by}) values ($1, $2, $3) \
             on conflict (site_id) do update set revision_id = excluded.revision_id, \
                 {by} = excluded.{by}, updated_at = now()"
        ))
        .bind(site_id)
        .bind(restored.id)
        .bind(user_id)
        .execute(&mut *transaction)
        .await?;
    }

    transaction.commit().await?;
    Ok(SettingsChange::Restored {
        revision: restored,
    })
}

/// Insert one revision row.
async fn insert_revision(
    transaction: &mut Transaction<'_, Postgres>,
    site_id: Uuid,
    revision_no: i32,
    input: &SettingsInput,
    user_id: Option<Uuid>,
    restored_from: Option<Uuid>,
) -> Result<SettingsRevision> {
    let revision = sqlx::query_as::<_, SettingsRevision>(&format!(
        "insert into theme_settings_revisions \
             (site_id, revision_no, theme_key, tokens, typography, layout, branding, \
              header_footer, default_mode, created_by, restored_from_id) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
         returning {REVISION_COLUMNS}"
    ))
    .bind(site_id)
    .bind(revision_no)
    .bind(&input.theme_key)
    .bind(&input.tokens)
    .bind(&input.typography)
    .bind(&input.layout)
    .bind(&input.branding)
    .bind(&input.header_footer)
    .bind(&input.default_mode)
    .bind(user_id)
    .bind(restored_from)
    .fetch_one(&mut **transaction)
    .await?;

    Ok(revision)
}

// ---------------------------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------------------------

/// Check a settings payload, and normalize the mode.
///
/// Every refusal here is a *field* refusal the customize screen can put next to the input that
/// caused it, which is why they are [`ContentError::InvalidField`] rather than a generic 400.
pub fn validate(input: &SettingsInput) -> Result<SettingsInput> {
    let theme_key = if input.theme_key.trim().is_empty() {
        DEFAULT_THEME_KEY.to_owned()
    } else {
        crate::validation::validate_key(&input.theme_key, "theme key")?
    };

    let default_mode = input.default_mode.trim().to_lowercase();
    let default_mode = if default_mode.is_empty() {
        "system".to_owned()
    } else if MODES.contains(&default_mode.as_str()) {
        default_mode
    } else {
        return Err(ContentError::InvalidField(format!(
            "default mode must be one of {}, not {default_mode:?}",
            MODES.join(", ")
        )));
    };

    for (name, section) in [
        ("typography", &input.typography),
        ("layout", &input.layout),
        ("branding", &input.branding),
        ("header_footer", &input.header_footer),
    ] {
        if !section.is_object() && !section.is_null() {
            return Err(ContentError::InvalidField(format!(
                "{name} must be an object of named values"
            )));
        }
    }

    // A token map is walked rather than trusted: the renderer writes every string in it into a
    // CSS custom property, so a value of `12px; background: url(evil)` is a stylesheet
    // injection that would be *saved* by a client that never meant it.
    for (name, section) in [
        ("tokens", &input.tokens),
        ("typography", &input.typography),
        ("layout", &input.layout),
        ("branding", &input.branding),
        ("header_footer", &input.header_footer),
    ] {
        check_token_values(name, section)?;
    }

    Ok(SettingsInput {
        theme_key,
        tokens: input.tokens.clone(),
        typography: input.typography.clone(),
        layout: input.layout.clone(),
        branding: input.branding.clone(),
        header_footer: input.header_footer.clone(),
        default_mode,
    })
}

/// Refuse a value that is not a plain colour, number, keyword or short text.
///
/// The rule is a character allow-list per kind rather than a parse of "is this a colour",
/// because a theme's token can legitimately be a font stack, a radius, a length or a
/// variant name. What no token may be is a value that ends a declaration and starts another.
fn check_token_values(section_name: &str, value: &Value) -> Result<()> {
    let Some(object) = value.as_object() else {
        if value.is_null() {
            return Ok(());
        }
        return Err(ContentError::InvalidField(format!(
            "{section_name} must be an object of named values"
        )));
    };

    for (key, entry) in object {
        if key.len() > 64 || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(ContentError::InvalidField(format!(
                "{section_name} key {key:?} must be letters, digits, dashes or underscores"
            )));
        }
        // `{ "surface": { "light": "#fff", "dark": "#111" } }` is a mode map; a value that is an
        // object is walked by the same rules, so the check recurses rather than assuming depth 1.
        if entry.is_object() {
            check_token_values(&format!("{section_name}.{key}"), entry)?;
            continue;
        }
        if let Some(text) = entry.as_str() {
            if text.len() > MAX_VALUE_LENGTH
                || text.contains([';', '{', '}', '<', '>', '\\', '"', '\''])
            {
                return Err(ContentError::InvalidField(format!(
                    "{section_name} value for {key:?} may not contain a semicolon, braces, \
                     angle brackets, a backslash or quotes"
                )));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Contrast
// ---------------------------------------------------------------------------------------------

/// Measure every text/background pair in a token map.
///
/// Pure and total: a value that is not a hex colour, or a token the theme never declared,
/// produces **no finding** rather than a failure. The reason is that this runs on every
/// keystroke in the customize screen and a missing token is the normal state of a screen
/// being filled in, not an error worth a red banner.
pub fn contrast_report(tokens: &Value) -> Vec<ContrastFinding> {
    let mut findings = Vec::new();

    for mode in ["light", "dark"] {
        for (foreground, background, required) in CONTRAST_PAIRS {
            let Some(front) = colour_of(tokens, foreground, mode) else {
                continue;
            };
            let Some(back) = colour_of(tokens, background, mode) else {
                continue;
            };
            let Some(ratio) = contrast_ratio(&front, &back) else {
                continue;
            };
            if ratio >= required {
                continue;
            }
            findings.push(ContrastFinding {
                foreground: foreground.to_owned(),
                background: background.to_owned(),
                mode: mode.to_owned(),
                // Two decimals: the badge says "3.12:1" and a value of 3.1184271 would make
                // the same number read differently twice in one screen.
                ratio: (ratio * 100.0).round() / 100.0,
                required,
                message: format!(
                    "{foreground} on {background} is {ratio:.2}:1 in {mode} mode, below the {required:.1}:1 minimum"
                ),
            });
        }
    }

    findings
}

/// The colour a token holds in one mode, or `None` when it is not a hex colour.
fn colour_of(tokens: &Value, name: &str, mode: &str) -> Option<String> {
    let value = tokens.get(name)?;
    match value {
        // A mode map: `tokens.surface[mode]`.
        Value::Object(modes) => modes
            .get(mode)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                // A theme may declare a token for one mode only and expect the other to fall
                // back to it, which is how `dark: {}` is read in CSS.
                if mode == "dark" {
                    modes.get("light").and_then(Value::as_str).map(str::to_owned)
                } else {
                    None
                }
            }),
        Value::String(text) => Some(text.clone()),
        _ => None,
    }
    .filter(|text| is_hex_colour(text))
}

/// Whether a string is a 3-, 4-, 6- or 8-digit hex colour, with or without the leading `#`.
pub fn is_hex_colour(value: &str) -> bool {
    let body = value.trim().strip_prefix('#').unwrap_or(value.trim());
    matches!(body.len(), 3 | 4 | 6 | 8) && body.chars().all(|c| c.is_ascii_hexdigit())
}

/// WCAG relative-luminance contrast ratio between two hex colours, or `None` if either is not
/// one.
///
/// The formula is the WCAG 2.1 definition, not an approximation: a "close enough" contrast
/// check passes pairs that a real audit tool fails, and a warning badge that disagrees with
/// the auditor is worse than no badge.
pub fn contrast_ratio(foreground: &str, background: &str) -> Option<f64> {
    let front = hex_to_rgb(foreground)?;
    let back = hex_to_rgb(background)?;
    let lighter = front.luminance().max(back.luminance());
    let darker = front.luminance().min(back.luminance());
    Some((lighter + 0.05) / (darker + 0.05))
}

/// `#rgb`, `#rrggbb` and their alpha forms, as 0–255 channels.
///
/// 4- and 8-digit hex carry an alpha channel, and **alpha is ignored**: the platform has no
/// compositing surface to composite against, and guessing one would produce a ratio for a
/// colour nobody is going to see. A half-transparent white on white would otherwise be
/// reported as failing a check it passes.
fn hex_to_rgb(value: &str) -> Option<(u8, u8, u8)> {
    let body = value.trim().strip_prefix('#').unwrap_or(value.trim());
    let expand = |c: char| u8::from_str_radix(&format!("{c}{c}"), 16).ok();
    match body.len() {
        3 => Some((
            expand(body.chars().next()?)?,
            expand(body.chars().nth(1)?)?,
            expand(body.chars().nth(2)?)?,
        )),
        6 | 8 => Some((
            u8::from_str_radix(body.get(0..2)?, 16).ok()?,
            u8::from_str_radix(body.get(2..4)?, 16).ok()?,
            u8::from_str_radix(body.get(4..6)?, 16).ok()?,
        )),
        _ => None,
    }
}

/// One channel's relative luminance.
trait Luminance {
    fn luminance(&self) -> f64;
}

impl Luminance for (u8, u8, u8) {
    fn luminance(&self) -> f64 {
        let [r, g, b] = [self.0, self.1, self.2];
        let channel = |value: u8| {
            let value = f64::from(value) / 255.0;
            if value <= 0.039_28 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
    }
}

/// Merge a site's overrides over a theme's defaults, one level deep into mode maps.
///
/// One level, because that is the shape a token map has: `{token: {light, dark}}`. A deep
/// merge would invent a `tokens.tokens.light` if a theme ever nested, and the renderer only
/// ever reads two levels.
pub fn merge_over(defaults: Value, overrides: Value) -> Value {
    let Some(mut merged) = defaults.as_object().cloned() else {
        return overrides;
    };
    let Some(overrides) = overrides.as_object() else {
        return overrides;
    };
    for (key, value) in overrides {
        match (merged.get(key), value) {
            (Some(Value::Object(base)), Value::Object(top)) => {
                let mut next = base.clone();
                for (mode, mode_value) in top {
                    next.insert(mode.clone(), mode_value.clone());
                }
                merged.insert(key.clone(), Value::Object(next));
            }
            _ => {
                merged.insert(key.clone(), value.clone());
            }
        }
    }
    Value::Object(merged)
}

/// A short human summary of a token map, for the audit row and the walkthrough's grep.
pub fn describe_settings(revision: &SettingsRevision) -> Value {
    json!({
        "revision_no": revision.revision_no,
        "theme_key": revision.theme_key,
        "default_mode": revision.default_mode,
        "token_count": revision.tokens.as_object().map_or(0, serde_json::Map::len),
    })
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens() -> Value {
        json!({
            "text": { "light": "#1a1a1a", "dark": "#f5f5f5" },
            "textMuted": { "light": "#5a5a5a", "dark": "#a0a0a0" },
            "accent": { "light": "#2b5cd9", "dark": "#7aa2f7" },
            "surface": { "light": "#ffffff", "dark": "#101010" },
            "surfaceRaised": { "light": "#f4f4f4", "dark": "#1c1c1c" }
        })
    }

    fn input() -> SettingsInput {
        SettingsInput {
            theme_key: "corporate".to_owned(),
            tokens: tokens(),
            typography: json!({ "baseSize": "16px" }),
            layout: json!({ "containerWidth": "1200px" }),
            branding: json!({ "logo": null }),
            header_footer: json!({ "header": "sticky" }),
            default_mode: "system".to_owned(),
        }
    }

    #[test]
    fn a_good_palette_reports_nothing() {
        assert_eq!(contrast_report(&tokens()), Vec::new());
    }

    #[test]
    fn black_on_white_is_the_documented_maximum() {
        // 21:1 is the ratio the formula must produce at its extremes; a rounding slip shows up
        // here and nowhere else.
        let ratio = contrast_ratio("#000000", "#ffffff").expect("both are colours");
        assert!((ratio - 21.0).abs() < 0.01, "got {ratio}");
    }

    #[test]
    fn the_same_colour_against_itself_is_one_to_one() {
        let ratio = contrast_ratio("#2b5cd9", "#2b5cd9").expect("both are colours");
        assert!((ratio - 1.0).abs() < 0.001, "got {ratio}");
    }

    #[test]
    fn three_and_six_digit_hex_agree() {
        let short = contrast_ratio("#fff", "#000").expect("both are colours");
        let long = contrast_ratio("#ffffff", "#000000").expect("both are colours");
        assert!((short - long).abs() < 0.001, "{short} vs {long}");
    }

    #[test]
    fn a_prefix_may_be_missing() {
        assert!(is_hex_colour("fff"));
        assert!(is_hex_colour("#FFF"));
        assert!(contrast_ratio("ffffff", "000000").is_some());
    }

    #[test]
    fn a_low_contrast_pair_is_reported_only_for_the_mode_that_is_low() {
        // Dark is deliberately *readable* here: `#101010` text on a `#101010` surface is 1.0:1
        // and the checker rightly reports it, which is why the two modes need different values.
        // Checking per mode rather than once is the whole point of the pair loop.
        let bad = json!({
            "text": { "light": "#bbbbbb", "dark": "#f5f5f5" },
            "surface": { "light": "#ffffff", "dark": "#101010" }
        });
        let findings = contrast_report(&bad);
        assert_eq!(findings.len(), 1, "{findings:#?}");
        let finding = &findings[0];
        assert_eq!(finding.mode, "light");
        assert_eq!(finding.foreground, "text");
        assert_eq!(finding.background, "surface");
        assert!(finding.ratio < finding.required);
        assert!(finding.message.contains("text on surface"), "{}", finding.message);
        assert!(!findings.iter().any(|f| f.mode == "dark"));
    }

    #[test]
    fn text_and_surface_in_the_same_colour_is_a_finding_in_both_modes() {
        // The inverse mistake is the one that would ship: a palette where the body colour
        // equals the page colour is unreadable, and a checker that only measured the light
        // mode would have called this theme fine.
        let invisible = json!({
            "text": { "light": "#101010", "dark": "#101010" },
            "surface": { "light": "#101010", "dark": "#101010" }
        });
        let findings = contrast_report(&invisible);
        assert_eq!(findings.len(), 2, "{findings:#?}");
        assert!(findings.iter().all(|f| f.ratio == 1.0), "{findings:#?}");
    }

    #[test]
    fn a_missing_token_is_not_a_finding() {
        // The customize screen runs this on every keystroke; a half-filled form is not an error.
        let report = contrast_report(&json!({ "text": { "light": "#111" } }));
        assert!(report.is_empty(), "{report:#?}");
    }

    #[test]
    fn a_value_that_is_not_a_colour_is_skipped_rather_than_refused() {
        let report = contrast_report(&json!({
            "text": { "light": "inherit" },
            "surface": { "light": "#ffffff" }
        }));
        assert!(report.is_empty(), "{report:#?}");
    }

    #[test]
    fn dark_falls_back_to_the_light_value_when_a_mode_is_absent() {
        // `dark: {}` is a legitimate declaration: the token has one colour for both modes.
        let only_light = json!({
            "text": { "light": "#1a1a1a" },
            "surface": { "light": "#ffffff" }
        });
        assert!(contrast_report(&only_light).is_empty());
    }

    #[test]
    fn the_alpha_channel_of_a_hex_value_is_ignored() {
        // No compositing surface exists, so a ratio against an unknown backdrop would be a
        // made-up number. The same colour either way must measure the same.
        let opaque = contrast_ratio("#808080", "#ffffff").expect("colour");
        let translucent = contrast_ratio("#80808080", "#ffffff").expect("colour");
        assert!((opaque - translucent).abs() < 0.001, "{opaque} vs {translucent}");
    }

    #[test]
    fn a_valid_payload_normalizes_the_mode_and_the_key() {
        let mut payload = input();
        payload.default_mode = " DARK ".to_owned();
        payload.theme_key = "  Corporate ".to_owned();
        let checked = validate(&payload).expect("valid");
        assert_eq!(checked.default_mode, "dark");
        assert_eq!(checked.theme_key, "corporate");
    }

    #[test]
    fn a_blank_mode_and_key_take_the_defaults() {
        let mut payload = input();
        payload.default_mode = String::new();
        payload.theme_key = String::new();
        let checked = validate(&payload).expect("valid");
        assert_eq!(checked.default_mode, "system");
        assert_eq!(checked.theme_key, DEFAULT_THEME_KEY);
    }

    #[test]
    fn an_unknown_mode_names_the_accepted_ones() {
        let mut payload = input();
        payload.default_mode = "sepia".to_owned();
        let error = validate(&payload).expect_err("sepia is not a mode");
        assert!(
            matches!(&error, ContentError::InvalidField(message) if message.contains("light, dark, system")),
            "{error:?}"
        );
    }

    #[test]
    fn a_declaration_breaker_in_a_token_is_refused() {
        // The renderer writes token values into CSS custom properties, so this is the injection
        // that matters, and a client-side validator is exactly the wrong place to catch it.
        for hostile in [
            "#fff; background-image: url(https://example.test/x)",
            "red}",
            "1px<script>",
        ] {
            let mut payload = input();
            payload.tokens = json!({ "surface": { "light": hostile } });
            let error = validate(&payload).expect_err("a declaration must not be smuggled in");
            assert!(
                matches!(error, ContentError::InvalidField(_)),
                "{hostile:?} was accepted"
            );
        }
    }

    #[test]
    fn a_token_key_that_is_not_an_identifier_is_refused() {
        let mut payload = input();
        payload.tokens = json!({ "surface color": "#fff" });
        assert!(validate(&payload).is_err());
    }

    #[test]
    fn nested_mode_maps_are_checked_too() {
        let mut payload = input();
        payload.tokens = json!({ "surface": { "light": { "nested": "#fff; x" } } });
        assert!(validate(&payload).is_err());
    }

    #[test]
    fn a_non_object_section_is_refused() {
        let mut payload = input();
        payload.typography = json!(["not", "an", "object"]);
        assert!(validate(&payload).is_err());
    }

    #[test]
    fn a_null_section_is_allowed_because_absent_is_not_the_same_as_empty() {
        let mut payload = input();
        payload.branding = Value::Null;
        payload.header_footer = Value::Null;
        assert!(validate(&payload).is_ok());
    }

    #[test]
    fn an_overlong_value_is_refused() {
        let mut payload = input();
        payload.typography = json!({ "fontStack": "a".repeat(MAX_VALUE_LENGTH + 1) });
        assert!(validate(&payload).is_err());
    }

    #[test]
    fn overrides_replace_wholesale_and_merge_per_mode() {
        let merged = merge_over(
            json!({
                "text": { "light": "#111", "dark": "#eee" },
                "surface": { "light": "#fff", "dark": "#000" }
            }),
            json!({ "text": { "dark": "#fafafa" } }),
        );
        assert_eq!(merged["text"]["light"], json!("#111"), "an untouched mode survives");
        assert_eq!(merged["text"]["dark"], json!("#fafafa"), "the named mode is replaced");
        assert_eq!(merged["surface"]["dark"], json!("#000"), "an unmentioned token survives");
    }

    #[test]
    fn a_scalar_override_replaces_the_whole_map() {
        let merged = merge_over(
            json!({ "text": { "light": "#111" } }),
            json!({ "text": "inherit" }),
        );
        assert_eq!(merged["text"], json!("inherit"));
    }

    #[test]
    fn merging_into_a_non_object_returns_the_overrides() {
        let merged = merge_over(json!({ "a": 1 }), json!("nonsense"));
        assert_eq!(merged, json!("nonsense"));
    }
}
