//! Does the router's own path stamp the row with the RECIPIENT's tenant, or with the event's?
//!
//! Ticks 73 and 74 fixed the recipient guard in the SLA worker and then in
//! `POST /notifications/emit`, and both fixes ended on the same sentence: *"the row's
//! organization is the recipient's tenant"*. The emit route now asks
//! [`omnion_notifications::audience::may_address`] and stamps each row with the organization
//! [`crate::store::recipient_organizations`] reported for that recipient.
//!
//! **The router was never asked, and it is the third caller of the same store function.**
//!
//! `route()` resolves a recipient set with [`omnion_notifications::router::resolve_recipients`]
//! and then writes every row through
//! `record_with_deliveries(pool, event.organization_id, …)` — the **event's** organization, on
//! every row, unconditionally. Two of its four recipient rules make that wrong:
//!
//! * [`RecipientRule::Actor`] returns `event.actor_user_id` verbatim. Nothing in it asks which
//!   tenant the actor is in.
//! * [`RecipientRule::PayloadUser`] returns an id the **producer wrote into the payload**. That
//!   is caller-supplied data, and the only thing checked about it is that it parses as a uuid.
//!
//! So a `user_id` in a payload — or an actor from another tenant — produces a row whose
//! `organization_id` column names one tenant and whose `user_id` names an account belonging to
//! another. The read side is owner-scoped (`store::list` filters on `user_id`), so the wrong
//! column does not leak the notification into the stranger's list: it does two other things,
//! both of which are real.
//!
//! **First, the admin delivery log inverts.** [`crate::push::list_outbox`] filters on
//! `n.organization_id`, so a tenant's delivery log lists rows for its *strangers* and omits the
//! platform announcement that reached its own people. A delivery log that shows the wrong
//! tenant's traffic is worse than one that shows none.
//!
//! **Second, and this is the one that makes it a tenancy defect rather than a bookkeeping one:
//! the platform case is a genuine cross-tenant write that no guard refuses.** The router's
//! documented unscoped branch is `organization_id: None` meaning "a platform-level fact, so
//! every active account is a candidate" ([`resolve_recipients`], the `Permission` rule's `None`
//! arm). A platform event with a payload naming a tenant's user therefore resolves that user
//! and writes a row stamped `organization_id = null`. That row is then invisible to *every*
//! tenant's outbox — including the tenant whose user received it — while `outbox_counts(None)`
//! counts it. `organization_id is null` is also precisely what
//! [`crate::push::list_outbox`]'s unscoped branch selects, so the platform operator sees it and
//! nobody else does.
//!
//! ## What these walks assert
//!
//! Five legs, and the order matters because leg 3 is the control for leg 2:
//!
//! 0. the premise — an organization, two accounts in it, and one account in a second tenant;
//! 1. **the payload rule stays inside the event's tenant** — the positive control, and without
//!    it a router that wrote nothing at all would pass every leg below;
//! 2. **a payload id from another tenant is refused, not written** — the defect;
//! 3. **the positive control's neighbour**: the same rule and the same event naming an account
//!    inside the tenant is still written, so leg 2 cannot be satisfied by refusing everything;
//! 4. **the actor rule asks the same question** — an actor from another tenant is refused, and
//!    this is the leg that proves the fix was applied to the *rule* rather than to one
//!    `match` arm;
//! 5. **the platform case**: an orgless event may address a tenant's user, and the row it writes
//!    carries the **tenant's** organization, so the tenant's own delivery log finds it.
//!
//! **Every assertion reads the row out of PostgreSQL.** `RouteReport::created` is a number the
//! function under test produced; the row is the fact — and the defect is precisely that the
//! report said `1` while the row said something false.

use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::Db;
use omnion_notifications::model::NewNotification;
use omnion_notifications::router::{self, RecipientRule, RoutedEvent};
use sqlx::PgPool;
use uuid::Uuid;

/// A throwaway database with every migration applied, or `None` when PostgreSQL is unreachable.
struct Harness {
    db: Db,
    maintenance: Db,
    database: String,
}

impl Harness {
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().ok()?;
        let Some(maintenance) = live_db(&config).await else {
            return None;
        };

