use tokomotive::guardrails::{Action, Direction, Engine, Kind, Rule, RuleSet};

fn rule(kind: Kind, action: Action) -> Rule {
    Rule {
        name: "test-rule".into(),
        kind,
        action,
        direction: Direction::Both,
    }
}
fn engine(rules: Vec<Rule>) -> Engine {
    Engine::new(vec![RuleSet {
        name: "test".into(),
        rules,
        ..Default::default()
    }])
    .unwrap()
}

#[tokio::test]
async fn detectors_reject_invalid_checksums_and_redact_valid_values() {
    for (detector, yes, no) in [
        ("email", "alice@example.com", "alice at example"),
        ("phone", "+442079460958", "12345"),
        ("credit_card", "4111 1111 1111 1111", "4111 1111 1111 1112"),
        (
            "iban",
            "GB82 WEST 1234 5698 7654 32",
            "GB83 WEST 1234 5698 7654 32",
        ),
        (
            "secret",
            "sk-ant-abcdefghijklmnopqrstuvwxyz123456",
            "sk-short",
        ),
    ] {
        let e = engine(vec![rule(
            Kind::Detector {
                detector: detector.into(),
            },
            Action::Redact,
        )]);
        let found = e.inspect(Direction::Request, yes).await;
        assert!(!found.text.contains(yes), "{detector}");
        assert_eq!(found.rules, ["test-rule"]);
        assert_eq!(
            e.inspect(Direction::Request, no).await.text,
            no,
            "{detector}"
        );
    }
}

#[tokio::test]
async fn actions_direction_precedence_and_limits() {
    let keyword = Kind::Keywords {
        words: vec!["secret".into()],
        whole_word: true,
        case_sensitive: false,
    };
    for (action, blocked, text) in [
        (Action::Log, false, "SECRET"),
        (Action::Redact, false, "[REDACTED:test-rule]"),
        (Action::Block, true, "SECRET"),
    ] {
        let e = engine(vec![rule(keyword.clone(), action)]);
        let v = e.inspect(Direction::Request, "SECRET").await;
        assert_eq!(v.blocked.is_some(), blocked);
        assert_eq!(v.text, text);
        assert!(
            e.inspect(Direction::Request, "secrets")
                .await
                .rules
                .is_empty()
        );
    }
    let mut block = rule(keyword.clone(), Action::Block);
    block.direction = Direction::Response;
    let e = engine(vec![rule(keyword, Action::Redact), block]);
    assert!(
        e.inspect(Direction::Request, "secret")
            .await
            .blocked
            .is_none()
    );
    assert!(
        e.inspect(Direction::Response, "secret")
            .await
            .blocked
            .is_some()
    );
    for pattern in ["a".repeat(8193), "a{999999}".into(), "a*".into()] {
        assert!(
            Engine::new(vec![RuleSet {
                rules: vec![rule(Kind::Regex { pattern }, Action::Block)],
                ..Default::default()
            }])
            .is_err()
        );
    }
    assert!(
        e.inspect(Direction::Request, &"x".repeat((1 << 20) + 1))
            .await
            .blocked
            .is_some()
    );
    assert!(
        e.inspect(Direction::Request, &"secret ".repeat(1025))
            .await
            .blocked
            .is_some()
    );
}

#[tokio::test]
async fn secret_families_and_iban_in_prose() {
    let e = engine(vec![rule(
        Kind::Detector {
            detector: "secret".into(),
        },
        Action::Block,
    )]);
    for text in [
        "sk-abcdefghijklmnopqrstuvwxyz123456",
        "sk-ant-abcdefghijklmnopqrstuvwxyz123456",
        "AKIAIOSFODNN7EXAMPLE",
        "ghp_abcdefghijklmnopqrstuvwxyz1234567890",
        "xoxb-1234567890-1234567890-abcdefghijklmnop",
        "xoxp-1234567890-1234567890-abcdefghijklmnop",
        "AIzaSyABCDEFGHIJKLMNOPQRSTUVWXYZ123456789",
        "-----BEGIN PRIVATE KEY-----",
        "-----BEGIN RSA PRIVATE KEY-----",
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ0ZXN0In0.c2lnbmF0dXJlX2V4YW1wbGU",
    ] {
        assert!(
            e.inspect(Direction::Request, text).await.blocked.is_some(),
            "{text}"
        );
    }
    for text in [
        "AKIAshort",
        "ghp_short",
        "xoxb-short",
        "AIza-short",
        "-----BEGIN PUBLIC KEY-----",
        "eyJshort.not.a-jwt",
    ] {
        assert!(
            e.inspect(Direction::Request, text).await.blocked.is_none(),
            "{text}"
        );
    }
    let e = engine(vec![rule(
        Kind::Detector {
            detector: "iban".into(),
        },
        Action::Redact,
    )]);
    for text in [
        "Account GB82 WEST 1234 5698 7654 32 END",
        "Account NL91 ABNA 0417 1643 00 END",
        "Account DE89370400440532013000 END",
    ] {
        assert!(
            e.inspect(Direction::Request, text)
                .await
                .text
                .contains("[REDACTED:test-rule]"),
            "{text}"
        );
    }
}

