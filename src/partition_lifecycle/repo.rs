use chrono::{Datelike, Months, NaiveDate};
use sqlx::PgPool;

use super::model::PartitionInfo;

/// The `<table>_<YYYY>_<MM>` name T-009 (decision 3) established and this
/// module extends going forward.
pub fn partition_name(parent_table: &str, month_start: NaiveDate) -> String {
    format!(
        "{parent_table}_{:04}_{:02}",
        month_start.year(),
        month_start.month()
    )
}

/// The inverse of `partition_name`: `None` for anything that doesn't match
/// `<parent_table>_YYYY_MM` exactly, so a partition created outside this
/// convention is skipped rather than guessed at (decision 3).
fn parse_partition_month(parent_table: &str, relname: &str) -> Option<NaiveDate> {
    let suffix = relname.strip_prefix(parent_table)?.strip_prefix('_')?;
    let (year, month) = suffix.split_once('_')?;
    if year.len() != 4 || month.len() != 2 {
        return None;
    }
    if !year.bytes().all(|b| b.is_ascii_digit())
        || !month.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    NaiveDate::from_ymd_opt(year.parse().ok()?, month.parse().ok()?, 1)
}

/// Every child partition of `parent_table` (`comms_request` or
/// `comms_event`), with the tablespace it currently lives on (`None` means
/// the database's default). A child whose name doesn't parse is logged and
/// left out — it is never a candidate for `move_to_tablespace` or
/// `detach_and_drop`.
pub async fn list_partitions(
    pool: &PgPool,
    parent_table: &str,
) -> Result<Vec<PartitionInfo>, sqlx::Error> {
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        r#"
        SELECT child.relname, ts.spcname
        FROM pg_inherits i
        JOIN pg_class child ON child.oid = i.inhrelid
        JOIN pg_class parent ON parent.oid = i.inhparent
        LEFT JOIN pg_tablespace ts ON ts.oid = child.reltablespace
        WHERE parent.relname = $1
        "#,
    )
    .bind(parent_table)
    .fetch_all(pool)
    .await?;

    let mut partitions = Vec::with_capacity(rows.len());
    for (name, tablespace) in rows {
        match parse_partition_month(parent_table, &name) {
            Some(month_start) => partitions.push(PartitionInfo {
                name,
                month_start,
                tablespace,
            }),
            None => {
                tracing::warn!(
                    partition = %name,
                    "skipping partition with a name that doesn't match <table>_YYYY_MM"
                );
            }
        }
    }
    Ok(partitions)
}

pub async fn partition_exists(pool: &PgPool, name: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(name)
        .fetch_one(pool)
        .await
}

/// Creates the `[month_start, month_start + 1 month)` partition of
/// `parent_table`. `parent_table` is always one of this module's own
/// `PARTITIONED_TABLES` constants and `month_start` is a validated date, not
/// caller-supplied text, so building the DDL with `format!` carries no
/// injection risk — the same reasoning T-009's own bootstrap migration
/// applied to its `EXECUTE format(...)` calls.
pub async fn create_partition(
    pool: &PgPool,
    parent_table: &str,
    month_start: NaiveDate,
) -> Result<(), sqlx::Error> {
    let name = partition_name(parent_table, month_start);
    let month_end = month_start
        .checked_add_months(Months::new(1))
        .expect("month_start + 1 month must not overflow NaiveDate");

    let stmt = format!(
        "CREATE TABLE {name} PARTITION OF {parent_table} \
         FOR VALUES FROM ('{month_start}') TO ('{month_end}')"
    );
    sqlx::query(&stmt).execute(pool).await?;
    Ok(())
}

/// Moves `partition_name`'s table AND every one of its indexes to
/// `tablespace` (decision 9 — `ALTER TABLE ... SET TABLESPACE` alone leaves
/// indexes behind).
pub async fn move_to_tablespace(
    pool: &PgPool,
    partition_name: &str,
    tablespace: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(&format!(
        "ALTER TABLE {partition_name} SET TABLESPACE {tablespace}"
    ))
    .execute(pool)
    .await?;

    let index_names: Vec<String> =
        sqlx::query_scalar("SELECT indexname FROM pg_indexes WHERE tablename = $1")
            .bind(partition_name)
            .fetch_all(pool)
            .await?;

    for index_name in index_names {
        sqlx::query(&format!(
            "ALTER INDEX {index_name} SET TABLESPACE {tablespace}"
        ))
        .execute(pool)
        .await?;
    }

    Ok(())
}

/// Plain (non-concurrent) detach, then drop (decision 8 — a partition old
/// enough to qualify is, per DESIGN.md §4.1, never rewritten and effectively
/// frozen, so there is no live writer to protect against).
pub async fn detach_and_drop(
    pool: &PgPool,
    parent_table: &str,
    partition_name: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(&format!(
        "ALTER TABLE {parent_table} DETACH PARTITION {partition_name}"
    ))
    .execute(pool)
    .await?;
    sqlx::query(&format!("DROP TABLE {partition_name}"))
        .execute(pool)
        .await?;
    Ok(())
}
