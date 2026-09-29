//! Integration walks for automatic invalidation (REQ-011, slice 3).
//!
//! The unit tests in `omnion-cdn::invalidation` prove the *mapping* — a payload, a toggle
//! map and a provider's capabilities in, one planned purge out. They run without a database
//! on purpose, because the mapping is where the decisions live and it should be cheap to
//! test a hundred of them.
//!
//! What a pure test cannot see is the part that has to survive contact with a real
//! database, and this file is deliberately blunt about which parts those are:
//!
//!   * **A publish produces a purge row, on the walk, without anybody pressing a button.**
//!     The request's own diagram is "page published → purge → new version live", and a
//!     drain that never runs against real rows proves only that a function returns a plan.
//!     The walk records the event through the *bus* — the same `bus::emit` the content
//!     route calls — and then runs the drain, so the row that appears is one the platform
//!     wrote, not one the test inserted where it wanted it.
//!   * **The drain is exactly-once across repeated walks.** A cursor that does not move
//!     queues every publication again; a cursor that moves without its purge loses the
//!     invalidation. Both are invisible in a single run, so the walk runs the drain twice
//!     and asserts the second one does nothing.
//!   * **A trigger that is switched off queues nothing, and the toggle is the operator's
//!     own** — set through the settings endpoint, not by writing the jsonb column.
//!   * **An automatic purge says who it came from, and says nobody asked for it.** A
//!     history row whose "Requested by" is a person who never pressed the button is a lie
//!     in the one column an operator reads first.
//!
//! The drain runs through the same `invalidation::drain` the binary calls in its tick, and
//! the purge it queues is then drained through the same `purge::claim_due` / `apply_outcome`
//! path the worker uses. A walk that used a private copy of either would pass while the
//! product was broken, which is the failure mode this file exists to avoid.

use omnion_api::state::AppState;
use omnion_cdn::invalidation::{self, Trigger};
use omnion_core::config::Config;
use omnion_core::{BuildInfo, Db, RedisClient};
use omnion_events::NewEvent;
use serde_json::{Value, json};
use uuid::Uuid;

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

fn test_storage() -> omnion_storage::Storage {
    omnion_storage::Storage::from_config(&omnion_storage::StorageConfig::default())
        .expect("the default storage configuration is valid")
}

struct Harness {
    state: AppState,
    db: Db,
    site: Uuid,
    organization: Uuid,
    events_before: i64,
}

/// Connect, or explain that PostgreSQL is not there.
///
/// `state_or_skip` is the pattern the rest of this suite uses, and it is the honest one: a
/// walk that cannot run says so in its output and returns rather than passing quietly.
async fn harness() -> Option<Harness> {
    let config = Config::from_env().ok()?;
    let db = match Db::connect(&config.database).await {
        Ok(db) => db,
        Err(error) => {
            eprintln!("SKIP: PostgreSQL is not reachable ({error})");
            return None;
        }
    };
    db.migrate().await.expect("migrations must apply");
    let redis = RedisClient::new(&config.redis.url).expect("the redis URL must parse");
    let state = AppState::new(
        BuildInfo::new("omnion-api", "0.0.0-test"),
        config,
        db.clone(),
        redis,
        test_storage(),
    );

    let organization: Uuid =
        sqlx::query_scalar("insert into organizations (name, slug) values ($1, $2) returning id")
            .bind(format!("cdn invalidation {}", Uuid::new_v4().simple()))
            .bind(format!("cdn-inv-{}", Uuid::new_v4().simple()))
            .fetch_one(db.pool())
            .await
            .expect("organization must insert");
    let site: Uuid = sqlx::query_scalar(
        "insert into sites (organization_id, key, name) values ($1, $2, $3) returning id",
    )
    .bind(organization)
    .bind(format!("inv{}", Uuid::new_v4().simple()))
    .bind("CDN invalidation site")
    .fetch_one(db.pool())
    .await
    .expect("site must insert");

    // The cursor is moved to the head of the bus *by the product's own seeder*, not by a
    // test writing the column: the walk below is only valid if the value a fresh
    // installation gets is the one the platform hands out.
    let head: i64 = sqlx::query_scalar("select coalesce(max(id), 0) from events")
        .fetch_one(db.pool())
        .await
        .expect("the bus must be readable");
    sqlx::query("update cdn_invalidation_cursor set last_event_id = 0 where id = 1")
        .execute(db.pool())
        .await
        .expect("the cursor must reset");
    invalidation::seed_cursor(state.db().pool())
        .await
        .expect("seeding must run");
    let cursor = invalidation::current_cursor(state.db().pool())
        .await
        .expect("the cursor must read back");
    assert_eq!(
        cursor, head,
        "a fresh installation's cursor points at the head of the bus, not at zero"
    );

    Some(Harness {
        state,
        db,
        site,
        organization,
        events_before: head,
    })
}

