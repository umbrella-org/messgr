use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

use super::model::{Tenant, status};

pub async fn find_by_slug(
    pool: &PgPool,
    slug: &str,
) -> Result<Option<Tenant>, sqlx::Error> {
    sqlx::query_as::<_, Tenant>(
        r#"
        SELECT id, slug, region, database_name, vault_mount, vault_role_id, vault_pepper_wrapped, webhook_token, status, created_at
        FROM tenant
        WHERE slug = $1
        "#,
    )
    .bind(slug)
    .fetch_optional(pool)
    .await
}

/// Looks up a tenant by its control-database id, the shape `resolve_producer`
/// (T-006) hands back — `find_by_slug`'s counterpart for callers that only
/// have `tenant_id` (T-011's tenant registry).
pub async fn find_by_id(
    pool: &PgPool,
    tenant_id: Uuid,
) -> Result<Option<Tenant>, sqlx::Error> {
    sqlx::query_as::<_, Tenant>(
        r#"
        SELECT id, slug, region, database_name, vault_mount, vault_role_id, vault_pepper_wrapped, webhook_token, status, created_at
        FROM tenant
        WHERE id = $1
        "#,
    )
    .bind(tenant_id)
    .fetch_optional(pool)
    .await
}

/// Persists the public AppRole RoleID Vault issued for this tenant
/// (`tenant.vault_role_id`, T-004). Never called with a SecretID — that is
/// never a column (DESIGN.md §7.6, this ticket's decision 2).
pub async fn record_vault_role_id(
    pool: &PgPool,
    tenant_id: Uuid,
    vault_role_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE tenant SET vault_role_id = $1 WHERE id = $2")
        .bind(vault_role_id)
        .bind(tenant_id)
        .execute(pool)
        .await
        .map(|_| ())
}

/// Persists the wrapped ciphertext of the tenant's HMAC pepper
/// (`tenant.vault_pepper_wrapped`, T-008). The plaintext is never
/// persisted — only handed to callers to cache in-process
/// (`key_cache::KeyCache`).
pub async fn record_vault_pepper_wrapped(
    pool: &PgPool,
    tenant_id: Uuid,
    vault_pepper_wrapped: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE tenant SET vault_pepper_wrapped = $1 WHERE id = $2")
        .bind(vault_pepper_wrapped)
        .bind(tenant_id)
        .execute(pool)
        .await
        .map(|_| ())
}

/// Inserts a new tenant row in `provisioning` status. Callers must have
/// already checked `find_by_slug` — this is not itself idempotent, since a
/// second insert for the same slug would violate the `UNIQUE` constraint;
/// `provision_tenant` (src/tenant/provision.rs) is what makes the overall
/// operation safe to repeat.
pub async fn insert_provisioning(
    pool: &PgPool,
    id: Uuid,
    slug: &str,
    region: &str,
    database_name: &str,
    vault_mount: &str,
    webhook_token: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO tenant (id, slug, region, database_name, vault_mount, webhook_token, status, created_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        "#,
    )
    .bind(id)
    .bind(slug)
    .bind(region)
    .bind(database_name)
    .bind(vault_mount)
    .bind(webhook_token)
    .bind(status::PROVISIONING)
    .bind(Utc::now())
    .execute(pool)
    .await
    .map(|_| ())
}

pub async fn mark_active(pool: &PgPool, tenant_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE tenant SET status = $1 WHERE id = $2")
        .bind(status::ACTIVE)
        .bind(tenant_id)
        .execute(pool)
        .await
        .map(|_| ())
}

pub async fn record_schema_version(
    pool: &PgPool,
    tenant_id: Uuid,
    version: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO tenant_schema_version (tenant_id, version, applied_at)
        VALUES ($1, $2, $3)
        ON CONFLICT (tenant_id) DO UPDATE SET version = EXCLUDED.version, applied_at = EXCLUDED.applied_at
        "#,
    )
    .bind(tenant_id)
    .bind(version)
    .bind(Utc::now())
    .execute(pool)
    .await
    .map(|_| ())
}
