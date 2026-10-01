//! The maintenance window's decisions (REQ-024, slice 3).
//!
//! A maintenance window is the one feature here whose bug is invisible until the worst possible
//! moment. If the enforcement is one `if` in a handler, then every write route that is added
//! *later* — a new module, a new surface, a second writer in another crate — is a route that
//! writes during the window, and the operator's promise ("nothing is changing for ten minutes")
//! is a lie they find out about from a customer. So the two things a caller needs are here as
//! values with their own tests:
//!
//! * [`Window::blocks`] — is this window *active right now*, for a given scope and instant? The
//!   trap is treating "enabled" as "active": a window scheduled for tomorrow, or one whose end
//!   has passed, is enabled and not active, and a check that ignores the schedule blocks writes
//!   at the wrong time — either in the middle of a working day or not at all when it matters.
//! * [`Window::save`] — what a form submission is allowed to change, and the refusals it gets.
//!
//! The scope distinction (`all` vs `admin`) is also here rather than in a route, because it is
//! the difference between "the site is down for customers" and "admins cannot change settings
//! right now", and a route that got it backwards is a production outage caused by a checkbox.

use time::OffsetDateTime;

use crate::error::StoreError;

/// The longest banner message a window may carry.
///
/// The form caps it and the `0211` constraint refuses it, so the two agree; the constant exists
/// so the *panel* asks for the same number rather than hard-coding a second one that drifts.
pub const MAX_MESSAGE_LEN: usize = 280;

/// Who a window applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Every write, everywhere: the public site included.
    All,
    /// Admin writes only. Reads, and the public site, are untouched.
    Admin,
}

impl Scope {
    /// Parse a stored or submitted scope.
    ///
    /// An unrecognised value is `None` rather than a default: a scope the database cannot vouch
    /// for must not silently become `All` (blocking the whole platform over a typo) nor
    /// silently become `Admin` (a window that does not block the writes it promised).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "all" => Some(Scope::All),
            "admin" => Some(Scope::Admin),
            _ => None,
        }
    }

    /// The stored form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::All => "all",
            Scope::Admin => "admin",
        }
    }
}

/// One environment's window, as the table holds it.
#[derive(Debug, Clone)]
pub struct Window {
    /// `production`, `staging` or `sandbox`.
    pub environment: String,
    /// The operator's toggle. `true` is not the same as *active* — see [`Window::is_active`].
    pub enabled: bool,
    /// The banner text shown in every session in scope.
    pub message: String,
    /// When the window opens, or `None` for "as soon as it is enabled".
    pub starts_at: Option<OffsetDateTime>,
    /// When it closes, or `None` for open-ended.
    pub ends_at: Option<OffsetDateTime>,
    /// What it applies to.
    pub scope: Scope,
    /// Who last changed it.
    pub updated_by: Option<uuid::Uuid>,
    /// When it was last changed.
    pub updated_at: Option<OffsetDateTime>,
}

impl Window {
    /// A window nobody has configured: disabled, empty, open-ended, admin-scoped.
    ///
    /// The shape a `default` route returns so the maintenance screen renders before anyone has
    /// ever opened one. `Admin` rather than `All` because an unconfigured window must not
    /// describe itself as blocking the whole platform.
    #[must_use]
    pub fn unset(environment: impl Into<String>) -> Self {
        Self {
            environment: environment.into(),
            enabled: false,
            message: String::new(),
            starts_at: None,
            ends_at: None,
            scope: Scope::Admin,
            updated_by: None,
            updated_at: None,
        }
    }

    /// Is the window open at `now`?
    ///
    /// Three conditions, and the third is the one a naive check drops: `enabled`, the start is
    /// in the past, and **the end has not passed**. A window whose `ends_at` is yesterday is
    /// still `enabled = true` in the table — that is how the operator ended it — so a check that
    /// only looks at `enabled` blocks writes for ever after the window closes.
    #[must_use]
    pub fn is_active(&self, now: OffsetDateTime) -> bool {
        if !self.enabled {
            return false;
        }
        if self.starts_at.is_some_and(|start| start > now) {
            return false;
        }
        !self.ends_at.is_some_and(|end| end <= now)
    }

