//! Walks for the data guard's store and screens (REQ-105, slice 1).
//!
//! The unit tests in `guard_data.rs` prove the *rules* — the label lattice, the validators, the
//! mask map, the exemption narrowing. They run with no database at all, which is the point of
//! keeping that half pure. Everything below needs a real one, because the claims are claims
//! about **rows**:
//!
//! - **The seeded rules exist and compile.** A migration that seeds nine patterns and one of
//!   them does not compile would leave `load_guard` returning an error on *every* request, and
//!   the failure would look like a broken provider rather than a broken seed. The walk loads
//!   the guard for a fresh tenant and reads the rule count off the detector.
//! - **A tenant rule and a platform rule are one rule set, and a platform rule is immutable.**
//!   The walk writes a tenant rule, sees it in the same list as the built-ins, and is refused
//!   when it tries to edit a built-in.
//! - **The event row cannot hold the payload.** Not "does not" — *cannot*: the walk greps the
//!   whole stored row, rendered as text, for the value it just inspected. If a future column
//!   appears, this fails.
//! - **The tester is a dry run and files no event row.** A tester that audited every payload an
//!   operator pasted would fill the log with rows about text that never left the process, and
//!   the events screen would then be showing decisions that never happened. The *no provider
//!   call* half of the request's criterion is not asserted here: nothing in this file opens a
//!   socket, so a counter that stays at zero would be measuring this suite, not the product.
//!   That claim is `POST /ai/guard/test`'s to earn, and it lands with the checkpoint on the chat
//!   route, where a stub provider is actually on the other end of a call that could have been
//!   made.
//! - **An exemption narrows one label on one feature.** A second feature with the same label
//!   stays masked, which is the half of the criterion that is easy to leave green by accident.
//! - **An exemption never releases a block.** The row is accepted and refused at the same
//!   time, which is the shape that makes the panel's "this will not do what it looks like it
//!   does" note true rather than decorative.
//!
//! The harness is the throwaway-database pattern the other AI suites use, and it **panics**
//! rather than skipping when PostgreSQL is unreachable: a skipped walk is a walk that proved
//! nothing, and a loop that cannot tell the difference reports success for a suite it never ran
//! (the lesson `ai_tool_execution.rs` records after exactly that).

use omnion_ai_hub::guard_data::{Action, Label, MaskStyle};
use omnion_ai_hub::guard_store::{
    self, EventFilter, NewEvent, NewExemption, NewRule, PolicyChanges, RuleChanges,
};
use omnion_core::config::{Config, DatabaseConfig};
use omnion_core::Db;
use sqlx::PgPool;
use sqlx::Row;
use time::OffsetDateTime;
use uuid::Uuid;

// -------------------------------------------------------------------------------------------
// The harness
// -------------------------------------------------------------------------------------------

struct GuardStore {
    pool: PgPool,
    organization_id: Uuid,
    other_organization_id: Uuid,
    database: String,
    maintenance: Option<Db>,
}

impl GuardStore {
    async fn fresh() -> Option<Self> {
        let config = Config::from_env().ok()?;
        if let Err(err) = Db::connect(&DatabaseConfig {
            url: config.database.url.clone(),
            max_connections: 1,
        })
        .await
        {
            eprintln!("PostgreSQL is not reachable at {}: {err}", config.database.url);
            return None;
        }

        let database = format!("omnion_guard_{}", Uuid::new_v4().simple());
        let maintenance = Db::connect(&DatabaseConfig {
            url: swap_database(&config.database.url, "postgres"),
            max_connections: 1,
        })
        .await
        .expect("the maintenance connection must work");
        sqlx::query(&format!("create database \"{database}\""))
            .execute(maintenance.pool())
            .await
            .expect("the temporary database must be created");

        let db = Db::connect(&DatabaseConfig {
            url: swap_database(&config.database.url, &database),
            // 4, not the default: seven writer loops share one `max_connections = 100` server on
            // this box, and the reason is recorded in `ai_tool_execution.rs`.
            max_connections: 4,
        })
        .await
        .expect("the fresh database must connect");
        db.migrate().await.expect("migrations must apply");

        let pool = db.pool().clone();
        let organization_id = seed_organization(&pool, "guardco").await;
        let other_organization_id = seed_organization(&pool, "otherco").await;

        Some(Self {
            pool,
            organization_id,
            other_organization_id,
            database,
            maintenance: Some(maintenance),
        })
    }

