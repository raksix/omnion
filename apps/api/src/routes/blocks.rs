//! `/api/v1/blocks` — the block registry of the page builder (REQ-063).
//!
//! The registry is code, so this surface is read-only by design: it answers with the
//! definitions the platform ships (version, categories, props schemas) and with a validator
//! that reports what is wrong with a block tree. The panel's insert panel, its inspector and
//! its `/blocks` reference are all generated from this document — none of them hard-codes a
//! list, so adding a block type is a code change that reaches the whole panel at once.
//!
//! The two routes carry the same read key: a dry run changes nothing, so needing more than
//! `content.blocks.read` to look at a tree would only teach editors to save a page to find out
//! what is wrong with it. Writing blocks is `content.pages.update` on the page's own route.

use axum::Json;
use axum::extract::State;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::auth::CurrentSession;
use crate::error::ApiError;
use crate::state::AppState;

/// `POST /api/v1/blocks/validate`.
///
/// A dry run: the tree is validated exactly as a save would validate it, and nothing is
/// written. The editor calls it on every change, so a page can show its blocking issues
/// before the author ever presses *Save draft*.
#[derive(Debug, Deserialize)]
pub struct ValidateBlocksRequest {
    /// The block tree, as the page's working draft would carry it.
    pub blocks: Value,
}

/// The block registry: every type, its categories and its props schema.
pub async fn list_blocks(
    State(_state): State<AppState>,
    _current: CurrentSession,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(omnion_content::registry_document()))
}

/// Validate a block tree against the registry without writing it.
pub async fn validate_blocks(
    State(_state): State<AppState>,
    _current: CurrentSession,
    Json(body): Json<ValidateBlocksRequest>,
) -> Result<Json<Value>, ApiError> {
    let report = omnion_content::validate(&body.blocks);
    Ok(Json(json!({
        "issues": report.issues,
        "block_count": report.block_count,
        "can_publish": report.can_publish,
    })))
}
