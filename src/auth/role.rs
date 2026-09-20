//! Closed vocabulary of query-api roles (DESIGN.md §11.1), mirroring
//! `suppression::model::reason`/`tenant_config::model::verification_mode`'s
//! plain-`&str`-constants convention.

pub const CUSTOMER_SERVICE: &str = "customer_service";
pub const COMPLIANCE: &str = "compliance";
pub const CAMPAIGN_OPS: &str = "campaign_ops";
pub const COMMS_OPS: &str = "comms_ops";
pub const ADMIN: &str = "admin";