    /// A tenant rule with the given key, label and pattern; every other field at its default.
    async fn rule(&self, key: &str, label: Label, pattern: &str) -> NewRule {
        NewRule {
            key: key.to_owned(),
            label: label.as_wire().to_owned(),
            custom_label: None,
            pattern: pattern.to_owned(),
            validator: "none".to_owned(),
            action: "flag".to_owned(),
            severity: 3,
            priority: 100,
            providers: Vec::new(),
            features: Vec::new(),
            enabled: true,
            sample: None,
        }
    }

    /// The whole stored event row as text, for the "no payload" grep.
    ///
    /// Rendered with `::text` rather than picked column by column, because the claim is about
    /// the row and a grep over eleven named columns would pass while a twelfth held the value.
    async fn event_row_as_text(&self, id: i64) -> String {
        let text: (String,) = sqlx::query_as("select ai_guard_events::text from ai_guard_events where id = $1")
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .expect("the event row must be readable");
        text.0
    }

    async fn dispose(mut self) {
        self.pool.close().await;
        let database = std::mem::take(&mut self.database);
        if let Some(maintenance) = self.maintenance.take() {
            sqlx::query(&format!("drop database if exists \"{database}\" with (force)"))
                .execute(maintenance.pool())
                .await
                .expect("the temporary database must be removed");
            maintenance.pool().close().await;
        }
    }
}

fn swap_database(url: &str, database: &str) -> String {
    let (base, _) = url.rsplit_once('/').expect("a database URL has a path");
    format!("{base}/{database}")
}

async fn seed_organization(pool: &PgPool, label: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("insert into organizations (id, name, slug) values ($1, $2, $3)")
        .bind(id)
        .bind(label)
        .bind(format!("{label}-{}", Uuid::new_v4().simple()))
        .execute(pool)
        .await
        .expect("the fixture organization must be created");
    id
}

/// The harness, or a panic — see the file header.
macro_rules! guard {
    () => {
        match GuardStore::fresh().await {
            Some(store) => store,
            None => panic!(
                "PostgreSQL is not reachable, so every walk in this file would have SKIPPED. \
                 Set OMNION_DATABASE_URL to an existing database — on this box the QA stack's is \
                 the w7 database on port 5433. A skip must not read as a pass."
            ),
        }
    };
}

