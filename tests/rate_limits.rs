use llm_accounting::{config::Config, db, ratelimit::Limits};

#[test]
fn config_rejects_negative_rate_limits() {
    for field in ["rpm_limit", "tpm_limit", "key_rpm_limit", "key_tpm_limit"] {
        assert!(
            toml::from_str::<Config>(&format!("[[users]]\nname = 'bad'\n{field} = -1")).is_err()
        );
    }
}

#[tokio::test]
async fn dashboard_creates_edits_and_validates_rate_limits() {
    let cfg: Config = toml::from_str("[server]\ndatabase = 'sqlite::memory:'\nadmin_password = 'pw'\nadmin_password_env = 'LLMA_TEST_UNSET_VAR'").unwrap();
    let st = llm_accounting::init(&cfg, None).await.unwrap();
    db::create_session(&st.db, "test-session", 1).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = llm_accounting::router(st.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let post = |path: &str| {
        http.post(format!("{base}{path}"))
            .header("cookie", "llma_session=test-session")
    };
    let response = post("/admin/users")
        .form(&[
            ("name", "limited"),
            ("rpm_limit", "12"),
            ("tpm_limit", "1000"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 303);
    let users = db::list_users(&st.db).await.unwrap();
    let user = users[0].id;
    assert_eq!(
        (users[0].rpm_limit, users[0].tpm_limit),
        (Some(12), Some(1000))
    );
    let response = post("/admin/keys")
        .form(&[
            ("user_id", user.to_string()),
            ("rpm_limit", "3".into()),
            ("tpm_limit", "250".into()),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let html = response.text().await.unwrap();
    assert!(html.contains("name=\"rpm_limit\"") && html.contains("value=\"3\""));
    let keys = db::list_keys(&st.db).await.unwrap();
    let key = keys[0].id;
    assert_eq!((keys[0].rpm_limit, keys[0].tpm_limit), (Some(3), Some(250)));
    for path in [
        "/admin/users".to_owned(),
        "/admin/keys".to_owned(),
        format!("/admin/users/{user}"),
        format!("/admin/keys/{key}/limits"),
    ] {
        for invalid in ["-1", "1.5", "9223372036854775808", "words"] {
            let response = post(&path)
                .form(&[
                    ("name", "bad"),
                    ("user_id", "1"),
                    ("rpm_limit", invalid),
                    ("tpm_limit", ""),
                ])
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 400, "{path}: {invalid}");
        }
        let response = post(&path)
            .form(&[
                ("name", "bad"),
                ("user_id", "1"),
                ("rpm_limit", ""),
                ("tpm_limit", "-1"),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400, "{path}: invalid TPM");
    }
    let response = post(&format!("/admin/users/{user}"))
        .form(&[("rpm_limit", "0"), ("tpm_limit", "")])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 303);
    let users = db::list_users(&st.db).await.unwrap();
    assert_eq!((users[0].rpm_limit, users[0].tpm_limit), (Some(0), None));
    let response = post(&format!("/admin/keys/{key}/limits"))
        .form(&[("rpm_limit", ""), ("tpm_limit", "0")])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 303);
    let keys = db::list_keys(&st.db).await.unwrap();
    assert_eq!((keys[0].rpm_limit, keys[0].tpm_limit), (None, Some(0)));
    let html = http
        .get(format!("{base}/admin/users"))
        .header("cookie", "llma_session=test-session")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("RPM") && html.contains("TPM") && html.contains("value=\"0\""));
    server.abort();
}

#[tokio::test]
async fn seed_rate_limits_and_keep_dashboard_changes() {
    let db = db::connect("sqlite::memory:").await.unwrap();
    let cfg: Config = toml::from_str(
        r#"
        [[users]]
        name = "limited"
        rpm_limit = 12
        tpm_limit = 1000
        key_rpm_limit = 3
        key_tpm_limit = 250
        keys = ["seed-key"]
    "#,
    )
    .unwrap();
    db::seed(&db, &cfg).await.unwrap();
    let auth = db::lookup_key(&db, "seed-key").await.unwrap().unwrap();
    assert_eq!(
        auth.key_limits,
        Limits {
            rpm: Some(3),
            tpm: Some(250)
        }
    );
    assert_eq!(
        auth.user_limits,
        Limits {
            rpm: Some(12),
            tpm: Some(1000)
        }
    );
    db::update_user_with_limits(
        &db,
        auth.user_id,
        None,
        None,
        false,
        false,
        Limits::default(),
    )
    .await
    .unwrap();
    db::update_key_limits(&db, auth.key_id, Limits::default())
        .await
        .unwrap();
    db::seed(&db, &cfg).await.unwrap();
    let auth = db::lookup_key(&db, "seed-key").await.unwrap().unwrap();
    assert_eq!(auth.user_limits, Limits::default());
    assert_eq!(auth.key_limits, Limits::default());
}

#[tokio::test]
async fn created_limits_are_listed_and_unlimited_defaults_still_work() {
    let db = db::connect("sqlite::memory:").await.unwrap();
    let limits = Limits {
        rpm: Some(0),
        tpm: Some(99),
    };
    let user = db::create_user_with_limits(&db, "test", None, None, false, limits)
        .await
        .unwrap();
    let (_, key) = db::create_key_with_limits(&db, user, "limited", None, limits)
        .await
        .unwrap();
    let auth = db::lookup_key(&db, &key).await.unwrap().unwrap();
    assert_eq!(auth.key_limits, limits);
    assert_eq!(auth.user_limits, limits);
    let users = db::list_users(&db).await.unwrap();
    assert_eq!(
        (users[0].rpm_limit, users[0].tpm_limit),
        (Some(0), Some(99))
    );
    let keys = db::list_keys(&db).await.unwrap();
    assert_eq!((keys[0].rpm_limit, keys[0].tpm_limit), (Some(0), Some(99)));
    let unlimited = db::create_user(&db, "unlimited", None, None, false)
        .await
        .unwrap();
    let (_, key) = db::create_key(&db, unlimited, "", None).await.unwrap();
    let auth = db::lookup_key(&db, &key).await.unwrap().unwrap();
    assert_eq!(auth.key_limits, Limits::default());
    assert_eq!(auth.user_limits, Limits::default());
}