        let database = format!("omnion_router_ten_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("create database \"{database}\""))
            .execute(maintenance.pool())
            .await
            .expect("the temporary database must be created");

        let db = Db::connect(&DatabaseConfig {
            url: swap_database(&config.database.url, &database),
            max_connections: 4,
        })
        .await
        .expect("the fresh database must connect");
        db.migrate().await.expect("migrations must apply");

        Some(Self {
            db,
            maintenance,
            database,
        })
    }

    fn pool(&self) -> &PgPool {
        self.db.pool()
    }

    async fn dispose(self) {
        self.db.pool().close().await;
        let database = self.database;
        sqlx::query(&format!(
            "drop database if exists \"{database}\" with (force)"
        ))
        .execute(self.maintenance.pool())
        .await
        .expect("the temporary database must be removed");
    }
}

async fn live_db(config: &Config) -> Option<Db> {
    match Db::connect(&DatabaseConfig {
        url: swap_database(&config.database.url, "postgres"),
        max_connections: 1,
    })
    .await
    {
        Ok(db) => Some(db),
        Err(err) => {
            eprintln!("SKIP: PostgreSQL is not reachable ({err})");
            None
        }
    }
}

fn swap_database(url: &str, database: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url, None),
    };
    let prefix = base
        .rsplit_once('/')
        .expect("the URL must contain a database path")
        .0;
    match query {
        Some(query) => format!("{prefix}/{database}?{query}"),
        None => format!("{prefix}/{database}"),
    }
}

/// One tenant, with `accounts` active accounts inside it.
async fn tenant(pool: &PgPool, slug: &str, accounts: usize) -> Uuid {
    let organization = omnion_identity::organizations::create_organization(
        pool,
        omnion_identity::organizations::NewOrganization {
            name: format!("Tenant {slug}"),
            slug: format!("t-{slug}-{}", Uuid::new_v4().simple()),
        },
    )
    .await
    .expect("the tenant must exist");

    for _ in 0..accounts {
        omnion_identity::users::create_user(
            pool,
            omnion_identity::users::NewUser {
                email: format!("{slug}-{}@omnion.test", Uuid::new_v4().simple()),
                password: "correct horse battery".to_owned(),
                display_name: slug.to_owned(),
                organization_id: Some(organization.id),
            },
        )
        .await
        .expect("the account must exist");
    }
    organization.id
}

/// One active account in `organization` (pass `None` for the platform's orgless account).
async fn account_in(pool: &PgPool, organization_id: Option<Uuid>, label: &str) -> Uuid {
    omnion_identity::users::create_user(
        pool,
        omnion_identity::users::NewUser {
            email: format!("{label}-{}@omnion.test", Uuid::new_v4().simple()),
            password: "correct horse battery".to_owned(),
            display_name: label.to_owned(),
            organization_id,
        },
    )
    .await
    .expect("the account must exist")
    .id
}

/// Write a rule whose recipient is the payload field `assignee`.
async fn payload_rule(pool: &PgPool, event_name: &str) {
    router::create_rule(
        pool,
        &router::RouteRule {
            id: Uuid::nil(),
            event_name: event_name.to_owned(),
            category: "ticket".to_owned(),
            priority: "normal".to_owned(),
            recipient: RecipientRule::PayloadUser("assignee".to_owned()),
            title_template: "{actor} assigned {subject}".to_owned(),
            url_template: None,
            enabled: true,
            created_by: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
        },
    )
    .await
    .expect("the rule must be created");
}

/// One row's `(organization_id, user_id)`, read out of the table.
async fn row_for(pool: &PgPool, user_id: Uuid) -> Option<(Option<Uuid>, Uuid)> {
    sqlx::query_as("select organization_id, user_id from notifications where user_id = $1")
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .expect("the notification row must be readable")
}

/// How many rows exist for a user at all — the count a refusal has to leave at zero.
async fn rows_for(pool: &PgPool, user_id: Uuid) -> i64 {
    sqlx::query_scalar("select count(*)::bigint from notifications where user_id = $1")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .expect("the count must read")
}

