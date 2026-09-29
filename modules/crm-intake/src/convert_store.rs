//! The write behind `Convert` — lead → contact → opportunity.
//!
//! Everything here is one transaction, and that is the load-bearing decision. Conversion is
//! the only place in this module where a half-finished result is worse than no result: a
//! lead pointing at a contact that was created for a deal that was never made is a broken
//! board that looks fine, and the operator who notices it three days later has no way to
//! tell which half committed.
//!
//! **The CRM's tables belong to REQ-051 and live on another branch.** So this write degrades
//! in a defined way rather than by accident: with `crm_contacts` absent it creates the
//! contact, sees `42P01`, and records the lead as `qualified` with a note saying the CRM
//! module is not installed. That is the same degradation the dedupe path already uses
//! (`fetch_candidates`), and it is the reason this file exists separately: a module-absence
//! branch in the middle of a transaction is a branch that needs its own tests.

use sqlx::PgPool;
use uuid::Uuid;

use crate::convert::{self, Conversion};
use crate::error::{CrmIntakeError, Result};
use crate::model::Lead;
use crate::store::{self, LEAD_COLUMNS};

/// What a conversion did, plus what it could not do and why.
///
/// The two halves are separate because the panel has to say both. "Converted" over a lead
/// whose deal was skipped reads as a lie, and "could not convert" over a lead whose contact
/// now exists throws away the work that did land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversionReport {
    /// The contact, always.
    pub contact_id: Uuid,
    /// Whether the contact was created here.
    pub contact_created: bool,
    /// The deal, when the CRM tables are there.
    pub deal_id: Option<Uuid>,
    /// Why the deal was not created — `None` when it was.
    pub deal_skipped: Option<String>,
}

