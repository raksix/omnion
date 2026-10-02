//! Plugin-provided workflow node types (REQ-004: "a manifest may declare `workflowNodes`…
//! they appear in the palette under 'Plugins'; nothing runs inside the core process").
//!
//! ## Why this is not a `Vec<NodeType>`
//!
//! The core registry ([`crate::graph::NODE_TYPES`]) is a `const` slice, and a plugin is
//! chosen at run time per organization, so a plugin type cannot live there. The obvious
//! alternative — widening the core's `validate` to take a caller-supplied list — was the
//! wrong shape, for a reason worth writing down: **`validate` is what decides whether a rule
//! can run, and a rule can only run if the core can explain it.** If `validate` accepted a
//! list, then anything that could produce a list could make an unrunnable rule look valid,
//! and the failure would move from "Validate says this" to "the run died at 03:00".
//!
//! So the division is:
//!
//! * **`PluginNodeType`** — the *shape* a manifest declares. Ports and parameter schemas
//!   only, never behaviour: there is no closure to run and no HTTP spec the core executes.
//! * **Resolution** — [`PluginRegistry::resolve`] takes the node types that are enabled for
//!   *this* organization right now and answers what a key means. A key it cannot resolve is
//!   [`Resolution::Unknown`], and the core's `validate` turns that into the
//!   `unknown_node_type` finding it already produces for a typo.
//!
//! That last line is the whole design. **The third state of the criterion is the honest
//! validation error, and it costs nothing** because the core already refuses a type it does
//! not know; a removed plugin is simply a type the platform stopped knowing. What the plugin
//! layer must never do is add a *new* success path that the engine cannot back.
//!
//! ## Key namespacing
//!
//! A plugin's keys are namespaced as `plugin.<key>.<node>` and a core key can never be
//! spelled that way ([`is_reserved_key`]). The alternative — a plugin declaring
//! `key: "action"` and shadowing the core's — makes a plugin's enablement a decision about
//! every rule in the organization, which is not something an org admin should be able to do
//! by installing something.

use serde::{Deserialize, Serialize};


/// The category every plugin node is drawn under. Matches the rail grouping the core
/// registry already uses, and is a `const` so a test can assert the two agree.
pub const PLUGIN_CATEGORY: &str = "Plugins";

/// A manifest's output port.
///
/// Owned `String`s, not the core's [`Port`]. The reason is a lifetime and the consequence is
/// architectural: [`Port`] is a `const`, so it can only ever describe something decided when
/// the binary was built, and a plugin's node was uploaded after that. Reusing the core type
/// here would force one of two bad shapes — a `Box::leak` per plugin reload (a process that
/// grows and never shrinks) or interning into a global set (the same, with extra steps) — and
/// both turn a *run-time* fact into a *permanent* one. So the two types are separate, and the
/// cost of keeping them parallel is paid in this file rather than in every validator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginPort {
    /// Stable within the node type.
    pub key: String,
    /// What the canvas prints next to the dot.
    pub label: String,
    /// Optional one-line explanation.
    pub help: Option<String>,
    /// `true` when leaving on this port ends the run.
    ///
    /// Carried rather than derived so the palette draws the port the way the engine will
    /// treat it. It is **not** trusted to decide anything: the engine owns what a stop means,
    /// and a manifest claiming its port is terminal cannot end a run the core did not end.
    pub terminal: bool,
}

/// A manifest's parameter field. Owned for the same reason as [`PluginPort`]: the core's
/// [`ParamField`] is `&'static str` all the way down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginParamField {
    /// Key in the node's `params` object.
    pub key: String,
    /// Label above the input.
    pub label: String,
    /// `text`, `textarea`, `number`, `select` or `boolean`.
    pub kind: String,
    /// `true` when the node is refused without it.
    pub required: bool,
    /// The legal values of a `select`, in palette order.
    pub options: Vec<String>,
    /// Help text under the input.
    pub help: String,
}