#[tokio::test]
async fn text_surfaces_are_rewritten_without_touching_images_or_metadata() {
    let e = engine(vec![rule(
        Kind::Detector {
            detector: "email".into(),
        },
        Action::Redact,
    )]);
    let mut body = serde_json::json!({"model":"alice@example.com","system":[{"type":"text","text":"alice@example.com"}],"messages":[{"role":"user","content":[{"type":"text","text":"alice@example.com"},{"type":"image_url","image_url":{"url":"alice@example.com"}},{"type":"tool_result","content":[{"type":"text","text":"alice@example.com"}]}]}],"input":["alice@example.com"]});
    let v = e.inspect_json(Direction::Request, &mut body).await;
    assert_eq!(v.rules, ["test-rule"]);
    assert_eq!(body["model"], "alice@example.com");
    assert_eq!(
        body["messages"][0]["content"][1]["image_url"]["url"],
        "alice@example.com"
    );
    assert_eq!(
        body["messages"][0]["content"][2]["content"][0]["text"],
        "[REDACTED:test-rule]"
    );
    assert_eq!(body["input"][0], "[REDACTED:test-rule]");
}

#[tokio::test]
async fn external_hook_timeout_is_fail_open_unless_closed() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/moderate", listener.local_addr().unwrap());
    let app = axum::Router::new().route(
        "/moderate",
        axum::routing::post(|| async {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            axum::Json(serde_json::json!({"action":"block","reason":"private text"}))
        }),
    );
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let engines: Vec<_> = [false, true]
        .into_iter()
        .map(|fail_closed| {
            Engine::new(vec![RuleSet {
                name: "moderation".into(),
                hook_url: url.clone(),
                fail_closed,
                ..Default::default()
            }])
            .unwrap()
        })
        .collect();
    let (open, closed) = tokio::join!(
        engines[0].inspect(Direction::Request, "hello"),
        engines[1].inspect(Direction::Response, "hello")
    );
    assert!(open.blocked.is_none());
    assert_eq!(closed.blocked.as_deref(), Some("moderation:external"));
    task.abort();
}

#[tokio::test]
async fn external_hook_actions_are_recorded_and_reply_is_bounded() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new().route(
        "/{action}",
        axum::routing::post(
            |axum::extract::Path(action): axum::extract::Path<String>,
             axum::Json(body): axum::Json<serde_json::Value>| async move {
                use axum::response::IntoResponse;
                assert_eq!(body["direction"], "response");
                assert_eq!(body["text"], "private text");
                if action == "large" {
                    return "x".repeat(8193).into_response();
                }
                axum::Json(serde_json::json!({"action":action,"reason":"private text"}))
                    .into_response()
            },
        ),
    );
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    for action in ["log", "redact", "block", "large"] {
        let e = Engine::new(vec![RuleSet {
            name: "moderation".into(),
            hook_url: format!("{base}/{action}"),
            fail_closed: true,
            ..Default::default()
        }])
        .unwrap();
        let v = e.inspect(Direction::Response, "private text").await;
        assert_eq!(v.rules, ["moderation:external"]);
        assert_eq!(v.blocked.is_some(), matches!(action, "block" | "large"));
        if action == "redact" {
            assert_eq!(v.text, "[REDACTED:moderation:external]");
        }
    }
    task.abort();
}

#[tokio::test]
async fn exact_text_limit_and_responses_tool_result_are_inspected() {
    let e = Engine::new(vec![RuleSet {
        max_text_bytes: 5,
        rules: vec![rule(
            Kind::Regex {
                pattern: "hello".into(),
            },
            Action::Log,
        )],
        ..Default::default()
    }])
    .unwrap();
    let mut value = serde_json::json!({"input":"hello"});
    assert!(
        e.inspect_json(Direction::Request, &mut value)
            .await
            .blocked
            .is_none()
    );
    let e = engine(vec![rule(
        Kind::Detector {
            detector: "email".into(),
        },
        Action::Redact,
    )]);
    let mut value = serde_json::json!({"input":[{"type":"function_call_output","call_id":"c","output":"alice@example.com"}]});
    let verdict = e.inspect_json(Direction::Request, &mut value).await;
    assert_eq!(verdict.rules, ["test-rule"]);
    assert_eq!(value["input"][0]["output"], "[REDACTED:test-rule]");
}

#[tokio::test]
async fn direction_does_not_limit_uninspected_text_and_keywords_respect_case() {
    let mut request_only = rule(
        Kind::Keywords {
            words: vec!["Hello".into()],
            whole_word: false,
            case_sensitive: true,
        },
        Action::Block,
    );
    request_only.direction = Direction::Request;
    let e = Engine::new(vec![RuleSet {
        max_text_bytes: 5,
        rules: vec![request_only],
        ..Default::default()
    }])
    .unwrap();
    assert!(
        e.inspect(Direction::Response, "Hello long response")
            .await
            .blocked
            .is_none()
    );
    assert!(
        e.inspect(Direction::Request, "hello")
            .await
            .blocked
            .is_none()
    );
    assert!(
        e.inspect(Direction::Request, "Hello")
            .await
            .blocked
            .is_some()
    );
}
