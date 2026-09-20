use serde::Deserialize;
use uuid::Uuid;

/// Every route here nests under `/t/{tenant_slug}`, so a single-parameter
/// `Path<Uuid>`/`Path<String>` extraction always sees two total captures
/// (`tenant_slug` plus the route's own `{id}`) and axum rejects that for a
/// scalar target. A named-field struct extracts by field name instead,
/// ignoring the extra capture.
#[derive(Debug, Deserialize)]
pub struct IdPath {
    pub id: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct CampaignIdPath {
    pub id: String,
}