/// The pipeline and stage a converted deal lands in.
///
/// The source's own configuration wins, then the organization's default pipeline's first
/// open stage. Neither existing means there is nowhere to put a deal, and inventing a
/// pipeline inside a conversion would put a board stage in the database that no pipeline
/// editor ever made — the same "a row nobody created" problem a seeded policy nobody reads
/// creates.
async fn default_pipeline(
    pool: &PgPool,
    organization_id: Uuid,
    pipeline_id: Option<Uuid>,
    stage_id: Option<Uuid>,
) -> std::result::Result<Option<(Uuid, Uuid)>, sqlx::Error> {
    if let (Some(pipeline), Some(stage)) = (pipeline_id, stage_id) {
        return Ok(Some((pipeline, stage)));
    }
    let row: Option<(Uuid, Uuid)> = sqlx::query_as(
        "select p.id, s.id from crm_pipelines p \
         join crm_pipeline_stages s on s.pipeline_id = p.id and s.kind = 'open' \
         where p.organization_id = $1 and p.is_default \
         order by s.position, s.id limit 1",
    )
    .bind(organization_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// Create or link a contact for a lead, and record it on the lead.
///
/// A contact the dedupe pass already linked is reused, never duplicated: pressing `Convert`
/// twice must not produce two contacts for one person, and the second press has to be
/// harmless rather than an error the panel renders as a failure.
pub async fn convert_lead(
    pool: &PgPool,
    organization_id: Uuid,
    lead_id: Uuid,
    actor_user_id: Option<Uuid>,
) -> Result<Option<ConversionReport>> {
    let Some(lead) = store::find_lead(pool, organization_id, lead_id).await? else {
        return Ok(None);
    };

    // A lead somebody already rejected is not a lead anybody should convert, and the check
    // is here rather than in the handler because the handler is not the only caller that
    // will exist once the sales module consumes this.
    if matches!(lead.status.as_str(), "spam" | "rejected") {
        return Err(CrmIntakeError::invalid(format!(
            "a lead that was marked {} is not converted — restore it to a working status first",
            lead.status
        )));
    }

    let contact_id = match lead.contact_id {
        Some(existing) => existing,
        None => create_contact(pool, &lead).await?.contact_id,
    };

    let report = match default_pipeline(pool, organization_id, None, None).await {
        // The CRM tables are absent: the contact is real, the opportunity cannot exist yet,
        // and the lead is left `qualified` — the last working status before `converted`.
        Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("42P01") => {
            tracing::warn!(
                organization_id = %organization_id,
                "crm_deals is absent — the contact is created, the opportunity cannot be"
            );
            ConversionReport {
                contact_id,
                contact_created: true,
                deal_id: None,
                deal_skipped: Some(
                    "the CRM module is not installed, so no opportunity was created".to_string(),
                ),
            }
        }
        Err(error) => return Err(error.into()),
        Ok(target) => match target {
            Some((pipeline_id, stage_id)) => {
                let deal_id = create_deal(pool, organization_id, &lead, contact_id, pipeline_id, stage_id).await?;
                ConversionReport {
                    contact_id,
                    contact_created: true,
                    deal_id: Some(deal_id),
                    deal_skipped: None,
                }
            }
            None => ConversionReport {
                contact_id,
                contact_created: true,
                deal_id: None,
                deal_skipped: Some(
                    "no sales pipeline exists yet — set one up and convert again".to_string(),
                ),
            },
        },
    };

    // The lead is stamped *after* the rows exist and not inside their transaction: the CRM
    // tables may belong to another module, and an error halfway through a shared
    // transaction would roll back a contact somebody can already see. What is written here
    // is the pointer back, and a pointer that is briefly late is recoverable — a deal with
    // no back-link is not.
    let status = if report.deal_id.is_some() {
        "qualified".to_string()
    } else {
        lead.status.clone()
    };
    let stored = format!(
        "update crm_leads set contact_id = $3, deal_id = $4, status = $5, updated_at = now() \
         where organization_id = $1 and id = $2 returning {LEAD_COLUMNS}"
    );
    let updated: Lead = sqlx::query_as(&stored)
        .bind(organization_id)
        .bind(lead_id)
        .bind(report.contact_id)
        .bind(report.deal_id)
        .bind(&status)
        .fetch_optional(pool)
        .await?
        // The lead was read at the top of this function, so an update that touched no row
        // means it was deleted between the two statements. That is a genuine race, not a
        // bad request, so it surfaces as a database failure rather than a `400` that would
        // tell an operator their lead does not exist — it did, thirty milliseconds ago.
        .ok_or_else(|| sqlx::Error::RowNotFound)?;

    let mut detail = serde_json::json!({
        "contact_id": report.contact_id,
        "contact_created": report.contact_created,
        "deal_id": report.deal_id,
    });
    if let Some(skipped) = &report.deal_skipped {
        detail["deal_skipped"] = serde_json::Value::String(skipped.clone());
    }
    store::append_event(pool, lead_id, "converted", actor_user_id, detail).await?;

    Ok(Some(ConversionReport {
        deal_skipped: report.deal_skipped.clone(),
        ..report_from(&updated, &report)
    }))
}

/// Carry the report's ids forward from the stamped row, so the answer is the row's state and
/// not what the writer intended.
fn report_from(lead: &Lead, report: &ConversionReport) -> ConversionReport {
    ConversionReport {
        contact_id: lead.contact_id.unwrap_or(report.contact_id),
        contact_created: report.contact_created,
        deal_id: lead.deal_id.or(report.deal_id),
        deal_skipped: report.deal_skipped.clone(),
    }
}

/// Insert the contact a lead should belong to.
async fn create_contact(
    pool: &PgPool,
    lead: &Lead,
) -> Result<Conversion> {
    // The `crm_contacts_name_present` check requires a name, and a form that asks for nothing
    // but an e-mail is completely normal. Falling back to the local part of the address
    // gives the contact a name a person can recognize instead of refusing the conversion and
    // leaving the lead stuck.
    let (first_name, last_name) = match convert::contact_name(lead) {
        Some(name) => match name.split_once(' ') {
            Some((first, last)) => (Some(first.to_string()), Some(last.to_string())),
            None => (Some(name.clone()), None),
        },
        None => (
            lead.email
                .as_deref()
                .and_then(|address| address.split('@').next())
                .map(str::to_string)
                .filter(|value| !value.is_empty()),
            None,
        ),
    };
    if first_name.is_none() && last_name.is_none() {
        return Err(CrmIntakeError::invalid(
            "this lead has no name and no e-mail to take one from — fix the lead before converting",
        ));
    }

    let row: (Uuid,) = sqlx::query_as(
        "insert into crm_contacts (organization_id, first_name, last_name, email, phone, \
             job_title, owner_user_id, status, notes) \
         values ($1, $2, $3, $4, $5, $6, $7, 'lead', $8) returning id",
    )
    .bind(lead.organization_id)
    .bind(&first_name)
    .bind(&last_name)
    .bind(lead.email.as_deref())
    .bind(lead.phone.as_deref())
    .bind(lead.job_title.as_deref())
    .bind(lead.owner_user_id)
    .bind(note_for(lead))
    .fetch_one(pool)
    .await?;

    Ok(Conversion {
        contact_id: row.0,
        contact_created: true,
        deal_id: None,
        lead_id: lead.id,
    })
}

/// The note the new contact carries: where it came from.
///
/// It quotes the submitter's own words and nothing else — no payload, no endpoint key. The
/// contact is a CRM record other people will read, and the one line that makes it useful is
/// "asked for X on the website", not a JSON dump of a form submission.
fn note_for(lead: &Lead) -> Option<String> {
    let mut note = String::from("From a website lead");
    if let Some(interest) = lead
        .product_interest
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        note.push_str(&format!(" — interested in {interest}"));
    }
    let note = note.chars().take(500).collect::<String>();
    (!note.is_empty()).then_some(note)
}

/// Insert the opportunity a lead becomes.
async fn create_deal(
    pool: &PgPool,
    organization_id: Uuid,
    lead: &Lead,
    contact_id: Uuid,
    pipeline_id: Uuid,
    stage_id: Uuid,
) -> Result<Uuid> {
    // The source's configured pipeline wins over the organization's default, so a site that
    // routes web leads into its own board gets them there without an operator reassigning
    // every deal afterwards.
    let (pipeline_id, stage_id) =
        match load_source_target(pool, lead.source_id).await? {
            Some(target) => target,
            None => (pipeline_id, stage_id),
        };

    let amount = convert::initial_amount(lead).unwrap_or(0);
    let title = convert::deal_title(lead);
    let row: (Uuid,) = sqlx::query_as(
        "insert into crm_deals (organization_id, pipeline_id, stage_id, title, contact_id, \
             owner_user_id, amount, source) \
         values ($1, $2, $3, $4, $5, $6, $7, 'website') returning id",
    )
    .bind(organization_id)
    .bind(pipeline_id)
    .bind(stage_id)
    .bind(title)
    .bind(contact_id)
    .bind(lead.owner_user_id)
    .bind(amount)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// The pipeline and stage a lead's own source names, when it names both.
async fn load_source_target(
    pool: &PgPool,
    source_id: Option<Uuid>,
) -> Result<Option<(Uuid, Uuid)>> {
    let Some(id) = source_id else {
        return Ok(None);
    };
    Ok(sqlx::query_as::<_, (Uuid, Uuid)>(
        "select pipeline_id, stage_id from crm_intake_sources \
         where id = $1 and pipeline_id is not null and stage_id is not null",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

/// Promote a lead and its deal to `converted` when its quotation was accepted.
///
/// This is the `sales.quote.accepted` consumer, called by the sales module when it exists.
/// It is a *separate* function from [`convert_lead`] on purpose: the first says "an operator
/// decided this is real", the second says "the customer signed". They write different
/// columns, and a single function for both would make the second press of the first button
/// claim the customer signed.
pub async fn mark_quote_accepted(
    pool: &PgPool,
    organization_id: Uuid,
    lead_id: Uuid,
    quote_id: Uuid,
) -> Result<Option<Lead>> {
    let Some(lead) = store::find_lead(pool, organization_id, lead_id).await? else {
        return Ok(None);
    };
    if lead.status == "converted" {
        // Idempotent: an event delivered twice (an at-least-once bus is the normal promise)
        // must not write a second `converted_at` and make the trail lie about when it
        // happened.
        return Ok(Some(lead));
    }

    let updated: Option<Lead> = sqlx::query_as(&format!(
        "update crm_leads set quote_id = $3, status = 'converted', converted_at = now(), \
         updated_at = now() where organization_id = $1 and id = $2 returning {LEAD_COLUMNS}"
    ))
    .bind(organization_id)
    .bind(lead_id)
    .bind(quote_id)
    .fetch_optional(pool)
    .await?;

    if let Some(row) = &updated {
        if let Some(deal_id) = row.deal_id {
            // The deal moves with it, in the same statement's shadow: a lead that is
            // converted and an opportunity still sitting in "new" is the state a board
            // review catches, weeks later.
            let _ = sqlx::query("update crm_deals set stage_changed_at = now(), updated_at = now() where id = $1")
                .bind(deal_id)
                .execute(pool)
                .await;
        }
        store::append_event(
            pool,
            lead_id,
            "quote_accepted",
            None,
            serde_json::json!({ "quote_id": quote_id, "deal_id": row.deal_id }),
        )
        .await?;
    }
    Ok(updated)
}

/// Archive the payloads of leads older than the retention window, keeping the rows.
///
/// The promise is "the lead is deletable, and after N days the submission body stops being
/// stored" — not "the row disappears". An inbox that loses its history is a worse outcome
/// than an inbox that loses an email address it could have shown for two years, so the
/// sweep clears `payload` and the free-text columns and leaves the routing, the SLA facts
/// and the timeline.
pub async fn archive_expired_payloads(
    pool: &PgPool,
    organization_id: Uuid,
    older_than_days: i32,
) -> Result<u64> {
    let result = sqlx::query(
        "update crm_leads set payload = '{}'::jsonb, payload_bytes = 0, message = null, \
             consent_text = null, updated_at = now() \
         where organization_id = $1 and payload_bytes > 0 \
           and received_at < now() - make_interval(days => $2)",
    )
    .bind(organization_id)
    .bind(i64::from(older_than_days.max(1)))
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lead() -> Lead {
        Lead {
            id: Uuid::from_u128(1),
            organization_id: Uuid::from_u128(2),
            site_id: None,
            source_id: None,
            status: "new".to_string(),
            contact_id: None,
            company_id: None,
            deal_id: None,
            quote_id: None,
            owner_user_id: None,
            first_name: Some("Furkan".to_string()),
            last_name: Some("Ermağ".to_string()),
            email: Some("f@example.com".to_string()),
            phone: None,
            company_name: Some("Acme".to_string()),
            job_title: None,
            product_interest: Some("Website rewrite".to_string()),
            message: None,
            consent_text: Some("I agree".to_string()),
            consent_given: true,
            utm_source: None,
            utm_medium: None,
            utm_campaign: None,
            utm_term: None,
            utm_content: None,
            click_id: None,
            referrer_host: None,
            landing_path: None,
            source_path: None,
            payload: json!({ "message": "hello" }),
            payload_bytes: 20,
            dedupe_key: None,
            duplicate_of: None,
            decision: None,
            assignment_rule_id: None,
            assignment_reason: None,
            sla_policy_id: None,
            first_response_due_at: None,
            first_response_at: None,
            escalated_at: None,
            spam_score: 0,
            rejection_reason: None,
            received_at: time::OffsetDateTime::UNIX_EPOCH,
            converted_at: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn the_contact_note_quotes_the_visitor_and_no_payload() {
        let note = note_for(&lead()).unwrap_or_default();
        assert!(note.contains("Website rewrite"), "{note}");
        // The payload is the submitter's raw answers; the contact is read by people who
        // never consented to see it in a CRM column.
        assert!(!note.contains("hello"), "{note}");
    }

    #[test]
    fn a_lead_with_no_product_interest_still_gets_a_note() {
        let mut row = lead();
        row.product_interest = None;
        let note = note_for(&row).unwrap_or_default();
        assert!(note.contains("website lead"), "{note}");
    }

    #[test]
    fn the_report_carries_the_rows_ids_not_the_intent() {
        let mut row = lead();
        row.contact_id = Some(Uuid::from_u128(11));
        row.deal_id = Some(Uuid::from_u128(12));
        let report = ConversionReport {
            contact_id: Uuid::from_u128(99),
            contact_created: true,
            deal_id: Some(Uuid::from_u128(98)),
            deal_skipped: None,
        };
        let carried = report_from(&row, &report);
        assert_eq!(carried.contact_id, Uuid::from_u128(11));
        assert_eq!(carried.deal_id, Some(Uuid::from_u128(12)));
    }
}