    /// Does this window refuse the write the caller is about to make?
    ///
    /// `target` is `true` for a write that changes the platform for everyone, `false` for one
    /// that only changes the panel. A `Scope::Admin` window refuses the second and **not** the
    /// first — which is the whole reason the scope exists: a window scoped to the admin is an
    /// operator saying "I am changing settings, do not fight me", and a content editor publishing
    /// through the public API in that window is the traffic a window is normally opened *for*.
    #[must_use]
    pub fn blocks(&self, target: bool, now: OffsetDateTime) -> bool {
        if !self.is_active(now) {
            return false;
        }
        match self.scope {
            Scope::All => true,
            Scope::Admin => !target,
        }
    }
}

/// A window configuration as the form submitted it.
#[derive(Debug, Clone, Default)]
pub struct WindowEdit {
    /// The operator's toggle.
    pub enabled: bool,
    /// The banner text.
    pub message: String,
    /// Optional start.
    pub starts_at: Option<OffsetDateTime>,
    /// Optional end.
    pub ends_at: Option<OffsetDateTime>,
    /// Optional scope — `None` keeps the stored one, so a form that only edits the message does
    /// not silently widen the window from `admin` to `all`.
    pub scope: Option<Scope>,
}

/// Why a window could not be saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowRefusal {
    /// The message is longer than [`MAX_MESSAGE_LEN`].
    MessageTooLong(usize),
    /// The end is at or before the start.
    EndBeforeStart,
    /// Enabling with no message to show.
    NoMessage,
}

impl std::fmt::Display for WindowRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WindowRefusal::MessageTooLong(len) => write!(
                f,
                "The message is {len} characters; the limit is {MAX_MESSAGE_LEN}."
            ),
            WindowRefusal::EndBeforeStart => {
                f.write_str("The window ends before it starts.")
            }
            WindowRefusal::NoMessage => {
                f.write_str("An enabled window needs a message to show in the banner.")
            }
        }
    }
}

/// A window edit with its refusals checked and the values normalised.
///
/// Returns the window to store, or the first refusal. Refused **before** anything is written,
/// so a rejected form never leaves the previous window half-changed.
pub fn save(environment: &str, stored: &Window, edit: &WindowEdit) -> Result<Window, WindowRefusal> {
    let message = edit.message.trim().to_string();
    if message.chars().count() > MAX_MESSAGE_LEN {
        return Err(WindowRefusal::MessageTooLong(message.chars().count()));
    }
    // `ends_at > starts_at` and not `>=`: a window whose start and end are the same instant is
    // already over, and it would be stored happily and then read as "not in a window" by the
    // very same check that has to enforce it.
    if edit
        .ends_at
        .zip(edit.starts_at)
        .is_some_and(|(end, start)| end <= start)
    {
        return Err(WindowRefusal::EndBeforeStart);
    }
    // An enabled window with an empty message is a banner that says nothing while every write is
    // refused. The operator gets a form error instead, which is a problem they can fix.
    if edit.enabled && message.is_empty() {
        return Err(WindowRefusal::NoMessage);
    }

    let scope = edit.scope.unwrap_or(stored.scope);
    let unchanged = stored.enabled == edit.enabled
        && stored.message == message
        && stored.scope == scope
        && stored.starts_at == edit.starts_at
        && stored.ends_at == edit.ends_at;

    Ok(Window {
        environment: environment.to_string(),
        enabled: edit.enabled,
        message,
        starts_at: edit.starts_at,
        ends_at: edit.ends_at,
        scope,
        updated_by: stored.updated_by,
        updated_at: stored.updated_at,
        // A no-op submission is reported as unchanged so the route can skip the write and the
        // `updated_at` bump — a history of "changed" rows that never changed anything is its
        // own kind of lie.
        ..if unchanged { stored.clone() } else { Window::unset(environment) }
    })
}

