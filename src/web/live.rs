//! Session-protected live Ledger rows.
use super::*;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::StreamExt;
use std::{convert::Infallible, time::Duration};

#[derive(Template)]
#[template(path = "ledger_row.html")]
struct Fragment {
    base: Base,
    r: LedgerView,
}
impl Fragment {
    fn columns(&self) -> usize {
        if self.base.simple { 8 } else { 10 }
    }
}

pub(super) async fn watch(
    _: Admin,
    State(st): State<AppState>,
    Query(q): Query<LedgerQ>,
    headers: HeaderMap,
) -> Response {
    let base = Base::new(&st);
    let (query, _, _) = match ledger_query(&st, &q, base.simple).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let cursor = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .or(q.cursor.as_deref());
    let session = session_cookie(&headers).expect("Admin checks the session");
    let Some(client) = st.ledger_live.client(db::hash_key(&session)) else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "15")],
            "Too many live Ledger clients. Close another live tab and try again.",
        )
            .into_response();
    };
    let (rx, replay, gap) = st.ledger_live.subscribe(cursor);
    let stream = futures_util::stream::unfold(
        (st, base, query, rx, replay, gap, client, session),
        |(st, mut base, query, mut rx, mut replay, gap, client, session)| async move {
            if gap {
                return Some((
                    Ok::<_, Infallible>(
                        Event::default().event("gap").data("Reload the first page."),
                    ),
                    (st, base, query, rx, replay, false, client, session),
                ));
            }
            loop {
                let n = match replay.pop_front() {
                    Some(n) => n,
                    None => match rx.recv().await {
                        Ok(n) => n,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            return Some((
                                Ok(Event::default().event("lagged").data(n.to_string())),
                                (st, base, query, rx, replay, false, client, session),
                            ));
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                    },
                };
                if !n.event.matches(&query) {
                    continue;
                }
                // A live tab must not retain access after logout, account changes
                // or session expiry. Never send another row on a stale session.
                if !matches!(crate::admin::session(&st.db, &session).await, Ok(Some(_))) {
                    return None;
                }
                base.simple = st.settings().mode == "simple";
                // Live rows use the Virtual Key label and Provider Key number.
                // Do not decrypt credentials, even to show their last characters.
                let r = LedgerView::build(&n.event.row, &|_, _| None);
                let html = match (Fragment {
                    base: base.clone(),
                    r,
                })
                .render()
                {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(error=%e, "Live Ledger row failed");
                        return None;
                    }
                };
                return Some((
                    Ok(Event::default().event("ledger").id(n.cursor).data(html)),
                    (st, base, query, rx, replay, false, client, session),
                ));
            }
        },
    );
    Sse::new(stream.take_until(tokio::time::sleep(Duration::from_secs(3600))))
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("heartbeat"),
        )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn response() -> (AppState, axum::body::BodyDataStream) {
        let cfg: crate::config::Config = toml::from_str(
            "[server]\ndatabase='sqlite::memory:'\n[[users]]\nname='alice'\nkeys=['key']",
        )
        .unwrap();
        let st = crate::init(&cfg, None).await.unwrap();
        sqlx::query("INSERT INTO admin_accounts(username,password_hash,role) VALUES('reader','test','viewer')").execute(&st.db).await.unwrap();
        let account = crate::admin::by_username(&st.db, "reader")
            .await
            .unwrap()
            .unwrap();
        crate::admin::create_session(&st.db, "token", 1, &account)
            .await
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, "tokomotive_session=token".parse().unwrap());
        let r = watch(
            Admin(account),
            State(st.clone()),
            Query(LedgerQ::default()),
            headers,
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        (st, r.into_body().into_data_stream())
    }

    #[tokio::test]
    async fn heartbeat_is_every_fifteen_seconds_and_lifetime_is_one_hour() {
        let (_st, mut body) = response().await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(14)).await;
        assert!(futures_util::poll!(body.next()).is_pending());
        tokio::time::advance(Duration::from_secs(1)).await;
        let beat = body.next().await.unwrap().unwrap();
        assert!(std::str::from_utf8(&beat).unwrap().contains("heartbeat"));
        tokio::time::advance(Duration::from_secs(3584)).await;
        assert!(
            body.next().await.is_some(),
            "stream must still exist before one hour"
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(
            body.next().await.is_none(),
            "one hour is the hard stream lifetime"
        );
    }

    #[tokio::test]
    async fn slow_client_gets_lagged_then_continues_with_bounded_history() {
        let (st, mut body) = response().await;
        let e: db::NewEntry = serde_json::from_value(serde_json::json!({
            "cache_hit":false,"ts_ms":1,"user_id":1,"key_id":1,"team_id":null,
            "provider":"test","subscription":false,"endpoint":"chat_completions",
            "requested_model":"route","model":"model","status":200,"streamed":false,
            "usage":crate::usage::Usage { input:1, output:2, ..Default::default() },"reported_cost_micros":null,"quote":crate::catalog::Quote::default(),
            "estimated":false,"latency_ms":1,"ttft_ms":null,"client_aborted":false,"provider_key":null,"attempts":1,"session_affinity":null
        })).unwrap();
        for _ in 0..1030 {
            db::insert_entry(&st.db, &e).await.unwrap();
        }
        let f = body.next().await.unwrap().unwrap();
        assert!(std::str::from_utf8(&f).unwrap().contains("event: lagged"));
        let f = body.next().await.unwrap().unwrap();
        assert!(std::str::from_utf8(&f).unwrap().contains("event: ledger"));
    }
}
