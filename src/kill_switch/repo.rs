use sqlx::PgPool;
use uuid::Uuid;

use super::model::{KillSwitch, scope, split_producer_channel};

/// Every currently-engaged switch (`released_at IS NULL`) — the full set
/// `KillSwitchCache::refresh` diffs against its previous snapshot.
pub async fn list_active(pool: &PgPool) -> Result<Vec<KillSwitch>, sqlx::Error> {
    sqlx::query_as::<_, KillSwitch>(
        r#"
        SELECT id, scope, scope_key, on_queued, engaged_by, engaged_at, reason,
               released_by, released_at
        FROM kill_switch
        WHERE released_at IS NULL
        "#,
    )
    .fetch_all(pool)
    .await
}

/// Live count of queued (`outbox.cancelled_at IS NULL`) rows a
/// `scope`/`scope_key` pair would cover, by `class` — the admin panel's
/// "blast radius" shown before an engage (T-049 decision 5). Mirrors
/// `dispatcher::repo::claim_for_scope`'s five scope arms exactly, computed
/// live rather than cached (an engage/release decision needs the truth at
/// the moment it's made, not a stale snapshot).
///
/// An unparseable `producer`/`producer_channel` `scope_key` returns an empty
/// result, matching `KillSwitch::matches`'s own "treat as matching nothing"
/// behaviour, rather than erroring.
pub async fn blast_radius(
    pool: &PgPool,
    scope: &str,
    scope_key: Option<&str>,
) -> Result<Vec<(String, i64)>, sqlx::Error> {
    let mut qb = sqlx::QueryBuilder::new(
        "SELECT class, count(*) FROM outbox WHERE cancelled_at IS NULL",
    );

    match scope {
        scope::GLOBAL => {}
        scope::CHANNEL => {
            let Some(channel) = scope_key else {
                return Ok(Vec::new());
            };
            qb.push(" AND channel = ");
            qb.push_bind(channel.to_string());
        }
        scope::PRODUCER => {
            let Some(producer_id) = scope_key.and_then(|k| Uuid::parse_str(k).ok())
            else {
                return Ok(Vec::new());
            };
            qb.push(" AND producer_id = ");
            qb.push_bind(producer_id);
        }
        scope::PRODUCER_CHANNEL => {
            let Some((producer_id, channel)) =
                scope_key.and_then(split_producer_channel)
            else {
                return Ok(Vec::new());
            };
            qb.push(" AND producer_id = ");
            qb.push_bind(producer_id);
            qb.push(" AND channel = ");
            qb.push_bind(channel.to_string());
        }
        scope::CAMPAIGN => {
            let Some(campaign_id) = scope_key else {
                return Ok(Vec::new());
            };
            qb.push(" AND campaign_id = ");
            qb.push_bind(campaign_id.to_string());
        }
        _ => return Ok(Vec::new()),
    }

    qb.push(" GROUP BY class");
    qb.build_query_as::<(String, i64)>().fetch_all(pool).await
}