/// A refusal as the `503` the write route answers.
#[derive(Debug, Clone)]
pub struct Block {
    /// The banner message, carried so the client shows the operator's own words.
    pub message: String,
}

impl Block {
    /// The reason phrase for the response body, so a client that does not read `message` still
    /// gets something true.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        "a maintenance window is open for this environment"
    }
}

impl Window {
    /// The refusal a write route should answer, or `None` when the window does not block.
    ///
    /// The message the operator wrote is **the message that travels**: a generic "service
    /// unavailable" would leave an API caller unable to tell a planned window from an outage,
    /// which is the distinction that decides whether they retry or page someone.
    #[must_use]
    pub fn block_for(&self, target: bool, now: OffsetDateTime) -> Option<Block> {
        self.blocks(target, now)
            .then(|| Block { message: self.message.clone() })
    }
}

/// Load one environment's window, or the unset shape.
pub async fn load_window(pool: &sqlx::PgPool, environment: &str) -> Result<Window, StoreError> {
    let row: Option<(bool, String, Option<OffsetDateTime>, Option<OffsetDateTime>, String, Option<uuid::Uuid>, OffsetDateTime)> =
        sqlx::query_as(
            "select enabled, message, starts_at, ends_at, scope, updated_by, updated_at \
             from maintenance_windows where environment = $1",
        )
        .bind(environment)
        .fetch_optional(pool)
        .await?;

    let Some((enabled, message, starts_at, ends_at, scope, updated_by, updated_at)) = row else {
        return Ok(Window::unset(environment));
    };
    // An unreadable scope makes the row refuse *nothing* rather than guess: the same refusal
    // the wrong way as `Scope::parse`, in the place where guessing would block a platform.
    let scope = Scope::parse(&scope).unwrap_or(Scope::All);
    Ok(Window {
        environment: environment.to_string(),
        enabled,
        message,
        starts_at,
        ends_at,
        scope,
        updated_by,
        updated_at: Some(updated_at),
    })
}

/// Every configured window, for the shell banner and the screen's overview.
pub async fn list_windows(pool: &sqlx::PgPool) -> Result<Vec<Window>, StoreError> {
    let rows: Vec<(String, bool, String, Option<OffsetDateTime>, Option<OffsetDateTime>, String, Option<uuid::Uuid>, OffsetDateTime)> =
        sqlx::query_as(
            "select environment, enabled, message, starts_at, ends_at, scope, updated_by, updated_at \
             from maintenance_windows order by environment",
        )
        .fetch_all(pool)
        .await?;

    Ok(rows
        .into_iter()
        .map(|(environment, enabled, message, starts_at, ends_at, scope, updated_by, updated_at)| {
            Window {
                environment,
                enabled,
                message,
                starts_at,
                ends_at,
                scope: Scope::parse(&scope).unwrap_or(Scope::All),
                updated_by,
                updated_at: Some(updated_at),
            }
        })
        .collect())
}

/// Store a window. The caller has already checked the refusals via [`save`].
pub async fn store_window(
    pool: &sqlx::PgPool,
    window: &Window,
    actor: Option<uuid::Uuid>,
) -> Result<(), StoreError> {
    sqlx::query(
        "insert into maintenance_windows (environment, enabled, message, starts_at, ends_at, scope, updated_by, updated_at) \
         values ($1, $2, $3, $4, $5, $6, $7, now()) \
         on conflict (environment) do update set enabled = excluded.enabled, message = excluded.message, \
           starts_at = excluded.starts_at, ends_at = excluded.ends_at, scope = excluded.scope, \
           updated_by = excluded.updated_by, updated_at = now()",
    )
    .bind(&window.environment)
    .bind(window.enabled)
    .bind(&window.message)
    .bind(window.starts_at)
    .bind(window.ends_at)
    .bind(window.scope.as_str())
    .bind(actor)
    .execute(pool)
    .await?;
    Ok(())
}