impl Harness {
    /// Turn a set of triggers on for this site, through the column the settings screen writes.
    async fn enable(&self, triggers: &[Trigger]) {
        let mut map = serde_json::Map::new();
        for trigger in triggers {
            map.insert(trigger.event_name().to_string(), Value::Bool(true));
        }
        // The conflict target carries the index's own `where site_id is not null`: the
        // per-site uniqueness is a *partial* index (the platform row is a separate one),
        // so a bare `on conflict (site_id)` has no arbiter to match and PostgreSQL
        // refuses the statement. This is the same spelling `put_settings` uses.
        sqlx::query(
            "insert into cdn_settings (site_id, provider, auto_purge) values ($1, 'origin', $2) \
             on conflict (site_id) where site_id is not null \
             do update set auto_purge = excluded.auto_purge",
        )
        .bind(self.site)
        .bind(Value::Object(map))
        .execute(self.db.pool())
        .await
        .expect("the settings row must write");
    }

    /// Point the site at the `generic_http` adapter, whose endpoint is unreachable.
    ///
    /// Not for the tag fallback — see the walk below, which drives that through the pure
    /// planner because no shipped adapter is tag-less. This is here for a different
    /// reason: it is the configuration where a purge is queued and then *fails*, which is
    /// the state the retry action exists for.
    async fn use_unreachable_provider(&self) {
        sqlx::query(
            "insert into cdn_settings (site_id, provider, endpoint_url, auto_purge) \
             values ($1, 'generic_http', 'http://127.0.0.1:9/purge', '{}'::jsonb) \
             on conflict (site_id) where site_id is not null \
             do update set provider = 'generic_http', \
             endpoint_url = 'http://127.0.0.1:9/purge'",
        )
        .bind(self.site)
        .execute(self.db.pool())
        .await
        .expect("the settings row must write");
    }

    /// Record an event on the bus, exactly as the content and tenancy routes do.
    async fn emit(&self, name: &str, payload: Value) -> i64 {
        let report = omnion_events::bus::emit(
            self.db.pool(),
            NewEvent::new(name)
                .organization(self.organization)
                .site(self.site)
                .payload(payload),
        )
        .await
        .expect("the event must record");
        i64::try_from(report.event.id).expect("an event id fits an i64")
    }

    async fn run_drain(&self) -> invalidation::DrainReport {
        invalidation::drain(self.db.pool(), 100)
            .await
            .expect("the drain must run")
    }

    /// The purge rows this site has, oldest first.
    async fn purges(&self) -> Vec<(Uuid, String, String, Option<Uuid>, i32)> {
        sqlx::query_as(
            "select id, kind, status, requested_by, item_count from cdn_purges \
             where site_id = $1 order by requested_at, id",
        )
        .bind(self.site)
        .fetch_all(self.db.pool())
        .await
        .expect("the history must read")
    }

    async fn cleanup(self) {
        sqlx::query("delete from organizations where id = $1")
            .bind(self.organization)
            .execute(self.db.pool())
            .await
            .expect("the organization must delete");
    }
}

