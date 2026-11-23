use super::{Client, ClientError};
use else_wire::computers::{CancelledComputers, ComputerState, Computers, QueuedComputers};
use reqwest::{Method, StatusCode};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

impl Client {
    pub async fn computer(self: Arc<Self>, bot: Uuid) -> Result<ComputerState, ClientError> {
        let this = self.clone();
        self.run(async move {
            let request = this.request(Method::GET, &format!("/v1/bots/{bot}/computer"));
            this.send_json(request).await
        })
        .await
    }

    pub async fn resume_computer(self: Arc<Self>, bot: Uuid) -> Result<(), ClientError> {
        self.post_computer(bot, "resume", None).await
    }

    /// Starts moving the computer to the server's latest image; the
    /// computer state reports its progress.
    pub async fn upgrade_computer(self: Arc<Self>, bot: Uuid) -> Result<(), ClientError> {
        self.post_computer(bot, "upgrade", None).await
    }

    /// Return the computer to its configured memory; it restarts if running.
    pub async fn reset_computer_memory(self: Arc<Self>, bot: Uuid) -> Result<(), ClientError> {
        self.post_computer(
            bot,
            "memory",
            Some(serde_json::json!({ "memory_mib": null })),
        )
        .await
    }

    async fn post_computer(
        self: Arc<Self>,
        bot: Uuid,
        action: &'static str,
        body: Option<serde_json::Value>,
    ) -> Result<(), ClientError> {
        let this = self.clone();
        self.run(async move {
            let mut request = this
                .request(Method::POST, &format!("/v1/bots/{bot}/computer/{action}"))
                .timeout(Duration::from_secs(if body.is_some() { 120 } else { 15 }));
            if let Some(body) = body {
                request = request.json(&body);
            }
            let response = this.send_request(request).await?;
            let status = response.status();
            if status == StatusCode::UNAUTHORIZED {
                return Err(ClientError::Unauthorized);
            }
            if status.is_success() {
                return Ok(());
            }
            if status.is_redirection() {
                return Err(ClientError::Protocol(format!(
                    "redirect ({status}) refused"
                )));
            }
            let bytes = response
                .bytes()
                .await
                .map_err(|err| ClientError::Protocol(err.without_url().to_string()))?;
            Err(ClientError::from_http(status, &bytes))
        })
        .await
    }

    pub async fn computers(self: Arc<Self>) -> Result<Computers, ClientError> {
        let this = self.clone();
        self.run(async move {
            let request = this.request(Method::GET, "/v1/computers");
            this.send_json(request).await
        })
        .await
    }

    pub async fn upgrade_all_computers(self: Arc<Self>) -> Result<Vec<Uuid>, ClientError> {
        let this = self.clone();
        self.run(async move {
            let request = this.request(Method::POST, "/v1/computers/upgrade-all");
            this.send_json::<QueuedComputers>(request)
                .await
                .map(|value| value.queued)
        })
        .await
    }