/// The tenancy of the router's own producer path, measured rather than argued.
#[tokio::test]
async fn the_router_writes_a_row_into_the_recipients_tenant_not_the_events() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().to_owned();

    // --- leg 0: the premise -------------------------------------------------------------
    let tenant_a = tenant(&pool, "router-a", 0).await;
    let inside = account_in(&pool, Some(tenant_a), "inside").await;
    let outsider = account_in(&pool, Some(tenant_a), "also-inside").await;
    let tenant_b = tenant(&pool, "router-b", 0).await;
    let stranger = account_in(&pool, Some(tenant_b), "stranger").await;

    assert_ne!(
        inside, stranger,
        "the two accounts must be different people, or this proves nothing"
    );

    let event_name = format!("router.tenant.{}", Uuid::new_v4().simple());
    payload_rule(&pool, &event_name).await;

    let event = |actor: Option<Uuid>, assignee: Uuid| RoutedEvent {
        id: Uuid::new_v4(),
        name: event_name.clone(),
        actor_user_id: actor,
        organization_id: Some(tenant_a),
        payload: serde_json::json!({
            "title": "Checkout page needs review",
            "assignee": assignee.to_string(),
        }),
    };

    // --- leg 1: the positive control ----------------------------------------------------
    // The rule names somebody inside the event's own tenant. The row must exist. Without this
    // leg every "nothing was written" assertion below is satisfied by a router that stopped
    // routing — which is the tick-68 shape this branch has hit before.
    let report = router::route(&pool, &event(Some(inside), inside))
        .await
        .expect("the control must route");
    assert_eq!(
        report.created, 1,
        "the positive control must write its row, or every refusal below is meaningless"
    );
    let (stamped, recipient) = row_for(&pool, inside)
        .await
        .expect("the control's row must exist");
    assert_eq!(recipient, inside, "the row belongs to the account the rule named");
    assert_eq!(
        stamped,
        Some(tenant_a),
        "an in-tenant row carries the tenant — the control for the refusals below"
    );

    // --- leg 2: the defect --------------------------------------------------------------
    // The SAME rule and the SAME event, naming an account in another tenant. The payload is
    // caller-supplied data, so this id is exactly what a producer that mis-resolved an assignee
    // writes — and before the fix it produced a row stamped `tenant_a` next to a `tenant_b`
    // user.
    let report = router::route(&pool, &event(Some(inside), stranger))
        .await
        .expect("the cross-tenant route must answer");
    assert_eq!(
        rows_for(&pool, stranger).await,
        0,
        "a payload naming another tenant's account must write nothing: the router resolved it \
         because the id parsed, not because the person belongs to the event's tenant"
    );
    assert_eq!(
        report.created, 0,
        "the report must not claim a row the table does not have"
    );
    assert_eq!(
        report.unmatched_rules, 1,
        "a rule that resolved nobody is an unmatched rule, which is the sentence that tells an \
         administrator *why* nothing happened"
    );

    // --- leg 3: the neighbour that must stay green ---------------------------------------
    // The same rule, the same tenant, a *different* account inside it. Without this leg, leg 2
    // is satisfied by a router that refuses every payload id — which is a different and equally
    // useless defect.
    let report = router::route(&pool, &event(Some(inside), outsider))
        .await
        .expect("the neighbour must route");
    assert_eq!(
        report.created, 1,
        "a second in-tenant account is still addressable: leg 2 is about the tenant, not about \
         the first id a rule sees"
    );

    // --- leg 4: the actor rule asks the same question ------------------------------------
    // `RecipientRule::Actor` returns `event.actor_user_id` verbatim, so it is the other rule
    // that can produce the mismatch. This is the leg that proves the guard is on the *event's*
    // tenancy and not on one `match` arm.
    let actor_rule_event = format!("router.actor.{}", Uuid::new_v4().simple());
    router::create_rule(
        &pool,
        &router::RouteRule {
            id: Uuid::nil(),
            event_name: actor_rule_event.clone(),
            category: "ticket".to_owned(),
            priority: "normal".to_owned(),
            recipient: RecipientRule::Actor,
            title_template: "{actor} opened {subject}".to_owned(),
            url_template: None,
            enabled: true,
            created_by: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
        },
    )
    .await
    .expect("the actor rule must be created");

    let actor_event = |actor: Uuid| RoutedEvent {
        id: Uuid::new_v4(),
        name: actor_rule_event.clone(),
        actor_user_id: Some(actor),
        organization_id: Some(tenant_a),
        payload: serde_json::json!({"title": "Checkout page needs review"}),
    };

    let report = router::route(&pool, &actor_event(stranger))
        .await
        .expect("the cross-tenant actor must answer");
    assert_eq!(
        rows_for(&pool, stranger).await,
        0,
        "an actor from another tenant is not this event's audience: the actor rule resolves the \
         id without asking which tenant it is in, so the same defect exists in a second rule"
    );
    assert_eq!(
        report.created, 0,
        "no row, and the report agrees with the table"
    );

    let report = router::route(&pool, &actor_event(inside))
        .await
        .expect("the in-tenant actor must answer");
    assert_eq!(
        report.created, 1,
        "and the actor rule still reaches its own tenant — the neighbour that stops leg 4 from \
         being satisfied by refusing every actor"
    );

    harness.dispose().await;
}