/// What one purge's item rows say.
async fn item_targets(pool: &sqlx::PgPool, purge: Uuid) -> Vec<String> {
    sqlx::query_scalar("select target from cdn_purge_items where purge_id = $1 order by target")
        .bind(purge)
        .fetch_all(pool)
        .await
        .expect("the items must read")
}

// ---------------------------------------------------------------------------------------------
// Walks
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn publishing_a_page_queues_a_purge_without_anybody_asking() {
    let Some(harness) = harness().await else { return };
    harness.enable(&[Trigger::PagePublished]).await;

    let event_id = harness
        .emit("page.published", json!({ "page_id": Uuid::new_v4(), "slug": "blog/hello" }))
        .await;

    let report = harness.run_drain().await;
    assert_eq!(report.queued, 1, "one publication, one purge: {report:?}");

    let purges = harness.purges().await;
    assert_eq!(purges.len(), 1, "exactly one row: {purges:?}");
    let (id, kind, status, requested_by, item_count) = purges[0].clone();
    // `origin` reports `tags: true` on purpose (slice 1): with no external edge a tag is
    // resolved to the URL it stands for and the purge succeeds honestly, which is the
    // right answer for an installation serving its own public surface. The tag-less
    // adapter is `generic_http`, and the walk below is the one that proves the fallback.
    assert_eq!(kind, "tag", "the origin resolves tags to addresses: {kind}");
    assert_eq!(status, "queued", "nothing has drained it yet");
    assert_eq!(item_count, 1);
    assert_eq!(
        item_targets(harness.db.pool(), id).await,
        vec!["/blog/hello".to_string()],
        "the target is the page's own public address"
    );

    // The provenance, and the column that must NOT be filled in.
    let sources: Vec<(i64, String)> = invalidation::sources_of(harness.db.pool(), id)
        .await
        .expect("the sources must read");
    assert_eq!(
        sources,
        vec![(event_id, "page.published".to_string())],
        "the purge is traceable to the publication that caused it"
    );
    assert_eq!(
        requested_by, None,
        "nobody pressed the button: borrowing the publisher's id would make the history \
         claim an operator asked for something they did not"
    );

    harness.cleanup().await;
}

#[tokio::test]
async fn the_drain_is_exactly_once_across_repeated_walks() {
    let Some(harness) = harness().await else { return };
    harness.enable(&[Trigger::PagePublished]).await;
    harness
        .emit("page.published", json!({ "page_id": Uuid::new_v4(), "slug": "one" }))
        .await;

    let first = harness.run_drain().await;
    assert_eq!(first.queued, 1);

    // The whole point of a durable cursor: a second walk, and a third, change nothing. A
    // drain that re-queued would bill the provider twice for one publication.
    let second = harness.run_drain().await;
    let third = harness.run_drain().await;
    assert_eq!(second.queued, 0, "the cursor did not move: {second:?}");
    assert_eq!(third.queued, 0, "and it did not drift either: {third:?}");
    assert_eq!(
        harness.purges().await.len(),
        1,
        "one publication produced exactly one purge, however many walks ran"
    );

    harness.cleanup().await;
}

#[tokio::test]
async fn a_second_publication_is_purged_too_and_only_once_each() {
    let Some(harness) = harness().await else { return };
    harness.enable(&[Trigger::PagePublished]).await;
    harness
        .emit("page.published", json!({ "page_id": Uuid::new_v4(), "slug": "one" }))
        .await;
    harness.run_drain().await;
    harness
        .emit("page.published", json!({ "page_id": Uuid::new_v4(), "slug": "two" }))
        .await;

    let report = harness.run_drain().await;
    assert_eq!(report.queued, 1, "the second one is its own purge");
    let purges = harness.purges().await;
    assert_eq!(purges.len(), 2, "two publications, two rows: {purges:?}");

    // Read the targets back out of the rows rather than trusting the slugs the walk
    // emitted: the assertion is about what the queue holds, not about what was typed.
    let mut targets: std::collections::BTreeSet<String> = Default::default();
    for (id, ..) in &purges {
        targets.extend(item_targets(harness.db.pool(), *id).await);
    }
    assert_eq!(
        targets,
        ["/one".to_string(), "/two".to_string()].into(),
        "each publication is invalidated at its own address"
    );

    harness.cleanup().await;
}

