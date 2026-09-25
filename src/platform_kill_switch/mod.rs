//! Platform-tier kill switches (DESIGN.md §5.2 "Two tiers in cloud", §4.11,
//! T-058): provider-operated switches in the control database that suspend
//! one tenant or the whole region, overriding every tenant's own
//! `kill_switch` table and never overridden by it. Enforcement reuses T-016's
//! `kill_switch::cache` machinery (see `model::PlatformKillSwitch::as_kill_switch`);
//! this module owns only the control-database side: rows, engage/release,
//! and the per-tenant `NOTIFY` fan-out.

pub mod configure;
pub mod model;
pub mod repo;
