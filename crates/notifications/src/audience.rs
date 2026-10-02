//! Who an emit may address, as one rule with two answers.
//!
//! ## Why this file exists
//!
//! `POST /notifications/emit` did three separate things and checked only the first. It asked
//! [`crate::store::existing_users`] — *is this a real account* — it stamped every row with
//! the **sender's** `organization_id`, and it never asked whether the recipient belongs to the
//! sender's tenant. `users.organization_id` is nullable, so "a real account" and "somebody in
//! my organization" are different sentences, and a tenant holding `notifications.send` could
//! write a row into any other tenant's inbox. Measured before the fix: a 202, `created: 1`,
//! and the row carried the *sending* organization next to a recipient belonging to a
//! different one.
//!
//! Tick 73 fixed the same shape one layer down — the SLA worker's recipient guard — and its
//! note says why it did **not** change [`crate::store::existing_users`]:
//!
//! > a caller that uses the existence check as a tenancy check sends a tenant's notification
//! > into another tenant's inbox, and the notification table will carry the *sending*
//! > organization next to a recipient who belongs to a different one
//!
//! This file is that sentence made executable, so the emit route can ask the tenancy question
//! without the pre-check changing its meaning.
//!
//! ## The rule
//!
//! * **A sender inside a tenant may address that tenant's accounts.** Nobody else. Not the
//!   platform account, not a sibling tenant, not a stranger's colleague.
//! * **A sender with no organization is the platform**, and the platform may address anybody.
//!   That is the router's documented unscoped branch — a platform-level fact has to be able to
//!   reach a tenant — and it is the reason this is not simply "recipients must share the
//!   sender's organization": that predicate would refuse the platform's own announcements.
//!
//! **The two questions stay separate, and that is deliberate.** [`crate::store::existing_users`]
//! is the access system's question ("is this an account at all?") and this file is tenancy's.
//! Keeping them apart is what stops a guard added for one threat model from silently
//! becoming the other: the emit route still refuses an id that is not an account, and now
//! separately refuses one that is an account somewhere it has no business writing to.
//!
//! **No `status` filter, in either direction.** A colleague disabled after a rule was saved is
//! still somebody who has to be told; filtering them here would drop every escalation for a
//! team on leave. Whether an account may be *addressed* and whether it may *sign in* are
//! independent questions and this file answers only the first.

use uuid::Uuid;

/// May a sender in `sender_organization` write into `recipient_organization`'s inbox?
///
/// `None` on the left is the platform account; `None` on the right is the platform's own
/// (orgless) user, which a **tenant** may not address — the population `organization_id is
/// null` was created for is not a recipient for a tenant's business.
///
/// This is a pure function on purpose: the tenancy rule is the one thing in this subsystem
/// that must not be re-spelled by each caller, and a rule that can only be exercised against a
/// live database is a rule nobody exercises. The database half — *which organization is this
/// id in* — is [`crate::store::recipient_organizations`].
pub fn may_address(
    sender_organization: Option<Uuid>,
    recipient_organization: Option<Uuid>,
) -> bool {
    match sender_organization {
        // The platform speaks to everyone, including itself: a platform-level fact is exactly
        // the case that has no single owning tenant.
        None => true,
        Some(sender) => recipient_organization == Some(sender),
    }
}

/// The recipients a sender may **not** address, in the order they were asked for.
///
/// Returns the ids themselves rather than a count, because the caller has to tell the emitter
/// *which* id to drop so it can send the rest — a refusal that names nothing is the "500
/// quoting a constraint name" problem [`crate::store::existing_users`] exists to avoid.
///
/// **The question is deliberately not "does this account exist".** A stranger's id and an id
/// that was never issued must produce the *same* answer, or the endpoint becomes an existence
/// oracle across the tenancy boundary: a caller could walk the `users` table of another tenant
/// by watching which ids it accepts. The route therefore reports both under one code and one
/// sentence, and the ids come back in the same shape either way.
pub fn refused_recipients(
    sender_organization: Option<Uuid>,
    asked: &[Uuid],
    organization_of: &dyn Fn(Uuid) -> Option<Option<Uuid>>,
) -> Vec<Uuid> {
    asked
        .iter()
        .copied()
        .filter(|id| match organization_of(*id) {
            // Unknown to the database: not an account at all. Refused, exactly like a
            // stranger — `None` here means "no row", which is a *weaker* fact than "another
            // tenant", so it must not be distinguishable from the refusal above.
            None => true,
            Some(recipient_organization) => {
                !may_address(sender_organization, recipient_organization)
            }
        })
        .collect()
}