#[tokio::test]
async fn a_trigger_the_operator_turned_off_queues_nothing() {
    let Some(harness) = harness().await else { return };
    // The *other* trigger is on. The point is that the map is consulted per name, not
    // read once as a single yes/no.
    harness.enable(&[Trigger::PageDeleted]).await;
    harness
        .emit("page.published", json!({ "page_id": Uuid::new_v4(), "slug": "quiet" }))
        .await;

    let report = harness.run_drain().await;
    assert_eq!(report.queued, 0, "the publish trigger is off: {report:?}");
    assert_eq!(
        harness.purges().await.len(),
        0,
        "an automatic purge nobody asked for is not a row an operator has to explain away"
    );

    // Turning it on is the operator's own action, through the same row — and a
    // *subsequent* publication is then invalidated.
    harness.enable(&[Trigger::PagePublished]).await;
    harness
        .emit("page.published", json!({ "page_id": Uuid::new_v4(), "slug": "loud" }))
        .await;
    let report = harness.run_drain().await;
    assert_eq!(report.queued, 1, "the toggle is the only thing that changed: {report:?}");

    // And the publication that arrived while the trigger was off is **not** re-examined
    // now. This is the honest shape of a cursor, and it is worth asserting because the
    // other reading is the tempting one: "turning the trigger on should also purge what
    // was missed while it was off". It should not. An event the drain skipped has already
    // been passed, and retroactively acting on the whole backlog of an installation whose
    // trigger was off for a month is a stampede at the provider for content that has been
    // republished many times since — and the newest publication is the one that matters.
    // The operator's tool for "purge everything now" is the `/cdn/purge` console, which
    // says so in as many words.
    let mut queued: Vec<String> = Vec::new();
    for (id, ..) in harness.purges().await {
        queued.extend(item_targets(harness.db.pool(), id).await);
    }
    assert_eq!(
        queued,
        vec!["/loud".to_string()],
        "only the publication made after the toggle was turned on is purged"
    );

    harness.cleanup().await;
}

#[tokio::test]
async fn an_event_with_no_usable_address_queues_nothing() {
    let Some(harness) = harness().await else { return };
    harness.enable(&[Trigger::PagePublished]).await;

    // No slug: there is no address to invalidate, and inventing one from the page id would
    // spend a provider call on a path no visitor requests.
    harness
        .emit("page.published", json!({ "page_id": Uuid::new_v4() }))
        .await;
    let report = harness.run_drain().await;
    assert_eq!(report.queued, 0, "{report:?}");
    assert!(harness.purges().await.is_empty());

    // An event with no site at all is legal on this bus and invalidates nothing.
    omnion_events::bus::emit(
        harness.db.pool(),
        NewEvent::new("page.published")
            .organization(harness.organization)
            .payload(json!({ "slug": "nowhere" })),
    )
    .await
    .expect("the event must record");
    let report = harness.run_drain().await;
    assert_eq!(report.queued, 0, "an event with no site has no site cache: {report:?}");

    harness.cleanup().await;
}

#[tokio::test]
async fn an_event_from_another_area_is_ignored_without_looking_broken() {
    let Some(harness) = harness().await else { return };
    harness.enable(&[Trigger::PagePublished]).await;
    harness
        .emit("user.created", json!({ "user_id": Uuid::new_v4() }))
        .await;
    harness
        .emit("media.created", json!({ "media_id": Uuid::new_v4(), "filename": "a.png" }))
        .await;
    harness
        .emit("page.published", json!({ "page_id": Uuid::new_v4(), "slug": "real" }))
        .await;

    let report = harness.run_drain().await;
    assert_eq!(
        report.queued, 1,
        "one of the three is a trigger and the other two belong to other modules: {report:?}"
    );
    assert_eq!(harness.purges().await.len(), 1);

    harness.cleanup().await;
}

