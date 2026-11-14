use serde_json::json;
use tokomotive::{db, models_dev};

fn payload() -> Vec<u8> {
    serde_json::to_vec(&json!({"test": {"id":"test", "name":"Test", "api":"https://example.org/v1", "models": {
        "m": {"id":"m", "name":"Model", "limit":{"context":1000,"output":400}, "cost":{"input":2,"output":6}}
    }}})).unwrap()
}

#[tokio::test]
async fn invalid_sync_keeps_previous_data() {
    let db = db::connect("sqlite::memory:").await.unwrap();
    models_dev::import(&db, &payload(), Some("v1"))
        .await
        .unwrap();
    for bad in [b"bad json".to_vec(), b"{}".to_vec()] {
        assert!(models_dev::import(&db, &bad, Some("bad")).await.is_err());
        assert_eq!(
            models_dev::search(&db, None, "Model", 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }
    let negative = String::from_utf8(payload())
        .unwrap()
        .replace("\"input\":2", "\"input\":-2");
    assert!(
        models_dev::import(&db, negative.as_bytes(), None)
            .await
            .is_err()
    );
    assert_eq!(
        models_dev::search(&db, None, "Model", 10)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn first_start_uses_snapshot_without_network() {
    let db = db::connect("sqlite::memory:").await.unwrap();
    models_dev::seed(&db).await.unwrap();
    assert!(
        !models_dev::search(&db, Some("anthropic"), "sonnet", 10)
            .await
            .unwrap()
            .is_empty()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM models_dev_models")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(count, 9135);
    models_dev::seed(&db).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM models_dev_models")
            .fetch_one(&db)
            .await
            .unwrap(),
        count
    );
}

#[tokio::test]
async fn sync_versions_changes_pins_subscriptions_and_mapping() {
    let db = db::connect("sqlite::memory:").await.unwrap();
    sqlx::query("INSERT INTO providers(name,kind,base_url,keys_json,preset) VALUES('p','\"openai\"','https://example.org/v1','[]','test')").execute(&db).await.unwrap();
    let id = db::create_model(&db, "m-2026-11-13", "p", &[])
        .await
        .unwrap();
    models_dev::import(&db, &payload(), Some("v1"))
        .await
        .unwrap();
    assert_eq!(
        models_dev::mapped(&db, id).await.unwrap(),
        Some(("test".into(), "m".into()))
    );
    assert_eq!(models_dev::output_limit(&db, id).await.unwrap(), Some(400));
    let history = db::card_history(&db, id).await.unwrap();
    assert_eq!(history.len(), 1);
    models_dev::import(&db, &payload(), Some("v2"))
        .await
        .unwrap();
    assert_eq!(db::card_history(&db, id).await.unwrap().len(), 1);
    let changed = String::from_utf8(payload())
        .unwrap()
        .replace("\"input\":2", "\"input\":4");
    models_dev::import(&db, changed.as_bytes(), Some("v3"))
        .await
        .unwrap();
    assert_eq!(db::card_history(&db, id).await.unwrap().len(), 2);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM models_dev_alerts")
            .fetch_one(&db)
            .await
            .unwrap(),
        1
    );
    sqlx::query("UPDATE models SET cost_pinned=1 WHERE id=?")
        .bind(id)
        .execute(&db)
        .await
        .unwrap();
    models_dev::import(&db, &payload(), Some("v4"))
        .await
        .unwrap();
    assert_eq!(db::card_history(&db, id).await.unwrap().len(), 2);
    sqlx::query("UPDATE models SET cost_pinned=0; UPDATE providers SET auth='chatgpt'")
        .execute(&db)
        .await
        .unwrap();
    models_dev::import(&db, &payload(), Some("v5"))
        .await
        .unwrap();
    assert_eq!(db::card_history(&db, id).await.unwrap().len(), 2);
}

#[test]
fn maps_cache_reasoning_and_context_tiers() {
    let card = models_dev::cost_card(&json!({"input":3,"output":10,"reasoning":15,"cache_read":0.3,"cache_write":3.75,
        "context_over_200k":{"input":6,"output":22.5},
        "tiers":[{"input":9,"output":30,"cache_write_1h":19,"tier":{"type":"context","size":500000}}]
    }), true).unwrap();
    assert_eq!(card.base.cache_read, 300_000);
    assert_eq!(card.base.cache_write_5m, 3_750_000);
    assert_eq!(card.base.cache_write_1h, 6_000_000);
    assert_eq!(card.base.output, 15_000_000);
    assert_eq!(card.tiers[0].above_input_tokens, 200_000);
    assert_eq!(card.tiers[0].rates.input, 6_000_000);
    assert_eq!(card.tiers[0].rates.cache_write_1h, 12_000_000);
    assert_eq!(card.tiers[1].rates.cache_write_1h, 19_000_000);
    assert_eq!(
        models_dev::cost_card(&json!({"input":3,"output":15,"cache_write":4}), false)
            .unwrap()
            .base
            .cache_write_1h,
        4_000_000
    );
}

async fn server(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    url
}

#[tokio::test]
async fn fetch_handles_etag_304_caps_size_and_refuses_redirects() {
    use axum::{
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::get,
    };
    let url = server(
        axum::Router::new()
            .route(
                "/etag",
                get(|headers: HeaderMap| async move {
                    assert_eq!(headers.get("if-none-match").unwrap(), "\"v1\"");
                    StatusCode::NOT_MODIFIED
                }),
            )
            .route("/data", get(|| async { ([("etag", "\"v2\"")], payload()) }))
            .route(
                "/large",
                get(|| async {
                    (
                        [("content-length", "16777217")],
                        axum::body::Body::from(vec![b' '; models_dev::MAX_BYTES + 1]),
                    )
                        .into_response()
                }),
            )
            .route(
                "/redirect",
                get(|| async {
                    (
                        StatusCode::FOUND,
                        [("location", "http://localhost:9/target")],
                    )
                }),
            ),
    )
    .await;
    assert_eq!(
        models_dev::fetch(&format!("{url}/etag"), Some("\"v1\""))
            .await
            .unwrap(),
        models_dev::Fetch::NotModified
    );
    assert_eq!(
        models_dev::fetch(&format!("{url}/data"), None)
            .await
            .unwrap(),
        models_dev::Fetch::Data(payload(), Some("\"v2\"".into()))
    );
    assert!(
        models_dev::fetch(&format!("{url}/large"), None)
            .await
            .unwrap_err()
            .to_string()
            .contains("16 MB")
    );
    assert!(
        models_dev::fetch(&format!("{url}/redirect"), None)
            .await
            .unwrap_err()
            .to_string()
            .contains("redirects")
    );
}

#[tokio::test]
async fn verified_presets_win_and_model_output_limit_is_not_capped() {
    let db = db::connect("sqlite::memory:").await.unwrap();
    let data = String::from_utf8(payload())
        .unwrap()
        .replace("\"test\"", "\"openai\"");
    models_dev::import(&db, data.as_bytes(), None)
        .await
        .unwrap();
    let presets = tokomotive::presets::merged(&db).await.unwrap();
    assert_eq!(presets.iter().filter(|p| p.name == "openai").count(), 1);
    assert_eq!(
        presets
            .iter()
            .find(|p| p.name == "openai")
            .unwrap()
            .base_url,
        "https://api.openai.com"
    );
    sqlx::query("INSERT INTO providers(name,kind,base_url,keys_json,preset) VALUES('p','\"openai\"','https://example.org/v1','[]','openai')").execute(&db).await.unwrap();
    let id = db::create_model(&db, "m", "p", &[]).await.unwrap();
    assert_eq!(
        models_dev::reservation_output(&db, Some(id), 100)
            .await
            .unwrap(),
        400
    );
    assert_eq!(
        models_dev::reservation_output(&db, None, 100)
            .await
            .unwrap(),
        100
    );
}

#[tokio::test]
async fn mapping_uses_overrides_then_preset_then_api_and_model_precedence() {
    let db = db::connect("sqlite::memory:").await.unwrap();
    let data = json!({
        "a":{"id":"a","name":"A","api":"https://a.example/v1","models":{
            "m":{"id":"m","name":"Short"},
            "m-long":{"id":"m-long","name":"Long"},
            "canonical":{"id":"canonical","name":"Canonical","canonical_model_id":"m-long-2026"},
            "m-long-2026":{"id":"m-long-2026","name":"Exact"}
        }},
        "b":{"id":"b","name":"B","api":"https://b.example","models":{"m":{"id":"m","name":"B Model"}}}
    });
    models_dev::import(&db, &serde_json::to_vec(&data).unwrap(), None)
        .await
        .unwrap();
    sqlx::query("INSERT INTO providers(name,kind,base_url,keys_json,preset) VALUES('p','\"openai\"','https://b.example/v1/','[]','a')").execute(&db).await.unwrap();
    let id = db::create_model(&db, "m-long-2026", "p", &[])
        .await
        .unwrap();
    assert_eq!(
        models_dev::mapped(&db, id).await.unwrap(),
        Some(("a".into(), "m-long-2026".into()))
    );
    sqlx::query("UPDATE models SET models_dev_model='m-long-2026-other'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        models_dev::mapped(&db, id).await.unwrap(),
        Some(("a".into(), "m-long-2026".into()))
    );
    sqlx::query("UPDATE models SET models_dev_model='m-long-2027'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        models_dev::mapped(&db, id).await.unwrap(),
        Some(("a".into(), "m-long".into()))
    );
    sqlx::query("UPDATE models SET models_dev_provider='b',models_dev_model='m'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        models_dev::mapped(&db, id).await.unwrap(),
        Some(("b".into(), "m".into()))
    );
    sqlx::query("UPDATE models SET models_dev_provider=NULL; UPDATE providers SET preset=NULL")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        models_dev::mapped(&db, id).await.unwrap(),
        Some(("b".into(), "m".into()))
    );
    sqlx::query(
        "UPDATE providers SET base_url='https://a.example'; UPDATE models SET models_dev_model='m'",
    )
    .execute(&db)
    .await
    .unwrap();
    assert_eq!(
        models_dev::mapped(&db, id).await.unwrap(),
        Some(("a".into(), "m".into()))
    );
    // Canonical IDs take priority over a dated-prefix fallback.
    let mut data = data;
    data["a"]["models"]
        .as_object_mut()
        .unwrap()
        .remove("m-long-2026");
    models_dev::import(&db, &serde_json::to_vec(&data).unwrap(), None)
        .await
        .unwrap();
    sqlx::query("UPDATE models SET models_dev_model='m-long-2026'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        models_dev::mapped(&db, id).await.unwrap(),
        Some(("a".into(), "canonical".into()))
    );
    // Four of five Models remain: exactly 20% loss is allowed; another is not.
    data["a"]["models"]
        .as_object_mut()
        .unwrap()
        .remove("m-long");
    assert!(
        models_dev::import(&db, &serde_json::to_vec(&data).unwrap(), None)
            .await
            .is_err()
    );
    assert_eq!(
        models_dev::mapped(&db, id).await.unwrap(),
        Some(("a".into(), "canonical".into()))
    );
}