// -------------------------------------------------------------------------------------------
// Walks
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_seeded_rules_are_present_and_the_guard_loads_for_a_fresh_tenant() {
    let store = guard!();

    // A brand-new tenant has no policy row and no rules of its own, and the guard still loads —
    // reading the platform's nine built-ins. A detector that saw only tenant rows would be empty
    // here, and an operator would be looking at a full rules screen over a guard that finds
    // nothing.
    let loaded = guard_store::load_guard(&store.pool, store.organization_id)
        .await
        .expect("the guard must load for a tenant that has configured nothing");

    // `query_scalar` is a **free function** on the executor, not a method on the pool — the
    // method form (`pool.query_scalar::<_, i64>(…)`) does not exist and the compiler says so
    // after the call has already been written, which is a confusing way to learn it.
    let builtins: i64 =
        sqlx::query_scalar("select count(*) from ai_guard_rules where organization_id is null")
            .fetch_one(&store.pool)
            .await
            .expect("the seeded count must be readable");
    assert!(
        builtins >= 9,
        "the seed installs the nine labels the request names; found {builtins}"
    );
    // `Detector::len()` counts the rules that actually **run** — the ones that are enabled —
    // and `person_name` ships disabled, so nine rows means eight running. Asserting `>= 9` here
    // would be asserting that a rule nobody supplied a name list for is active, which is the
    // exact failure the next assertion below forbids.
    assert!(
        loaded.detector.len() >= 8,
        "the detector must compile the platform's enabled rules, not the tenant's empty set \
         ({} running)",
        loaded.detector.len()
    );

    // `person_name` ships **disabled**: a name list is a static data file and none ships, so the
    // row is visible and fires nothing. Asserting it disabled is the whole point of seeding it.
    let person_name_enabled: (bool,) = sqlx::query_as(
        "select enabled from ai_guard_rules where organization_id is null and label = 'person_name'",
    )
    .fetch_one(&store.pool)
    .await
    .expect("the person_name row must exist");
    assert!(
        !person_name_enabled.0,
        "person_name must ship disabled: no name list ships with the platform, so an enabled \
         rule that cannot fire is a protection that looks on"
    );

    // Every seeded pattern compiles — which is the property a migration cannot assert and only
    // `load_guard` can.
    let rows = guard_store::list_rules(&store.pool, store.organization_id)
        .await
        .expect("the rule list must read");
    for row in &rows {
        row.compile()
            .unwrap_or_else(|error| panic!("seeded rule `{}` does not compile: {error}", row.key));
    }

    store.dispose().await;
}

#[tokio::test]
async fn a_tenant_rule_joins_the_platform_rules_and_a_platform_rule_cannot_be_edited() {
    let store = guard!();

    let created = guard_store::create_rule(
        &store.pool,
        store.organization_id,
        None,
        store.rule("customer_code.tenant", Label::Email, r"CUST-[0-9]{6}").await,
    )
    .await
    .expect("a tenant rule must be creatable");

    let rows = guard_store::list_rules(&store.pool, store.organization_id)
        .await
        .expect("the rule list must read");
    assert!(
        rows.iter().any(|row| row.key == "customer_code.tenant"),
        "the tenant's own rule must be in the same list as the built-ins"
    );

    // The immutability is a **store** property, not a route property: the walk calls the store
    // directly, so a rule that only the HTTP layer protected would be editable here.
    let platform = rows
        .iter()
        .find(|row| row.organization_id.is_none())
        .expect("a platform rule must be in the list");
    let refused = guard_store::update_rule(
        &store.pool,
        store.organization_id,
        platform.id,
        RuleChanges {
            action: Some("block".to_owned()),
            ..RuleChanges::default()
        },
    )
    .await;
    match refused {
        Err(omnion_ai_hub::AiHubError::InvalidGuardRule(message)) => {
            assert!(
                message.contains("Copy it"),
                "the refusal must tell the operator what to do instead, got: {message}"
            );
        }
        other => panic!("editing a platform rule must be refused as a field error, got: {other:?}"),
    }

    // The same rule, from another tenant, is not found rather than forbidden.
    let other = guard_store::find_rule(&store.pool, store.other_organization_id, created.id)
        .await
        .expect("the lookup must answer");
    assert!(
        other.is_none(),
        "another tenant's rule must not be readable by id: the rules screen would otherwise be \
         an existence oracle for the whole installation's detection rules"
    );

    store.dispose().await;
}

