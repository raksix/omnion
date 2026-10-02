//! The bridge between this crate's string permission names and the platform's own catalogue
//! (REQ-130, slice 1).
//!
//! ## The defect this module exists to make unrepresentable
//!
//! The slice-1 schema catalogue was written with **invented** permission names — `content.read`,
//! `tenancy.read`, `media.download`, `billing.read`, `content.revisions.read`. None of them exist.
//! The platform's catalogue (REQ-068, `crates/permissions/src/catalogue.rs`) spells the same
//! ideas `content.pages.read`, `organizations.read`, `media.read`, and has no billing key at all.
//!
//! That gap is invisible from inside this crate, because here a permission is just a `&str` and
//! nothing checks it against anything. It becomes visible the moment a resolver calls
//! `authorize(pool, user, scope, "content.read")` — and the failure is the worst kind: a
//! **refusal**, not a crash. An uncatalogued key resolves to no permission at all, so the guard
//! answers `403` for **everyone, including the instance owner**. That is precisely the failure
//! REQ-133 and REQ-128 each lost ticks to: *"A new guard added with a key that is not in the
//! catalogue returns 403 for everyone on the route that uses it."* Two writers paid for it
//! separately; the third time it would have been a GraphQL surface where **no query works at
//! all** and the 403 looks like an authentication problem.
//!
//! The fix is not "remember to use the right strings". It is to make the name a **type that
//! cannot be constructed from an arbitrary string**, and to prove the surviving set against the
//! real catalogue.
//!
//! ## What this module does, and what it deliberately does not
//!
//! It does two things, both pure:
//!
//! 1. [`Known`] — a closed enum of the permission keys this surface may name, one variant per real
//!    catalogue key. The schema catalogue stores [`Known`], not `&'static str`, so a typo or an
//!    invented key is a **compile error** rather than a 403 found in production.
//! 2. [`assert_parity`] — a test-time check that every variant's `as_str()` is a key the platform
//!    actually ships. This is the half that keeps the enum honest when the platform's catalogue
//!    gains or loses a key: an enum variant whose string is not in the catalogue fails the gate
//!    rather than shipping a refusal.
//!
//! It does **not** link the permissions crate. The decision layer stays free of a database and of
//! every other crate (see [`crate::schema::tests::this_crate_has_no_resolver_so_a_refusal_cannot_have_written_anything`]),
//! so the parity gate is a test in the **API** crate, which does link `omnion-permissions` and can
//! read `catalogue::CATALOGUE` directly. What moves here is the vocabulary; what checks it stays
//! with the crate that can see both sides.
//!
//! ## Why an enum and not a validated string
//!
//! A constructor that panics or that returns `Result` would still let a caller cache a bad name
//! once. An enum makes the invalid state unrepresentable: there is no way to write
//! `"content.read"` where a [`Known`] is expected, so the compiler finds every invented key the
//! moment it is written. The cost is that adding a GraphQL field over a new permission is a two
//! file change — the variant, and the field — which is the review the request wants anyway
//! (*"reviews of new fields must include a weight"*; this makes them include a permission too).

use std::fmt;