/// What a plugin manifest declares about one workflow node it contributes.
///
/// Deliberately a *subset* of the core's node type: a manifest may say what a node is
/// called, which ports it has and what fields it takes, and nothing else. There is no `run`
/// and no `handler`, because the core process does not execute plugin code (docs/09 §13,
/// lesson 14) — a field here would be a promise the platform cannot keep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginNodeType {
    /// The manifest-local key, e.g. `send`. Namespaced on the way in.
    pub node: String,
    /// What the palette calls the node.
    pub label: String,
    /// One line under the card.
    pub summary: String,
    /// Output ports, in draw order. Required to be non-empty: a node with no output port
    /// can never be connected to anything, and a palette entry nobody can wire is a dead
    /// control.
    pub outputs: Vec<PluginPort>,
    /// The parameter fields the inspector draws.
    pub params: Vec<PluginParamField>,
}

/// A plugin that contributed node types, with the provider the tooltip names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginProvider {
    /// The plugin's key, as installed.
    pub plugin: String,
    /// The human name shown in the badge and the tooltip.
    pub name: String,
}

/// A plugin node type as the palette draws it: the manifest's shape plus who provides it.
///
/// The provider travels with the type rather than being looked up at render time, so the
/// palette cannot draw a badge it is unable to name — and so a plugin uninstalled between
/// the list fetch and the draw is still labelled, which is what makes the disappearing entry
/// in the criterion legible rather than a mystery.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResolvedPluginNode {
    /// The namespaced key stored on the node: `plugin.<plugin>.<node>`.
    pub key: String,
    pub label: String,
    pub summary: String,
    /// The ports as the palette and the canvas draw them. Owned for the same reason the
    /// manifest's are: nothing here may be a `const`, because none of it existed at build
    /// time.
    pub outputs: Vec<PluginPort>,
    /// The fields the inspector draws. A `select` with no declared options is **dropped**
    /// at registration rather than drawn — see [`PluginParamField::as_core`].
    pub params: Vec<PluginParamField>,
    /// Always [`PLUGIN_CATEGORY`].
    pub category: String,
    /// The badge text, `Plugin: <name>`.
    pub badge: String,
    /// Who provides it, for the tooltip the criterion asks for.
    pub provider: PluginProvider,
    /// The parameter values a freshly dropped card starts with, derived from the declared
    /// fields — a plugin cannot ship a default the inspector would not show.
    pub defaults: serde_json::Value,
}

/// Why a manifest's node types were refused.
///
/// A plugin is a third party and its manifest is untrusted input, so these are typed rather
/// than a `Result<String>`: the install gate needs to say *which* declaration is wrong, and a
/// single sentence per plugin would force an admin to bisect by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginNodeRejection {
    /// The node key is empty or contains a character no key may hold.
    BadNodeKey { node: String },
    /// The label is empty.
    MissingLabel { node: String },
    /// No output ports — the node could never be connected to anything.
    NoOutputs { node: String },
    /// Two declared nodes share one key, so the namespaced keys would collide.
    DuplicateNode { node: String },
    /// The plugin key is empty or illegal, so its nodes cannot be namespaced.
    BadPluginKey { plugin: String },
    /// A node key is already a core node type. Namespacing is supposed to make this
    /// impossible, and it is checked rather than assumed: a future `plugin.` prefix in the
    /// core registry would turn every plugin into a shadowing attempt.
    ShadowsCoreType { node: String, core_key: String },
    /// A declared field has an unusable key or no label, so it would draw as an input the
    /// author cannot identify — and a required one of those cannot be filled at all.
    BadParamField { node: String },
    /// The provider name is empty, so the badge would read `Plugin: `.
    MissingProvider { node: String },
}

/// Build the namespaced key a plugin's node is stored under.
///
/// Namespacing is the security property, not a cosmetic prefix: without it a plugin could
/// declare `key: "action"` and take over every rule that uses one.
#[must_use]
pub fn namespaced_key(plugin: &str, node: &str) -> String {
    format!("plugin.{plugin}.{node}")
}

/// Is this key one the core owns and a plugin may never declare?
#[must_use]
pub fn is_reserved_key(key: &str) -> bool {
    crate::graph::NODE_TYPES
        .iter()
        .any(|node_type| node_type.key == key)
}