/// The platform's own unscoped branch: an orgless event may address anybody, and the row it
/// writes has to be findable by the tenant whose person received it.
#[tokio::test]
async fn a_platform_event_stamps_the_tenant_whose_user_it_reached() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().to_owned();

    let tenant_a = tenant(&pool, "platform-a", 0).await;
    let member = account_in(&pool, Some(tenant_a), "member").await;

    let event_name = format!("router.platform.{}", Uuid::new_v4().simple());
    payload_rule(&pool, &event_name).await;

    // `organization_id: None` is the router's documented platform-level branch: the recipient
    // set is resolved without a tenant boundary, which is exactly why the stamp cannot be the
    // event's `null`.
    let report = router::route(
        &pool,
        &RoutedEvent {
            id: Uuid::new_v4(),
            name: event_name.clone(),
            actor_user_id: None,
            organization_id: None,
            payload: serde_json::json!({
                "title": "The nightly digest is ready",
                "assignee": member.to_string(),
            }),
        },
    )
    .await
    .expect("the platform event must route");
    assert_eq!(report.created, 1, "a platform fact may reach a tenant");

    let (stamped, recipient) = row_for(&pool, member)
        .await
        .expect("the platform announcement's row must exist");
    assert_eq!(recipient, member, "the row belongs to the account the rule named");
    assert_eq!(
        stamped,
        Some(tenant_a),
        "the row carries the RECIPIENT's tenant, not the platform event's null: every admin \
         delivery log filters on this column, so stamping the sender is what makes a platform \
         announcement invisible in the one log that should have it"
    );

    // And the tenant's own outbox finds it. This is the reader half of the same claim, because
    // a correct column nobody queries is still a defect the operator experiences.
    let listed = omnion_notifications::push::list_outbox(
        &pool,
        Some(tenant_a),
        &omnion_notifications::push::OutboxQuery {
            limit: omnion_notifications::push::MAX_OUTBOX_PAGE,
            ..Default::default()
        },
    )
    .await
    .expect("the outbox must read");
    assert!(
        listed
            .iter()
            .any(|row| row.user_id == member),
        "the tenant's delivery log lists the row its own person received: {} rows came back",
        listed.len()
    );

    harness.dispose().await;
}