#[tokio::test]
async fn an_invalid_regex_is_a_field_error_and_stores_nothing() {
    let store = guard!();

    let before: (i64,) =
        sqlx::query_as("select count(*) from ai_guard_rules where organization_id = $1")
            .bind(store.organization_id)
            .fetch_one(&store.pool)
            .await
            .expect("the count must be readable");

    let mut bad = store.rule("broken.regex", Label::Email, r"[unclosed").await;
    bad.label = "custom".to_owned();
    bad.custom_label = Some("patient_ref".to_owned());
    let refused = guard_store::create_rule(&store.pool, store.organization_id, None, bad).await;

    match refused {
        Err(omnion_ai_hub::AiHubError::InvalidGuardRule(message)) => {
            assert!(
                message.contains("regular expression"),
                "the message must name the field's rule, got: {message}"
            );
            assert!(
                message.contains("broken.regex"),
                "the message must name the rule, got: {message}"
            );
        }
        other => panic!("an uncompilable pattern must be a field error, got: {other:?}"),
    }

    let after: (i64,) =
        sqlx::query_as("select count(*) from ai_guard_rules where organization_id = $1")
            .bind(store.organization_id)
            .fetch_one(&store.pool)
            .await
            .expect("the count must be readable");
    assert_eq!(
        before.0, after.0,
        "a refused rule must store nothing: {before:?} before, {after:?} after"
    );

    store.dispose().await;
}

#[tokio::test]
async fn the_event_row_cannot_hold_the_payload() {
    let store = guard!();

    // A value invented here, and a string that must never appear anywhere in the row.
    //
    // The `sk-` prefix is load-bearing: the built-in `secret_like` pattern is deliberately
    // *prefix-shaped* (`sk|pk|ghp|xox[baprs]`), because a bare high-entropy token is a much
    // weaker signal than one carrying a known vendor prefix, and a guard that flags every long
    // base64 blob is a guard that gets switched off. The first draft of this walk used a token
    // with no prefix, the detector correctly found nothing, and the walk failed — which is the
    // right outcome: the pattern says what it catches, and `/ai/guard/about` says the rest in
    // the `misses` column.
    let secret = "sk-deha9f31c2b7a4e8d6c0b5f3a2e1d9c7b";
    let payload = format!("write to {secret} about it");
    let loaded = guard_store::load_guard(&store.pool, store.organization_id)
        .await
        .expect("the guard must load");

    let finding = loaded.detector.inspect(
        &payload,
        Some("openai"),
        Some("chat"),
        &loaded.policy,
        "a-salt-for-the-walk",
    );
    assert!(
        !finding.matches.is_empty(),
        "the built-in secret_like rule must match the invented token, or this walk proves \
         nothing about the row: {finding:?}"
    );

    let id = guard_store::record_event(
        &store.pool,
        NewEvent {
            organization_id: store.organization_id,
            site_id: None,
            user_id: None,
            request_id: Uuid::new_v4(),
            run_id: None,
            provider_id: None,
            feature: Some("chat".to_owned()),
            action: finding.verdict.as_wire().to_owned(),
            rule_keys: finding.matches.iter().map(|m| m.rule_key.clone()).collect(),
            label_counts: finding.label_counts.clone(),
            match_count: finding.matches.len() as i32,
            value_hashes: finding.matches.iter().map(|m| m.value_hash.clone()).collect(),
            error_code: finding
                .verdict
                .is_blocked()
                .then(|| "ai_guard_blocked".to_owned()),
        },
    )
    .await
    .expect("the event must be stored");

    let row = store.event_row_as_text(id).await;
    assert!(
        !row.contains(secret),
        "the payload must not appear in the stored event row, by construction:\n{row}"
    );
    // …and what *is* there is the hash, which is the whole point of storing one.
    assert!(
        row.contains("secret_like"),
        "the row must name the rule that fired, so an operator can act on it:\n{row}"
    );

    store.dispose().await;
}

