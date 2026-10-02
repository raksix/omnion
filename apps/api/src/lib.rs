//! Omnion API crate.
//!
//! This crate is the HTTP layer only: routing, extractors, response shapes and wiring. Real
//! work is delegated to the crates under `crates/` (see `docs/04-MONOREPO.md`).

#![forbid(unsafe_code)]

pub mod analytics_runner;
pub mod auth;
pub mod automation_runner;
pub mod backup_schedule_runner;
pub mod backup_sweep_runner;
pub mod client_ip;
pub mod cookies;
pub mod dto;
pub mod error;
pub mod event_retention_runner;
pub mod event_runner;
pub mod guards;
pub mod headers_middleware;
pub mod health_events;
pub mod health_runner;
pub mod idempotency_middleware;
pub mod intent_resolver;
pub mod notification_runner;
pub mod rate_limit_middleware;
pub mod reliability_middleware;
pub mod request_log;
pub mod restore_job_runner;
pub mod retention_runner;
pub mod openapi_emit;
pub mod routes;
pub mod scope;
pub mod search_runner;
pub mod security_ip;
pub mod seeds;
pub mod secrets_runner;
pub mod state;
pub mod workflow_runner;