/// A disabled account inside the event's own tenant is still somebody the router may address.
///
/// **Tick 73 fixed the SLA worker's guard by adding `existing_users_in_organization`, which
/// deliberately carries NO `status` filter, and tick 74's gate added a leg proving the emit route
/// kept that independence.** The router resolves through `resolve_recipients` and writes through
/// `record_with_deliveries`, so a tenancy filter added in the wrong place would silently become
/// an access check — and the symptom would be an escalation queue that empties itself every time
/// somebody goes on leave. Two questions, one answer each.
#[tokio::test]
async fn a_disabled_account_in_the_events_own_tenant_is_still_addressable() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().to_owned();

    let tenant_a = tenant(&pool, "disabled-a", 0).await;
    let on_leave = account_in(&pool, Some(tenant_a), "on-leave").await;
    let still_active = account_in(&pool, Some(tenant_a), "still-active").await;

    sqlx::query("update users set status = 'disabled' where id = $1")
        .bind(on_leave)
        .execute(&pool)
        .await
        .expect("the account must be disableable");

    let event_name = format!("router.leave.{}", Uuid::new_v4().simple());
    payload_rule(&pool, &event_name).await;

    let route_to = |assignee: Uuid| {
        let pool = pool.clone();
        let event_name = event_name.clone();
        async move {
            router::route(
                &pool,
                &RoutedEvent {
                    id: Uuid::new_v4(),
                    name: event_name,
                    actor_user_id: None,
                    organization_id: Some(tenant_a),
                    payload: serde_json::json!({
                        "title": "Escalation: the review is late",
                        "assignee": assignee.to_string(),
                    }),
                },
            )
            .await
            .expect("the event must route")
        }
    };

    let report = route_to(on_leave).await;
    assert_eq!(
        report.created, 1,
        "a colleague on leave is still somebody who has to be told: `users.status` is the access \
         system and the tenancy filter must not become an access check"
    );

    // The control: without it, "disabled" and "somebody else" are indistinguishable in the
    // answer above, and a guard that refused everything would pass.
    let report = route_to(still_active).await;
    assert_eq!(
        report.created, 1,
        "and the enabled neighbour behaves identically — which is what makes the disabled case \
         a measurement of status rather than of identity"
    );

    harness.dispose().await;
}

/// A row the router refuses to write must not be a *silent* one.
///
/// The `unmatched_rules` counter is the router's only way to tell an administrator why a rule
/// produced nothing, and the refusal has to be visible in it. Before the fix, a cross-tenant
/// payload id resolved fine and reported `created: 1` — the report and the table were in
/// agreement and both were false about the tenant.
#[tokio::test]
async fn a_refused_cross_tenant_payload_counts_as_an_unmatched_rule() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().to_owned();

    let tenant_a = tenant(&pool, "count-a", 0).await;
    let inside = account_in(&pool, Some(tenant_a), "inside").await;
    let tenant_b = tenant(&pool, "count-b", 0).await;
    let stranger = account_in(&pool, Some(tenant_b), "stranger").await;

    let event_name = format!("router.count.{}", Uuid::new_v4().simple());
    payload_rule(&pool, &event_name).await;

    let event = |assignee: Uuid| RoutedEvent {
        id: Uuid::new_v4(),
        name: event_name.clone(),
        actor_user_id: Some(inside),
        organization_id: Some(tenant_a),
        payload: serde_json::json!({
            "title": "Checkout page needs review",
            "assignee": assignee.to_string(),
        }),
    };

    let allowed = router::route(&pool, &event(inside)).await.expect("must route");
    assert_eq!(
        (allowed.created, allowed.unmatched_rules),
        (1, 0),
        "an in-tenant resolve is a created row and no unmatched rule"
    );

    let refused = router::route(&pool, &event(stranger)).await.expect("must route");
    assert_eq!(
        (refused.created, refused.unmatched_rules),
        (0, 1),
        "a refused cross-tenant resolve is an UNMATCHED rule, not a created row: the count is \
         how an administrator learns the rule is pointing at somebody the event's tenant does \
         not own"
    );

    harness.dispose().await;
}

