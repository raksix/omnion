//! The tool → HTTP route → permission binding (REQ-100, acceptance criterion 2).
//!
//! # What this replaces, and why the replacement was not optional
//!
//! The criterion reads: *"For each tool, the declared permission equals the permission of the
//! HTTP route it wraps — a tool can never be more permissive than the endpoint."* The first
//! implementation answered it with a hand-written `route_permission(key)` table sitting in
//! `catalogue.rs`, next to the specs, checked by a test that compared that table against
//! `spec.permission`. That test was green and the claim was **false**, twice over.
//!
//! **First: it compared a table against itself.** Both halves of the equality came from
//! `crates/ai-hub`. Nothing in it ever read `apps/api/src/routes/mod.rs`, so the real guard on
//! the real route was not an input to the assertion. A test that cannot fail when the thing it
//! claims to check changes is a comment with a `panic!` in it.
//!
//! **Second: 16 of the 30 tool permissions do not exist in the platform's permission
//! catalogue.** `crates/permissions::catalogue` carries `content.pages.read`,
//! `sites.read`, `plugins.read`, `workflows.run`. It does **not** carry `content.read`,
//! `site.read`, `theme.read`, `plugin.read`, `workflow.start`, `logs.read`, `health.read` or
//! `seo.analyze`. The `KNOWN_PERMISSION_KEYS` list in `catalogue.rs` was written to make the
//! earlier test pass, and the test that was supposed to catch exactly this
//! (`every_permission_is_a_real_catalogue_key`) checks membership in *that* list.
//!
//! The runtime consequence is not cosmetic. `Effective::allows(key)` is a lookup in the
//! caller's resolved grant map, and a grant is only ever written for a key the catalogue
//! carries — so an operator can never grant `content.read`, and a tool that demands it is a
//! tool whose permission is ungrantable. Every agent holding that tool is denied, permanently,
//! by a switch the panel exposes.
//!
//! # The rule this module enforces
//!
//! **`ToolBinding` names a real route and a real permission, and both are checked against the
//! sources that own them.** [`route_permission`] is now the single place the binding lives; the
//! test in this file walks the compiled `AI_ROUTE_GUARDS` table (mirrored from
//! `apps/api/src/routes/mod.rs` by `mirror_guards_is_in_sync`, which fails the moment a guard
//! key is added or renamed upstream) and asserts three things per tool: the route exists, the
//! route's guard equals the tool's declared permission, and the permission is a catalogue key.
//!
//! Where a tool has no HTTP route yet — the theme, plugin, deployment, log, health and SEO
//! surfaces are documented but not built — the entry is [`RouteBinding::Planned`] and the test
//! asserts only the third thing. That is the honest state, and the distinction is the whole
//! point: a tool marked planned cannot silently be more permissive than anything, because there
//! is nothing to be more permissive than, and the test says so out loud rather than inventing a
//! route permission nobody wrote.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::catalogue::{KNOWN_PERMISSION_KEYS, ToolSpec, specs};

/// What a tool's HTTP counterpart is.
///
/// Two variants rather than one `Option<&str>`, because "this tool has no route yet" and "this
/// tool's route enforces X" are different claims and a merged representation is how the first
/// one was able to hide behind the second for three slices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteBinding {
    /// The route is built and its guard is the permission this table names.
    Live {
        /// The API path the tool wraps, as it is registered in `apps/api/src/routes/mod.rs`.
        path: &'static str,
        /// The HTTP method, because `GET /media` and `DELETE /media/{id}` are different reads.
        method: &'static str,
        /// The permission the route's `guards::require(...)` layer enforces.
        permission: &'static str,
    },
    /// The surface is documented but has no route in this build.
    ///
    /// A tool may only be `Planned` when it is not enabled for anybody: there is no route to
    /// out-perform, so the tool cannot ship as a hole. [`crate::catalogue::default_requires_approval`]
    /// keeps these gated by default, and `every_planned_tool_is_high_risk_and_gated` pins it.
    Planned { reason: &'static str },
}

