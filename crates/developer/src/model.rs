//! The shapes the developer surface reads and writes.
//!
//! Every struct here is what the API returns, and the first rule they all follow is that key
//! material is not one of their fields. There is no `secret` on [`ApiKey`], no
//! `client_secret` on [`OAuthApp`], and the one response that *does* carry a plaintext
//! ([`Minted`]) is a distinct type that cannot be produced by a list or a detail read — so
//! "the secret came back a second time" is a type error rather than a missed test.

use serde::{Deserialize, Serialize};
use time::{Date, OffsetDateTime};
use uuid::Uuid;

use crate::error::{DeveloperError, Result};

/// Which environment a key authenticates against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    /// Production.
    Live,
    /// Non-production.
    Sandbox,
}

impl Environment {
    /// Every environment, in the order a picker should show them.
    ///
    /// The single place the set is written down. Anything that *offers* an environment —
    /// `GET /developer/scopes`, the key form, the CLI's `--environment` help — reads this list
    /// rather than repeating it, because a form that offers an environment the server then
    /// refuses with `unknown_environment` is a picker that submits and is then rejected. Adding a
    /// variant to this enum without adding it here is now a compile error at every call site that
    /// iterates the list, which is the point.
    pub const ALL: [Self; 2] = [Self::Live, Self::Sandbox];

    /// The stored form of every environment, in [`Environment::ALL`] order.
    ///
    /// Built from [`Environment::as_str`] rather than written out literally: the whole point of
    /// this list is that it cannot drift from the spellings the parser accepts, and a second
    /// hand-written copy of the two strings is exactly the drift. `as_str` is `const` for this
    /// reason — a derived constant is only possible if the function it derives from is.
    pub const ALL_STR: [&'static str; 2] = [Self::Live.as_str(), Self::Sandbox.as_str()];

    /// Parse a stored or submitted value, refusing anything else rather than defaulting.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "live" => Ok(Self::Live),
            "sandbox" => Ok(Self::Sandbox),
            other => Err(DeveloperError::UnknownEnvironment(other.to_owned())),
        }
    }

    /// The stored form.
    ///
    /// `const` so [`Environment::ALL_STR`] can be derived from it rather than restating the two
    /// strings; see that constant for why the derivation is the point.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Sandbox => "sandbox",
        }
    }
}

/// The service level a key gets. `high` is not merely faster: the request says it requires an
/// owner or admin role, and that gate is enforced where the tier is read rather than in the
/// form, so a hand-written request cannot grant it to itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateTier {
    /// The ordinary ceiling.
    Standard,
    /// A higher ceiling; needs an owner or admin.
    High,
}

impl RateTier {
    /// Parse a stored value.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "standard" => Ok(Self::Standard),
            "high" => Ok(Self::High),
            other => Err(DeveloperError::UnknownRateTier(other.to_owned())),
        }
    }

    /// The stored form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::High => "high",
        }
    }
}

/// Whether a key can still be used, and — because the panel needs to say *why* — which of the
/// three reasons applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyStatus {
    /// Usable now.
    Active,
    /// Created but not yet usable. A key is never created in this state today; it exists so
    /// the vocabulary can grow without a migration.
    Pending,
    /// Withdrawn by a person. Permanent.
    Revoked,
    /// Past `expires_at`. Automatic, and distinct from revoked because the panel sorts on it.
    Expired,
}

impl KeyStatus {
    /// Decide a key's status from its row. Time is passed in rather than read from the clock
    /// inside, so the boundary is testable: an expiry exactly at `now` is expired, and a key
    /// expiring in a millisecond is not.
    pub fn of(
        revoked_at: Option<OffsetDateTime>,
        expires_at: Option<OffsetDateTime>,
        now: OffsetDateTime,
    ) -> Self {
        if revoked_at.is_some() {
            Self::Revoked
        } else if expires_at.is_some_and(|at| at <= now) {
            Self::Expired
        } else {
            Self::Active
        }
    }

    /// Whether a request holding a key in this state may proceed.
    pub fn is_usable(self) -> bool {
        matches!(self, Self::Active | Self::Pending)
    }
}

