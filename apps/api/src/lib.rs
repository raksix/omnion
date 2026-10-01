//! Omnion API crate.
//!
//! This crate is the HTTP layer only: routing, extractors, response shapes and wiring. Real
//! work is delegated to the crates under `crates/` (see `docs/04-MONOREPO.md`).

#![forbid(unsafe_code)]

pub mod analytics_runner;
pub mod audit_retention;
pub mod auth;
pub mod automation_runner;
pub mod backup_schedule_runner;
pub mod backup_sweep_runner;
pub mod cdn_purge_runner;
pub mod client_ip;
pub mod cluster_runtime;
pub mod cookies;
pub mod deployment_check;
pub mod deployment_runner;
pub mod dto;
pub mod environment_clone_runner;
pub mod error;
pub mod event_retention_runner;
pub mod event_runner;
pub mod guards;
pub mod headers_middleware;
pub mod health_events;
pub mod health_runner;
pub mod intent_resolver;
pub mod module_guard;
pub mod notification_runner;
pub mod rate_limit_middleware;
pub mod restore_job_runner;
pub mod retention_runner;
pub mod routes;
pub mod scope;
pub mod search_runner;
pub mod state;
pub mod workflow_runner;