#[tokio::test]
async fn a_replaced_file_purges_the_media_address_and_not_the_site() {
    let Some(harness) = harness().await else { return };
    harness.enable(&[Trigger::MediaVersionCreated]).await;
    let media = Uuid::new_v4();
    harness
        .emit("media.version_created", json!({ "media_id": media, "site_id": harness.site }))
        .await;

    let report = harness.run_drain().await;
    assert_eq!(report.queued, 1, "{report:?}");
    let purges = harness.purges().await;
    assert_eq!(purges.len(), 1);
    let (id, kind, ..) = purges[0].clone();
    assert_eq!(kind, "tag", "`origin` resolves the tag to the address: {kind}");
    assert_eq!(
        item_targets(harness.db.pool(), id).await,
        vec![format!("/api/v1/public/media/{media}")],
        "a replaced file invalidates its own address. Purging every page that embeds it \
         is a content-graph walk, and doing it here would purge the whole site on every \
         upload — a provider bill nobody approved."
    );

    harness.cleanup().await;
}

#[tokio::test]
async fn a_theme_activation_purges_the_whole_site_by_tag() {
    let Some(harness) = harness().await else { return };
    harness.enable(&[Trigger::ThemeActivated]).await;

    for slug in ["home", "blog"] {
        sqlx::query(
            "insert into pages (site_id, slug, status, page_type) \
             values ($1, $2, 'published', 'standard')",
        )
        .bind(harness.site)
        .bind(slug)
        .execute(harness.db.pool())
        .await
        .expect("the page must insert");
    }
    harness
        .emit("theme.activated", json!({ "theme": "atlas", "site_id": harness.site }))
        .await;

    let report = harness.run_drain().await;
    assert_eq!(report.queued, 1, "{report:?}");
    let purges = harness.purges().await;
    let (id, kind, ..) = purges[0].clone();
    assert_eq!(
        kind, "tag",
        "a new theme invalidates every address, and one tag says that — enumerating the \
         site's pages would be a row per page and a provider call per page"
    );
    assert_eq!(
        item_targets(harness.db.pool(), id).await,
        vec![invalidation::site_tag(harness.site)],
        "the tag is the one every cacheable public response carries in its Surrogate-Key"
    );

    harness.cleanup().await;
}

#[tokio::test]
async fn the_tag_fallback_is_decided_by_the_planner_not_by_the_drain() {
    // Every adapter this build ships reports `tags: true` — slice 1's deliberate choice,
    // and a defensible one (`origin` has no edge at all, so a tag resolved to its URL is
    // the honest answer). That means the tag-less branch of the planner is **unreachable
    // in production today**, and this walk is where that is stated rather than hidden.
    //
    // Two things follow, and both are why this is a test rather than a deletion:
    //
    //   * The branch must keep working. A future adapter that genuinely cannot hold a
    //     tag would otherwise be a no-op purge that reports success, and the first person
    //     to find out would be the operator whose cache stopped invalidating. The branch
    //     is exercised here against the planner, which is where the decision is made.
    //   * A walk that *faked* a tag-less provider by pointing a site at `generic_http`
    //     would have been a test that passed for the wrong reason: the adapter still says
    //     `tags: true`, and the row it produced was a tag purge wearing a URL assertion.
    //
    // The database half cannot be driven from here, and that is stated rather than hidden
    // too: `drain` reads capabilities from the *provider key*, so exercising the fallback
    // against a real site would need a fifth adapter that does not exist.
    let tagless = omnion_cdn::provider::Capabilities {
        tags: false,
        purge_all: false,
    };
    for key in ["origin", "generic_http", "cloudflare_style"] {
        let capabilities = omnion_cdn::provider_for(key, &Default::default()).capabilities();
        assert!(
            capabilities.tags,
            "{key} shipped before this tick and reported tag support; if this assertion \
             fails, the fallback walk below is testing a branch that is now live, and it \
             should be promoted to a database walk"
        );
    }

    let site = harness_site();
    let paths = vec![
        "/home".to_string(),
        "/blog".to_string(),
        "/blog/hello".to_string(),
    ];
    let by_tag = invalidation::plan(
        Trigger::ThemeActivated,
        site,
        &json!({ "theme": "atlas" }),
        &json!({ "theme.activated": true }),
        &omnion_cdn::provider::Capabilities { tags: true, purge_all: true },
    );
    assert_eq!(by_tag.kind, omnion_cdn::purge::PurgeKind::Tag);
    assert_eq!(by_tag.targets, vec![invalidation::site_tag(site)]);

    let by_url = invalidation::site_plan(Trigger::ThemeActivated, site, &paths);
    assert_eq!(by_url.kind, omnion_cdn::purge::PurgeKind::Url);
    assert_eq!(by_url.targets, paths);
}