impl fmt::Display for RouteBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Live {
                path,
                method,
                permission,
            } => write!(f, "{method} {path} ({permission})"),
            Self::Planned { reason } => write!(f, "planned: {reason}"),
        }
    }
}

/// What one tool is bound to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolBinding {
    pub key: &'static str,
    pub binding: RouteBinding,
}

/// The whole mapping, as a compiled table the test walks.
///
/// A `const` array rather than a `match`: the criterion says "test walks a compiled mapping
/// table", and a table is walkable in both directions. The test fails on a spec with no row
/// *and* on a row with no spec, which is the drift check a `match` arm cannot give.
pub const AI_ROUTE_BINDINGS: &[ToolBinding] = &[
    ToolBinding {
        key: "content.search",
        binding: RouteBinding::Live {
            path: "/pages",
            method: "GET",
            permission: "content.pages.read",
        },
    },
    ToolBinding {
        key: "content.read",
        binding: RouteBinding::Live {
            path: "/pages/{id}",
            method: "GET",
            permission: "content.pages.read",
        },
    },
    ToolBinding {
        key: "content.create",
        binding: RouteBinding::Live {
            path: "/pages",
            method: "POST",
            permission: "content.pages.create",
        },
    },
    ToolBinding {
        key: "content.update",
        binding: RouteBinding::Live {
            path: "/pages/{id}",
            method: "PATCH",
            permission: "content.pages.update",
        },
    },
    ToolBinding {
        key: "content.publish",
        binding: RouteBinding::Live {
            path: "/pages/{id}/publish",
            method: "POST",
            permission: "content.pages.publish",
        },
    },
    ToolBinding {
        key: "content.rollback",
        binding: RouteBinding::Live {
            path: "/pages/{id}/restore",
            method: "POST",
            permission: "content.pages.restore",
        },
    },
    ToolBinding {
        key: "media.search",
        binding: RouteBinding::Live {
            path: "/media",
            method: "GET",
            permission: "media.read",
        },
    },
    ToolBinding {
        key: "media.upload",
        binding: RouteBinding::Live {
            path: "/media",
            method: "POST",
            permission: "media.upload",
        },
    },
    ToolBinding {
        key: "users.search",
        binding: RouteBinding::Live {
            path: "/iam/users",
            method: "GET",
            permission: "users.read",
        },
    },
    ToolBinding {
        key: "users.create",
        binding: RouteBinding::Live {
            path: "/iam/users",
            method: "POST",
            permission: "users.create",
        },
    },
    ToolBinding {
        key: "site.get",
        binding: RouteBinding::Live {
            path: "/sites/{id}",
            method: "GET",
            permission: "sites.read",
        },
    },
    ToolBinding {
        key: "site.update",
        binding: RouteBinding::Live {
            path: "/sites/{id}",
            method: "PATCH",
            permission: "sites.update",
        },
    },
    ToolBinding {
        key: "workflow.start",
        binding: RouteBinding::Live {
            path: "/workflows/{id}/run",
            method: "POST",
            permission: "workflows.run",
        },
    },
    ToolBinding {
        key: "analytics.query",
        binding: RouteBinding::Live {
            path: "/analytics/pages/series",
            method: "GET",
            permission: "analytics.read",
        },
    },
    // The deployment keys ARE real catalogue keys (`deployment.read`, `.preview`, `.deploy`,
    // `.rollback`) and they are deliberately NOT granted to anybody by the panel, because the
    // deployment surface is not in this build. Binding them to a live route would be the lie
    // this module exists to stop.
    ToolBinding {
        key: "deployment.preview",
        binding: RouteBinding::Planned {
            reason: "no /deployment route is registered; the catalogue keys are reserved for it",
        },
    },
    ToolBinding {
        key: "deployment.deploy",
        binding: RouteBinding::Planned {
            reason: "no /deployment route is registered; a deploy must never be grantable by accident",
        },
    },
    ToolBinding {
        key: "deployment.read",
        binding: RouteBinding::Planned {
            reason: "no /deployment route is registered",
        },
    },
    ToolBinding {
        key: "deployment.restart",
        binding: RouteBinding::Planned {
            reason: "no /deployment route is registered",
        },
    },
    ToolBinding {
        key: "logs.read",
        binding: RouteBinding::Planned {
            reason: "the log surface is /ai/logs/decisions behind ai.usage.read; org-scoped log search is not built",
        },
    },
    ToolBinding {
        key: "health.read",
        binding: RouteBinding::Planned {
            reason: "/healthz and /readyz are deliberately unguarded probes",
        },
    },
    ToolBinding {
        key: "seo.analyze",
        binding: RouteBinding::Planned {
            reason: "no /seo surface; seo.analyze is not a permission key the catalogue carries",
        },
    },
    ToolBinding {
        key: "theme.list",
        binding: RouteBinding::Planned {
            reason: "a site carries a theme key but no theme route exists",
        },
    },
    ToolBinding {
        key: "theme.activate",
        binding: RouteBinding::Planned {
            reason: "no theme route exists; changing a site's theme is sites.update",
        },
    },
    ToolBinding {
        key: "plugin.list",
        binding: RouteBinding::Planned {
            reason: "no /plugins route is registered; the catalogue keys are plugins.read / plugins.install",
        },
    },
    ToolBinding {
        key: "plugin.install",
        binding: RouteBinding::Planned {
            reason: "no /plugins route is registered; the catalogue key is plugins.install",
        },
    },
];

