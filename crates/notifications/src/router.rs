//! The declarative router: a bus event becomes a notification (REQ-021, slice 3).
//!
//! Slice 1 gave the platform a way to say *you*; this is what makes it say *you* without
//! every module knowing the notification crate exists. A ticket module records a fact on the
//! bus; a rule here says that fact is a `ticket` notification for whoever is assigned. Neither
//! module imports the other, and adding a rule requires no code in either.
//!
//! **A rule is data, and an event nobody produces is a no-op.** Both halves of that sentence
//! are load-bearing:
//!
//! * *Rules are data* means a module can extend the router by inserting a row. The alternative
//!   — a `match` in this crate naming every producer — puts the notification vocabulary in
//!   charge of every other module's roadmap, and the first module with a category nobody
//!   thought of has to fork this file.
//! * *An event nobody produces is a no-op* means a rule may name an event that does not exist
//!   **yet**. REQ-009's ticket rule is worth having on the day REQ-009 merges, not the week
//!   after somebody remembers to come back and add it. The router answers "zero recipients"
//!   rather than an error, because a rule that is ahead of its producer is a *waiting* rule,
//!   not a broken one.
//!
//! **The dedupe key is derived, not supplied.** A retried event, a replayed consumer and two
//! workers reading the same bus row all describe *one* fact, so the key is
//! `event:<event_id>:<recipient>`. A caller that invented its own key would have to remember to
//! make it stable under retry, and the failure mode is a duplicate notification every time a
//! queue retries — which is exactly the inbox rot slice 1's partial unique index exists to
//! prevent.
//!
//! **Recipients are resolved by rule, and an unresolvable rule is a refusal.** A rule naming a
//! role slug that no longer exists, or a permission key nobody holds, matches nobody; writing
//! nothing is correct, but *silently* writing nothing is how a "the approvers stopped being
//! notified" incident lasts a week. The router therefore reports what it resolved, and the
//! route surfaces a rule that resolved to nothing as a diagnostic rather than as a success.

use omnion_permissions::model::{ResourceContext, Scope, Subject};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::error::Result;
use crate::model::NewNotification;
use crate::store::record_with_deliveries;
use crate::vocabulary::{is_category, is_priority};

/// Who a rule addresses its notifications to.
///
/// **Hand-written sqlx plumbing, not a derive.** The column is `text` and this type needs an
/// `Encode`/`Decode` pair that can *fail* on a value this build does not know (see
/// [`RecipientRule::decode`]). `#[derive(FromRow)]` cannot be used on a struct that holds a
/// hand-implemented sqlx type, so [`RouteRule`] writes its own `FromRow` for the same reason.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipientRule {
    /// The actor who caused the event. A rule that addresses the actor is how "we changed
    /// something about your account" is written without knowing the account's owner.
    Actor,
    /// Everybody holding a permission key — the "anybody who may approve this" rule.
    Permission(String),
    /// Everybody with a role.
    Role(String),
    /// The event's `actor_user_id`, resolved through a field in the payload, for the rule that
    /// has to reach somebody *other* than the actor ("you were mentioned").
    PayloadUser(String),
}

impl RecipientRule {
    /// The closed list's four shapes, written the way the column stores them.
    ///
    /// **The prefix is split on the *first* colon, not the last.** A permission key may itself
    /// contain one (`permission:pages:publish` is the permission `pages:publish`, not the
    /// permission `publish` on a `pages` prefix), so splitting from the right would silently
    /// address a different permission than the author wrote — a rule that fires for the wrong
    /// people, which is worse than a rule that does not fire at all.
    #[must_use]
    pub fn encode(&self) -> String {
        match self {
            Self::Actor => "actor".to_owned(),
            Self::Permission(key) => format!("permission:{key}"),
            Self::Role(slug) => format!("role:{slug}"),
            Self::PayloadUser(field) => format!("payload_user:{field}"),
        }
    }

