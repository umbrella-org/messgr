//! Buffer for a send whose audit-record crypto (DEK resolution, HMAC,
//! encryption) could not complete because Vault or the tenant's Postgres
//! was unreachable (F1 rework) -- the OTP itself was already sent by the
//! time this runs (`handler::send_otp` calls the provider first), so like
//! `buffer.rs`'s own DB-write buffer this is retried out-of-band rather
//! than failing the request or blocking on the outage.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;

use crate::customer_dek::lifecycle::get_or_create_dek;
use crate::destination_hmac;
use crate::encryption;
use crate::keystore::KeyStore;
use crate::tenant::registry::TenantRegistry;

use super::model::{AuditRecord, PendingAuditRecord};
use super::{buffer, repo};

/// Serializes one record as a JSON line and appends+flushes synchronously
/// before returning -- same durability shape as `buffer::append`.
pub fn append(path: &Path, record: &PendingAuditRecord) -> std::io::Result<()> {
    let line = serde_json::to_string(record)
        .expect("PendingAuditRecord contains no non-serializable field");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")?;
    file.sync_all()
}

/// Attempts the DEK/HMAC/encryption step for every pending record against
/// its own tenant pool. A record whose crypto now succeeds is handed to the
/// normal write-or-buffer path (`buffer_path`, `repo::write_audit_record`)
/// exactly like a fresh send would be -- so a write failure at that point
/// still degrades to `buffer.rs`'s own retry, not a dropped record.
/// Anything still failing crypto stays pending for the next pass. A missing
/// file (nothing ever buffered) is treated as an empty buffer, not an
/// error.
pub async fn drain(
    path: &Path,
    buffer_path: &Path,
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
            tracing::error!(%err, "sms-sender: reading the pending-crypto buffer file failed");
            return;
        }
    };

    let mut remaining: Vec<PendingAuditRecord> = Vec::new();
    for line in contents.lines().filter(|line| !line.trim().is_empty()) {
        let record: PendingAuditRecord = match serde_json::from_str(line) {
            Ok(record) => record,
            Err(err) => {
                tracing::error!(
                    %err,
                    "sms-sender: pending-crypto buffer file contains an unparsable record, dropping it"
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
                    "sms-sender: pending-crypto drain could not open the tenant pool, keeping for next pass"
                );
                remaining.push(record);
                continue;
            }
        };

        let dek = match get_or_create_dek(
            &tenant.pool,
            keystore,
            &tenant.dek_cache,
            &tenant.tenant.vault_mount,
            record.customer_id,
        )
        .await
        {
            Ok(dek) => dek,
            Err(err) => {
                tracing::error!(
                    %err,
                    comms_request_id = %record.comms_request_id,
                    "sms-sender: pending-crypto drain could not resolve the DEK, keeping for next pass"
                );
                remaining.push(record);
                continue;
            }
        };

        let aad = record.comms_request_id.as_bytes();
        let destination_hmac =
            destination_hmac::compute(&tenant.pepper, &record.destination);
        let destination_ciphertext = match encryption::encrypt(
            &dek,
            aad,
            record.destination.as_bytes(),
        ) {
            Ok(ciphertext) => ciphertext,
            Err(err) => {
                tracing::error!(
                    %err,
                    comms_request_id = %record.comms_request_id,
                    "sms-sender: pending-crypto drain encryption failed, keeping for next pass"
                );
                remaining.push(record);
                continue;
            }
        };

        let audit_record = AuditRecord {
            tenant_id: record.tenant_id,
            comms_request_id: record.comms_request_id,
            created_at: record.created_at,
            customer_id: record.customer_id,
            producer_id: record.producer_id,
            destination_hmac,
            destination_ciphertext,
            final_status: record.final_status,
            provider_ref: record.provider_ref,
            provider_status: record.provider_status,
        };

        if let Err(err) = repo::write_audit_record(&tenant.pool, &audit_record).await {
            tracing::error!(
                %err,
                comms_request_id = %audit_record.comms_request_id,
                "sms-sender: pending-crypto drain resolved crypto but the write failed, buffering to the write-retry buffer"
            );
            if let Err(io_err) = buffer::append(buffer_path, &audit_record) {
                tracing::error!(
                    %io_err,
                    comms_request_id = %audit_record.comms_request_id,
                    "sms-sender: buffering the recovered record also failed -- record is lost"
                );
            }
        }
    }

    let rewritten: String = remaining
        .iter()
        .map(|record| {
            serde_json::to_string(record)
                .expect("PendingAuditRecord contains no non-serializable field")
        })
        .map(|line| format!("{line}\n"))
        .collect();

    if let Err(err) = std::fs::write(path, rewritten) {
        tracing::error!(%err, "sms-sender: rewriting the pending-crypto buffer file after drain failed");
    }
}

/// Drains forever on `interval` -- same shape as `buffer::run_drain_loop`.
#[allow(clippy::too_many_arguments)]
pub async fn run_drain_loop(
    path: PathBuf,
    buffer_path: PathBuf,
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
            &buffer_path,
            &control_pool,
            &control_database_url,
            keystore.as_ref(),
            &registry,
            tenant_pool_max_connections,
        )
        .await;
    }
}
