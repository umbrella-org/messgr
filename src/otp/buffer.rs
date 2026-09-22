//! Local-disk buffer for a `comms_request`/`comms_event` write that failed
//! against Postgres: the OTP send already completed one way or the other by
//! the time this runs, so the write is retried out-of-band rather than
//! failing the request.
//!
//! `ponytail: single buffer file, full-file rewrite per drain pass -- fine
//! at OTP volumes; shard by tenant if throughput ever makes the rewrite
//! itself a bottleneck.`

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;

use crate::keystore::KeyStore;
use crate::tenant::registry::TenantRegistry;

use super::model::AuditRecord;
use super::repo;

/// Serializes one record as a JSON line and appends+flushes synchronously
/// before returning (durability requirement) -- `fsync`s the file, not just
/// a userspace buffer, since a raw `File` (no `BufWriter`) has none.
pub fn append(path: &Path, record: &AuditRecord) -> std::io::Result<()> {
    let line = serde_json::to_string(record)
        .expect("AuditRecord contains no non-serializable field");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")?;
    file.sync_all()
}

/// Reads every buffered line, attempts `repo::write_audit_record` for each
/// against the record's own tenant pool, and rewrites the file with
/// whatever's left after this pass. A missing file (nothing ever buffered)
/// is treated as an empty buffer, not an error.
pub async fn drain(
    path: &Path,
    control_pool: &PgPool,
    control_database_url: &str,
    keystore: &dyn KeyStore,
    registry: &TenantRegistry,
    tenant_pool_max_connections: u32,
) {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
        Err(err) => {
            tracing::error!(%err, "otp: reading the buffer file failed");
            return;
        }
    };

    let mut remaining: Vec<AuditRecord> = Vec::new();
    for line in contents.lines().filter(|line| !line.trim().is_empty()) {
        let record: AuditRecord = match serde_json::from_str(line) {
            Ok(record) => record,
            Err(err) => {
                tracing::error!(
                    %err,
                    "otp: buffer file contains an unparsable record, dropping it"
                );
                continue;
            }
        };

        let tenant = match registry
            .get_or_open(
                control_pool,
                control_database_url,
                keystore,
                record.tenant_id,
                tenant_pool_max_connections,
            )
            .await
        {
            Ok(tenant) => tenant,
            Err(err) => {
                tracing::error!(
                    %err,
                    tenant_id = %record.tenant_id,
                    "otp: buffer drain could not open the tenant pool, keeping for next pass"
                );
                remaining.push(record);
                continue;
            }
        };

        if let Err(err) = repo::write_audit_record(&tenant.pool, &record).await {
            tracing::error!(
                %err,
                comms_request_id = %record.comms_request_id,
                "otp: buffer drain write failed, keeping for next pass"
            );
            remaining.push(record);
        }
    }

    let rewritten: String = remaining
        .iter()
        .map(|record| {
            serde_json::to_string(record)
                .expect("AuditRecord contains no non-serializable field")
        })
        .map(|line| format!("{line}\n"))
        .collect();

    if let Err(err) = std::fs::write(path, rewritten) {
        tracing::error!(%err, "otp: rewriting the buffer file after drain failed");
    }
}

/// Drains forever on `interval` -- same shape as `auth_flag::run_refresh_loop`,
/// against the buffer file instead of the control DB.
#[allow(clippy::too_many_arguments)]
pub async fn run_drain_loop(
    path: PathBuf,
    control_pool: PgPool,
    control_database_url: String,
    keystore: Arc<dyn KeyStore>,
    registry: Arc<TenantRegistry>,
    tenant_pool_max_connections: u32,
    interval: Duration,
) {
    loop {
        tokio::time::sleep(interval).await;
        drain(
            &path,
            &control_pool,
            &control_database_url,
            keystore.as_ref(),
            &registry,
            tenant_pool_max_connections,
        )
        .await;
    }
}