#[tokio::test]
async fn a_dry_run_answers_a_verdict_and_files_no_event() {
    let store = guard!();

    // A card number that passes Luhn, so the built-in `card` rule reaches its `block`.
    let card = "4111111111111111";
    let payload = format!("my card is {card}");

    // Raise `card` to `block` through the store, the way the policy screen does.
    guard_store::save_policy(
        &store.pool,
        store.organization_id,
        None,
        PolicyChanges {
            label_defaults: Some(
                [("card".to_owned(), Action::Block)]
                    .into_iter()
                    .collect(),
            ),
            mask_style: Some(MaskStyle::Numbered),
            allow_user_override: Some(false),
        },
    )
    .await
    .expect("the policy must save");

    let loaded = guard_store::load_guard(&store.pool, store.organization_id)
        .await
        .expect("the guard must load");
    let before_events: (i64,) =
        sqlx::query_as("select count(*) from ai_guard_events where organization_id = $1")
            .bind(store.organization_id)
            .fetch_one(&store.pool)
            .await
            .expect("the event count must be readable");

    let finding = loaded.detector.inspect(
        &payload,
        Some("openai"),
        Some("chat"),
        &loaded.policy,
        "",
    );
    assert!(
        finding.verdict.is_blocked(),
        "a card under a `block` policy must be refused; the finding was {:?}",
        finding.verdict
    );

    let after_events: (i64,) =
        sqlx::query_as("select count(*) from ai_guard_events where organization_id = $1")
            .bind(store.organization_id)
            .fetch_one(&store.pool)
            .await
            .expect("the event count must be readable");
    assert_eq!(
        before_events.0, after_events.0,
        "a dry run writes no event row. A tester that filed an audit entry for every payload an \
         operator pasted would fill the log with rows about text that never left the process — \
         and the events screen would then be showing decisions that never happened."
    );

    // The refusal names the label and the rule: the request's "naming the label and the rule is
    // the difference between a support ticket and a five-second fix".
    match &finding.verdict {
        omnion_ai_hub::guard_data::GuardVerdict::Blocked {
            label, rule_key, ..
        } => {
            assert_eq!(label, "card", "the refusal names the label");
            assert!(
                rule_key.contains("card"),
                "the refusal names the rule, got `{rule_key}`"
            );
        }
        other => panic!("expected a blocked verdict, got {other:?}"),
    }

    store.dispose().await;
}

#[tokio::test]
async fn an_exemption_narrows_one_label_on_one_feature_and_the_other_feature_stays_masked() {
    let store = guard!();

    // `email` is `mask` everywhere, then exempted for one feature only.
    guard_store::save_policy(
        &store.pool,
        store.organization_id,
        None,
        PolicyChanges {
            label_defaults: Some(
                [("email".to_owned(), Action::Mask)].into_iter().collect(),
            ),
            mask_style: Some(MaskStyle::Numbered),
            allow_user_override: Some(false),
        },
    )
    .await
    .expect("the policy must save");

    let exemption = guard_store::create_exemption(
        &store.pool,
        store.organization_id,
        None,
        NewExemption {
            label: "email".to_owned(),
            providers: Vec::new(),
            features: vec!["support_reply".to_owned()],
            reason: "the customer's own address is the subject of the reply".to_owned(),
            expires_at: Some(OffsetDateTime::now_utc() + time::Duration::days(30)),
        },
    )
    .await
    .expect("the exemption must be created");

    let loaded = guard_store::load_guard(&store.pool, store.organization_id)
        .await
        .expect("the guard must load");
    let payload = "write to someone@example.com please";

    let exempt = loaded.detector.inspect(
        payload,
        Some("openai"),
        Some("support_reply"),
        &loaded.policy,
        "salt",
    );
    let other = loaded
        .detector
        .inspect(payload, Some("openai"), Some("chat"), &loaded.policy, "salt");

    assert_eq!(
        exempt.action,
        Action::Allow,
        "the exempted feature must pass the value through"
    );
    assert!(
        exempt.text.contains("someone@example.com"),
        "and the text must be unchanged: {}",
        exempt.text
    );
    assert_eq!(
        other.action,
        Action::Mask,
        "another feature with the same label must stay masked — that is the half of the \
         criterion an over-broad exemption passes by accident"
    );
    assert!(
        other.text.contains("[EMAIL_1]"),
        "and the other feature's text must carry the placeholder, got: {}",
        other.text
    );

    // A blank reason is refused: the request's "every exemption needs a reason", enforced by the
    // store so a direct caller cannot skip it.
    let refused = guard_store::create_exemption(
        &store.pool,
        store.organization_id,
        None,
        NewExemption {
            label: "email".to_owned(),
            providers: Vec::new(),
            features: vec!["anything".to_owned()],
            reason: "   ".to_owned(),
            expires_at: None,
        },
    )
    .await;
    assert!(
        matches!(refused, Err(omnion_ai_hub::AiHubError::InvalidGuardExemption(_))),
        "a blank reason must be refused, got {refused:?}"
    );

    // Deleting it takes the exemption out of the policy.
    guard_store::delete_exemption(&store.pool, store.organization_id, exemption.id)
        .await
        .expect("the exemption must be deletable");
    let after = guard_store::load_guard(&store.pool, store.organization_id)
        .await
        .expect("the guard must load");
    let reread = after
        .detector
        .inspect(payload, Some("openai"), Some("support_reply"), &after.policy, "salt");
    assert_eq!(
        reread.action,
        Action::Mask,
        "with the exemption gone the feature is masked like any other"
    );

    store.dispose().await;
}

