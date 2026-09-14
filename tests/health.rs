//! `messgr::health` router (T-032): plain, unauthenticated liveness probe
//! shared by `messgr-ingest` and `messgr-dispatcher`.

use std::net::SocketAddr;

#[tokio::test]
async fn healthz_returns_200_over_plain_http() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, messgr::health::router())
            .await
            .unwrap();
    });

    let resp = reqwest::get(format!("http://{addr}/healthz"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
}
