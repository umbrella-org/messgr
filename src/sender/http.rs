//! Generic JSON-over-HTTP `Sender` adapter (T-012 decision 1). Speaks a
//! small contract of this codebase's own design — `POST {base_url}/messages`
//! — rather than a named vendor's API, since DESIGN.md leaves provider
//! selection genuinely open (Still Open #5). Swapping in a real vendor is a
//! later ticket's adapter, not a change to `Sender` itself.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::{SendOutcome, Sender, SenderError};

pub struct HttpSender {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl HttpSender {
    /// `api_key` is supplied directly by the caller. `messgr-dispatcher` resolves it from
    /// `provider_config.credential_path` against Vault before calling this (T-023); tests
    /// construct it with a literal string instead.
    pub fn new(base_url: String, api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url,
            api_key,
        }
    }
}

#[derive(Serialize)]
struct SendRequest<'a> {
    to: &'a str,
    body: &'a str,
}

#[derive(Deserialize)]
struct SendResponse {
    message_id: String,
    status: String,
}

#[async_trait]
impl Sender for HttpSender {
    async fn send(
        &self,
        destination: &str,
        body: &str,
    ) -> Result<SendOutcome, SenderError> {
        let response = self
            .client
            .post(format!("{}/messages", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&SendRequest {
                to: destination,
                body,
            })
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(SenderError::Provider {
                status: status.as_u16(),
                body,
            });
        }

        let parsed: SendResponse = response.json().await?;
        Ok(SendOutcome {
            provider_ref: parsed.message_id,
            provider_status: parsed.status,
        })
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[tokio::test]
    async fn successful_send_parses_provider_ref_and_status() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/messages"))
            .and(header("Authorization", "Bearer test-key"))
            .and(body_json(serde_json::json!({
                "to": "+15551234567",
                "body": "your code is 123456",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message_id": "msg-1",
                "status": "queued",
            })))
            .mount(&mock_server)
            .await;

        let sender = HttpSender::new(mock_server.uri(), "test-key".to_string());
        let outcome = sender
            .send("+15551234567", "your code is 123456")
            .await
            .expect("send should succeed");

        assert_eq!(outcome.provider_ref, "msg-1");
        assert_eq!(outcome.provider_status, "queued");
    }

    #[tokio::test]
    async fn non_success_status_maps_to_provider_error() {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(
                ResponseTemplate::new(500).set_body_string("provider unavailable"),
            )
            .mount(&mock_server)
            .await;

        let sender = HttpSender::new(mock_server.uri(), "test-key".to_string());
        let err = sender
            .send("+15551234567", "hello")
            .await
            .expect_err("send should fail");

        match err {
            SenderError::Provider { status, body } => {
                assert_eq!(status, 500);
                assert_eq!(body, "provider unavailable");
            }
            SenderError::Http(err) => {
                panic!("expected Provider error, got Http({err})")
            }
        }
    }

    #[tokio::test]
    async fn unreachable_base_url_maps_to_http_error() {
        let sender =
            HttpSender::new("http://127.0.0.1:1".to_string(), "test-key".to_string());

        let err = sender
            .send("+15551234567", "hello")
            .await
            .expect_err("send should fail");

        assert!(
            matches!(err, SenderError::Http(_)),
            "expected Http error, got {err:?}"
        );
    }
}