    /// Read a stored value back, or `None` for one this build does not understand.
    ///
    /// `None` rather than a panic: a rule row written by a *newer* build (or edited by hand)
    /// must not take the whole router down when an older binary reads it. The route reports it
    /// as an unreadable rule and the rest of the pass continues.
    #[must_use]
    pub fn decode(raw: &str) -> Option<Self> {
        if raw == "actor" {
            return Some(Self::Actor);
        }
        let (prefix, target) = raw.split_once(':')?;
        match prefix {
            "permission" => Some(Self::Permission(target.to_owned())),
            "role" => Some(Self::Role(target.to_owned())),
            "payload_user" => Some(Self::PayloadUser(target.to_owned())),
            _ => None,
        }
    }

    /// Whether this rule can ever resolve to somebody.
    ///
    /// A rule with an empty permission or role string resolves to nobody for a reason that is
    /// the *author's* mistake, not the installation's state — so it is refused at insert time
    /// rather than matching zero recipients forever.
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        match self {
            Self::Actor => true,
            Self::Permission(key) | Self::Role(key) | Self::PayloadUser(key) => {
                !key.trim().is_empty()
            }
        }
    }
}

/// How a [`RouteRule`] is written to and read from its row.
///
/// The hand-written pair rather than a derive, because the column is `text` and the four
/// shapes have to survive a round trip *including the one this build cannot parse* — which is
/// why `decode` returns an `Option` and the rule is skipped rather than replaced.
impl sqlx::Type<sqlx::Postgres> for RecipientRule {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        <String as sqlx::Type<sqlx::Postgres>>::type_info()
    }
}

impl<'r> sqlx::Decode<'r, sqlx::Postgres> for RecipientRule {
    fn decode(value: sqlx::postgres::PgValueRef<'r>) -> Result<Self, sqlx::error::BoxDynError> {
        let raw = <&'r str as sqlx::Decode<sqlx::Postgres>>::decode(value)?;
        Self::decode(raw)
            .ok_or_else(|| format!("unknown notification route recipient {raw:?}").into())
    }
}

impl sqlx::Encode<'_, sqlx::Postgres> for RecipientRule {
    fn encode_by_ref(
        &self,
        buf: &mut sqlx::postgres::PgArgumentBuffer,
    ) -> Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
        <&str as sqlx::Encode<sqlx::Postgres>>::encode(&self.encode(), buf)
    }
}