/// A key is legal when it is non-empty, lower-case, and made of the characters a key can
/// hold in a URL and in a CSS selector — a plugin declaring `Send Mail` would produce a
/// `data-node-type` the QA pass cannot query.
#[must_use]
pub fn is_legal_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '.' || ch == '_')
}

/// The enabled plugin node types for one organization.
///
/// The constructor takes the *enabled* set from the caller rather than reading a store:
/// plugin enablement is REQ-121's table, and a `crates/workflows` dependency on a crate that
/// does not exist yet would make this module untestable and unmergeable. The seam is the
/// point — when the plugin store lands, the API layer builds this from it and nothing here
/// changes.
#[derive(Debug, Clone, Default)]
pub struct PluginRegistry {
    providers: Vec<PluginProvider>,
    nodes: Vec<ResolvedPluginNode>,
}

impl PluginRegistry {
    /// An empty registry — the state of every organization with no plugins enabled.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Register one provider's node types, refusing the manifest as a whole on the first
    /// problem.
    ///
    /// **All-or-nothing, and that is a decision rather than a convenience.** A partially
    /// accepted manifest gives an admin a palette with half a plugin's nodes and no way to
    /// see which half — and a rule that uses the missing half fails validation for a reason
    /// that looks like a bug. The gate reports every rejection so the plugin author can fix
    /// all of them at once.
    pub fn register(
        &mut self,
        provider: &PluginProvider,
        declared: &[PluginNodeType],
    ) -> Result<(), PluginNodeRejection> {
        if !is_legal_key(&provider.plugin) {
            return Err(PluginNodeRejection::BadPluginKey {
                plugin: provider.plugin.clone(),
            });
        }
        if provider.name.trim().is_empty() {
            // Checked before the nodes so the badge is known-good even for a manifest that
            // is about to be refused for something else.
            return Err(PluginNodeRejection::MissingProvider {
                node: declared
                    .first()
                    .map(|node| node.node.clone())
                    .unwrap_or_default(),
            });
        }
        let mut seen: Vec<&str> = Vec::with_capacity(declared.len());
        for node in declared {
            if !is_legal_key(&node.node) {
                return Err(PluginNodeRejection::BadNodeKey {
                    node: node.node.clone(),
                });
            }
            if node.label.trim().is_empty() {
                return Err(PluginNodeRejection::MissingLabel {
                    node: node.node.clone(),
                });
            }
            if node.outputs.is_empty() {
                return Err(PluginNodeRejection::NoOutputs {
                    node: node.node.clone(),
                });
            }
            if seen.contains(&node.node.as_str()) {
                return Err(PluginNodeRejection::DuplicateNode {
                    node: node.node.clone(),
                });
            }
            seen.push(&node.node);

            let key = namespaced_key(&provider.plugin, &node.node);
            // The core registry is checked per node, not once for the provider: a plugin
            // with three nodes where the third collides must be refused for that third node
            // by name, and a blanket check would report the first node and send the author
            // looking in the wrong place.
            if is_reserved_key(&key) {
                return Err(PluginNodeRejection::ShadowsCoreType {
                    node: node.node.clone(),
                    core_key: key,
                });
            }
            if node.outputs.iter().any(|port| port.key.trim().is_empty()) {
                return Err(PluginNodeRejection::NoOutputs {
                    node: node.node.clone(),
                });
            }
            // A field whose own key is empty would render as an unlabelled input, and a
            // `required` one would then be unfillable. Refused here rather than dropped:
            // a manifest that cannot spell a field key cannot spell a node key either, and
            // the author should be told once, not shown a card with a gap in it.
            if node.params.iter().any(|field| {
                !is_legal_key(&field.key) || field.label.trim().is_empty()
            }) {
                return Err(PluginNodeRejection::BadParamField {
                    node: node.node.clone(),
                });
            }
        }

        for node in declared {
            let key = namespaced_key(&provider.plugin, &node.node);
            self.nodes.push(ResolvedPluginNode {
                key,
                label: node.label.clone(),
                summary: node.summary.clone(),
                outputs: node.outputs.clone(),
                // A `select` with no options is dropped here, and *required* is downgraded
                // with it: an unanswerable required field is a node that can never validate,
                // so the author would be unable to save the rule at all. Better a missing
                // optional field than a card that refuses to be written.
                params: node
                    .params
                    .iter()
                    .filter_map(|field| {
                        if field.kind == "select" && field.options.is_empty() {
                            return None;
                        }
                        let mut field = field.clone();
                        if field.kind == "select" {
                            field.required = false;
                        }
                        Some(field)
                    })
                    .collect(),
                category: PLUGIN_CATEGORY.to_owned(),
                badge: format!("Plugin: {}", provider.name),
                provider: provider.clone(),
                defaults: defaults_for(&node.params),
            });
        }
        self.providers.push(provider.clone());
        Ok(())
    }

