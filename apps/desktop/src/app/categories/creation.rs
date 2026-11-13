use std::sync::Arc;

use uuid::Uuid;

use crate::api::{Category, Client, ClientError};

#[derive(Debug, Clone)]
enum Stage {
    Draft {
        request_id: Uuid,
    },
    InDoubt {
        request_id: Uuid,
        name: String,
        bots: Vec<Uuid>,
    },
    Adding {
        category: Category,
        pending: Vec<Uuid>,
    },
}

#[derive(Debug, Clone)]
pub(super) struct Creation {
    stage: Stage,
}

impl Creation {
    pub(super) fn new() -> Self {
        Self {
            stage: Stage::Draft {
                request_id: Uuid::new_v4(),
            },
        }
    }

    pub(super) fn locked(&self) -> bool {
        !matches!(self.stage, Stage::Draft { .. })
    }

    pub(super) fn adding_bots(&self) -> bool {
        matches!(self.stage, Stage::Adding { .. })
    }

    pub(super) fn bot_ids(&self, draft_ids: &[Uuid]) -> Vec<Uuid> {
        match &self.stage {
            Stage::Draft { .. } => draft_ids.to_vec(),
            Stage::InDoubt { bots, .. } => bots.clone(),
            Stage::Adding { pending, .. } => pending.clone(),
        }
    }

    pub(super) async fn save(
        &mut self,
        client: Arc<Client>,
        name: String,
        bots: Vec<Uuid>,
    ) -> Result<Category, String> {
        if let Stage::Draft { request_id } | Stage::InDoubt { request_id, .. } = self.stage {
            let (name, bots) = match &self.stage {
                Stage::InDoubt { name, bots, .. } => (name.clone(), bots.clone()),
                _ => (name, bots),
            };
            match client
                .clone()
                .create_category(request_id, name.clone())
                .await
            {
                Ok(category) => {
                    self.stage = Stage::Adding {
                        category,
                        pending: bots,
                    }
                }
                Err(err) if err.outcome_unknown() || err.is_request_conflict() => {
                    self.stage = Stage::InDoubt {
                        request_id,
                        name,
                        bots,
                    };
                    return Err(format!(
                        "{err} Create will retry the original request unchanged."
                    ));
                }
                Err(err) => {
                    *self = Self::new();
                    return Err(err.to_string());
                }
            }
        }
        let Stage::Adding { category, pending } = &mut self.stage else {
            unreachable!("a successful create moves to Adding");
        };
        let total = pending.len();
        let failed = assign(&client, category.id, pending).await;
        *pending = failed.iter().map(|(bot, _)| *bot).collect();
        match failed.first() {
            None => Ok(category.clone()),
            Some((_, err)) => Err(format!(
                "Created \"{}\", but {} of {total} bots could not be added: {err} \
                 Retry adds only those bots.",
                category.name,
                failed.len(),
            )),
        }
    }
}