/// The recipients a sender may address, each with the organization its row must carry.
///
/// Returns the **pairs**, not the ids, because the second half of the rule is a *stamp* and a
/// caller that filtered correctly but stamped the sender's organization would still write a row
/// whose `organization_id` column names one tenant while `user_id` names an account in another.
/// Handing the pair back is what makes the correct thing the convenient thing.
///
/// **This is [`refused_recipients`]'s other arm, and the two must stay the same rule.** They
/// are separate functions rather than one because they answer different questions in different
/// places — a caller that needs to *tell an emitter which ids to drop* wants the refusal, and a
/// producer that is about to write wants the survivors — but a filter and its complement that
/// disagreed would refuse a row on one path and write it on the other. A tenant boundary with
/// two spellings is not a boundary.
pub fn addressable_recipients(
    sender_organization: Option<Uuid>,
    asked: &[Uuid],
    organization_of: &dyn Fn(Uuid) -> Option<Option<Uuid>>,
) -> Vec<(Uuid, Option<Uuid>)> {
    asked
        .iter()
        .copied()
        .filter_map(|id| match organization_of(id) {
            // Unknown to the database: there is no row to stamp and no person to tell, and the
            // insert would be refused by `notifications_user_id_fkey` with a constraint name
            // instead of a sentence. Refusing it here is what turns a payload naming a
            // fabricated uuid into an unmatched rule rather than a 500.
            None => None,
            Some(recipient_organization)
                if may_address(sender_organization, recipient_organization) =>
            {
                Some((id, recipient_organization))
            }
            Some(_) => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn org(n: u8) -> Uuid {
        let mut bytes = [0u8; 16];
        bytes[15] = n;
        Uuid::from_bytes(bytes)
    }

    const PLATFORM: Option<Uuid> = None;

    #[test]
    fn a_tenant_may_address_its_own_accounts() {
        let a = org(1);
        assert!(may_address(Some(a), Some(a)));
    }

    #[test]
    fn a_tenant_may_not_address_another_tenant() {
        assert!(!may_address(Some(org(1)), Some(org(2))));
    }

    #[test]
    fn a_tenant_may_not_address_the_platform_account() {
        // The orgless user is a real row, and it is the one population `organization_id is
        // null` exists for — which is exactly why existence and tenancy are two questions.
        assert!(!may_address(Some(org(1)), PLATFORM));
    }

    #[test]
    fn the_platform_may_address_anybody() {
        assert!(may_address(PLATFORM, Some(org(1))));
        assert!(may_address(PLATFORM, Some(org(2))));
        assert!(may_address(PLATFORM, PLATFORM));
    }

    #[test]
    fn refused_recipients_keeps_the_order_they_were_asked_in() {
        let a = org(1);
        let b = org(2);
        let asked = vec![a, b, org(3)];
        let organizations = |id: Uuid| match id {
            x if x == a => Some(Some(a)),
            x if x == b => Some(Some(b)),
            _ => Some(PLATFORM),
        };
        assert_eq!(
            refused_recipients(Some(a), &asked, &organizations),
            vec![b, org(3)]
        );
    }

    #[test]
    fn an_id_that_is_no_account_is_refused_exactly_like_a_stranger() {
        // **The ids come back differently and must.** The refusal names the ids so an emitter
        // can drop them and send the rest, and a stranger's id is a different string from a
        // ghost's — so comparing the two lists for equality is the wrong assertion, and it is
        // wrong in a way that looks right. What must match is the *shape*: both refused, and
        // both refused for the same reason. That is what keeps the endpoint from being an
        // existence oracle across the tenancy boundary — a caller may learn "this id is not
        // yours", never "this id exists and belongs to somebody".
        let stranger = org(1);
        let ghost = org(9);
        let ask = |id: Uuid| {
            let organizations = |asked: Uuid| {
                if asked == stranger {
                    Some(Some(org(2)))
                } else {
                    None
                }
            };
            refused_recipients(Some(org(3)), &[id], &organizations)
        };
        let stranger_refusal = ask(stranger);
        let ghost_refusal = ask(ghost);
        assert_eq!(
            stranger_refusal.len(),
            ghost_refusal.len(),
            "one refusal each"
        );
        assert_eq!(
            stranger_refusal,
            vec![stranger],
            "the stranger is named back"
        );
        assert_eq!(ghost_refusal, vec![ghost], "the ghost is named back too");

        // The sentence the caller reads is the same either way: the route reports every refused
        // id under one code, so a difference in *which* id came back is the only thing a caller
        // can observe, and it cannot tell "other tenant" from "never issued".
        let describe = |refusals: Vec<Uuid>| format!("{} refused", refusals.len());
        assert_eq!(describe(stranger_refusal), describe(ghost_refusal));
    }

    #[test]
    fn a_sender_that_asks_for_nobody_is_refused_nobody() {
        assert!(refused_recipients(Some(org(1)), &[], &|_: Uuid| None).is_empty());
    }

    #[test]
    fn the_survivors_carry_their_own_organization_not_the_senders() {
        // The stamp is the half that made the leak look correct in a query, so it is asserted
        // as a *value* rather than as a boolean. A filter that returned only ids would pass
        // every other test in this file and still let a caller bind the sender's column.
        let a = org(1);
        let b = org(2);
        let asked = vec![a, b, org(3)];
        let organizations = |id: Uuid| match id {
            x if x == a => Some(Some(a)),
            x if x == b => Some(Some(b)),
            _ => Some(PLATFORM),
        };
        assert_eq!(
            addressable_recipients(Some(a), &asked, &organizations),
            vec![(a, Some(a))],
            "only the in-tenant id survives, and it is handed back WITH its own organization"
        );
    }

    #[test]
    fn a_platform_sender_gets_each_recipients_own_tenant() {
        // The router's unscoped branch. The whole point of handing back pairs is this: a
        // platform event addressing two tenants produces two rows in two tenants, and a single
        // `organization_id` for the batch cannot express that.
        let a = org(1);
        let b = org(2);
        let asked = vec![a, b];
        let organizations = |id: Uuid| Some(Some(id));
        assert_eq!(
            addressable_recipients(PLATFORM, &asked, &organizations),
            vec![(a, Some(a)), (b, Some(b))],
            "each survivor carries its own tenant, so one platform event reaches two tenants' \
             delivery logs instead of neither"
        );
    }

    #[test]
    fn an_id_that_is_no_account_survives_as_nobody() {
        // Same fact `refused_recipients` states from the other side. Asserted here because the
        // two functions must not drift, and a fabricated uuid in a payload is the realistic way
        // to reach it: the id parses, so nothing before the database would have complained.
        let ghost = org(9);
        assert!(
            addressable_recipients(PLATFORM, &[ghost], &|_: Uuid| None).is_empty(),
            "an id with no row cannot be stamped, and `notifications_user_id_fkey` would refuse \
             the whole insert with a constraint name"
        );
    }

    #[test]
    fn the_filter_and_its_complement_partition_the_same_list() {
        // **The drift guard.** The emit route names refused ids back to a caller; the router
        // keeps the survivors and writes them. If the two ever disagreed, one path would refuse
        // a row the other wrote, which is the exact failure the pair is documented against — and
        // it would be invisible, because both are "correct" in isolation.
        for sender in [PLATFORM, Some(org(1)), Some(org(2))] {
            let a = org(1);
            let b = org(2);
            let asked = vec![a, b, org(3), org(4)];
            let organizations = |id: Uuid| match id {
                x if x == a => Some(Some(a)),
                x if x == b => Some(Some(b)),
                x if x == org(3) => Some(PLATFORM),
                _ => None,
            };
            let kept = addressable_recipients(sender, &asked, &organizations);
            let refused = refused_recipients(sender, &asked, &organizations);
            let kept_ids: Vec<Uuid> = kept.iter().map(|(id, _)| *id).collect();
            let refused_ids: Vec<Uuid> = refused.to_vec();
            let mut union = kept_ids.clone();
            union.extend(refused_ids.iter().copied());
            union.sort();
            union.dedup();
            assert_eq!(
                union.len(),
                asked.len(),
                "every asked id is in exactly one list"
            );
            assert!(
                !kept_ids.iter().any(|id| refused_ids.contains(id)),
                "no id is both kept and refused: sender {sender:?}"
            );
        }
    }
}