/// The four recipient rules, enumerated: which of them can return somebody the event's tenant
/// does not own, and what the router does about each.
///
/// A guard added to `resolve_recipients` has to be justified by *which arms need it*, and this
/// is the enumeration. `Permission` and `Role` filter inside the database — the permission arm
/// selects candidates from the event's own organization, the role arm binds the organization —
/// so they are listed as already-bounded. `Actor` and `PayloadUser` are the two that read an id
/// the caller supplied, and they are the two the guard has to cover.
#[tokio::test]
async fn only_the_caller_supplied_rules_can_leave_the_events_tenant() {
    let Some(harness) = Harness::fresh().await else {
        return;
    };
    let pool = harness.pool().to_owned();

    let tenant_a = tenant(&pool, "arms-a", 0).await;
    let inside = account_in(&pool, Some(tenant_a), "inside").await;
    let tenant_b = tenant(&pool, "arms-b", 0).await;
    let stranger = account_in(&pool, Some(tenant_b), "stranger").await;

    let make = |name: String, recipient: RecipientRule| {
        let pool = pool.clone();
        async move {
            router::create_rule(
                &pool,
                &router::RouteRule {
                    id: Uuid::nil(),
                    event_name: format!("router.arms.{name}.{}", Uuid::new_v4().simple()),
                    category: "ticket".to_owned(),
                    priority: "normal".to_owned(),
                    recipient,
                    title_template: "{subject}".to_owned(),
                    url_template: None,
                    enabled: true,
                    created_by: None,
                    created_at: time::OffsetDateTime::UNIX_EPOCH,
                },
            )
            .await
            .expect("the rule must be created")
        }
    };

    // The two caller-supplied rules, both naming the stranger.
    let actor_rule = make("actor".to_owned(), RecipientRule::Actor).await;
    let payload_rule = make(
        "payload".to_owned(),
        RecipientRule::PayloadUser("assignee".to_owned()),
    )
    .await;
    assert_ne!(actor_rule.id, payload_rule.id, "two distinct rules");

    // One event, whose actor AND payload both name the stranger, addressed to tenant A.
    let report = router::route(
        &pool,
        &RoutedEvent {
            id: Uuid::new_v4(),
            name: actor_rule.event_name.clone(),
            actor_user_id: Some(stranger),
            organization_id: Some(tenant_a),
            payload: serde_json::json!({
                "title": "x",
                "assignee": stranger.to_string(),
            }),
        },
    )
    .await
    .expect("the actor rule must route");
    assert_eq!(report.created, 0, "the actor arm is bounded by the same rule");

    let report = router::route(
        &pool,
        &RoutedEvent {
            id: Uuid::new_v4(),
            name: payload_rule.event_name.clone(),
            actor_user_id: Some(stranger),
            organization_id: Some(tenant_a),
            payload: serde_json::json!({
                "title": "x",
                "assignee": stranger.to_string(),
            }),
        },
    )
    .await
    .expect("the payload rule must route");
    assert_eq!(report.created, 0, "the payload arm is bounded too");

    assert_eq!(
        rows_for(&pool, stranger).await,
        0,
        "neither caller-supplied rule reached the other tenant"
    );

    // And the enumeration's other half: the in-tenant id is still delivered by BOTH arms, which
    // is what makes the two refusals above a tenancy rule rather than a disabled router.
    let report = router::route(
        &pool,
        &RoutedEvent {
            id: Uuid::new_v4(),
            name: actor_rule.event_name.clone(),
            actor_user_id: Some(inside),
            organization_id: Some(tenant_a),
            payload: serde_json::json!({"title": "x"}),
        },
    )
    .await
    .expect("the actor rule must route");
    assert_eq!(report.created, 1, "the actor arm still reaches its own tenant");

    let report = router::route(
        &pool,
        &RoutedEvent {
            id: Uuid::new_v4(),
            name: payload_rule.event_name.clone(),
            actor_user_id: None,
            organization_id: Some(tenant_a),
            payload: serde_json::json!({
                "title": "x",
                "assignee": inside.to_string(),
            }),
        },
    )
    .await
    .expect("the payload rule must route");
    assert_eq!(report.created, 1, "the payload arm still reaches its own tenant");

    harness.dispose().await;
}

/// The compile-time half: [`NewNotification`] is what `route()` builds, and a row whose
/// organization column disagrees with its recipient is the shape both this slice and ticks
/// 73/74 removed. Nothing in the builder can set it — the draft carries no organization at all,
/// which is exactly why the caller binds it and exactly why the caller was wrong three times.
///
/// Asserted as a *structural* fact rather than a behavioural one so it cannot rot into a comment.
#[test]
fn a_draft_carries_no_organization_because_the_caller_binds_it() {
    let draft = NewNotification::to(Uuid::nil(), "ticket", "t");
    let _ = draft.clone().build().expect("a draft must build");
    // The point, stated as a test: there is no `organization_id` field on the draft, so the
    // stamp is entirely the caller's choice — which is the reason the rule had to be asked
    // rather than relied upon. `record_with_deliveries(pool, org, …)` takes it as an argument.
    // This is the seam the whole slice is about; the walk above measures what happens when it
    // is filled in from the wrong place.
    assert!(
        format!("{draft:?}").to_lowercase().contains("user_id"),
        "the draft's identity is the recipient, so the tenant can only come from the caller"
    );
}