async fn assign(client: &Arc<Client>, category: Uuid, bots: &[Uuid]) -> Vec<(Uuid, ClientError)> {
    let mut failed = Vec::new();
    let mut bots = bots.iter().copied();
    let assign = |bot: Option<Uuid>| {
        let client = client.clone();
        async move {
            let bot = bot?;
            client
                .set_category(bot, Some(category))
                .await
                .err()
                .map(|err| (bot, err))
        }
    };
    while let Some(first) = bots.next() {
        let (a, b, c, d) = tokio::join!(
            assign(Some(first)),
            assign(bots.next()),
            assign(bots.next()),
            assign(bots.next()),
        );
        failed.extend([a, b, c, d].into_iter().flatten());
    }
    failed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Origin, OwnerToken};
    use serde_json::{Value, json};
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Debug, Clone)]
    struct Request {
        method: String,
        path: String,
        body: Value,
    }

    type Reply = (u16, Value);
    type Script = dyn Fn(&Request, usize) -> Reply + Send + Sync;

    async fn serve(
        script: impl Fn(&Request, usize) -> Reply + Send + Sync + 'static,
    ) -> (Arc<Client>, Arc<Mutex<Vec<Request>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = Origin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let seen = Arc::new(Mutex::new(Vec::<Request>::new()));
        let script: Arc<Script> = Arc::new(script);
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (log, script) = (log.clone(), script.clone());
                tokio::spawn(async move {
                    let request = read_request(&mut socket).await;
                    let reply = {
                        let mut log = log.lock().unwrap();
                        let earlier = log.iter().filter(|r| r.path == request.path).count();
                        log.push(request.clone());
                        script(&request, earlier)
                    };
                    let body = reply.1.to_string();
                    let response = format!(
                        "HTTP/1.1 {} X\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        reply.0,
                        body.len()
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                });
            }
        });
        let token = OwnerToken::parse(&"T".repeat(43)).unwrap();
        let client = Client::new(origin, &token, tokio::runtime::Handle::current()).unwrap();
        (Arc::new(client), seen)
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> Request {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = socket.read(&mut chunk).await.unwrap();
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf);
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let length = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(str::to_owned)
                    })
                    .map_or(0, |v| v.trim().parse().unwrap());
                if body.len() >= length || n == 0 {
                    let mut start = head.lines().next().unwrap().split(' ');
                    return Request {
                        method: start.next().unwrap().to_owned(),
                        path: start.next().unwrap().to_owned(),
                        body: serde_json::from_str(body).unwrap_or(Value::Null),
                    };
                }
            }
        }
    }

    fn error(status: u16, code: &str) -> Reply {
        (
            status,
            json!({ "error": { "code": code, "message": format!("{code}."), "retryable": false } }),
        )
    }

    fn created(request: &Request) -> Reply {
        let id = Uuid::new_v4();
        (
            201,
            json!({ "category": { "id": id, "name": request.body["name"] } }),
        )
    }

    fn assigned(request: &Request) -> Reply {
        let bot = request.path.split('/').nth(3).unwrap();
        (200, json!({ "bot": { "id": bot, "name": "Bot" } }))
    }

    fn creates(seen: &Mutex<Vec<Request>>) -> Vec<Value> {
        let seen = seen.lock().unwrap();
        seen.iter()
            .filter(|r| r.method == "POST" && r.path == "/v1/categories")
            .map(|r| r.body.clone())
            .collect()
    }

    fn assigned_bots(seen: &Mutex<Vec<Request>>) -> Vec<String> {
        let seen = seen.lock().unwrap();
        seen.iter()
            .filter(|r| r.method == "PATCH")
            .map(|r| r.path.split('/').nth(3).unwrap().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn uncertain_create_retries_the_same_request_and_selection() {
        let (client, seen) = serve(|request, earlier| match request.path.as_str() {
            "/v1/categories" if earlier == 0 => error(503, "unavailable"),
            "/v1/categories" => created(request),
            _ => assigned(request),
        })
        .await;
        let bot = Uuid::new_v4();
        let mut creation = Creation::new();

        let err = creation
            .save(client.clone(), "Family".into(), vec![bot])
            .await
            .unwrap_err();
        assert!(err.contains("retry the original request"), "{err}");
        assert!(creation.locked());
        assert!(!creation.adding_bots());
        assert_eq!(creation.bot_ids(&[]), vec![bot]);

        let category = creation
            .save(client, "Edited".into(), vec![Uuid::new_v4()])
            .await
            .unwrap();
        assert_eq!(category.name, "Family");
        let creates = creates(&seen);
        assert_eq!(creates.len(), 2);
        assert_eq!(creates[0], creates[1], "the retry repeats the payload");
        assert_eq!(assigned_bots(&seen), vec![bot.to_string()]);
    }

    #[tokio::test]
    async fn rejected_create_returns_to_an_editable_fresh_draft() {
        let (client, seen) = serve(|request, earlier| match earlier {
            0 => error(409, "duplicate_category"),
            _ => created(request),
        })
        .await;
        let mut creation = Creation::new();
        let first = Uuid::new_v4();

        let err = creation
            .save(client.clone(), "Family".into(), vec![])
            .await
            .unwrap_err();
        assert_eq!(err, "duplicate_category.");
        assert!(!creation.locked());
        assert_eq!(creation.bot_ids(&[first]), vec![first]);

        let category = creation
            .save(client, "Friends".into(), vec![])
            .await
            .unwrap();
        assert_eq!(category.name, "Friends");
        let creates = creates(&seen);
        assert_ne!(
            creates[0]["request_id"], creates[1]["request_id"],
            "an edited draft is a new request"
        );
    }

    #[tokio::test]
    async fn failed_assignments_retry_alone_without_a_second_create() {
        let bots: Vec<Uuid> = Vec::from_fn(6, |_| Uuid::new_v4());
        let flaky = [bots[1].to_string(), bots[4].to_string()];
        let (client, seen) = serve(move |request, earlier| {
            let bot = request.path.split('/').nth(3).unwrap_or_default();
            match request.path.as_str() {
                "/v1/categories" => created(request),
                _ if earlier == 0 && flaky.iter().any(|f| f == bot) => error(500, "internal"),
                _ => assigned(request),
            }
        })
        .await;
        let mut creation = Creation::new();

        let err = creation
            .save(client.clone(), "Work".into(), bots.clone())
            .await
            .unwrap_err();
        assert!(err.contains("2 of 6 bots"), "{err}");
        assert!(creation.locked());
        assert!(creation.adding_bots());
        assert_eq!(creation.bot_ids(&bots), vec![bots[1], bots[4]]);

        let category = creation
            .save(client, "Work".into(), bots.clone())
            .await
            .unwrap();
        assert_eq!(category.name, "Work");
        assert_eq!(creates(&seen).len(), 1);
        let assigned = assigned_bots(&seen);
        assert_eq!(assigned.len(), 8);
        assert_eq!(
            assigned[6..]
                .iter()
                .collect::<std::collections::HashSet<_>>(),
            [bots[1].to_string(), bots[4].to_string()].iter().collect(),
        );
        let seen = seen.lock().unwrap();
        let category_id = json!(category.id);
        assert!(
            seen.iter()
                .filter(|r| r.method == "PATCH")
                .all(|r| r.body == json!({ "category_id": category_id })),
        );
    }

    #[tokio::test]
    async fn an_empty_selection_creates_only_the_category() {
        let (client, seen) = serve(|request, _| created(request)).await;
        let mut creation = Creation::new();

        let category = creation.save(client, "Empty".into(), vec![]).await.unwrap();
        assert_eq!(category.name, "Empty");
        assert_eq!(creates(&seen).len(), 1);
        assert!(assigned_bots(&seen).is_empty());
    }
}