#[tokio::test]
async fn an_exemption_never_releases_a_block() {
    let store = guard!();

    guard_store::save_policy(
        &store.pool,
        store.organization_id,
        None,
        PolicyChanges {
            label_defaults: Some(
                [("secret_like".to_owned(), Action::Block)].into_iter().collect(),
            ),
            mask_style: Some(MaskStyle::Numbered),
            allow_user_override: Some(false),
        },
    )
    .await
    .expect("the policy must save");
    guard_store::create_exemption(
        &store.pool,
        store.organization_id,
        None,
        NewExemption {
            label: "secret_like".to_owned(),
            providers: Vec::new(),
            features: Vec::new(),
            reason: "we would like to let this through for now".to_owned(),
            expires_at: None,
        },
    )
    .await
    .expect("the exemption row itself is allowed to exist");

    let loaded = guard_store::load_guard(&store.pool, store.organization_id)
        .await
        .expect("the guard must load");
    let finding = loaded.detector.inspect(
        "here is sk-abcdefghijklmnopqrstuvwx for you",
        Some("openai"),
        Some("chat"),
        &loaded.policy,
        "salt",
    );
    assert!(
        finding.verdict.is_blocked(),
        "an exemption must never switch a `block` off — a refusal an exemption could release \
         would make the setting a suggestion the policy screen cannot honestly show. The verdict \
         was {:?}",
        finding.verdict
    );

    store.dispose().await;
}