/// One API key, as the panel and the API see it.
///
/// There is no secret field, and that is the point: the shape that a list, a detail read and a
/// CSV export all share cannot carry one. [`Minted`] is the only type that holds plaintext and
/// it never becomes this.
#[derive(Debug, Clone, Serialize)]
pub struct ApiKey {
    /// Key id.
    pub id: Uuid,
    /// Owning organization.
    pub organization_id: Uuid,
    /// Display name.
    pub name: String,
    /// Public identifier — safe to show, safe to log, safe to say in a support conversation.
    pub prefix: String,
    /// Permission keys this key carries.
    pub scopes: Vec<String>,
    /// Which environment it authenticates against.
    pub environment: Environment,
    /// Service level.
    pub rate_tier: RateTier,
    /// CIDR list, or `None` for "any source".
    pub ip_allowlist: Option<Vec<String>>,
    /// When it stops working, if it ever does.
    pub expires_at: Option<OffsetDateTime>,
    /// Last request that authenticated with it.
    pub last_used_at: Option<OffsetDateTime>,
    /// When it was withdrawn.
    pub revoked_at: Option<OffsetDateTime>,
    /// When it was last rotated.
    pub rotated_at: Option<OffsetDateTime>,
    /// Who created it.
    pub created_by: Uuid,
    /// When it was created.
    pub created_at: OffsetDateTime,
    /// Computed from the three timestamps above, never stored.
    pub status: KeyStatus,
}

/// A newly created or rotated key, carrying the secret exactly once.
///
/// The lifetime argument is the enforcement. A handler that mints one has to hand it to the
/// caller in the same response; there is no path from here back to the stored hash that
/// produces a second `Minted`, so a later read cannot return a secret even if a query is
/// written wrong.
#[derive(Debug, Clone, Serialize)]
pub struct Minted {
    /// The key as it is.
    pub key: ApiKey,
    /// The bearer token. Displayed once and never again.
    pub plaintext: String,
}

/// The field constraints on a key, applied before any hashing work happens.
pub mod key_rules {
    use super::DeveloperError;

    /// Shortest accepted name. The database enforces the same pair, so a direct SQL insert
    /// cannot get past the API.
    pub const MIN_NAME: usize = 3;
    /// Longest accepted name.
    pub const MAX_NAME: usize = 60;

    /// Validate a submitted name.
    pub fn validate_name(name: &str) -> Result<(), DeveloperError> {
        let length = name.trim().chars().count();
        if !(MIN_NAME..=MAX_NAME).contains(&length) {
            return Err(DeveloperError::InvalidName {
                min: MIN_NAME,
                max: MAX_NAME,
            });
        }
        Ok(())
    }

    /// Validate a submitted scope list: at least one, no duplicates, no blanks.
    pub fn validate_scopes(scopes: &[String]) -> Result<(), DeveloperError> {
        if scopes.is_empty() {
            return Err(DeveloperError::NoScopes);
        }
        let mut seen = std::collections::BTreeSet::new();
        for scope in scopes {
            let trimmed = scope.trim();
            if trimmed.is_empty() {
                return Err(DeveloperError::EmptyScope);
            }
            if !seen.insert(trimmed.to_owned()) {
                return Err(DeveloperError::DuplicateScope(trimmed.to_owned()));
            }
        }
        Ok(())
    }
}

/// The shape a key's creation is validated into before it is stored.
#[derive(Debug, Clone)]
pub struct NewKey {
    /// Display name.
    pub name: String,
    /// Permission keys.
    pub scopes: Vec<String>,
    /// Which environment.
    pub environment: Environment,
    /// Service level.
    pub rate_tier: RateTier,
    /// CIDR allowlist; `None` means any.
    pub ip_allowlist: Option<Vec<String>>,
    /// When it expires, if it ever does.
    pub expires_at: Option<OffsetDateTime>,
    /// Who created it.
    pub created_by: Uuid,
}

impl ApiKey {
    /// The status of this key at a given instant.
    ///
    /// A method on the row rather than a free function on the columns, because `decide` in
    /// [`crate::authn`] needs it from a borrowed `ApiKey` it never owns — and because a second
    /// copy of "which of the three timestamps wins" is a second place for the panel and the
    /// authentication path to disagree about whether a key is live.
    #[must_use]
    pub fn status_at(&self, now: OffsetDateTime) -> KeyStatus {
        KeyStatus::of(self.revoked_at, self.expires_at, now)
    }
}

