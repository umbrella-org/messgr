//! mTLS identity resolution (DESIGN.md §4.9, §11.1, T-006): client cert
//! subject → `producer_cert` (control DB) → `(tenant_id, producer_id)`.
//! Deliberately control-database-only — never opens a tenant pool — per
//! §4.11's "mTLS producer certs resolve to a tenant BEFORE any tenant
//! database is opened". `resolve_producer`'s only input is the cert subject
//! string: there is no parameter through which a caller could smuggle an
//! asserted tenant/producer identity instead (§11, §4.9).

use sqlx::PgPool;
use uuid::Uuid;

use crate::tenant::model::status;

use super::cert_repo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedIdentity {
    pub tenant_id: Uuid,
    pub producer_id: Uuid,
}

/// Unknown and disabled are kept distinguishable (T-005 decision 5 kept the
/// `producer_cert` row on disable specifically so this split is possible;
/// collapsing them back into one rejection here would waste that).
#[derive(Debug)]
pub enum ResolutionError {
    UnknownCert,
    Disabled {
        tenant_id: Uuid,
        producer_id: Uuid,
    },
    /// `tenant.status != 'active'` (T-016, closes T-011/F3). Allowlisting
    /// `active` — rather than blocklisting `suspended`/`offboarding_*` —
    /// also rejects a stray request against a still-`provisioning` tenant,
    /// which is strictly safer and free: no producer cert should exist for
    /// one yet.
    TenantNotActive {
        tenant_id: Uuid,
        producer_id: Uuid,
    },
    Database(sqlx::Error),
}

impl std::fmt::Display for ResolutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownCert => write!(f, "unknown certificate subject"),
            Self::Disabled { producer_id, .. } => {
                write!(f, "producer {producer_id} is disabled")
            }
            Self::TenantNotActive { tenant_id, .. } => {
                write!(f, "tenant {tenant_id} is not active")
            }
            Self::Database(err) => write!(f, "identity resolution failed: {err}"),
        }
    }
}

impl std::error::Error for ResolutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(err) => Some(err),
            Self::UnknownCert
            | Self::Disabled { .. }
            | Self::TenantNotActive { .. } => None,
        }
    }
}

impl From<sqlx::Error> for ResolutionError {
    fn from(err: sqlx::Error) -> Self {
        Self::Database(err)
    }
}

/// Resolves a cert subject (CN/SAN) to `(tenant_id, producer_id)` with a
/// single control-database query. `cert_subject` is presented as a plain
/// string — extracting it from an actual TLS handshake is a future ingest
/// binary's job, not this layer's (T-011).
pub async fn resolve_producer(
    control_pool: &PgPool,
    cert_subject: &str,
) -> Result<ResolvedIdentity, ResolutionError> {
    match cert_repo::find_producer_cert(control_pool, cert_subject).await? {
        None => Err(ResolutionError::UnknownCert),
        Some(cert) if !cert.enabled => Err(ResolutionError::Disabled {
            tenant_id: cert.tenant_id,
            producer_id: cert.producer_id,
        }),
        Some(cert) if cert.tenant_status != status::ACTIVE => {
            Err(ResolutionError::TenantNotActive {
                tenant_id: cert.tenant_id,
                producer_id: cert.producer_id,
            })
        }
        Some(cert) => Ok(ResolvedIdentity {
            tenant_id: cert.tenant_id,
            producer_id: cert.producer_id,
        }),
    }
}
