use chrono::Utc;
use sqlx::PgPool;
use uuid::Uuid;

use crate::tenant::pool::connect_tenant_pool;
use crate::tenant::repo as tenant_repo;

use super::model::Template;
use super::render::{self, RenderError};
use super::repo;

/// An approve/show/list/render run can fail on the control-database side,
/// the tenant-database side, a domain-level rejection (an unknown
/// `tenant_slug`, or a template already approved for this `(template_id,
/// version, locale)` — T-010 decision 5), or a render-time failure (a
/// missing variable or an unterminated placeholder — T-010 decision 3).
#[derive(Debug)]
pub enum ApproveError {
    Database(sqlx::Error),
    Rejected(String),
    Render(RenderError),
}

impl std::fmt::Display for ApproveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(err) => write!(f, "template operation failed: {err}"),
            Self::Rejected(message) => {
                write!(f, "template operation rejected: {message}")
            }
            Self::Render(err) => write!(f, "template operation failed: {err}"),
        }
    }
}

impl std::error::Error for ApproveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(err) => Some(err),
            Self::Rejected(_) => None,
            Self::Render(err) => Some(err),
        }
    }
}

impl From<sqlx::Error> for ApproveError {
    fn from(err: sqlx::Error) -> Self {
        Self::Database(err)
    }
}

#[derive(Debug)]
pub struct ApproveOutcome {
    /// Always "created" — a repeat approval is returned as an `Err` instead
    /// (T-010 decision 5; there is no "idempotent" outcome here).
    pub outcome: &'static str,
}

/// Approves a new template version (T-010 decision 5): resolves
/// `tenant_slug`, opens its pool, and inserts the row only if this exact
/// `(template_id, version, locale)` has never been approved before —
/// templates are immutable once approved, so a repeat approval is rejected
/// rather than upserted. Writes exactly one `platform_audit` row
/// (`template.approve`) on every outcome, including a rejected one.
#[allow(clippy::too_many_arguments)]
pub async fn approve_template(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
    template_id: &str,
    version: i32,
    channel: &str,
    locale: &str,
    body: &str,
    actor: &str,
) -> Result<ApproveOutcome, ApproveError> {
    let tenant = match tenant_repo::find_by_slug(control_pool, tenant_slug).await? {
        Some(tenant) => tenant,
        None => {
            audit(
                control_pool,
                actor,
                None,
                template_id,
                version,
                channel,
                locale,
                "rejected",
            )
            .await?;

            return Err(ApproveError::Rejected(format!(
                "no tenant registered with slug {tenant_slug:?}"
            )));
        }
    };

    let tenant_pool = connect_tenant_pool(
        control_pool,
        base_db_url,
        tenant.id,
        &tenant.database_name,
        5,
    )
    .await?;

    let result = approve_template_inner(
        control_pool,
        &tenant_pool.pool,
        tenant.id,
        template_id,
        version,
        channel,
        locale,
        body,
        actor,
    )
    .await;

    tenant_pool.pool.close().await;
    result
}

#[allow(clippy::too_many_arguments)]
async fn approve_template_inner(
    control_pool: &PgPool,
    tenant_pool: &PgPool,
    tenant_id: Uuid,
    template_id: &str,
    version: i32,
    channel: &str,
    locale: &str,
    body: &str,
    actor: &str,
) -> Result<ApproveOutcome, ApproveError> {
    if repo::find(tenant_pool, template_id, version, locale)
        .await?
        .is_some()
    {
        audit(
            control_pool,
            actor,
            Some(tenant_id),
            template_id,
            version,
            channel,
            locale,
            "rejected",
        )
        .await?;

        return Err(ApproveError::Rejected(format!(
            "template {template_id:?} version {version} locale {locale:?} is already \
             approved; approve a new version instead"
        )));
    }

    repo::insert(
        tenant_pool,
        template_id,
        version,
        channel,
        locale,
        body,
        actor,
        Utc::now(),
    )
    .await?;

    audit(
        control_pool,
        actor,
        Some(tenant_id),
        template_id,
        version,
        channel,
        locale,
        "created",
    )
    .await?;

    Ok(ApproveOutcome { outcome: "created" })
}

/// Loads one template version/locale for display (`messgr-control template
/// show`). `None` means no such `(template_id, version, locale)` has been
/// approved.
pub async fn show_template(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
    template_id: &str,
    version: i32,
    locale: &str,
) -> Result<Option<Template>, ApproveError> {
    let tenant = resolve_tenant(control_pool, tenant_slug).await?;

    let tenant_pool = connect_tenant_pool(
        control_pool,
        base_db_url,
        tenant.id,
        &tenant.database_name,
        5,
    )
    .await?;
    let template = repo::find(&tenant_pool.pool, template_id, version, locale).await;
    tenant_pool.pool.close().await;

    Ok(template?)
}

/// Lists every approved version/locale for a `template_id`
/// (`messgr-control template list`).
pub async fn list_template_versions(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
    template_id: &str,
) -> Result<Vec<Template>, ApproveError> {
    let tenant = resolve_tenant(control_pool, tenant_slug).await?;

    let tenant_pool = connect_tenant_pool(
        control_pool,
        base_db_url,
        tenant.id,
        &tenant.database_name,
        5,
    )
    .await?;
    let templates = repo::list_versions(&tenant_pool.pool, template_id).await;
    tenant_pool.pool.close().await;

    Ok(templates?)
}

/// Loads one template version/locale and renders it against `variables`
/// (`messgr-control template render`) — the same render path T-011's ingest
/// path will call. Rejected if no such template exists; a render-time
/// failure (a missing variable or an unterminated placeholder) is returned
/// as `ApproveError::Render`.
#[allow(clippy::too_many_arguments)]
pub async fn render_preview(
    control_pool: &PgPool,
    base_db_url: &str,
    tenant_slug: &str,
    template_id: &str,
    version: i32,
    locale: &str,
    variables: &std::collections::HashMap<String, String>,
) -> Result<String, ApproveError> {
    let tenant = resolve_tenant(control_pool, tenant_slug).await?;

    let tenant_pool = connect_tenant_pool(
        control_pool,
        base_db_url,
        tenant.id,
        &tenant.database_name,
        5,
    )
    .await?;
    let template = repo::find(&tenant_pool.pool, template_id, version, locale).await;
    tenant_pool.pool.close().await;

    let template = template?.ok_or_else(|| {
        ApproveError::Rejected(format!(
            "no such template_id={template_id:?} version={version} locale={locale:?}"
        ))
    })?;

    render::render(&template.body, variables).map_err(ApproveError::Render)
}

async fn resolve_tenant(
    control_pool: &PgPool,
    tenant_slug: &str,
) -> Result<crate::tenant::model::Tenant, ApproveError> {
    tenant_repo::find_by_slug(control_pool, tenant_slug)
        .await?
        .ok_or_else(|| {
            ApproveError::Rejected(format!(
                "no tenant registered with slug {tenant_slug:?}"
            ))
        })
}

#[allow(clippy::too_many_arguments)]
async fn audit(
    control_pool: &PgPool,
    actor: &str,
    tenant_id: Option<Uuid>,
    template_id: &str,
    version: i32,
    channel: &str,
    locale: &str,
    outcome: &str,
) -> Result<(), sqlx::Error> {
    crate::platform_audit::record(
        control_pool,
        actor,
        "template.approve",
        tenant_id,
        serde_json::json!({
            "template_id": template_id,
            "version": version,
            "channel": channel,
            "locale": locale,
            "outcome": outcome,
        }),
    )
    .await
}