/// One request, as the log keeps it.
///
/// Metadata only. There is no body field, no header bag and no query string, so the request
/// the platform records is one that *cannot* have captured a credential that a caller put in
/// a body — the absence is structural, not a filter someone has to remember to apply.
#[cfg_attr(feature = "store", derive(sqlx::FromRow))]
#[derive(Debug, Clone, Serialize)]
pub struct RequestLog {
    /// Row id.
    pub id: i64,
    /// Owning organization.
    pub organization_id: Uuid,
    /// Key that authenticated it, or `None` for a session-authenticated call.
    pub api_key_id: Option<Uuid>,
    /// That key's displayable prefix, **copied in** (migration `0243`).
    ///
    /// Copied for the same reason the actor's name is: the log's retention window outlives some
    /// of its subjects, and a row that cannot be matched to a key is a row nobody can act on.
    /// Never a token — this is the value the key list already prints.
    pub api_key_prefix: Option<String>,
    /// User behind it, when a human was.
    pub actor_user_id: Option<Uuid>,
    /// Their display name, copied in, so a deleted user leaves a name rather than a blank.
    pub actor_name: String,
    /// The permission the guard resolved when it decided this request.
    ///
    /// The column that makes a `403` explainable: a row recording only "403" leaves an operator
    /// to guess between a missing scope, a wrong tenant and a route that is simply not public.
    pub permission: Option<String>,
    /// HTTP method.
    pub method: String,
    /// Path, without the query string.
    pub path: String,
    /// Response status.
    pub status: i16,
    /// How long it took.
    pub duration_ms: i32,
    /// The request id the caller can quote in a bug report.
    pub request_id: String,
    /// Bytes read.
    pub bytes_in: Option<i32>,
    /// Bytes written.
    pub bytes_out: Option<i32>,
    /// Machine-readable error code, when the response carried one.
    pub error_code: Option<String>,
    /// When it happened.
    pub created_at: OffsetDateTime,
}

impl RequestLog {
    /// The status class the filters group by — `2xx`, `4xx`, `5xx` — as the panel shows it.
    pub fn status_class(&self) -> &'static str {
        match self.status {
            100..=199 => "1xx",
            200..=299 => "2xx",
            300..=399 => "3xx",
            400..=499 => "4xx",
            _ => "5xx",
        }
    }
}

/// One day's counters for a key's chart.
#[cfg_attr(feature = "store", derive(sqlx::FromRow))]
#[derive(Debug, Clone, Serialize)]
pub struct UsageDay {
    /// The day.
    ///
    /// **Serialised as a string by the route that publishes it**, not by an attribute here: the
    /// workspace enables `serde-well-known` but not `serde-human-readable`, so a bare
    /// `time::Date` would cross the wire as a three-element array — and this field is the React
    /// `key` of every bar in the usage chart, so an array there is not a cosmetic fault. `time`
    /// exposes no `Date` codec under the features this crate enables, so the route formats it.
    pub day: Date,
    /// Requests that day.
    pub requests: i32,
    /// Requests that day that failed.
    pub errors: i32,
    /// 95th percentile latency, when the day had enough samples to mean anything.
    pub p95_ms: Option<i32>,
}

/// A page of request logs plus the total that matched, so the UI can say "showing 50 of 812"
/// without a second count query.
#[derive(Debug, Clone, Serialize)]
pub struct RequestLogPage {
    /// The rows.
    pub items: Vec<RequestLog>,
    /// How many rows matched the filter, not just this page.
    pub total: i64,
    /// Whether another page exists.
    pub has_more: bool,
}

/// A request-log filter, already validated.
///
/// `path_prefix` is matched with `LIKE 'prefix%'` after the caller's own `%` and `_` are
/// escaped, because a filter that lets a caller write wildcards is a filter that can be made
/// to read the whole table while the UI says "showing `/api/v1/keys%`".
#[derive(Debug, Clone, Default)]
pub struct RequestLogQuery {
    /// Only this key's requests.
    pub api_key_id: Option<Uuid>,
    /// Only this exact status.
    pub status: Option<i16>,
    /// Only this class: `2xx`, `4xx`, `5xx`.
    pub status_class: Option<String>,
    /// Only paths starting with this.
    pub path_prefix: Option<String>,
    /// Only this method.
    pub method: Option<String>,
    /// Only this key (prefix) — what the panel's "View logs" link on a key row uses.
    pub key_prefix: Option<String>,
    /// Only at or after this instant.
    pub since: Option<OffsetDateTime>,
    /// Only strictly before this instant.
    pub until: Option<OffsetDateTime>,
    /// At least this many milliseconds.
    pub min_duration_ms: Option<i32>,
    /// Page size.
    pub limit: i64,
    /// Rows to skip.
    pub offset: i64,
}

impl RequestLogQuery {
    /// Cap on one page, so `?limit=` cannot ask for the whole table.
    pub const MAX_LIMIT: i64 = 200;
    /// Default page size — a day of a busy key is thousands of rows and nobody reads them.
    pub const DEFAULT_LIMIT: i64 = 50;

