//! Dashboard saves from htmx must navigate, not swap a page into #errors.

use tokomotive::config::Config;

async fn app() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir_in("/arboretum").unwrap();
    let mut cfg: Config = toml::from_str("").unwrap();
    cfg.server.database = format!("sqlite://{}/db.sqlite", dir.path().display());
    let st = tokomotive::init(&cfg, None).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = tokomotive::router(st);
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (dir, url)
}

#[tokio::test]
async fn htmx_save_answers_with_hx_redirect_and_plain_post_with_303() {
    let (_dir, url) = app().await;
    let no_follow = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    no_follow
        .post(format!("{url}/setup"))
        .form(&[("password", "long-password")])
        .send()
        .await
        .unwrap();
    let cookie: String = no_follow
        .post(format!("{url}/admin/login"))
        .form(&[("password", "long-password")])
        .send()
        .await
        .unwrap()
        .headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .into();
    // Follow redirects like a browser XHR does.
    let c = reqwest::Client::new();

    let r = c
        .post(format!("{url}/admin/providers"))
        .header("hx-request", "true")
        .header("cookie", &cookie)
        .form(&[
            ("name", "mock"),
            ("kind", "openai"),
            ("base_url", "http://127.0.0.1:9"),
            ("api_key", "sk-test-1234"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["hx-redirect"], "/admin/providers");
    assert!(
        r.text().await.unwrap().len() < 200,
        "htmx response must not carry a page"
    );

    let r = no_follow
        .post(format!("{url}/admin/providers"))
        .header("cookie", &cookie)
        .form(&[
            ("name", "mock2"),
            ("kind", "openai"),
            ("base_url", "http://127.0.0.1:9"),
            ("api_key", "sk-test-5678"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    assert_eq!(r.headers()["location"], "/admin/providers");
}