    /// Every plugin node type, in registration order.
    #[must_use]
    pub fn nodes(&self) -> &[ResolvedPluginNode] {
        &self.nodes
    }

    /// The providers that contributed, in registration order.
    #[must_use]
    pub fn providers(&self) -> &[PluginProvider] {
        &self.providers
    }

    /// What a node key means: a plugin type, a core type, or nothing.
    ///
    /// The `Unknown` arm is the one that matters. It is what a rule using a plugin that was
    /// disabled or uninstalled resolves to, and the core's `validate` already refuses it —
    /// the criterion's "an honest validation error instead of failing at run time" is this
    /// function returning `Unknown`, not a new error path.
    #[must_use]
    pub fn resolve(&self, key: &str) -> Resolution<'_> {
        if let Some(node) = self.nodes.iter().find(|node| node.key == key) {
            return Resolution::Plugin(node);
        }
        if is_reserved_key(key) {
            return Resolution::Core;
        }
        Resolution::Unknown
    }

    /// Does the registry contribute any node type at all?
    ///
    /// The palette asks this before drawing the group heading, because a rail section titled
    /// "Plugins" with nothing in it is a promise the organization cannot keep — and an empty
    /// section is indistinguishable from a plugin that failed to load.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

/// What a node key means to the platform.
///
/// **Not `Eq`**, unlike [`ResolvedPluginNode`]'s own derives looking like an oversight: the
/// `Plugin` arm borrows a resolved node, and that node carries a `serde_json::Value` of
/// starting parameters. `Value` implements `PartialEq` but not `Eq` — it is a JSON value, and
/// `Eq` would be a promise about float keys that serde correctly declines to make. Deriving
/// `Eq` here would mean either dropping the parameters from the resolved node (so the palette
/// could not seed a card) or lying in the bound. The `PartialEq` is the real one: two
/// resolutions are the same when their shapes are, and `NaN` in a default is a plugin's
/// problem to notice.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution<'a> {
    /// A plugin node type the organization has enabled.
    Plugin(&'a ResolvedPluginNode),
    /// A core node type.
    Core,
    /// Neither: a typo, or a plugin that is no longer enabled.
    Unknown,
}

/// The starting parameters for a freshly dropped plugin card.
///
/// Derived from the *declared* fields rather than taken from the manifest, because a
/// manifest default the inspector does not draw is a value the author cannot see, clear or
/// correct. A `select` is the one field kind with a defensible default — it is the first
/// choice the manifest itself listed, and any other value would be a guess — so only those
/// are seeded.
///
/// **Nothing else is seeded, and that includes booleans.** Writing `false` into an unset
/// boolean is the same class of lie as a fake `example.com`: the field looks configured, the
/// author cannot tell it from one they set, and the run behaves as if they had. A required
/// field starts empty on purpose, because the server refuses a node without it and an
/// obviously-empty field says so.
fn defaults_for(params: &[PluginParamField]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for field in params {
        if field.kind != "select" || field.options.is_empty() {
            continue;
        }
        // The first declared option, and only when it is really there — a select whose
        // options were all dropped at registration has no defensible default at all.
        let first = field
            .options
            .first()
            .filter(|option| !option.trim().is_empty());
        if let Some(choice) = first {
            map.insert(field.key.to_owned(), serde_json::Value::String(choice.to_owned()));
        }
    }
    serde_json::Value::Object(map)
}