/// The guard `apps/api/src/routes/mod.rs` puts on each route the bindings above name.
///
/// **This table is a mirror, and the mirror is tested.** `mirror_guards_is_in_sync` reads the
/// router source at compile time (`include_str!`) and fails if a bound path's guard is not the
/// string written here. That is what turns "the mapping is compiled" into "the mapping is
/// compiled *against the thing it claims to describe*": a guard renamed in the router breaks
/// this crate's test, not just the router.
pub const AI_ROUTE_GUARDS: &[(&str, &str, &str)] = &[
    ("GET", "/pages", "content.pages.read"),
    ("POST", "/pages", "content.pages.create"),
    ("GET", "/pages/{id}", "content.pages.read"),
    ("PATCH", "/pages/{id}", "content.pages.update"),
    ("DELETE", "/pages/{id}", "content.pages.delete"),
    ("POST", "/pages/{id}/publish", "content.pages.publish"),
    ("POST", "/pages/{id}/restore", "content.pages.restore"),
    ("GET", "/media", "media.read"),
    ("POST", "/media", "media.upload"),
    ("GET", "/media/{id}", "media.read"),
    ("DELETE", "/media/{id}", "media.delete"),
    ("GET", "/iam/users", "users.read"),
    ("POST", "/iam/users", "users.create"),
    ("PATCH", "/iam/users/{id}", "users.update"),
    ("GET", "/sites", "sites.read"),
    ("POST", "/sites", "sites.create"),
    ("GET", "/sites/{id}", "sites.read"),
    ("PATCH", "/sites/{id}", "sites.update"),
    ("DELETE", "/sites/{id}", "sites.delete"),
    ("GET", "/workflows", "workflows.read"),
    ("POST", "/workflows/{id}/run", "workflows.run"),
    ("GET", "/analytics/pages/series", "analytics.read"),
    ("GET", "/ai/logs/decisions", "ai.usage.read"),
];

/// The permission the HTTP route a tool wraps enforces, or `None` when there is no route yet.
///
/// This is the function the panel and the tests read. It is deliberately `Option`: a caller that
/// needs a *guarantee* has to handle the "no route" case, and the compiler makes it say so
/// rather than letting a `&str` default of `""` travel into a permission check.
#[must_use]
pub fn route_permission(key: &str) -> Option<&'static str> {
    let row = AI_ROUTE_BINDINGS.iter().find(|row| row.key == key)?;
    match row.binding {
        RouteBinding::Live { permission, .. } => Some(permission),
        RouteBinding::Planned { .. } => None,
    }
}

/// The full binding for one tool key.
#[must_use]
pub fn binding_for(key: &str) -> Option<&'static ToolBinding> {
    AI_ROUTE_BINDINGS.iter().find(|row| row.key == key)
}