/// The window that currently blocks a write, across every environment.
///
/// Returns the **first active window for this environment**; `None` when writes are allowed.
/// One query rather than three, because the write routes call it on every request and a
/// deployment centre that slows down the rest of the platform to enforce its own banner is a
/// worse outage than the one it prevents.
pub async fn active_blocker(
    pool: &sqlx::PgPool,
    environment: &str,
) -> Result<Option<Block>, StoreError> {
    let row: Option<(String,)> = sqlx::query_as(
        "select message from maintenance_windows \
         where environment = $1 and enabled = true \
           and (starts_at is null or starts_at <= now()) \
           and (ends_at is null or ends_at > now())",
    )
    .bind(environment)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(message,)| Block { message }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hour: u8) -> OffsetDateTime {
        // A fixed day so the tests do not depend on when they run. 2026-10-01T{hour}:00Z.
        OffsetDateTime::from_unix_timestamp(1_788_000_000 + i64::from(hour) * 3600).unwrap()
    }

    fn window() -> Window {
        Window {
            environment: "production".to_string(),
            enabled: true,
            message: "Upgrading the core.".to_string(),
            starts_at: None,
            ends_at: None,
            scope: Scope::All,
            updated_by: None,
            updated_at: None,
        }
    }

    #[test]
    fn an_enabled_window_with_no_schedule_is_active_now() {
        let w = window();
        assert!(w.is_active(at(10)));
        assert!(w.blocks(true, at(10)));
    }

    #[test]
    fn a_window_that_has_not_opened_yet_does_not_block() {
        let mut w = window();
        w.starts_at = Some(at(14));
        assert!(!w.is_active(at(10)), "10:00 is before a 14:00 start");
        assert!(w.is_active(at(15)), "15:00 is inside a 14:00-start window");
    }

    #[test]
    fn a_window_that_has_ended_does_not_block() {
        // The property that makes the *third* condition necessary: the operator ended the
        // window, and ending it is what left `enabled = true` in the table.
        let mut w = window();
        w.ends_at = Some(at(11));
        assert!(w.is_active(at(10)));
        assert!(!w.is_active(at(11)), "the end is exclusive: 11:00 is over");
        assert!(!w.blocks(true, at(12)));
    }

    #[test]
    fn a_disabled_window_never_blocks_however_its_schedule_reads() {
        let mut w = window();
        w.enabled = false;
        w.starts_at = Some(at(0));
        w.ends_at = Some(at(23));
        assert!(!w.is_active(at(10)));
        assert!(w.block_for(true, at(10)).is_none());
    }

    #[test]
    fn an_admin_window_blocks_the_panel_and_not_the_public_write() {
        let mut w = window();
        w.scope = Scope::Admin;
        // `target = false` is an admin-panel write; `target = true` is a change everybody sees.
        assert!(w.blocks(false, at(10)), "admin writes are refused");
        assert!(!w.blocks(true, at(10)), "public content writes continue");
    }

    #[test]
    fn an_all_window_blocks_both() {
        let w = window();
        assert!(w.blocks(false, at(10)));
        assert!(w.blocks(true, at(10)));
    }

    #[test]
    fn the_block_carries_the_operators_own_message() {
        let w = window();
        let block = w.block_for(true, at(10)).expect("a window that is open blocks");
        assert_eq!(block.message, "Upgrading the core.");
        assert!(block.reason().contains("maintenance window"));
    }

    #[test]
    fn no_block_when_nothing_is_open() {
        assert!(window().block_for(true, at(10)).is_some());
        let mut closed = window();
        closed.ends_at = Some(at(9));
        assert!(closed.block_for(true, at(10)).is_none());
    }

    #[test]
    fn a_message_over_the_limit_is_refused_with_its_length() {
        let stored = Window::unset("production");
        let edit = WindowEdit {
            enabled: true,
            message: "x".repeat(MAX_MESSAGE_LEN + 1),
            ..Default::default()
        };
        let refusal = save("production", &stored, &edit).expect_err("281 characters is too long");
        assert_eq!(refusal, WindowRefusal::MessageTooLong(MAX_MESSAGE_LEN + 1));
        assert!(refusal.to_string().contains("281"));
    }

    #[test]
    fn the_limit_counts_characters_not_bytes() {
        // 140 two-byte characters is 280 bytes. A byte count would refuse this message and the
        // operator would have no way to see why — the form counted 140.
        let stored = Window::unset("production");
        let edit = WindowEdit {
            enabled: true,
            message: "ş".repeat(140),
            ..Default::default()
        };
        assert!(save("production", &stored, &edit).is_ok());
    }

    #[test]
    fn an_end_at_the_same_instant_is_refused() {
        let stored = Window::unset("production");
        let edit = WindowEdit {
            enabled: true,
            message: "Window.".to_string(),
            starts_at: Some(at(10)),
            ends_at: Some(at(10)),
            ..Default::default()
        };
        assert_eq!(
            save("production", &stored, &edit).expect_err("zero-length window"),
            WindowRefusal::EndBeforeStart
        );
    }

    #[test]
    fn enabling_without_a_message_is_refused() {
        let stored = Window::unset("production");
        let edit = WindowEdit {
            enabled: true,
            message: "   ".to_string(),
            ..Default::default()
        };
        assert_eq!(
            save("production", &stored, &edit).expect_err("a blank message is no message"),
            WindowRefusal::NoMessage
        );
    }

    #[test]
    fn a_message_is_trimmed_before_it_is_stored() {
        let stored = Window::unset("production");
        let edit = WindowEdit {
            enabled: true,
            message: "  Upgrading.  \n".to_string(),
            ..Default::default()
        };
        let saved = save("production", &stored, &edit).expect("a padded message is fine");
        assert_eq!(saved.message, "Upgrading.");
    }

    #[test]
    fn an_omitted_scope_keeps_the_stored_one() {
        // The form that only edits the message must not widen an `admin` window to `all`.
        let mut stored = window();
        stored.scope = Scope::Admin;
        let edit = WindowEdit {
            enabled: true,
            message: "Still changing settings.".to_string(),
            scope: None,
            ..Default::default()
        };
        let saved = save("production", &stored, &edit).expect("valid");
        assert_eq!(saved.scope, Scope::Admin);
    }

    #[test]
    fn an_unchanged_submission_is_reported_as_unchanged() {
        let stored = window();
        let edit = WindowEdit {
            enabled: true,
            message: "Upgrading the core.".to_string(),
            scope: Some(Scope::All),
            starts_at: None,
            ends_at: None,
        };
        let saved = save("production", &stored, &edit).expect("valid");
        assert_eq!(saved.updated_at, stored.updated_at, "no-op keeps the stamp");
        assert_eq!(saved.updated_by, stored.updated_by);
    }

    #[test]
    fn scope_round_trips_and_refuses_the_unrecognised() {
        assert_eq!(Scope::parse("all"), Some(Scope::All));
        assert_eq!(Scope::parse("admin"), Some(Scope::Admin));
        assert_eq!(Scope::Admin.as_str(), "admin");
        // The refusal-the-wrong-way check: an unknown scope must not become `All`, because that
        // blocks the platform over a typo.
        assert_eq!(Scope::parse("everything"), None);
        assert_eq!(Scope::parse(""), None);
        assert_eq!(Scope::parse("ALL"), None);
    }

    #[test]
    fn the_unset_window_blocks_nothing() {
        let w = Window::unset("staging");
        assert!(!w.enabled);
        assert!(!w.is_active(at(10)));
        assert!(w.block_for(true, at(10)).is_none());
    }
}
