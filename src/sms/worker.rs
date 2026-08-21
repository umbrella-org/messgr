use std::time::Duration;

use sqlx::PgPool;
use tokio::sync::mpsc;
use uuid::Uuid;

use super::{model::SmsMessage, repo};

const BATCH_MAX_SIZE: usize = 100;
const BATCH_MAX_WAIT: Duration = Duration::from_millis(50);

pub async fn run(pool: PgPool, mut rx: mpsc::Receiver<SmsMessage>) {
    while let Some(first) = rx.recv().await {
        let mut batch = Vec::with_capacity(BATCH_MAX_SIZE);
        batch.push(first);

        let deadline = tokio::time::sleep(BATCH_MAX_WAIT);
        tokio::pin!(deadline);

        while batch.len() < BATCH_MAX_SIZE {
            tokio::select! {
                item = rx.recv() => match item {
                    Some(item) => batch.push(item),
                    None => break,
                },
                _ = &mut deadline => break,
            }
        }

        let batch_size = batch.len();
        if let Err(err) = repo::batch_insert(&pool, &batch).await {
            let ids: Vec<Uuid> = batch.iter().map(|m| m.id).collect();
            tracing::error!(
                ?err,
                ?ids,
                batch_size,
                "failed to batch insert sms messages"
            );
        } else {
            tracing::debug!(batch_size, "batch inserted sms messages");
        }
    }

    tracing::info!("sms worker channel closed and drained, exiting");
}