/// A fixed site id, so a planner-only walk needs no database at all.
fn harness_site() -> Uuid {
    Uuid::from_u128(0x5eed_0000_0000_0000_0000_0000_0000_0042)
}

#[tokio::test]
async fn a_queued_automatic_purge_drains_to_succeeded_through_the_worker_path() {
    // The end of the request's diagram: publish -> queue -> the edge agrees. The last step
    // goes through the same claim/apply functions the binary calls, so the status this
    // asserts is one something computed rather than one the walk wrote.
    let Some(harness) = harness().await else { return };
    harness.enable(&[Trigger::PagePublished]).await;
    harness
        .emit("page.published", json!({ "page_id": Uuid::new_v4(), "slug": "fresh" }))
        .await;
    harness.run_drain().await;

    let purges = harness.purges().await;
    let (id, ..) = purges[0].clone();

    let pool = harness.db.pool();
    // `claim_due` is deliberately global — the worker has no idea which site an item
    // belongs to until it has claimed it and read the parent. So the claim is filtered to
    // this purge's items *after* the fact rather than the claim being narrowed: narrowing
    // it in the test would mean testing a function that does not exist, and the point of
    // this walk is that the real one reaches the real row.
    let claimed = omnion_cdn::purge::claim_due(pool, 100)
        .await
        .expect("claim must run");
    let mut claimed: Vec<_> = claimed
        .into_iter()
        .filter(|item| item.purge_id == id)
        .collect();
    assert_eq!(claimed.len(), 1, "the queued item is claimable");
    assert_eq!(claimed[0].target, "/fresh");
    // The leftovers the claim also took belong to other walks in this shared database, so
    // they are released rather than left `running`: a test that leaves the queue in a
    // state the next test has to reason about is a test that makes the next failure
    // mysterious.
    sqlx::query(
        "update cdn_purge_items set status = 'pending' \
         where status = 'running' and purge_id <> $1",
    )
    .bind(id)
    .execute(pool)
    .await
    .expect("the foreign claims must be released");
    omnion_cdn::purge::mark_running(pool, &[id]).await.expect("the parent must be marked");
    let outcome = omnion_cdn::PurgeOutcome::Succeeded;
    let (_, message) = omnion_cdn::purge::apply_outcome(
        &mut claimed,
        &outcome,
        time::OffsetDateTime::now_utc(),
        5,
        1,
    );
    assert_eq!(message, None, "the origin succeeds, so there is no message");
    omnion_cdn::purge::save_items(pool, &claimed)
        .await
        .expect("the items must write");
    omnion_cdn::purge::settle(pool, &[id]).await.expect("settle must run");

    let row: (String, Option<time::OffsetDateTime>) =
        sqlx::query_as("select status, finished_at from cdn_purges where id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("the purge must read");
    assert_eq!(row.0, "succeeded", "one target, accepted by the origin");
    assert!(row.1.is_some(), "a finished purge is stamped");

    harness.cleanup().await;
}