    pub async fn cancel_computer_upgrades(self: Arc<Self>) -> Result<Vec<Uuid>, ClientError> {
        let this = self.clone();
        self.run(async move {
            let request = this.request(Method::POST, "/v1/computers/upgrade-all/cancel");
            this.send_json::<CancelledComputers>(request)
                .await
                .map(|value| value.cancelled)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use else_wire::computers::{Computer, ComputerState, ComputerUpgrade};
    use serde_json::json;

    fn computer(upgrade: serde_json::Value) -> Computer {
        serde_json::from_value(json!({
            "bot_id": Uuid::nil(), "state": "running", "desired_state": "auto",
            "image": "else-guest:3", "latest_image": "else-guest:4", "upgrade": upgrade,
            "disk_used_bytes": 13314398208u64, "disk_size_bytes": 34359738368u64,
            "cpus": 2, "cpu_percent": 38.5, "memory_used_bytes": 2254857830u64,
            "memory_limit_bytes": 4294967296u64, "last_used_at": "2026-11-16T21:00:00Z"
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn computers_requests_authenticate_and_preserve_refusals() {
        use crate::origin::OwnerToken;
        for (operation, response, path) in [
            (
                0,
                "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{\"latest_image\":null,\"rollback_bytes\":null,\"computers\":[]}",
                "/v1/computers",
            ),
            (
                1,
                "HTTP/1.1 202 Accepted\r\nConnection: close\r\n\r\n{\"queued\":[]}",
                "/v1/computers/upgrade-all",
            ),
            (
                2,
                "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{\"cancelled\":[]}",
                "/v1/computers/upgrade-all/cancel",
            ),
            (
                3,
                "HTTP/1.1 409 Conflict\r\nConnection: close\r\n\r\n{\"error\":{\"code\":\"computer_busy\",\"message\":\"The bot is using its computer.\"}}",
                "/v1/bots/00000000-0000-0000-0000-000000000000/computer/upgrade",
            ),
            (
                4,
                "HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\n\r\n{\"error\":{\"code\":\"computers_unavailable\",\"message\":\"Unavailable\"}}",
                "/v1/computers",
            ),
        ] {
            let (origin, server) = super::super::tests::serve_once(response).await;
            let token = OwnerToken::parse(&"T".repeat(43)).unwrap();
            let client =
                Arc::new(Client::new(origin, &token, tokio::runtime::Handle::current()).unwrap());
            match operation {
                0 => assert!(client.computers().await.unwrap().computers.is_empty()),
                1 => assert!(client.upgrade_all_computers().await.unwrap().is_empty()),
                2 => assert!(client.cancel_computer_upgrades().await.unwrap().is_empty()),
                3 => assert_eq!(
                    client
                        .upgrade_computer(Uuid::nil())
                        .await
                        .unwrap_err()
                        .to_string(),
                    "The bot is using its computer."
                ),
                _ => assert_eq!(
                    client.computers().await.unwrap_err().code(),
                    Some("computers_unavailable")
                ),
            }
            let request = server.await.unwrap();
            let method = if operation == 0 || operation == 4 {
                "GET"
            } else {
                "POST"
            };
            assert!(request.starts_with(&format!("{method} {path} ")));
            assert!(request.contains(&format!("Bearer {}", "T".repeat(43))));
            assert!(!request.lines().next().unwrap().contains(&"T".repeat(43)));
        }
    }

    #[test]
    fn computers_contract_decodes_every_upgrade_state_and_nullable_stats() {
        let variants = [
            (json!(null), None),
            (
                json!({"state":"queued","position":2}),
                Some(ComputerUpgrade::Queued { position: 2 }),
            ),
            (
                json!({"state":"waiting","reason":"Busy"}),
                Some(ComputerUpgrade::Waiting {
                    reason: "Busy".into(),
                }),
            ),
            (json!({"state":"running"}), Some(ComputerUpgrade::Running)),
            (
                json!({"state":"failed","error":"No disk"}),
                Some(ComputerUpgrade::Failed {
                    error: "No disk".into(),
                }),
            ),
            (
                json!({"state":"completed","unrestored_packages":["ffmpeg"]}),
                Some(ComputerUpgrade::Completed {
                    unrestored_packages: vec!["ffmpeg".into()],
                }),
            ),
        ];
        for (raw, expected) in variants {
            let computer = computer(raw);
            assert_eq!(computer.upgrade, expected);
            assert!(computer.outdated());
            assert_eq!(computer.cpu_percent, Some(38.5));
            assert_eq!(computer.memory_limit_bytes, 4294967296);
        }
        let report: Computers = serde_json::from_value(json!({
            "latest_image": null, "rollback_bytes": null, "computers": [{
                "bot_id": Uuid::nil(), "state": "stopped", "desired_state": "stopped",
                "image": "else-guest:3", "latest_image": null, "upgrade": null,
                "disk_used_bytes": null, "disk_size_bytes": null, "cpus": 2,
                "cpu_percent": null, "memory_used_bytes": null,
                "memory_limit_bytes": 4294967296u64, "last_used_at": null
            }]
        }))
        .unwrap();
        assert!(!report.computers[0].outdated());
        assert_eq!(report.computers[0].cpu_percent, None);
        assert_eq!(report.computers[0].memory_used_bytes, None);
    }

    fn resume_client(origin: crate::origin::Origin) -> Arc<Client> {
        let token = crate::origin::OwnerToken::parse(&"T".repeat(43)).unwrap();
        Arc::new(Client::new(origin, &token, tokio::runtime::Handle::current()).unwrap())
    }

    #[test]
    fn computer_state_reports_an_outdated_image_and_upgrade_progress() {
        let parse =
            |json: serde_json::Value| -> ComputerState { serde_json::from_value(json).unwrap() };
        let base = serde_json::json!({ "state": "stopped", "desired_state": "auto" });
        assert!(
            !parse(base.clone()).outdated(),
            "an older server sends no images"
        );
        let outdated = parse(serde_json::json!({
            "state": "stopped", "desired_state": "auto",
            "image": "img:old", "latest_image": "img:new", "upgrade": null,
        }));
        assert!(outdated.outdated());
        assert_eq!(outdated.upgrade, None);
        let running = parse(serde_json::json!({
            "state": "stopped", "desired_state": "auto", "upgrade": { "state": "running" },
        }));
        assert_eq!(running.upgrade, Some(ComputerUpgrade::Running));
        let failed = parse(serde_json::json!({
            "state": "stopped", "desired_state": "auto",
            "upgrade": { "state": "failed", "error": "busy" },
        }));
        assert_eq!(
            failed.upgrade,
            Some(ComputerUpgrade::Failed {
                error: "busy".into()
            })
        );
        let partial = parse(serde_json::json!({
            "state": "stopped", "desired_state": "auto",
            "upgrade": { "state": "completed", "unrestored_packages": ["ffmpeg"] },
        }));
        assert_eq!(
            partial.upgrade,
            Some(ComputerUpgrade::Completed {
                unrestored_packages: vec!["ffmpeg".into()]
            })
        );
    }

    #[tokio::test]
    async fn resume_accepts_no_content_and_keeps_the_token_in_its_header() {
        let (origin, server) =
            crate::testing::serve_once("HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .await;
        let bot = Uuid::new_v4();
        resume_client(origin).resume_computer(bot).await.unwrap();
        let request = server.await.unwrap();
        assert!(request.starts_with(&format!("POST /v1/bots/{bot}/computer/resume ")));
        assert!(request.contains(&format!("Bearer {}", "T".repeat(43))));
        assert!(!request.lines().next().unwrap().contains(&"T".repeat(43)));
    }
}