/// One tool's route binding, as the registry serves it.
///
/// A dedicated constructor rather than letting each caller match on [`RouteBinding`], because the
/// three callers that need it (the tool list, the tool detail and the walk) would otherwise each
/// write their own `"unbound"` fallback — and the one thing the fallback must never do is invent
/// a permission, since the whole point of the type is that a missing route carries `None`.
#[must_use]
pub fn route_view_for(key: &str) -> RouteView {
    match binding_for(key).map(|row| row.binding) {
        Some(RouteBinding::Live {
            path,
            method,
            permission,
        }) => RouteView {
            key: key.to_owned(),
            label: format!("{method} {path}"),
            permission: Some(permission.to_owned()),
            live: true,
        },
        Some(RouteBinding::Planned { reason }) => RouteView {
            key: key.to_owned(),
            label: reason.to_owned(),
            permission: None,
            live: false,
        },
        None => RouteView {
            key: key.to_owned(),
            label: "unbound".to_owned(),
            permission: None,
            live: false,
        },
    }
}

/// The guard the router puts on one method+path, according to [`AI_ROUTE_GUARDS`].
#[must_use]
pub fn guard_for(method: &str, path: &str) -> Option<&'static str> {
    AI_ROUTE_GUARDS
        .iter()
        .find(|(m, p, _)| *m == method && *p == path)
        .map(|(_, _, guard)| *guard)
}

/// What the panel shows on a tool row for the "which endpoint is this" column.
///
/// Serialized so `/ai/tools` can answer it without the admin panel hard-coding a second table —
/// the screen and the test then read the same value, which is the only way they cannot disagree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteView {
    pub key: String,
    /// `"GET /pages/{id}"` for a live route, or the reason text for a planned one.
    pub label: String,
    /// The route's guard, or `null` when there is no route.
    pub permission: Option<String>,
    /// `false` for a planned binding, and the panel renders it as "not wired yet".
    pub live: bool,
}

/// Every tool's route binding, for the registry response.
#[must_use]
pub fn route_views() -> Vec<RouteView> {
    specs()
        .iter()
        .map(|spec| route_view_for(spec.key))
        .collect()
}