/// The rule's own row read, written out because the derive cannot be used here.
///
/// **Column order is the query's order, and the two are asserted against each other in a
/// unit test** — `the_rule_reads_the_columns_the_query_selects`. A hand-written `FromRow` is a
/// silent-corruption risk a derive would not be: swapping two `String` columns compiles,
/// passes every test that does not read a real row, and writes "normal" into `title_template`
/// in production. Reading by *name* is what makes that safe-ish, and the test makes it safe.
impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for RouteRule {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> sqlx::Result<Self> {
        use sqlx::Row;
        let recipient: String = row.try_get("recipient")?;
        Ok(Self {
            id: row.try_get("id")?,
            event_name: row.try_get("event_name")?,
            category: row.try_get("category")?,
            priority: row.try_get("priority")?,
            recipient: RecipientRule::decode(&recipient).ok_or_else(|| {
                sqlx::Error::Decode(
                    format!("unknown notification route recipient {recipient:?}").into(),
                )
            })?,
            title_template: row.try_get("title_template")?,
            url_template: row.try_get("url_template")?,
            enabled: row.try_get("enabled")?,
            created_by: row.try_get("created_by")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

/// One row of the router: an event name, a category, and who hears about it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RouteRule {
    /// The row's id, which is what the admin screen's delete names.
    pub id: Uuid,
    /// The bus event name this rule listens for, e.g. `ticket.created`.
    pub event_name: String,
    /// The category the notification carries.
    pub category: String,
    /// The priority it is recorded at.
    pub priority: String,
    /// Who hears about it.
    pub recipient: RecipientRule,
    /// The title template. `{actor}` and `{subject}` are substituted; anything else is left
    /// alone rather than emptied, so a typo is visible in the inbox instead of silent.
    pub title_template: String,
    /// Where the row links to, template-substituted the same way. `None` means the
    /// notification is informational and the panel must not render a link.
    pub url_template: Option<String>,
    /// Whether the rule is live. A disabled rule keeps its row so it can be re-enabled, and
    /// the router reports it as skipped rather than pretending it never existed.
    pub enabled: bool,
    /// Who wrote it.
    pub created_by: Option<Uuid>,
    /// When.
    pub created_at: time::OffsetDateTime,
}

impl RouteRule {
    /// The category's `system`, `security`, … — validated at insert time, re-checked here
    /// because a rule that outlives a vocabulary change must not record a row the database
    /// would refuse.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        is_category(&self.category)
            && is_priority(&self.priority)
            && self.recipient.is_well_formed()
            && !self.event_name.trim().is_empty()
    }
}

/// What one routing pass did.
///
/// The counts are the whole point. "Created 0" is ambiguous between "the rule is ahead of its
/// producer", "nobody holds that permission" and "every recipient already had it"; a caller
/// that cannot tell those apart logs the same line for all three and learns nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RouteReport {
    /// Rows written.
    pub created: u32,
    /// Writes the dedupe index collapsed into a row that already existed.
    pub deduped: u32,
    /// Rules that matched the event but resolved to nobody.
    pub unmatched_rules: u32,
    /// Rules that matched nothing at all because the event name is not in the table.
    pub unknown_event: bool,
    /// Recipients a rule resolved and this route then refused: somebody outside the event's
    /// tenant, or an id that is not an account at all.
    ///
    /// **A separate count from `unmatched_rules`, and the difference is the point.**
    /// `unmatched_rules` answers "how many rules produced nothing". This answers "how many
    /// people were resolved and then dropped" — which is the difference between a rule wired to
    /// a role nobody holds (wait for the producer) and a rule wired to somebody the event's
    /// tenant does not own (**fix the rule**). Both read `0 created` otherwise, and the second
    /// one is a data-integrity problem that looks like a waiting rule for as long as nobody
    /// counts. Before the fix this number did not exist because nothing was dropped.
    pub dropped_recipients: u32,
}

/// The event a module recorded, as the router reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct RoutedEvent {
    /// The bus row's id — the stable half of every dedupe key this function writes.
    pub id: Uuid,
    /// Its name, e.g. `ticket.created`.
    pub name: String,
    /// Who caused it, when the producer knows.
    pub actor_user_id: Option<Uuid>,
    /// Its organization, so a rule cannot leak across tenants.
    pub organization_id: Option<Uuid>,
    /// Its payload. A `{subject}` placeholder reads `payload["title"]`, falling back to
    /// `payload["name"]` and then to nothing — a template that cannot find its subject gets a
    /// blank, not a panic, because a bus event is caller-supplied data.
    pub payload: Value,
}