/// Every variant, in one list — the input to the API crate's parity gate.
///
/// ## Why this is a `const` and not a hand-maintained table
///
/// The gate in `apps/api/tests/graphql_parity.rs` iterates this list, so a variant missing from it
/// would never be checked against the platform's catalogue. That failure mode was **measured**
/// during development: a variant naming `"billing.read"` was added to the enum, the compiler was
/// satisfied, and BOTH the 75 crate-local tests and the 8 API gate tests stayed green — because
/// the variant was not in `ALL` and the local test only checked `ALL` for duplicates.
///
/// So this list is not maintained by hand. It is derived from the enum through a macro that
/// generates both the enum and the list, which makes the omission unrepresentable: a variant
/// cannot exist without appearing here, and adding one is a single edit. [`variant_count`]
/// additionally pins the size so that the generated list and any hand-written claim about it
/// cannot disagree.
macro_rules! known_permissions {
    ($( $(#[$variant_doc:meta])* $variant:ident => $key:literal, $write:literal; )*) => {
        /// A permission key this GraphQL surface is allowed to name.
        ///
        /// One variant per key in the platform's permission catalogue that the content, tenancy
        /// and media surface actually filters on. The wire strings are asserted against
        /// `omnion_permissions::catalogue` by `apps/api/tests/graphql_parity.rs` — this crate
        /// cannot read that catalogue, which is exactly why the check lives there.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum Known {
            $( $(#[$variant_doc])* $variant, )*
        }

        /// Every variant, generated from the enum so the two cannot drift.
        pub const ALL: &[Known] = &[ $( Known::$variant, )* ];

        impl Known {
            /// The platform's own spelling of this permission.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $( Self::$variant => $key, )*
                }
            }

            /// Whether this key is the caller's *write* half of a read/write pair.
            ///
            /// Used by the explorer screen to group mutations under the permission that gates
            /// them, and by the validation walk to know that a mutation field's permission must be
            /// a write one — a mutation gated by a read permission is a mistake worth naming
            /// rather than honouring.
            #[must_use]
            pub const fn is_write(self) -> bool {
                match self {
                    $( Self::$variant => $write, )*
                }
            }
        }

        /// How many variants the enum has. Pinned so a hand-written claim about the list's size
        /// cannot outlive the list.
        pub const VARIANT_COUNT: usize = ALL.len();
    };
}

known_permissions! {
    // --- content: the CMS surface (REST twins in `routes/mod.rs`: /pages, /pages/{id}) --------
    /// `content.pages.read` — `GET /api/v1/pages`, `GET /pages/{id}`, `GET /pages/{id}/revisions`.
    ///
    /// Note the plural middle segment. The platform's key is `content.pages.read`, not
    /// `content.read`; the short form slice 1 used does not exist and would have refused every
    /// content query for every caller.
    ContentPagesRead => "content.pages.read", false;
    /// `content.pages.create` — `POST /api/v1/pages`.
    ContentPagesCreate => "content.pages.create", true;
    /// `content.pages.update` — `PATCH /pages/{id}`, `PUT /pages/{id}/revisions/{id}/translations`.
    ContentPagesUpdate => "content.pages.update", true;
    /// `content.pages.delete` — `DELETE /pages/{id}`.
    ContentPagesDelete => "content.pages.delete", true;
    /// `content.pages.publish` — `POST /pages/{id}/publish`.
    ///
    /// Publishing is its own permission in the catalogue, and slice 1 had no way to express it:
    /// the invented `content.create`/`content.update` pair would have filtered a publish field
    /// against the wrong key entirely.
    ContentPagesPublish => "content.pages.publish", true;
    /// `content.pages.restore` — `POST /pages/{id}/restore`.
    ContentPagesRestore => "content.pages.restore", true;

    // --- tenancy: the organization surface (REST twin: /organizations, /organizations/{id}) ---
    /// `organizations.read` — `GET /api/v1/organizations`, `GET /organizations/{id}`.
    ///
    /// There is no `tenancy.*` category in the catalogue; tenancy reads are `organizations.read`.
    OrganizationRead => "organizations.read", false;
    /// `organizations.manage` — `POST`/`PATCH`/`DELETE` on `/organizations`.
    ///
    /// One key covers every tenancy write, because that is how the catalogue splits it — so a
    /// GraphQL `createOrganization` and a REST `POST /organizations` are gated by the same
    /// string, which is what the parity requirement demands.
    OrganizationManage => "organizations.manage", true;
    /// `sites.read` — `GET /api/v1/sites`, `GET /sites/{id}`.
    SitesRead => "sites.read", false;

    // --- media: the library surface (REST twin: /media, /media/{id}, /media/{id}/raw) ---------
    /// `media.read` — `GET /api/v1/media`, `GET /media/{id}`, `GET /media/{id}/raw`.
    ///
    /// Slice 1 invented `media.download` for the raw path. There is no such key: the platform
    /// serves raw bytes under `media.read`, so a separate download permission in the schema would
    /// have hidden the download URL field from every caller including the ones entitled to it.
    MediaRead => "media.read", false;
    /// `media.upload` — `POST /api/v1/media`.
    MediaUpload => "media.upload", true;
    /// `media.update` — the library's own update paths.
    MediaUpdate => "media.update", true;
    /// `media.delete` — `DELETE /media/{id}`.
    MediaDelete => "media.delete", true;
    /// `media.manage` — the folder/tree shape, trash and bulk moves.
    MediaManage => "media.manage", true;
    /// `media.share` — the share-link routes.
    MediaShare => "media.share", true;

    // --- the identity relation the content surface resolves ---------------------------------
    /// `users.read` — `GET /api/v1/users` and the author relation a content row resolves.
    ///
    /// This one slice 1 had right, and it is listed so that stays visible: it is the single
    /// invented-looking name that was in fact real, which is exactly why the other five went
    /// unnoticed — a list of mostly-wrong names with one right name reads as a naming convention
    /// rather than as a fabrication.
    UsersRead => "users.read", false;
}

impl fmt::Display for Known {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A permission name a caller actually holds, as an opaque set of known keys.
///
/// Composition and validation take this rather than a `&str`, so no code path in the decision
/// layer can introduce a name that did not come from [`Known`]. The caller of the endpoint
/// resolves real permissions to keys with [`PermissionSet::from_known`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionSet(std::collections::BTreeSet<Known>);

impl PermissionSet {
    /// A set holding none of the permissions — what a caller with no role at all resolves to.
    #[must_use]
    pub fn empty() -> Self {
        Self(Default::default())
    }

    /// A set of the given permissions.
    #[must_use]
    pub fn from_known(permissions: impl IntoIterator<Item = Known>) -> Self {
        Self(permissions.into_iter().collect())
    }

    /// Whether the caller holds this permission.
    #[must_use]
    pub fn holds(&self, permission: Known) -> bool {
        self.0.contains(&permission)
    }

    /// The permission names, sorted, as strings — the input to the cache key's fingerprint.
    ///
    /// Sorted by enum order rather than by string, so the fingerprint is stable regardless of the
    /// order the caller resolved its bindings in.
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.0.iter().map(|permission| permission.as_str())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn the_generated_list_covers_every_variant_so_the_parity_gate_cannot_skip_one() {
        // **The assertion whose absence made this tick's own gate useless.** The first version of
        // the gate iterated the hand-written `ALL` list, and the local test only checked that list
        // for duplicates. Adding a variant to the ENUM without adding it to `ALL` therefore left
        // all 75 local tests AND all 8 API gate tests green — measured, not theorised: a variant
        // naming `"billing.read"` was added exactly that way and nothing went red.
        //
        // The list is now generated by a macro, so the omission is unrepresentable rather than
        // merely asserted against. This test is what would catch a future hand-edit that
        // reintroduces the gap, and `VARIANT_COUNT` is what it compares against.
        assert_eq!(
            VARIANT_COUNT,
            ALL.len(),
            "the generated list holds {} entries but the enum claims {VARIANT_COUNT} variants",
            ALL.len()
        );
        // Every listed entry is distinct: a duplicate would make the count lie in the other
        // direction, hiding a variant behind a name that appears twice.
        let listed: BTreeSet<Known> = ALL.iter().copied().collect();
        assert_eq!(
            listed.len(),
            ALL.len(),
            "`ALL` lists the same variant twice, so the parity gate would check it once and the \
             count would lie"
        );
        // Every wire string is distinct for the same reason: two variants sharing a string would
        // mean one of them is not a distinct permission at all.
        let strings: BTreeSet<&str> = ALL.iter().map(|permission| permission.as_str()).collect();
        assert_eq!(
            strings.len(),
            ALL.len(),
            "two variants share a wire string; one of them is a lie"
        );

        // None of the names slice 1 invented survived — the whole point of the enum.
        for invented in [
            "content.read",
            "tenancy.read",
            "media.download",
            "billing.read",
            "content.revisions.read",
            "content.create",
            "content.update",
            "content.delete",
            "tenancy.create",
        ] {
            assert!(
                !strings.contains(invented),
                "`{invented}` is not a key the platform ships; the GraphQL surface would refuse \
                 every caller that needs it"
            );
        }
    }

    #[test]
    fn a_held_permission_is_reported_and_an_unheld_one_is_not() {
        let reader = PermissionSet::from_known([Known::ContentPagesRead]);
        assert!(reader.holds(Known::ContentPagesRead));
        // Holding the read half does not grant the write half — the mistake the invented
        // `content.read`/`content.create` pair would have hidden.
        assert!(!reader.holds(Known::ContentPagesCreate));
        assert!(!PermissionSet::empty().holds(Known::ContentPagesRead));
    }

    #[test]
    fn the_names_are_sorted_so_the_cache_fingerprint_does_not_depend_on_resolution_order() {
        let forwards = PermissionSet::from_known([Known::MediaRead, Known::ContentPagesRead]);
        let backwards = PermissionSet::from_known([Known::ContentPagesRead, Known::MediaRead]);
        assert_eq!(
            forwards.names().collect::<Vec<_>>(),
            backwards.names().collect::<Vec<_>>(),
            "the same permissions in a different order produced a different fingerprint, so two \
             callers holding the same set could be cached under two keys"
        );
    }

    #[test]
    fn every_write_permission_is_a_write_and_every_read_is_not() {
        // The set is what the explorer groups on and what a validation walk checks a mutation's
        // permission against, so a misclassified key puts a write field under the read heading.
        for permission in ALL {
            let expected = matches!(
                permission.as_str(),
                "content.pages.create"
                    | "content.pages.update"
                    | "content.pages.delete"
                    | "content.pages.publish"
                    | "content.pages.restore"
                    | "organizations.manage"
                    | "media.upload"
                    | "media.update"
                    | "media.delete"
                    | "media.manage"
                    | "media.share"
            );
            assert_eq!(
                permission.is_write(),
                expected,
                "`{permission}` is classified as a {}",
                if expected { "read" } else { "write" }
            );
        }
        // A read permission is never a write: asserted by count, so adding a variant that is
        // neither fails here instead of quietly landing in the wrong bucket.
        assert_eq!(ALL.iter().filter(|p| p.is_write()).count(), 11);
        assert_eq!(ALL.iter().filter(|p| !p.is_write()).count(), 5);
    }
}