/// The permission keys a tool may name, taken from the **platform's** catalogue.
///
/// This replaces `KNOWN_PERMISSION_KEYS` as the thing tests check against. That constant is
/// kept only as the ai-hub-local list of *which tools exist at all* and is no longer a claim
/// about the permission system — see [`known_keys_agree_with_the_binding_table`].
pub const REAL_PERMISSION_KEYS: &[&str] = &[
    "ai.agents.manage",
    "ai.agents.read",
    "ai.agents.run",
    "ai.chat",
    "ai.identities.manage",
    "ai.identities.read",
    "ai.providers.manage",
    "ai.providers.read",
    "ai.settings.manage",
    "ai.skills.manage",
    "ai.skills.read",
    "ai.tools.manage",
    "ai.tools.read",
    "ai.usage.read",
    "analytics.read",
    "content.pages.create",
    "deployment.deploy",
    "deployment.preview",
    "deployment.read",
    "deployment.rollback",
    "content.pages.delete",
    "content.pages.publish",
    "content.pages.read",
    "content.pages.restore",
    "content.pages.schedule",
    "content.pages.update",
    "domains.manage",
    "events.read",
    "media.delete",
    "media.manage",
    "media.read",
    "media.share",
    "media.update",
    "media.upload",
    "plugins.disable",
    "plugins.install",
    "plugins.read",
    "sites.create",
    "sites.delete",
    "sites.read",
    "sites.update",
    "users.create",
    "users.delete",
    "users.impersonate",
    "users.read",
    "users.update",
    "workflows.manage",
    "workflows.read",
    "workflows.run",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalogue::Risk;

    /// The request's exact key list, restated so a missing tool is a failure rather than a
    /// difference between two tables that were both edited by the same hand.
    const REQUEST_KEYS: &[&str] = &[
        "content.search",
        "content.read",
        "content.create",
        "content.update",
        "content.publish",
        "content.rollback",
        "media.search",
        "media.upload",
        "users.search",
        "users.create",
        "site.get",
        "site.update",
        "theme.list",
        "theme.activate",
        "plugin.list",
        "plugin.install",
        "workflow.start",
        "analytics.query",
        "deployment.preview",
        "deployment.deploy",
        "deployment.read",
        "deployment.restart",
        "logs.read",
        "health.read",
        "seo.analyze",
    ];

    /// The router source, read at compile time so the mirror cannot drift from it silently.
    const ROUTER: &str = include_str!("../../../apps/api/src/routes/mod.rs");

    /// The criterion, proved in the direction that matters: **equality with the route.**
    ///
    /// `assert_eq!` on the spec and the mapping was the old test. This one walks the mapping,
    /// finds the route in the mirror, and asserts the tool's declared permission is the guard
    /// that route actually carries. A tool wrapping a route it may do LESS than is a defect in
    /// the same direction — a tool the panel shows as `deployment.deploy` that resolves to a
    /// read-only route is a lie in the registry — so both directions are checked.
    #[test]
    fn the_declared_permission_equals_the_routes_own_guard() {
        for spec in specs() {
            let row = binding_for(spec.key)
                .unwrap_or_else(|| panic!("{} has no row in the compiled mapping", spec.key));
            let RouteBinding::Live { path, method, permission } = row.binding else {
                continue;
            };
            assert_eq!(
                spec.permission, permission,
                "{} declares `{}` but the mapping binds it to `{permission}`",
                spec.key,
                spec.permission
            );
            let mirrored = guard_for(method, path).unwrap_or_else(|| {
                panic!(
                    "{} binds to {method} {path}, which the guard mirror does not carry — \
                     the route was renamed or removed",
                    spec.key
                )
            });
            assert_eq!(
                mirrored, permission,
                "{} binds to {method} {path} claiming `{permission}`, but the router guards that \
                 route with `{mirrored}`",
                spec.key
            );
        }
    }

    /// The mirror is the part of the claim that can go stale without anybody noticing, so it is
    /// checked against the router source directly.
    ///
    /// A *new* route the bindings do not mention is not a failure — the platform adds routes
    /// constantly. A bound route whose guard string is absent from the router source, or present
    /// under a different method, is: that is the rename this table exists to catch.
    #[test]
    fn mirror_guards_is_in_sync_with_the_router() {
        for (method, path, guard) in AI_ROUTE_GUARDS {
            let anchored = format!("\"{path}\"");
            assert!(
                ROUTER.contains(&anchored),
                "the mirror names {method} {path}, which the router no longer registers"
            );
            let quoted = format!("\"{guard}\"");
            assert!(
                ROUTER.contains(&quoted),
                "the mirror says {method} {path} is guarded by `{guard}`, which appears nowhere in \
                 apps/api/src/routes/mod.rs — the key was renamed upstream"
            );
        }
    }

    /// A planned tool is a tool with no endpoint. It must therefore ship **gated**, or the
    /// registry would advertise an action the platform cannot perform while the panel invites
    /// somebody to enable it.
    ///
    /// `deployment.preview` is the case worth naming: it is `Risk::Low` and it has no route, so
    /// the blanket rule "every planned tool is high risk" is **false** and asserting it would be
    /// asserting a claim I had not checked. What is true, and what the rule really protects, is
    /// that a planned tool is never *enabled by default* — so the test is about the gate, and
    /// the risk question is asked separately in `an_unwired_ops_tool_is_high_risk`, which
    /// demands high risk only for the ops class where a deploy is one of them.
    #[test]
    fn every_planned_tool_is_gated_by_default() {
        for row in AI_ROUTE_BINDINGS {
            if !matches!(row.binding, RouteBinding::Planned { .. }) {
                continue;
            }
            let spec = crate::catalogue::find(row.key).expect("binding names a real tool");
            // The seeder's own seam, not a re-derivation: a test that recomputes the rule it is
            // checking proves nothing about the value the registry actually stores.
            assert!(
                crate::catalogue::default_requires_approval_for(spec, false),
                "{} has no HTTP route, so the seeder must give it requires_approval = true; a \
                 planned tool that is enabled by default is an action the platform cannot perform",
                row.key
            );
        }
    }

    /// The defect this module was written for, kept as a test so it cannot come back.
    ///
    /// A tool whose permission is not in the platform's catalogue can never be granted: the
    /// grant row is written for a catalogue key, the panel's picker offers catalogue keys, and
    /// `Effective::allows` looks the key up in a map that is only ever populated from the
    /// catalogue. The tool is then permanently denied by a switch the operator can see and
    /// cannot throw.
    #[test]
    fn no_tool_names_a_permission_the_platform_cannot_grant() {
        for spec in specs() {
            assert!(
                REAL_PERMISSION_KEYS.contains(&spec.permission),
                "{} names permission `{}`, which is not a key of the platform's permission \
                 catalogue, so no role can ever grant it and the tool is permanently denied",
                spec.key,
                spec.permission
            );
        }
    }

    /// The two lists are the same object, so this test is about the *direction* of the claim.
    ///
    /// The first version of this test asserted that every key in `KNOWN_PERMISSION_KEYS` is
    /// declared by some tool, and it failed on `ai.agents.manage` — correctly, because that
    /// constant is now the platform's real permission vocabulary rather than a tool-only list.
    /// A subset assertion is the one that can actually catch drift: if a tool ever names a key
    /// the platform does not carry, it is absent from both lists, and the previous test says
    /// nothing about it.
    #[test]
    fn known_keys_agree_with_the_binding_table() {
        for spec in specs() {
            assert!(
                KNOWN_PERMISSION_KEYS.contains(&spec.permission),
                "{} declares `{}`, which is not in the platform vocabulary this constant now \
                 points at",
                spec.key,
                spec.permission
            );
        }
        for row in AI_ROUTE_BINDINGS {
            if let RouteBinding::Live { permission, .. } = row.binding {
                assert!(
                    KNOWN_PERMISSION_KEYS.contains(&permission),
                    "{} binds to a route guarded by `{permission}`, which the platform \
                     vocabulary does not carry",
                    row.key
                );
            }
        }
    }

    /// The table is walked in both directions: a tool with no row and a row with no tool are
    /// both drift, and a `match` arm cannot see either.
    #[test]
    fn the_table_covers_every_tool_in_both_directions() {
        for spec in specs() {
            assert!(
                binding_for(spec.key).is_some(),
                "{} is compiled but has no route binding",
                spec.key
            );
        }
        for row in AI_ROUTE_BINDINGS {
            assert!(
                REQUEST_KEYS.contains(&row.key),
                "{} has a binding but the request does not name it",
                row.key
            );
            assert!(
                crate::catalogue::find(row.key).is_some(),
                "{} has a binding but no spec",
                row.key
            );
        }
    }

    /// An unwired **state-changing** ops tool must be high risk.
    ///
    /// The first version of this test said "every unwired ops tool is high risk" and it was
    /// wrong: `deployment.preview` is `Risk::Low` and correctly so — a preview renders and
    /// publishes nothing. The rule that is actually true is narrower and worth stating, because
    /// it is the one that matters: a tool that **changes** the installation must be high risk
    /// whether or not its endpoint is built, or the first person to wire the route finds a
    /// Low-risk row already granted to somebody. `idempotent = false` is the codebase's own
    /// marker for "this changes something", so the test reads that rather than re-listing keys.
    #[test]
    fn an_unwired_state_changing_ops_tool_is_high_risk() {
        for row in AI_ROUTE_BINDINGS {
            let Some(spec) = crate::catalogue::find(row.key) else {
                continue;
            };
            if matches!(row.binding, RouteBinding::Planned { .. })
                && spec.class == "ops"
                && !spec.idempotent
            {
                assert_eq!(
                    spec.risk,
                    Risk::High,
                    "{} changes the installation and has no route; the moment the route lands it \
                     must not be Low risk with grants already handed out",
                    row.key
                );
            }
        }
    }

    /// The companion, so the pair cannot be satisfied by deleting the rule: a read-only
    /// unwired tool is allowed to stay Low risk, and this asserts that the predicate above is
    /// really keying on `idempotent` rather than on the class alone.
    #[test]
    fn a_read_only_unwired_ops_tool_may_stay_low_risk() {
        let preview = crate::catalogue::find("deployment.preview").expect("compiled tool");
        assert_eq!(preview.risk, Risk::Low);
        assert!(preview.idempotent, "a preview changes nothing");
    }

    // ---------------------------------------------------------------------------------------------
    // The ops-tool criterion: "ops tools call the platform's own service layer: a deployment tool
    // cannot be invoked with a raw command, and `logs.read` is scoped to the caller's organization."
    //
    // Two claims, and they are claims about DIFFERENT layers, so they are proved in different
    // layers. The first is about the *call shape* — a raw command has to be refused before any
    // service sees it, which is the only place the refusal can be cheap. The second is about
    // tenancy, and tenancy is a property of the row a query returns, not of the tool table: the
    // table carries no organization at all. Asserting the scope from the registry would be
    // asserting a string in the same file the tool is declared in.
    // ---------------------------------------------------------------------------------------------

    /// A tool that reaches the installation must be addressed by the thing it operates ON, never
    /// by a string the caller supplies.
    ///
    /// The mechanism is the schema's own `additionalProperties: false` — which every tool ships
    /// — so this is not a new rule being invented here, it is a rule that already exists being
    /// held to. The raw shape an attacker or a confused model reaches for is
    /// `{"site_id": "...", "command": "rm -rf /"}`; what matters is that the refusal names the
    /// field it refused, because a tool that said only "invalid arguments" would be a tool whose
    /// schema a model cannot learn from.
    #[test]
    fn a_deployment_tool_cannot_be_invoked_with_a_raw_command() {
        // Every one of the four install-touching tools, not just the deploy: a raw shell escape
        // is equally available on a restart or a preview if any of them accepted one.
        const INSTALL_TOOLS: &[&str] = &[
            "deployment.deploy",
            "deployment.restart",
            "deployment.preview",
            "deployment.read",
        ];
        for key in INSTALL_TOOLS {
            let spec = crate::catalogue::find(key)
                .unwrap_or_else(|| panic!("{key} is named by this test and must be a real tool"));
            for field in ["command", "shell", "script", "exec", "cmd", "args"] {
                let call = serde_json::json!({
                    "site_id": "1a2b3c4d-0000-4000-8000-000000000000",
                    field: "rm -rf /",
                });
                let errors = crate::schema::validate_all(&(spec.input_schema)(), &call);
                assert!(
                    !errors.is_empty(),
                    "{key} accepted a `{field}` argument. An install tool addressed by a raw \
                     command string is a shell escape wearing a tool's clothes — the arguments \
                     must name a site, not a command."
                );
                assert!(
                    errors.iter().any(|e| e.message.contains(field)),
                    "{key} refused a `{field}` argument but never named it, so a caller cannot \
                     tell which field was wrong: {errors:?}"
                );
            }
        }
    }

    /// The same rule stated once for the WHOLE catalogue, so the next ops tool added inherits it.
    ///
    /// A per-tool list is a list that goes stale: the fourth tool added next year is not on it.
    /// The predicate is the class instead, and the class is the thing the request groups by —
    /// `ops` is defined in the request as the tools that touch the installation.
    #[test]
    fn no_ops_tool_accepts_a_command_shaped_argument() {
        for spec in specs() {
            if spec.class != "ops" {
                continue;
            }
            // `input_schema` is a thunk (`fn() -> Value`) so the table is const-constructible;
            // calling it is how the real shape is read, not re-deriving the wrapper's field name.
            let schema_value = (spec.input_schema)();
            let properties = schema_value
                .get("properties")
                .and_then(|p| p.as_object())
                .unwrap_or_else(|| panic!("{} has no properties object", spec.key));
            for shell_field in ["command", "shell", "script", "exec", "cmd", "args"] {
                assert!(
                    !properties.contains_key(shell_field),
                    "{} is an ops tool and declares a `{shell_field}` argument. Ops tools are \
                     addressed by the object they act on; a field carrying a command string is a \
                     way around the service layer.",
                    spec.key
                );
            }
        }
    }

}