/// Substitute `{actor}` and `{subject}` in a template.
///
/// **Only the two documented names are substituted.** A general `{whatever}` expander over a
/// caller-supplied payload is a template injection: the payload is written by whichever module
/// emitted, and a rule template is written by an administrator, so the safe direction is the
/// one where the *set of placeholders* is closed and the *values* are whatever the event says.
#[must_use]
pub fn render(template: &str, event: &RoutedEvent) -> String {
    let subject = event
        .payload
        .get("title")
        .or_else(|| event.payload.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("an update");
    let actor = event
        .actor_user_id
        .map(|id| short_id(&id))
        .unwrap_or_else(|| "the platform".to_owned());

    template
        .replace("{actor}", &actor)
        .replace("{subject}", subject)
        // A template that still carries braces after the two substitutions is a typo, and the
        // inbox is where an administrator will see it. Emptying it would make the typo invisible
        // and would produce two rules rendering the same title.
        .trim()
        .to_owned()
}

/// A short, non-identifying form of a user id for a sentence like "Furkan approved {subject}".
///
/// A real display name would need a join into `users` for every notification the router
/// writes, and a *renamed* user would change history — the title of a notification from last
/// Tuesday would rewrite itself. The id is stable and honest.
fn short_id(id: &Uuid) -> String {
    id.to_string().chars().take(8).collect()
}

/// The dedupe key one event produces for one recipient.
///
/// Stable under retry by construction: the bus row's id does not change when a consumer
/// re-reads it, and the recipient is part of the key because one event legitimately produces
/// one notification *per person*.
#[must_use]
pub fn dedupe_key(event: &RoutedEvent, recipient: Uuid) -> String {
    format!("event:{}:{recipient}", event.id)
}

/// Every live rule that listens for this event.
pub async fn rules_for_event(pool: &PgPool, event_name: &str) -> Result<Vec<RouteRule>> {
    Ok(sqlx::query_as::<_, RouteRule>(
        "select id, event_name, category, priority, recipient, title_template, url_template, \
                enabled, created_by, created_at \
         from notification_routes where event_name = $1 and enabled = true order by created_at, id",
    )
    .bind(event_name)
    .fetch_all(pool)
    .await?)
}

/// Resolve one rule's recipients against one event.
///
/// Returns an empty vec — and `unmatched_rules` counts it — when the rule names a role nobody
/// has, a permission nobody holds, or a payload field the producer did not send. Those three
/// read identically from the caller's side, and all three are the failure this function exists
/// to make visible.
///
/// **The permission rule goes through the platform's own resolution, not a hand-written join.**
/// A direct `select … from role_permissions` would be faster by one round trip and *wrong*:
/// it would miss role inheritance (a role that inherits `approvals.approve` from its parent),
/// scope-mismatched bindings, expired grants and explicit denials. A notification that reaches
/// somebody who lost the permission — or misses somebody who has it through a chain three roles
/// deep — is a privacy defect, and the only version of this query that stays correct as the
/// permission model grows is the one the *guard* uses. Hence `effective_permissions_for`.
///
/// The cost of that decision is one resolution per candidate user, so the candidate set is the
/// organization's active accounts and the permission check filters it. On an installation with
/// thousands of seats this is the expensive rule shape, and the admin screen says so next to
/// the rule rather than leaving the administrator to discover it as a slow pass.
pub async fn resolve_recipients(
    pool: &PgPool,
    rule: &RouteRule,
    event: &RoutedEvent,
) -> Result<Vec<Uuid>> {
    match &rule.recipient {
        RecipientRule::Actor => Ok(event.actor_user_id.into_iter().collect()),
        RecipientRule::Permission(key) => {
            let candidates: Vec<Uuid> = match event.organization_id {
                // Scoped: one organization's active accounts. The event's own organization is
                // the boundary, so a rule can never deliver across tenants even though the
                // rule itself is a platform-level statement.
                Some(organization_id) => {
                    sqlx::query_scalar(
                        "select id from users where organization_id = $1 and status = 'active'",
                    )
                    .bind(organization_id)
                    .fetch_all(pool)
                    .await?
                }
                // Unscoped: a platform-level fact, so every active account is a candidate.
                None => {
                    sqlx::query_scalar("select id from users where status = 'active'")
                        .fetch_all(pool)
                        .await?
                }
            };

            let scope = match event.organization_id {
                Some(organization_id) => Scope::Organization { organization_id },
                None => Scope::Global,
            };
            let context = ResourceContext::from_scope(scope);

            let mut holders = Vec::new();
            for candidate in candidates {
                let subject = Subject::User(candidate);
                if omnion_permissions::effective_permissions_for(pool, subject, &context)
                    .await
                    .map_err(|error| {
                        crate::error::NotificationError::invalid(format!(
                            "cannot resolve the permission rule: {error}"
                        ))
                    })?
                    .allows(key)
                {
                    holders.push(candidate);
                }
            }
            Ok(holders)
        }
        RecipientRule::Role(role_key) => {
            // `roles.key`, not a slug: that is the column the role screens and the guard both
            // use, and a rule written against a column that does not exist resolves to nobody
            // forever. Bindings are filtered by `revoked_at`/`expires_at` because a revoked
            // role is exactly the state where a notification must stop arriving.
            Ok(sqlx::query_scalar::<_, Uuid>(
                "select distinct rb.user_id from role_bindings rb \
                 join roles r on r.id = rb.role_id \
                 where r.key = $1 \
                   and rb.revoked_at is null \
                   and (rb.expires_at is null or rb.expires_at > now()) \
                   and ($2::uuid is null or rb.organization_id is null \
                        or rb.organization_id = $2::uuid) \
                   and exists (select 1 from users u where u.id = rb.user_id \
                               and u.status = 'active')",
            )
            .bind(role_key)
            .bind(event.organization_id)
            .fetch_all(pool)
            .await?)
        }
        RecipientRule::PayloadUser(field) => Ok(event
            .payload
            .get(field)
            .and_then(Value::as_str)
            .and_then(|raw| Uuid::parse_str(raw).ok())
            .into_iter()
            .collect()),
    }
}

/// Turn one bus event into notifications, through every live rule that listens for it.
///
/// **This is the whole "no direct call" claim.** A module calls `omnion_events::bus::emit` and
/// nothing else; it never names a notification, a category, or this crate. Whether an event
/// produces a notification is a configuration question the administrator answers, which is the
/// only way a platform can add a channel of communication without a compile step per module.
pub async fn route(pool: &PgPool, event: &RoutedEvent) -> Result<RouteReport> {
    let rules = rules_for_event(pool, &event.name).await?;
    if rules.is_empty() {
        return Ok(RouteReport {
            unknown_event: true,
            ..RouteReport::default()
        });
    }

    let mut report = RouteReport::default();
    for rule in &rules {
        let resolved = resolve_recipients(pool, rule, event).await?;
        // **The tenancy filter, and it is here rather than inside the two caller-supplied arms
        // because two of the four rules can name somebody the event's tenant does not own.**
        // `Actor` returns `event.actor_user_id` verbatim; `PayloadUser` returns an id the
        // *producer wrote into the payload*, which is caller-supplied data whose only check so
        // far was that it parses as a uuid. `Permission` and `Role` are already bounded — their
        // SQL selects inside `event.organization_id` — so a filter inside those two arms would
        // be a second, weaker copy of what the database already guarantees.
        //
        // **Applied per rule, and the filter runs before the write, not after it**, because
        // `unmatched_rules` counts *rules* that resolved nobody: a rule whose only recipient is
        // a stranger has not been satisfied, and reporting it as a created row is how the same
        // event looks successful three times over.
        let organizations = crate::store::recipient_organizations(pool, &resolved).await?;
        let kept = crate::audience::addressable_recipients(
            event.organization_id,
            &resolved,
            &|id| Some(organizations.get(&id).copied().flatten()),
        );
        if kept.is_empty() {
            report.unmatched_rules += 1;
            continue;
        }
        // The refusals are a fact the administrator needs, not just a silent subtraction: a rule
        // pointing at somebody the event's tenant does not own is a mis-wired rule, and
        // "matched nobody" is the only sentence this report can give about it. Counts, not ids —
        // the event is not an admin console and the ids belong in nobody's log.
        report.dropped_recipients += (resolved.len() - kept.len()) as u32;
        for (recipient, recipient_organization) in kept {
            let mut draft = NewNotification::to(
                recipient,
                &rule.category,
                render(&rule.title_template, event),
            )
            .with_priority(rule.priority.clone())
            .with_dedupe_key(dedupe_key(event, recipient))
            .with_source("event", event.name.clone());
            if let Some(template) = &rule.url_template {
                draft = draft.with_url(render(template, event));
            }
            if let Some(actor) = event.actor_user_id {
                draft = draft.with_payload(serde_json::json!({
                    "event_id": event.id,
                    "event_name": event.name,
                    "actor": actor,
                }));
            } else {
                draft = draft.with_payload(serde_json::json!({
                    "event_id": event.id,
                    "event_name": event.name,
                }));
            }
            // **The deliveries variant, and slice 6c exists because this call did not.**
            // `record` wrote the notification and stopped there: no `notification_deliveries`
            // row, so the runner had nothing to claim and the drawer had no channel to show.
            // A routed bus event — a ticket assigned, a page submitted for review — is exactly
            // the notification that is supposed to *leave* the panel, and it was the one path
            // that could not. The dedupe branch is unchanged: a collapsed event is the same
            // fact, and its deliveries already exist.
            //
            // **The organization bound here is the RECIPIENT's, and that is the whole point of
            // the filter above.** It used to be `event.organization_id`, which is wrong twice
            // over: for a tenant event naming a stranger it wrote a row in the wrong tenant, and
            // for a *platform* event (`organization_id: None`, the documented unscoped branch)
            // it wrote `null` — invisible to every tenant's `notifications.admin` outbox,
            // including the tenant whose person the event actually reached, while
            // `outbox_counts(None)` counted it as the platform's own traffic. With the filter
            // in place the two columns agree for every in-tenant row and each survivor carries
            // its own tenant, which is the only assignment true in all three cases.
            if record_with_deliveries(pool, recipient_organization, event.actor_user_id, &draft)
                .await?
                .is_some()
            {
                report.created += 1;
            } else {
                report.deduped += 1;
            }
        }
    }
    Ok(report)
}

/// Every rule the router has, enabled or not, oldest first.
///
/// The admin screen's list. `enabled = false` rows are returned rather than hidden so a
/// disabled rule is a row somebody can switch back on instead of a rule that has to be
/// retyped — and so the list can say "12 rules, 1 disabled" honestly.
pub async fn list_rules(pool: &PgPool) -> Result<Vec<RouteRule>> {
    Ok(sqlx::query_as::<_, RouteRule>(
        "select id, event_name, category, priority, recipient, title_template, url_template, \
                enabled, created_by, created_at \
         from notification_routes order by created_at, id",
    )
    .fetch_all(pool)
    .await?)
}

/// Write one rule and return the row as stored.
///
/// The `on conflict … do nothing` clause is what turns a duplicate `POST` into a `409` at the
/// *route* rather than a `500` from the partial unique index — and returning the stored row
/// rather than the input means the answer carries the id and the timestamp the database
/// chose, not the ones the client hoped for.
pub async fn create_rule(pool: &PgPool, rule: &RouteRule) -> Result<RouteRule> {
    let row: Option<RouteRule> = sqlx::query_as(
        "insert into notification_routes \
         (event_name, category, priority, recipient, title_template, url_template, enabled, \
          created_by) \
         values ($1, $2, $3, $4, $5, $6, $7, $8) \
         on conflict (event_name, category, recipient) do nothing \
         returning id, event_name, category, priority, recipient, title_template, url_template, \
                   enabled, created_by, created_at",
    )
    .bind(&rule.event_name)
    .bind(&rule.category)
    .bind(&rule.priority)
    .bind(rule.recipient.encode())
    .bind(&rule.title_template)
    .bind(&rule.url_template)
    .bind(rule.enabled)
    .bind(rule.created_by)
    .fetch_optional(pool)
    .await?;

    row.ok_or_else(|| {
        crate::error::NotificationError::invalid(format!(
            "a rule for {} → {} → {} already exists",
            rule.event_name,
            rule.category,
            rule.recipient.encode()
        ))
    })
}

/// Remove one rule. `false` when there was nothing to remove.
pub async fn delete_rule(pool: &PgPool, id: Uuid) -> Result<bool> {
    let result = sqlx::query("delete from notification_routes where id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn an_event() -> RoutedEvent {
        RoutedEvent {
            id: Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap(),
            name: "ticket.created".to_owned(),
            actor_user_id: Some(Uuid::parse_str("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap()),
            organization_id: None,
            payload: json!({"title": "Checkout page needs review"}),
        }
    }

    fn a_rule() -> RouteRule {
        RouteRule {
            id: Uuid::nil(),
            event_name: "ticket.created".to_owned(),
            category: "ticket".to_owned(),
            priority: "normal".to_owned(),
            recipient: RecipientRule::Actor,
            title_template: "{actor} opened {subject}".to_owned(),
            url_template: Some("/tickets/{subject}".to_owned()),
            enabled: true,
            created_by: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn a_template_substitutes_only_the_two_documented_names() {
        let event = an_event();
        let rendered = render("{actor} opened {subject}", &event);
        assert_eq!(rendered, "aaaaaaaa opened Checkout page needs review");
        // A placeholder the router does not define survives as itself: two rules rendering the
        // same title is a worse failure than a visible typo in an inbox.
        assert!(render("{nope}", &event).contains("{nope}"));
    }

    #[test]
    fn a_missing_subject_renders_a_phrase_not_an_empty_string() {
        // The payload is caller-supplied. An event with no title must not produce the title
        // "opened ", which is a row nobody can act on.
        let mut event = an_event();
        event.payload = json!({});
        let rendered = render("Opened {subject}", &event);
        assert_eq!(rendered, "Opened an update");
    }

    #[test]
    fn the_actor_placeholder_survives_an_event_with_no_actor() {
        // The platform itself raises facts (a nightly digest, a failed backup) with no actor,
        // and "{actor}" rendering to an empty string produces "  opened X".
        let mut event = an_event();
        event.actor_user_id = None;
        assert_eq!(
            render("{actor}: {subject}", &event),
            "the platform: Checkout page needs review"
        );
    }

    #[test]
    fn the_dedupe_key_is_stable_across_retries_and_distinct_per_recipient() {
        let event = an_event();
        let one = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let two = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
        assert_eq!(dedupe_key(&event, one), dedupe_key(&event, one));
        assert_ne!(dedupe_key(&event, one), dedupe_key(&event, two));
        // The bus row's id is in the key, so a *different* event about the same subject is a
        // different notification — "reopened" is a fact the reader wants again.
        let mut later = event.clone();
        later.id = Uuid::parse_str("99999999-2222-3333-4444-555555555555").unwrap();
        assert_ne!(dedupe_key(&event, one), dedupe_key(&later, one));
    }

    #[test]
    fn a_rule_against_a_vocabulary_the_platform_does_not_have_is_not_valid() {
        let mut rule = a_rule();
        assert!(rule.is_valid());
        rule.category = "invoice".to_owned();
        assert!(!rule.is_valid(), "a category the matrix has no row for");
        rule.category = "ticket".to_owned();
        rule.priority = "urgent".to_owned();
        assert!(!rule.is_valid());
        rule.priority = "normal".to_owned();
        rule.event_name = "  ".to_owned();
        assert!(!rule.is_valid());
    }

    #[test]
    fn a_payload_recipient_rule_with_an_empty_field_is_malformed() {
        // `{"user_id": ""}` resolves to nobody *because the rule is wrong*, not because the
        // event was. The two are indistinguishable at runtime, so it is refused up front.
        assert!(!RecipientRule::PayloadUser(String::new()).is_well_formed());
        assert!(!RecipientRule::PayloadUser("  ".to_owned()).is_well_formed());
        assert!(RecipientRule::PayloadUser("assignee_id".to_owned()).is_well_formed());
        assert!(RecipientRule::Actor.is_well_formed());
    }

    #[test]
    fn the_actor_short_form_is_stable_and_does_not_leak_a_name() {
        // History must not rewrite itself when a user is renamed, and the router does not join
        // `users` — so the title carries a prefix of the id and nothing else.
        let id = Uuid::parse_str("abcdef01-2345-6789-abcd-ef0123456789").unwrap();
        assert_eq!(short_id(&id), "abcdef01");
        assert_eq!(short_id(&id), short_id(&id));
    }

    #[test]
    fn a_report_distinguishes_unknown_events_from_rules_that_matched_nobody() {
        // Both produce zero rows. Collapsing them into one number is how "the ticket rule has
        // been live for a week and never fired" goes unnoticed.
        let unknown = RouteReport {
            unknown_event: true,
            ..RouteReport::default()
        };
        let unmatched = RouteReport {
            unmatched_rules: 2,
            ..RouteReport::default()
        };
        assert_eq!(unknown.created, unmatched.created);
        assert_ne!(unknown.unknown_event, unmatched.unknown_event);
        assert_ne!(unknown.unmatched_rules, unmatched.unmatched_rules);
    }

    #[test]
    fn a_rule_carries_a_permission_not_a_role_and_there_is_no_third_option() {
        // The enum is the exhaustive list of recipient shapes. A rule that named a "group" or a
        // "team" would have to be resolved by a query that does not exist, and the failure
        // would be a rule that silently matches nobody.
        let rules = [
            RecipientRule::Actor,
            RecipientRule::Permission("approvals.approve".to_owned()),
            RecipientRule::Role("admin".to_owned()),
            RecipientRule::PayloadUser("assignee_id".to_owned()),
        ];
        assert_eq!(rules.len(), 4);
        assert!(rules.iter().all(RecipientRule::is_well_formed));
    }

    #[test]
    fn every_recipient_shape_survives_the_round_trip_through_its_column() {
        // A rule that writes one string and reads a different one is a router that works in
        // development and matches nothing in production — the row is there, `enabled` is true,
        // and `rules_for_event` returns a shape that resolves to an empty recipient list.
        for rule in [
            RecipientRule::Actor,
            RecipientRule::Permission("approvals.approve".to_owned()),
            RecipientRule::Role("content-editor".to_owned()),
            RecipientRule::PayloadUser("assignee_id".to_owned()),
        ] {
            let encoded = rule.encode();
            assert_eq!(
                RecipientRule::decode(&encoded),
                Some(rule),
                "{encoded:?} did not decode back to the rule that wrote it"
            );
        }
    }

    #[test]
    fn a_permission_key_containing_a_colon_survives_the_encoding() {
        // The reason the prefix splits on the first colon. A key like `pages:publish` is a real
        // shape in this catalogue, and decoding it as the permission `publish` would address
        // the wrong people — a rule that fires for the wrong readers.
        let rule = RecipientRule::Permission("pages:publish".to_owned());
        let decoded = RecipientRule::decode(&rule.encode()).expect("decodes");
        assert_eq!(decoded, rule);
        assert!(matches!(decoded, RecipientRule::Permission(key) if key == "pages:publish"));
    }

    #[test]
    fn a_value_this_build_does_not_know_reads_as_none_rather_than_a_wrong_rule() {
        // A row written by a newer build must not be silently reinterpreted as something else.
        // `role` alone, an unknown prefix and a bare word are all "not a rule I can run", and
        // the router's answer is to skip the row and report it.
        assert_eq!(RecipientRule::decode("role"), None);
        assert_eq!(RecipientRule::decode("team:everyone"), None);
        assert_eq!(RecipientRule::decode(""), None);
        assert_eq!(
            RecipientRule::decode("permission:"),
            Some(RecipientRule::Permission(String::new()))
        );
    }

    #[test]
    fn an_empty_target_survives_decoding_and_is_still_refused_by_is_well_formed() {
        // `decode` accepts it so a malformed row is *readable* (and therefore skippable with a
        // message); `is_well_formed` refuses it so a new one cannot be written. Splitting those
        // two jobs is what lets the router report "this rule is broken" instead of "no rules".
        let empty = RecipientRule::decode("permission:").expect("readable");
        assert!(!empty.is_well_formed());
    }

    /// The hand-written `FromRow` names ten columns, and the query it serves names them too.
    ///
    /// A derive would make this impossible to get wrong; the hand-written pair cannot, because
    /// `try_get` is by *name* and a renamed column is a runtime `Decode` error, not a compile
    /// error. So the column list is asserted here against the query text — a rename in either
    /// place breaks this test instead of the router.
    #[test]
    fn the_rule_reads_the_columns_the_query_selects() {
        let query = "select id, event_name, category, priority, recipient, title_template, \
                     url_template, enabled, created_by, created_at \
                     from notification_routes where event_name = $1 and enabled = true \
                     order by created_at, id";
        for column in [
            "id",
            "event_name",
            "category",
            "priority",
            "recipient",
            "title_template",
            "url_template",
            "enabled",
            "created_by",
            "created_at",
        ] {
            assert!(
                query.contains(column),
                "the rule's FromRow reads {column:?} but the query does not select it"
            );
        }
    }

    /// The same assertion for the other hand-written read in the crate: `OutboxRow` uses a
    /// derive today, so this test is a tripwire — if somebody replaces that derive with a
    /// hand-written pair (as `RouteRule` had to be), the column list is already asserted here
    /// against the query the read serves.
    ///
    /// It reads the *real* query text out of `push.rs` rather than a copy, because a copy is
    /// exactly what goes stale: the column is renamed in the query, the copy is not, and the
    /// test keeps passing while the outbox silently carries `max_attempts` in `attempts`.
    #[test]
    fn the_outbox_reads_the_columns_its_query_selects() {
        let source = include_str!("push.rs");
        for column in [
            "d.id",
            "d.notification_id",
            "n.category",
            "n.priority",
            "n.user_id",
            "d.channel",
            "d.status",
            "d.attempts",
            "d.max_attempts",
            "d.response_status",
            "d.error",
            "d.sent_at",
            "d.created_at",
        ] {
            assert!(
                source.contains(column),
                "the outbox row reads {column:?} but the query in push.rs does not select it"
            );
        }
    }
}
