use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};

fn input(id: &str) -> Installation {
    Installation {
        id: PackageId::try_from(id.to_owned()).unwrap(),
        name: "T3 Code".into(),
        description: "Code and threads".into(),
        skill: "Use T3 for code tasks.".into(),
        documentation: "Read the schemas before calling a tool.".into(),
        config: Transport::Http {
            secrets: Vec::new(),
            url: "http://127.0.0.1:1/mcp".into(),
            headers: BTreeMap::from([("Authorization".into(), "Bearer private-token".into())]),
        },
    }
}

#[tokio::test]
async fn packages_survive_restart_and_are_private_shared_and_removable() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("mcp");
    let gate = Arc::new(crate::maintenance::Gate::default());
    let library = Library::new(root.clone(), gate.clone());
    let input = input("t3-code");
    let installed = library
        .install(input.clone(), "Maple".into())
        .await
        .unwrap();
    let repeated = library
        .install(input.clone(), "Scout".into())
        .await
        .unwrap();
    assert_eq!(installed.installed_at, repeated.installed_at);
    assert_eq!(repeated.installed_by, "Maple");
    let reopened = Library::new(root.clone(), gate);
    assert_eq!(reopened.list().await.unwrap().len(), 1);
    let package = reopened.read(&input.id).await.unwrap();
    assert_eq!(package.skill, input.skill);
    assert_eq!(package.documentation, input.documentation);
    assert_eq!(package.config.label(), "http");
    let public = serde_json::to_string(&package).unwrap();
    assert!(!public.contains("private-token"));
    assert!(!public.contains("config"));
    for name in ["manifest.json", "SKILL.md", "README.md", "config.json"] {
        assert_eq!(
            std::fs::metadata(root.join("t3-code").join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    let mut changed = input.clone();
    changed.skill = "Something different".into();
    assert!(matches!(
        reopened.install(changed, "Scout".into()).await,
        Err(Error::Conflict)
    ));
    reopened.remove(&input.id).await.unwrap();
    assert!(reopened.list().await.unwrap().is_empty());
    assert!(matches!(
        reopened.read(&input.id).await,
        Err(Error::NotFound)
    ));
    assert!(matches!(
        reopened
            .request(&input.id, "tools/list".into(), serde_json::json!({}))
            .await,
        Err(Error::NotFound)
    ));
}

#[tokio::test]
async fn simultaneous_same_id_installs_publish_one_complete_package() {
    let temp = tempfile::tempdir().unwrap();
    let library = Library::new(temp.path().join("mcp"), Arc::default());
    let a = input("shared");
    let mut b = a.clone();
    b.skill = "Different instructions".into();
    let (a, b) = tokio::join!(
        library.install(a, "Maple".into()),
        library.install(b, "Scout".into())
    );
    assert_ne!(a.is_ok(), b.is_ok());
    assert_eq!(library.list().await.unwrap().len(), 1);
    let package = library.read(&input("shared").id).await.unwrap();
    assert_eq!(
        package.skill,
        if a.is_ok() {
            "Use T3 for code tasks."
        } else {
            "Different instructions"
        }
    );
}

#[tokio::test]
async fn unsafe_paths_links_and_reserved_credentials_are_rejected() {
    for id in [
        "",
        "../escape",
        "/absolute",
        ".hidden",
        "UPPER",
        "id/nested",
    ] {
        assert!(PackageId::try_from(id.to_owned()).is_err());
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("mcp");
    let library = Library::new(root.clone(), Arc::default());
    let mut bad = input("bad");
    bad.config = Transport::Stdio {
        command: "env".into(),
        args: vec![],
        env: BTreeMap::from([("SECRET_KEY".into(), "must-not-be-injected".into())]),
    };
    assert!(matches!(
        library.install(bad, "Maple".into()).await,
        Err(Error::Invalid(_))
    ));
    assert!(!root.exists());
    library
        .install(input("safe"), "Maple".into())
        .await
        .unwrap();
    std::fs::remove_file(root.join("safe/SKILL.md")).unwrap();
    let outside = temp.path().join("outside");
    std::fs::write(&outside, "private").unwrap();
    symlink(&outside, root.join("safe/SKILL.md")).unwrap();
    assert!(library.read(&input("safe").id).await.is_err());
    library.remove(&input("safe").id).await.unwrap();
    assert_eq!(std::fs::read_to_string(outside).unwrap(), "private");
}

#[tokio::test]
async fn nonexistent_probes_and_removed_packages_do_not_exhaust_actors() {
    let temp = tempfile::tempdir().unwrap();
    let library = Library::new(temp.path().join("mcp"), Arc::default());
    for i in 0..1030 {
        let id = PackageId::try_from(format!("missing-{i}")).unwrap();
        assert!(matches!(library.read(&id).await, Err(Error::NotFound)));
    }
    for i in 0..3 {
        let package = input(&format!("installed-{i}"));
        library
            .install(package.clone(), "Maple".into())
            .await
            .unwrap();
        library.remove(&package.id).await.unwrap();
        library
            .install(package.clone(), "Scout".into())
            .await
            .unwrap();
    }
    assert_eq!(library.list().await.unwrap().len(), 3);
}

#[tokio::test]
async fn discovery_ignores_stray_directories_and_can_remove_a_damaged_config() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("mcp");
    let library = Library::new(root.clone(), Arc::default());
    library
        .install(input("valid"), "Maple".into())
        .await
        .unwrap();
    std::fs::create_dir(root.join("Stray folder")).unwrap();
    std::fs::create_dir(root.join("scratch")).unwrap();
    std::fs::write(root.join("valid/config.json"), "broken").unwrap();
    assert_eq!(library.list().await.unwrap().len(), 1);
    library.remove(&input("valid").id).await.unwrap();
    assert!(library.list().await.unwrap().is_empty());
}

#[test]
fn startup_cleanup_removes_only_unpublished_or_retired_package_files() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    for prefix in [".install-", ".removed-"] {
        let dir = root.join(format!("{prefix}{}", Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("config.json"), "private credentials").unwrap();
    }
    std::fs::create_dir(root.join(".unrelated")).unwrap();
    std::fs::create_dir(root.join("installed")).unwrap();
    cleanup(root).unwrap();
    let mut names: Vec<_> = std::fs::read_dir(root)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(names, vec![".unrelated", "installed"]);
}

#[tokio::test]
async fn maintenance_blocks_mutation_and_execution_but_not_discovery() {
    let temp = tempfile::tempdir().unwrap();
    let gate = Arc::new(crate::maintenance::Gate::default());
    let library = Library::new(temp.path().join("mcp"), gate.clone());
    library
        .install(input("shared"), "Maple".into())
        .await
        .unwrap();
    let closed = gate
        .close(
            std::time::Duration::from_secs(1),
            crate::maintenance::Pause::backup(None),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(library.list().await.unwrap().len(), 1);
    assert!(matches!(
        library.install(input("new"), "Scout".into()).await,
        Err(Error::Paused)
    ));
    assert!(matches!(
        library.remove(&input("shared").id).await,
        Err(Error::Paused)
    ));
    let shared = input("shared").id;
    let list = || library.request(&shared, "tools/list".into(), serde_json::json!({}));
    assert!(
        !matches!(list().await, Err(Error::Paused)),
        "running turns keep their MCP tools while the backup drains them"
    );
    closed
        .capture(std::time::Duration::from_secs(1))
        .await
        .unwrap();
    assert!(matches!(list().await, Err(Error::Paused)));
}

#[tokio::test]
async fn disconnect_closes_the_shared_stdio_session() {
    let dir = tempfile::tempdir().unwrap();
    let library = Library::new(dir.path().join("mcp"), Arc::default());
    let mut package = input("stdio-counter");
    package.config = Transport::Stdio {
        command: "python3".into(),
        args: vec!["-u".into(), "-c".into(), r#"
import json, sys
calls = 0
for line in sys.stdin:
    message = json.loads(line)
    if 'id' not in message:
        continue
    if message['method'] == 'initialize':
        result = {'protocolVersion': message['params']['protocolVersion'], 'capabilities': {'tools': {}}, 'serverInfo': {'name': 'counter', 'version': '1'}}
    elif message['method'] == 'tools/list':
        calls += 1
        result = {'tools': [{'name': 'counter', 'description': str(calls), 'inputSchema': {'type': 'object'}}]}
    else:
        result = {}
    print(json.dumps({'jsonrpc': '2.0', 'id': message['id'], 'result': result}), flush=True)
"#.into()],
        env: BTreeMap::new(),
    };
    let id = package.id.clone();
    library.install(package, "installer".into()).await.unwrap();
    for expected in ["1", "2"] {
        let response = library
            .request(&id, "tools/list".into(), serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(response["tools"][0]["description"], expected);
    }
    library.disconnect(&id).await.unwrap();
    assert_eq!(
        library.connection_status(&id).await.unwrap().status,
        "disconnected"
    );
    let response = library
        .request(&id, "tools/list".into(), serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(
        response["tools"][0]["description"], "1",
        "the process was restarted"
    );
    library.remove(&id).await.unwrap();
}

#[tokio::test]
async fn approved_replacement_keeps_sign_ins_only_for_the_same_connection() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("mcp");
    let library = Library::new(root.clone(), Arc::new(crate::maintenance::Gate::default()));
    let original = input("github");
    library
        .install(original.clone(), "Maple".into())
        .await
        .unwrap();
    let sign_in = root
        .join("github")
        .join(format!("oauth-{}.json", Uuid::new_v4()));
    std::fs::write(&sign_in, "{}").unwrap();

    let mut documented = original.clone();
    documented.skill = "Prefer the search tool.".into();
    let card = library
        .read(&original.id)
        .await
        .unwrap()
        .replacement(&documented);
    assert!(card.contains("Changes: skill"), "{card}");
    assert!(
        card.contains("sign-ins and MCP tool permissions are kept"),
        "{card}"
    );
    let replaced = library
        .replace(documented.clone(), "Scout".into())
        .await
        .unwrap();
    assert!(!replaced.reconnect);
    assert_eq!(replaced.manifest.installed_by, "Scout");
    assert_eq!(
        library.read(&original.id).await.unwrap().skill,
        documented.skill
    );
    assert!(sign_in.exists(), "same connection keeps saved sign-ins");

    let mut moved = documented.clone();
    moved.config = Transport::Http {
        secrets: Vec::new(),
        url: "https://example.com/mcp?token=private-query".into(),
        headers: BTreeMap::from([("Authorization".into(), "Bearer private-token".into())]),
    };
    let card = library
        .read(&original.id)
        .await
        .unwrap()
        .replacement(&moved);
    assert!(
        card.contains("New: T3 Code, HTTP https://example.com/mcp?… with headers Authorization"),
        "{card}"
    );
    assert!(
        !card.contains("private-token") && !card.contains("private-query"),
        "{card}"
    );
    let replaced = library
        .replace(moved.clone(), "Scout".into())
        .await
        .unwrap();
    assert!(replaced.reconnect);
    assert!(!sign_in.exists(), "a new connection drops saved sign-ins");
    assert!(
        library
            .read(&original.id)
            .await
            .unwrap()
            .same_connection(&moved)
    );
    let leftovers: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(leftovers, [std::ffi::OsString::from("github")]);
    assert!(matches!(
        library.install(original, "Maple".into()).await,
        Err(Error::Conflict)
    ));
}

#[tokio::test]
async fn owner_listings_name_the_http_origin_without_credentials() {
    let temp = tempfile::tempdir().unwrap();
    let library = Library::new(temp.path().join("mcp"), Arc::default());
    let mut http = input("sentry");
    http.config = Transport::Http {
        secrets: Vec::new(),
        url: "https://mcp.sentry.dev:8443/secret/mcp?token=private".into(),
        headers: BTreeMap::new(),
    };
    let mut stdio = input("local");
    stdio.config = Transport::Stdio {
        command: "npx".into(),
        args: vec!["server".into()],
        env: BTreeMap::new(),
    };
    library.install(http, "Maple".into()).await.unwrap();
    library.install(stdio, "Maple".into()).await.unwrap();
    let listings = library.listings().await.unwrap();
    let websites: Vec<_> = listings
        .iter()
        .map(|listing| (listing.manifest.id.0.as_str(), listing.website.as_deref()))
        .collect();
    assert_eq!(
        websites,
        [
            ("local", None),
            ("sentry", Some("https://mcp.sentry.dev:8443"))
        ]
    );
    let public = serde_json::to_value(&listings).unwrap();
    assert_eq!(public[1]["name"], "T3 Code");
    assert!(!public.to_string().contains("secret"));
    assert!(!public.to_string().contains("private"));
}

#[test]
fn header_placeholders_name_task_secret_variables() {
    let http = |value: &str, secrets: &[&str]| Transport::Http {
        url: "https://mcp.example.com/mcp".into(),
        headers: BTreeMap::from([("Authorization".into(), value.into())]),
        secrets: secrets.iter().map(|s| (*s).to_owned()).collect(),
    };
    assert!(http("Bearer ${API_TOKEN}", &["vendor"]).validate().is_ok());
    assert!(http("plain $value", &[]).validate().is_ok());
    for (value, secrets) in [
        ("Bearer ${API_TOKEN}", &[][..]),
        ("Bearer ${API_TOKEN", &["vendor"]),
        ("Bearer ${api_token}", &["vendor"]),
        ("Bearer ${PATH}", &["vendor"]),
        ("Bearer ${API_TOKEN}", &["Vendor"]),
        ("Bearer ${API_TOKEN}", &["vendor", "vendor"]),
        ("Bearer ${API_TOKEN}", &["a", "b", "c", "d", "e"]),
    ] {
        assert!(
            matches!(http(value, secrets).validate(), Err(Error::Invalid(_))),
            "{value} {secrets:?}"
        );
    }
    assert!(
        http("x", &["vendor"])
            .describe()
            .ends_with("from task secrets vendor")
    );
}

#[test]
fn an_exchange_swaps_two_directories() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a"), dir.path().join("b"));
    for (path, name) in [(&a, "a"), (&b, "b")] {
        std::fs::create_dir(path).unwrap();
        std::fs::write(path.join(name), "").unwrap();
    }
    exchange(&a, &b).unwrap();
    assert!(a.join("b").exists() && b.join("a").exists());
    assert!(exchange(&a, &dir.path().join("missing")).is_err());
}
