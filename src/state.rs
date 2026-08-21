use sqlx::PgPool;
use tokio::sync::mpsc;

use crate::sms::model::SmsMessage;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub sms_tx: mpsc::Sender<SmsMessage>,
}
