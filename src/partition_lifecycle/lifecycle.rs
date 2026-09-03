//! Keeps `comms_request`/`comms_event` partitions self-managing (DESIGN.md
//! §4.1, §7.2, §7.5, T-014): create-ahead, move to a slower tablespace after
//! 18 months, detach + drop past the tenant's retention boundary. `as_of` is
//! always an explicit parameter (decision 4) so tests can synthesize old
//! partitions deterministically instead of waiting real time.

use chrono::{DateTime, Datelike, Months, NaiveDate, Utc};
use sqlx::PgPool;

use crate::tenant::pool::connect_tenant_pool;
use crate::tenant::repo as tenant_repo;
use crate::tenant_config::model::TenantConfig;
use crate::tenant_config::repo as tenant_config_repo;

use super::model::LifecycleReport;
use super::repo;

/// §7.5: partitions older than this move to `COLD_TABLESPACE`. Fixed
/// platform-wide, unlike the drop boundary (decision 6).
pub const MOVE_AFTER_MONTHS: u32 = 18;
/// Created once per Postgres cluster by `just tablespace-init` (decision
/// 10) — never by this module.
pub const COLD_TABLESPACE: &str = "messgr_cold";
const PARTITIONED_TABLES: [&str; 2] = ["comms_request", "comms_event"];

/// A run can fail on the control-database side, the tenant-database side,
/// or a domain-level rejection (an unknown `tenant_slug`) — the last is
/// carried as `sqlx::Error::Configuration`, matching `ConfigureError`'s
/// shape in `tenant_config::configure`.
#[derive(Debug)]
pub enum PartitionLifecycleError {
    Database(sqlx::Error),
}

impl std::fmt::Display for PartitionLifecycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(err) => {
                write!(f, "partition lifecycle operation failed: {err}")
            }
        }
    }
}

impl std::error::Error for PartitionLifecycleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(err) => Some(err),
        }
    }
}

impl From<sqlx::Error> for PartitionLifecycleError {
    fn from(err: sqlx::Error) -> Self {
        Self::Database(err)
    }
}

fn rejected(message: String) -> PartitionLifecycleError {
    sqlx::Error::Configuration(message.into()).into()
}

/// Resolves `tenant_slug`, opens its pool, loads `tenant_config` (`None` is
/// legal — decision 7), and runs the lifecycle against it. The entry point
/// `messgr-control partition-lifecycle run` calls; mirrors
/// `tenant_config::configure::set_tenant_config`'s resolve/connect/close
/// shape so `src/bin/control.rs` stays as thin as every other command.
pub async fn run_for_tenant(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
    as_of: DateTime<Utc>,
) -> Result<LifecycleReport, PartitionLifecycleError> {
    let tenant = tenant_repo::find_by_slug(control_pool, tenant_slug)
        .await?
        .ok_or_else(|| {
            rejected(format!("no tenant registered with slug {tenant_slug:?}"))
        })?;

    let tenant_pool = connect_tenant_pool(
        control_pool,
        base_db_url,
        tenant.id,
        &tenant.database_name,
        5,
    )
    .await?;

    let result = run_inner(&tenant_pool.pool, as_of).await;

    tenant_pool.pool.close().await;
    result
}

async fn run_inner(
    tenant_pool: &PgPool,
    as_of: DateTime<Utc>,
) -> Result<LifecycleReport, PartitionLifecycleError> {
    let tenant_config = tenant_config_repo::load(tenant_pool).await?;
    run(tenant_pool, as_of, tenant_config.as_ref()).await
}

/// The lifecycle proper, against an already-open tenant pool. Exposed
/// directly (not just via `run_for_tenant`) so tests can synthesize
/// `tenant_config`/`as_of` combinations without provisioning through the
/// CLI's tenant-resolution path.
pub async fn run(
    pool: &PgPool,
    as_of: DateTime<Utc>,
    tenant_config: Option<&TenantConfig>,
) -> Result<LifecycleReport, PartitionLifecycleError> {
    let mut report = LifecycleReport::default();
    let current_month = month_start(as_of.date_naive());
    let next_month = add_months(current_month, 1);
    let move_boundary = subtract_months(current_month, MOVE_AFTER_MONTHS);

    for table in PARTITIONED_TABLES {
        // Create-ahead (decision 5): current + next month always present.
        for month in [current_month, next_month] {
            let name = repo::partition_name(table, month);
            if !repo::partition_exists(pool, &name).await? {
                repo::create_partition(pool, table, month).await?;
                report.created.push(name);
            }
        }

        let partitions = repo::list_partitions(pool, table).await?;

        // Move to slow tablespace (decision 6): fixed 18-month threshold,
        // independent of tenant_config.
        for partition in &partitions {
            let partition_end = add_months(partition.month_start, 1);
            if partition_end <= move_boundary
                && partition.tablespace.as_deref() != Some(COLD_TABLESPACE)
            {
                repo::move_to_tablespace(pool, &partition.name, COLD_TABLESPACE)
                    .await?;
                report.moved.push(partition.name.clone());
            }
        }

        // Detach + drop (decision 7): only when tenant_config is known.
        match tenant_config {
            Some(config) => {
                let retention_months = (config.retention_years as u32) * 12;
                let drop_boundary = subtract_months(current_month, retention_months);
                for partition in &partitions {
                    let partition_end = add_months(partition.month_start, 1);
                    if partition_end <= drop_boundary {
                        repo::detach_and_drop(pool, table, &partition.name).await?;
                        report.dropped.push(partition.name.clone());
                    }
                }
            }
            None => report.retention_skipped = true,
        }
    }

    if report.retention_skipped {
        tracing::warn!(
            "tenant_config not set -- skipping partition drop; partitions past retention are \
             being kept, not lost"
        );
    }

    Ok(report)
}

fn month_start(date: NaiveDate) -> NaiveDate {
    NaiveDate::from_ymd_opt(date.year(), date.month(), 1)
        .expect("a valid date's own year/month always produces a valid day-1 date")
}

fn add_months(date: NaiveDate, months: u32) -> NaiveDate {
    date.checked_add_months(Months::new(months))
        .expect("adding a bounded number of months to a valid date must not overflow")
}

fn subtract_months(date: NaiveDate, months: u32) -> NaiveDate {
    date.checked_sub_months(Months::new(months)).expect(
        "subtracting a bounded number of months from a valid date must not underflow",
    )
}
