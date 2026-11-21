use super::{Client, ClientError};
use else_wire::skills::{
    Answer, InstallReview, InstalledSkill, Library, SkillContents, SkillInstall, SkillScope,
};
use reqwest::Method;
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};
use uuid::Uuid;

impl Client {
    pub async fn skill_install(
        self: Arc<Self>,
        bot: Uuid,
        id: Uuid,
    ) -> Result<InstallReview, ClientError> {
        self.call::<InstallReview>(
            Method::GET,
            format!("/v1/bots/{bot}/skill-installs/{id}"),
            None,
        )
        .await
    }

    pub async fn answer_skill_install(
        self: Arc<Self>,
        bot: Uuid,
        id: Uuid,
        approval: Option<(SkillScope, Option<BTreeMap<String, String>>)>,
    ) -> Result<SkillInstall, ClientError> {
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
        .map(|answer| answer.install)
    }

    pub async fn installed_skills(self: Arc<Self>) -> Result<Vec<InstalledSkill>, ClientError> {
        self.call::<Library>(Method::GET, "/v1/settings/skills".into(), None)
            .await
            .map(|library| library.skills)
    }

    pub async fn skill_detail(self: Arc<Self>, id: String) -> Result<SkillContents, ClientError> {
        self.call::<SkillContents>(Method::GET, format!("/v1/settings/skills/{id}"), None)
            .await
    }

    pub async fn remove_skill(self: Arc<Self>, id: String) -> Result<(), ClientError> {
        self.call_unit(Method::DELETE, format!("/v1/settings/skills/{id}"), None)
            .await
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
        self.call_unit(method, format!("/v1/task-secrets/{secret}"), body)
            .await
    }
}