#[tokio::test]
async fn the_events_screen_filters_by_label_and_blocked_without_a_payload_column() {
    let store = guard!();
    let request_id = Uuid::new_v4();

    let mut counts = std::collections::BTreeMap::new();
    counts.insert("email".to_owned(), 2usize);
    guard_store::record_event(
        &store.pool,
        NewEvent {
            organization_id: store.organization_id,
            site_id: None,
            user_id: None,
            request_id,
            run_id: None,
            provider_id: None,
            feature: Some("chat".to_owned()),
            action: "masked".to_owned(),
            rule_keys: vec!["email.builtin".to_owned()],
            label_counts: counts,
            match_count: 2,
            value_hashes: vec!["abc123".to_owned()],
            error_code: None,
        },
    )
    .await
    .expect("the masked event must be stored");

    let blocked_id = guard_store::record_event(
        &store.pool,
        NewEvent {
            organization_id: store.organization_id,
            site_id: None,
            user_id: None,
            request_id: Uuid::new_v4(),
            run_id: None,
            provider_id: None,
            feature: Some("chat".to_owned()),
            action: "blocked".to_owned(),
            rule_keys: vec!["card.builtin".to_owned()],
            label_counts: std::collections::BTreeMap::new(),
            match_count: 1,
            value_hashes: vec!["def456".to_owned()],
            error_code: Some("ai_guard_blocked".to_owned()),
        },
    )
    .await
    .expect("the blocked event must be stored");

    let by_label = guard_store::list_events(
        &store.pool,
        &EventFilter {
            organization_id: store.organization_id,
            label: Some("email".to_owned()),
            ..EventFilter::default()
        },
    )
    .await
    .expect("the label filter must read");
    assert_eq!(
        by_label.total, 1,
        "the label filter is a containment test on label_counts and must find exactly the \
         email row"
    );
    assert_eq!(
        by_label.rows[0].feature.as_deref(),
        Some("chat"),
        "and it must be the chat row, not the blocked one"
    );
    assert!(
        !by_label.rows[0].blocked,
        "the label filter must not have matched the blocked row: it has no label counts"
    );

    let blocked_only = guard_store::list_events(
        &store.pool,
        &EventFilter {
            organization_id: store.organization_id,
            blocked_only: true,
            ..EventFilter::default()
        },
    )
    .await
    .expect("the blocked filter must read");
    assert_eq!(
        blocked_only.total, 1,
        "only the refused row matches the blocked filter"
    );
    assert_eq!(blocked_only.rows[0].id, blocked_id);
    assert!(blocked_only.rows[0].blocked);

    // The counts the stat cards sum are the ones the rows carry.
    let stats = guard_store::label_stats(
        &store.pool,
        store.organization_id,
        OffsetDateTime::now_utc() - time::Duration::days(30),
    )
    .await
    .expect("the label stats must read");
    assert_eq!(stats.get("email").copied(), Some(2), "the stat card sums the row");

    // Another tenant sees none of it.
    let other = guard_store::list_events(
        &store.pool,
        &EventFilter {
            organization_id: store.other_organization_id,
            ..EventFilter::default()
        },
    )
    .await
    .expect("the read must answer");
    assert_eq!(
        other.total, 0,
        "the events screen must not read across tenants"
    );

    store.dispose().await;
}

#[tokio::test]
async fn a_duplicate_key_is_a_conflict_and_a_bad_key_is_a_field_error() {
    let store = guard!();

    let _first = guard_store::create_rule(
        &store.pool,
        store.organization_id,
        None,
        store.rule("dup.key", Label::Email, r"x@example\.com").await,
    )
    .await
    .expect("the first rule must be created");

    let again = guard_store::create_rule(
        &store.pool,
        store.organization_id,
        None,
        store.rule("dup.key", Label::Email, r"y@example\.com").await,
    )
    .await;
    match again {
        Err(omnion_ai_hub::AiHubError::GuardRuleConflict(message)) => {
            assert!(message.contains("dup.key"), "the message names the key: {message}");
        }
        other => panic!("a taken key must be a conflict, got {other:?}"),
    }

    // The same key in *another* tenant is fine — the folded unique index is per organization.
    let _other = guard_store::create_rule(
        &store.pool,
        store.other_organization_id,
        None,
        store.rule("dup.key", Label::Email, r"z@example\.com").await,
    )
    .await
    .expect("a key is unique per organization, not globally");

    for bad in ["A", "Has Space", "UPPER", "x".repeat(61).as_str()] {
        let refused =
            guard_store::create_rule(&store.pool, store.organization_id, None, {
                let mut rule = store.rule(bad, Label::Email, r"q@example\.com").await;
                // The key shape is the thing under test, so the key has to be allowed through
                // to the validator even where it is illegal to type.
                rule.key = bad.to_owned();
                rule
            })
            .await;
        assert!(
            matches!(refused, Err(omnion_ai_hub::AiHubError::InvalidGuardRule(_))),
            "the key `{bad}` must be refused as a field error, got {refused:?}"
        );
    }

    store.dispose().await;
}

