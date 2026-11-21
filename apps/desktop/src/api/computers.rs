use super::{Client, ClientError};
use else_wire::computers::{CancelledComputers, Computers, QueuedComputers};
use reqwest::Method;
use std::sync::Arc;
use uuid::Uuid;

impl Client {
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
    use else_wire::computers::{Computer, ComputerUpgrade};
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
        use crate::config::OwnerToken;
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
}
