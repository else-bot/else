use crate::common;

use axum::http::StatusCode;
use base64::Engine as _;
use common::TestApp;
use serde_json::{Value, json};

async fn advertised(app: &TestApp) -> Value {
    app.owner("GET", "/v1/capabilities", None).await.body["server"]["icon"].clone()
}

async fn set(app: &TestApp, body: Value) -> common::Reply {
    app.owner("PUT", "/v1/settings/server/icon", Some(body))
        .await
}

fn png(side: u32) -> String {
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(side, side)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    base64::engine::general_purpose::STANDARD.encode(png.into_inner())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_owner_sets_the_icon_every_desktop_shows_and_restart_keeps() {
    let app = TestApp::installed().await;
    assert_eq!(advertised(&app).await, Value::Null, "no icon yet");

    let emoji = json!({ "kind": "emoji", "emoji": "🐙", "hue": 195 });
    let saved = set(&app, json!({ "icon": emoji })).await;
    assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
    assert_eq!(saved.body["server"]["icon"], emoji);
    assert_eq!(advertised(&app).await, emoji);

    let picture = png(128);
    let saved = set(
        &app,
        json!({ "icon": { "kind": "picture" }, "picture": picture }),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{:?}", saved.body);
    let digest = saved.body["server"]["icon"]["digest"].clone();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&picture)
        .unwrap();
    assert_eq!(
        digest,
        hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes)),
        "the digest is the picture's SHA-256"
    );
    assert_eq!(
        advertised(&app).await,
        json!({ "kind": "picture", "digest": digest }),
        "capabilities carry the digest, not the picture"
    );
    let fetched = app.owner("GET", "/v1/settings/server/icon", None).await;
    assert_eq!(fetched.status, StatusCode::OK);
    assert_eq!(fetched.body["picture"], picture.as_str());

    let app = app.restart().await;
    assert_eq!(advertised(&app).await["digest"], digest);

    let default = json!({ "kind": "default" });
    let saved = set(&app, json!({ "icon": default })).await;
    assert_eq!(saved.body["server"]["icon"], default);
    let fetched = app.owner("GET", "/v1/settings/server/icon", None).await;
    assert_eq!(
        fetched.body,
        json!({ "icon": default, "picture": null }),
        "a new icon drops the old picture"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_carried_icon_only_fills_a_server_without_one() {
    let app = TestApp::installed().await;
    let first = json!({ "kind": "pattern", "hue": 145 });
    let second = json!({ "kind": "letters", "style": "monogram", "letters": "Q" });
    for (icon, kept) in [(&first, &first), (&second, &first)] {
        let carried = set(&app, json!({ "icon": icon, "only_if_unset": true })).await;
        assert_eq!(carried.status, StatusCode::OK, "{:?}", carried.body);
        assert_eq!(&carried.body["server"]["icon"], kept);
    }
    assert_eq!(advertised(&app).await, first);
    let chosen = set(&app, json!({ "icon": second })).await;
    assert_eq!(
        chosen.body["server"]["icon"], second,
        "a choice replaces it"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_icons_are_refused_and_change_nothing() {
    let app = TestApp::installed().await;
    let kept = json!({ "kind": "pattern" });
    assert_eq!(
        set(&app, json!({ "icon": kept })).await.status,
        StatusCode::OK
    );
    let refused = [
        json!({ "icon": { "kind": "pattern", "hue": 360 } }),
        json!({ "icon": { "kind": "letters", "style": "gradient", "letters": "abc" } }),
        json!({ "icon": { "kind": "letters", "style": "gradient", "letters": " a" } }),
        json!({ "icon": { "kind": "emoji", "emoji": "" } }),
        json!({ "icon": { "kind": "emoji", "emoji": "x".repeat(33) } }),
        json!({ "icon": { "kind": "picture" } }),
        json!({ "icon": { "kind": "picture" }, "picture": "not base64!" }),
        json!({ "icon": { "kind": "picture" }, "picture": png(64) }),
        json!({ "icon": { "kind": "pattern" }, "picture": png(128) }),
        json!({ "icon": { "kind": "picture", "digest": "00" }, "picture": png(128) }),
        json!({ "icon": { "kind": "sticker" } }),
        json!({ "icon": { "kind": "default", "hue": 3 } }),
    ];
    for body in refused {
        let response = set(&app, body.clone()).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{body}");
    }
    assert_eq!(advertised(&app).await, kept);
}
