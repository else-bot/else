use super::{Client, ClientError};
use crate::model::{SkillFile, SkillInstall, SkillScope, SkillSource};
use reqwest::Method;
use serde::Deserialize;
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};
use uuid::Uuid;

#[derive(Clone, Deserialize)]
pub struct InstalledSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub source: SkillSource,
    pub scope: SkillScope,
    pub bot_id: Option<Uuid>,
    pub bot_name: Option<String>,
    pub installed_by: Uuid,
    pub installed_by_name: String,
    pub installed_at: String,
    pub files: Vec<SkillFile>,
    pub env: Vec<String>,
    pub secret: String,
    pub key_stored: bool,
}

impl InstalledSkill {
    pub fn access(&self) -> String {
        match self.scope {
            SkillScope::All => "All bots".into(),
            SkillScope::Bot => self.bot_name.clone().unwrap_or_else(|| {
                self.bot_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "Installing bot".into())
            }),
        }
    }
}

#[derive(Deserialize)]
pub struct SkillContents {
    pub contents: BTreeMap<String, String>,
}

impl Client {
    pub async fn skill_install(
        self: Arc<Self>,
        bot: Uuid,
        id: Uuid,
    ) -> Result<SkillContents, ClientError> {
        self.call::<SkillContents>(
            Method::GET,
            format!("/v1/bots/{bot}/skill-installs/{id}"),
            None,
        )
        .await
        .map(|(_, detail)| detail)
    }

    pub async fn answer_skill_install(
        self: Arc<Self>,
        bot: Uuid,
        id: Uuid,
        approval: Option<(SkillScope, Option<BTreeMap<String, String>>)>,
    ) -> Result<SkillInstall, ClientError> {
        #[derive(Deserialize)]
        struct Answer {
            install: SkillInstall,
        }
        let (action, body) = match approval {
            Some((scope, env)) => {
                let mut body = json!({"scope": scope});
                if let Some(env) = env {
                    body["env"] = json!(env);
                }
                ("approve", Some(body))
            }
            None => ("decline", None),
        };
        self.call::<Answer>(
            Method::POST,
            format!("/v1/bots/{bot}/skill-installs/{id}/{action}"),
            body,
        )
        .await
        .map(|(_, answer)| answer.install)
    }

    pub async fn installed_skills(self: Arc<Self>) -> Result<Vec<InstalledSkill>, ClientError> {
        #[derive(Deserialize)]
        struct Library {
            skills: Vec<InstalledSkill>,
        }
        self.call::<Library>(Method::GET, "/v1/settings/skills".into(), None)
            .await
            .map(|(_, library)| library.skills)
    }

    pub async fn skill_detail(self: Arc<Self>, id: String) -> Result<SkillContents, ClientError> {
        self.call::<SkillContents>(Method::GET, format!("/v1/settings/skills/{id}"), None)
            .await
            .map(|(_, detail)| detail)
    }

    pub async fn remove_skill(self: Arc<Self>, id: String) -> Result<(), ClientError> {
        self.call::<()>(Method::DELETE, format!("/v1/settings/skills/{id}"), None)
            .await
            .map(|_| ())
    }

    pub async fn save_skill_key(
        self: Arc<Self>,
        secret: String,
        env: Option<BTreeMap<String, String>>,
    ) -> Result<(), ClientError> {
        let (method, body) = match env {
            Some(env) => (Method::PUT, Some(json!({"env": env}))),
            None => (Method::DELETE, None),
        };
        self.call::<serde_json::Value>(method, format!("/v1/task-secrets/{secret}"), body)
            .await
            .map(|_| ())
    }
}