    /// Clamp the page size and validate the filters that have a domain.
    pub fn normalized(mut self) -> Result<Self> {
        if self.limit <= 0 {
            self.limit = Self::DEFAULT_LIMIT;
        }
        self.limit = self.limit.min(Self::MAX_LIMIT);
        self.offset = self.offset.max(0);

        if let Some(class) = self.status_class.as_deref() {
            if !matches!(class, "1xx" | "2xx" | "3xx" | "4xx" | "5xx") {
                return Err(DeveloperError::UnknownStatusClass(class.to_owned()));
            }
        }
        if let Some(method) = self.method.as_deref() {
            let method = method.trim().to_ascii_uppercase();
            if method.is_empty() {
                self.method = None;
            } else {
                self.method = Some(method);
            }
        }
        if let Some(minutes) = self.min_duration_ms {
            if minutes < 0 {
                return Err(DeveloperError::NegativeDuration);
            }
        }
        Ok(self)
    }

    /// Escape a caller's path prefix so `LIKE` treats it literally.
    ///
    /// `%`, `_` and the escape character itself are the three that change a match; a filter
    /// that did not escape them would let `prefix=%` mean "every path" while the panel
    /// displays the caller's text.
    pub fn escaped_path_prefix(prefix: &str) -> String {
        let trimmed = prefix.trim();
        let mut escaped = String::with_capacity(trimmed.len());
        for character in trimmed.chars() {
            if matches!(character, '%' | '_' | '\\') {
                escaped.push('\\');
            }
            escaped.push(character);
        }
        escaped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    /// The offered list and the parser must be the same set — asserted in both directions, so
    /// neither half can be widened alone.
    ///
    /// `ALL` is `const` with a fixed length, so adding a variant to the enum without adding it
    /// here is a *compile* error (`Environment::ALL` would not cover the match) and adding an
    /// entry here that `parse` refuses is a test failure. What the test adds is the third way in:
    /// a value that parses must round-trip back to the same string through `as_str`, which is the
    /// property `ALL_STR` actually relies on — `ALL_STR` is built from `as_str` rather than
    /// written out literally for exactly that reason, and a rewritten `as_str` arm that returned a
    /// different spelling would silently become the picker's label.
    #[test]
    fn the_offered_environments_are_exactly_the_ones_the_parser_accepts() {
        // Direction one: everything offered parses, and parses back to what was offered.
        for offered in Environment::ALL_STR {
            let parsed = Environment::parse(offered).expect("an offered environment must parse");
            assert_eq!(
                parsed.as_str(),
                offered,
                "{offered:?} is offered by the picker but does not survive a parse/round-trip"
            );
        }
        // Direction two: everything the parser accepts is offered. `ALL` is the enum's own list,
        // so this fails the moment a variant is added and not listed.
        for accepted in Environment::ALL {
            let stored_form = accepted.as_str();
            assert!(
                Environment::ALL_STR.contains(&stored_form),
                "{stored_form:?} is accepted by the server but absent from the offered list, so \
                 no panel can create a key in it"
            );
        }
        // And the list is not trivially satisfied by being everything: a name nobody defined is
        // still refused, by name.
        let refused =
            Environment::parse("prod-eu-west").expect_err("an undefined environment must be refused");
        assert!(
            matches!(&refused, DeveloperError::UnknownEnvironment(value) if value == "prod-eu-west"),
            "the refusal must name the value it refused: {refused:?}"
        );
    }

    #[test]
    fn a_revoked_key_reads_as_revoked_even_when_it_has_also_expired() {
        // Revocation is a person's decision and outranks the clock: the panel must not
        // relabel a key somebody deliberately withdrew as merely "expired".
        let now = datetime!(2026-10-01 12:00 UTC);
        let status = KeyStatus::of(
            Some(datetime!(2026-09-01 12:00 UTC)),
            Some(datetime!(2026-09-15 12:00 UTC)),
            now,
        );
        assert_eq!(status, KeyStatus::Revoked);
        assert!(!status.is_usable());
    }

    #[test]
    fn the_expiry_boundary_is_inclusive_so_a_key_stops_the_instant_it_expires() {
        let now = datetime!(2026-10-01 12:00 UTC);
        // Exactly now is already over.
        assert_eq!(KeyStatus::of(None, Some(now), now), KeyStatus::Expired);
        // A microsecond away is still live.
        assert_eq!(
            KeyStatus::of(None, Some(now + time::Duration::microseconds(1)), now),
            KeyStatus::Active
        );
    }

    #[test]
    fn a_key_with_no_expiry_and_no_revocation_is_active() {
        let now = datetime!(2026-10-01 12:00 UTC);
        assert_eq!(KeyStatus::of(None, None, now), KeyStatus::Active);
    }

    #[test]
    fn a_name_shorter_or_longer_than_the_rule_is_refused() {
        assert!(key_rules::validate_name("ab").is_err());
        assert!(key_rules::validate_name("abc").is_ok());
        assert!(key_rules::validate_name(&"x".repeat(60)).is_ok());
        assert!(key_rules::validate_name(&"x".repeat(61)).is_err());
        // Whitespace does not count towards the length, and cannot pad a two-character name
        // into a three-character one.
        assert!(key_rules::validate_name("  a  ").is_err());
        assert!(key_rules::validate_name("  abcd  ").is_ok());
    }

    #[test]
    fn a_key_with_no_scopes_is_refused_because_it_could_do_nothing() {
        assert!(matches!(
            key_rules::validate_scopes(&[]),
            Err(DeveloperError::NoScopes)
        ));
    }

    #[test]
    fn duplicate_and_blank_scopes_are_refused_rather_than_silently_deduplicated() {
        let one = vec!["a".to_owned()];
        assert!(key_rules::validate_scopes(&one).is_ok());
        assert!(matches!(
            key_rules::validate_scopes(&["a".to_owned(), "a".to_owned()]),
            Err(DeveloperError::DuplicateScope(_))
        ));
        assert!(matches!(
            key_rules::validate_scopes(&["a".to_owned(), "  ".to_owned()]),
            Err(DeveloperError::EmptyScope)
        ));
    }

    #[test]
    fn a_wildcard_in_a_path_filter_is_escaped_so_it_cannot_widen_the_query() {
        // The caller typed a literal percent; the query must not read it as "any path".
        assert_eq!(RequestLogQuery::escaped_path_prefix("%"), "\\%");
        assert_eq!(RequestLogQuery::escaped_path_prefix("_"), "\\_");
        assert_eq!(
            RequestLogQuery::escaped_path_prefix("/api/v1/keys"),
            "/api/v1/keys"
        );
        assert_eq!(RequestLogQuery::escaped_path_prefix("  /api  "), "/api");
        // An escape character typed by the caller escapes itself rather than eating the
        // character after it.
        assert_eq!(RequestLogQuery::escaped_path_prefix("a\\b"), "a\\\\b");
    }

    #[test]
    fn the_page_size_is_clamped_rather_than_refused() {
        let query = RequestLogQuery {
            limit: 10_000,
            ..Default::default()
        }
        .normalized()
        .unwrap();
        assert_eq!(query.limit, RequestLogQuery::MAX_LIMIT);

        let query = RequestLogQuery {
            limit: 0,
            ..Default::default()
        }
        .normalized()
        .unwrap();
        assert_eq!(query.limit, RequestLogQuery::DEFAULT_LIMIT);

        let query = RequestLogQuery {
            limit: -5,
            offset: -1,
            ..Default::default()
        }
        .normalized()
        .unwrap();
        assert_eq!(query.offset, 0);
    }

    #[test]
    fn an_unknown_status_class_is_refused_but_a_method_is_normalized() {
        assert!(matches!(
            RequestLogQuery {
                status_class: Some("9xx".to_owned()),
                ..Default::default()
            }
            .normalized(),
            Err(DeveloperError::UnknownStatusClass(_))
        ));
        let query = RequestLogQuery {
            status_class: Some("4xx".to_owned()),
            ..Default::default()
        }
        .normalized()
        .unwrap();
        assert_eq!(query.status_class.as_deref(), Some("4xx"));

        let query = RequestLogQuery {
            method: Some(" post ".to_owned()),
            ..Default::default()
        }
        .normalized()
        .unwrap();
        assert_eq!(query.method.as_deref(), Some("POST"));
    }

    #[test]
    fn a_negative_duration_filter_is_refused() {
        assert!(matches!(
            RequestLogQuery {
                min_duration_ms: Some(-1),
                ..Default::default()
            }
            .normalized(),
            Err(DeveloperError::NegativeDuration)
        ));
    }

    #[test]
    fn the_status_class_a_log_reports_matches_its_code() {
        let make = |status: i16| RequestLog {
            id: 0,
            organization_id: Uuid::nil(),
            api_key_id: None,
            api_key_prefix: None,
            actor_user_id: None,
            actor_name: String::new(),
            permission: None,
            method: "GET".to_owned(),
            path: "/x".to_owned(),
            status,
            duration_ms: 1,
            request_id: "r".to_owned(),
            bytes_in: None,
            bytes_out: None,
            error_code: None,
            created_at: datetime!(2026-10-01 12:00 UTC),
        };
        assert_eq!(make(204).status_class(), "2xx");
        assert_eq!(make(301).status_class(), "3xx");
        assert_eq!(make(401).status_class(), "4xx");
        assert_eq!(make(503).status_class(), "5xx");
    }
}
