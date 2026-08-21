mod db;
mod sms;
mod state;
mod web;

use axum::Router;
use std::env;
use tokio::sync::mpsc;

use state::AppState;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::init();

    let database_url = env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let server_addr =
        env::var("SERVER_ADDR").unwrap_or_else(|_| "0.0.0.0:8888".to_string());
    let database_max_connections = env::var("DATABASE_MAX_CONNECTIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let sms_queue_capacity = env::var("SMS_QUEUE_CAPACITY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1024usize);

    let pool = db::connect(&database_url, database_max_connections)
        .await
        .expect("failed to connect to database");

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("failed to run migrations");

    let (sms_tx, sms_rx) = mpsc::channel(sms_queue_capacity);
    let worker_handle = tokio::spawn(sms::worker::run(pool.clone(), sms_rx));

    let state = AppState {
        pool: pool.clone(),
        sms_tx: sms_tx.clone(),
    };

    let app = Router::new()
        .merge(web::routes())
        .merge(sms::routes())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&server_addr)
        .await
        .expect("failed to bind server address");

    tracing::info!("listening on {}", server_addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");

    drop(sms_tx);

    tracing::info!("waiting for sms worker to drain remaining queued messages");
    if let Err(err) = worker_handle.await {
        tracing::error!(?err, "sms worker task panicked");
    }

    tracing::info!("shutdown complete");
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received, no longer accepting new connections");
}
