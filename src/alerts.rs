//! Best-effort asynchronous webhook events. Secrets and response bodies never enter status text.
use crate::{budgets, db, settings, state::AppState};
use anyhow::{Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Clone, Debug, Serialize)]
pub struct Event {
    pub kind: String,
    pub subject: String,
    pub message: String,
    pub details: Value,
}
impl Event {
    pub fn new(kind: &str, subject: &str, message: &str) -> Self {
        Self {
            kind: kind.into(),
            subject: subject.into(),
            message: message.into(),
            details: json!({}),
        }
    }
}

#[derive(sqlx::FromRow)]
pub struct Webhook {
    pub id: i64,
    pub host: String,
    pub format: String,
    pub enabled: bool,
    pub last_status: String,
}
pub async fn list(db: &db::Db) -> Result<Vec<Webhook>> {
    Ok(
        sqlx::query_as("SELECT id,host,format,enabled,last_status FROM alert_webhooks ORDER BY id")
            .fetch_all(db)
            .await?,
    )
}
pub async fn thresholds(db: &db::Db) -> Result<String> {
    Ok(settings::value(db, "alert_thresholds")
        .await?
        .unwrap_or_else(|| "50,80,100".into()))
}
pub fn parse_thresholds(value: &str) -> Result<Vec<i64>> {
    let mut values = Vec::new();
    for value in value.split(',') {
        let n = value
            .trim()
            .parse::<i64>()
            .map_err(|_| anyhow::anyhow!("Enter comma-separated thresholds from 1 to 100."))?;
        ensure!((1..=100).contains(&n), "Enter thresholds from 1 to 100.");
        values.push(n);
    }
    values.sort_unstable();
    values.dedup();
    Ok(values)
}
pub async fn save(
    st: &AppState,
    id: Option<i64>,
    url: &str,
    format: &str,
    enabled: bool,
) -> Result<()> {
    ensure!(
        matches!(format, "generic" | "discord" | "slack"),
        "Choose JSON, Discord or Slack."
    );
    if url.trim().is_empty() {
        let id = id.ok_or_else(|| anyhow::anyhow!("Enter a webhook URL."))?;
        sqlx::query("UPDATE alert_webhooks SET format=?,enabled=? WHERE id=?")
            .bind(format)
            .bind(enabled)
            .bind(id)
            .execute(&st.db)
            .await?;
        return Ok(());
    }
    // Do not include the submitted URL in any parse error.
    let parsed = reqwest::Url::parse(url.trim())
        .map_err(|_| anyhow::anyhow!("Enter an HTTP or HTTPS webhook URL."))?;
    ensure!(
        matches!(parsed.scheme(), "https" | "http")
            && parsed.host_str().is_some()
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.fragment().is_none(),
        "Use an HTTP or HTTPS URL without user information or a fragment."
    );
    let host = parsed.host_str().unwrap();
    let secret = st.secret_key.encrypt(parsed.as_str())?;
    if let Some(id) = id {
        sqlx::query("UPDATE alert_webhooks SET url=?,host=?,format=?,enabled=? WHERE id=?")
            .bind(secret)
            .bind(host)
            .bind(format)
            .bind(enabled)
            .bind(id)
            .execute(&st.db)
            .await?;
    } else {
        sqlx::query("INSERT INTO alert_webhooks(url,host,format,enabled) VALUES(?,?,?,?)")
            .bind(secret)
            .bind(host)
            .bind(format)
            .bind(enabled)
            .execute(&st.db)
            .await?;
    }
    Ok(())
}

fn payload(event: &Event, format: &str) -> Value {
    // Discord and Slack have message length limits. Avoid splitting UTF-8.
    let message = event.message.chars().take(1900).collect::<String>();
    match format {
        "discord" => json!({"content":message,"allowed_mentions":{"parse":[]}}),
        "slack" => json!({"text":message,"mrkdwn":false}),
        _ => serde_json::to_value(event).expect("event JSON"),
    }
}

/// Queue an event. Callers never wait for HTTP delivery or receive delivery errors.
pub fn emit(st: &AppState, event: Event) {
    emit_to(st, event, None);
}
pub fn emit_to(st: &AppState, event: Event, id: Option<i64>) {
    let state = st.clone();
    st.spawn_tracked(async move {
        if let Err(e) = deliver(&state, &event, id).await {
            tracing::warn!(error=%e,"alert storage failed");
        }
    });
}
async fn deliver(st: &AppState, event: &Event, id: Option<i64>) -> Result<()> {
    let hooks: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT id,url,format FROM alert_webhooks WHERE enabled=1 AND (? IS NULL OR id=?)",
    )
    .bind(id)
    .bind(id)
    .fetch_all(&st.db)
    .await?;
    let client = st.egress.client(None, crate::egress::Purpose::Webhook);
    for (id, secret, format) in hooks {
        let mut status = "Stored URL could not be decrypted".to_owned();
        if let Ok(url) = st.secret_key.decrypt(&secret) {
            for attempt in 0..3 {
                let retry = match client
                    .post(&url)
                    .json(&payload(event, &format))
                    .send()
                    .await
                {
                    Ok(response) => {
                        let code = response.status();
                        status = format!("HTTP {}", code.as_u16());
                        if code.is_success() {
                            break;
                        }
                        code.is_server_error() || code.as_u16() == 429
                    }
                    Err(e) => {
                        status = if e.is_timeout() {
                            "Timeout (10 seconds)".into()
                        } else {
                            "Connection failed".into()
                        };
                        true
                    }
                };
                if !retry || attempt == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100 * (attempt + 1))).await;
            }
        }
        sqlx::query("UPDATE alert_webhooks SET last_status=?,last_delivery_at=? WHERE id=?")
            .bind(status)
            .bind(db::now_s())
            .bind(id)
            .execute(&st.db)
            .await?;
    }
    Ok(())
}

async fn first(st: &AppState, subject: &str, threshold: i64, start: i64) -> Result<bool> {
    Ok(sqlx::query(
        "INSERT OR IGNORE INTO alert_dedupe(subject,threshold,period_start) VALUES(?,?,?)",
    )
    .bind(subject)
    .bind(threshold)
    .bind(start)
    .execute(&st.db)
    .await?
    .rows_affected()
        == 1)
}
/// Check committed spend, never Reservations. Dedupe survives restarts.
pub async fn check_budgets(st: &AppState, auth: &db::KeyAuth, now: i64) -> Result<()> {
    if st.settings().mode == "simple" {
        return Ok(());
    }
    let thresholds = parse_thresholds(&thresholds(&st.db).await?)?;
    for b in budgets::applicable(&st.db, auth, now).await? {
        let limit = b.limit.unwrap();
        for threshold in &thresholds {
            if i128::from(b.spent) * 100 >= i128::from(limit) * i128::from(*threshold)
                && first(
                    st,
                    &format!("{}:{}", b.subject.name(), b.period),
                    *threshold,
                    b.start,
                )
                .await?
            {
                let mut event = Event::new(
                    "budget_threshold",
                    &b.subject.name(),
                    &format!(
                        "{} reached {}% of its {} Budget",
                        b.subject.name(),
                        threshold,
                        b.period
                    ),
                );
                event.details = json!({"threshold":threshold,"period":b.period,"period_start":b.start,"resets_at":b.end,"spent_micros":b.spent,"budget_micros":limit});
                emit(st, event);
            }
        }
    }
    Ok(())
}

pub async fn models_dev(st: &AppState) -> Result<()> {
    if st.settings().mode == "simple" {
        return Ok(());
    }
    let rows:Vec<(i64,i64,String,String,String)>=sqlx::query_as("SELECT a.id,a.model_id,m.name,a.old_spec,a.new_spec FROM models_dev_alerts a JOIN models m ON m.id=a.model_id WHERE NOT EXISTS(SELECT 1 FROM alert_dedupe d WHERE d.subject='models_dev:'||a.id AND d.threshold=0 AND d.period_start=0)").fetch_all(&st.db).await?;
    for (id, model, name, old, new) in rows {
        let subject = format!("models_dev:{id}");
        if first(st, &subject, 0, 0).await? {
            let mut event = Event::new(
                "model_price_change",
                &format!("model:{model}"),
                &format!("models.dev price alert for Model {name}"),
            );
            event.details = json!({"alert_id":id,"model_id":model,"model":name,"old":serde_json::from_str::<Value>(&old)?,"new":serde_json::from_str::<Value>(&new)?});
            emit(st, event);
        }
    }
    Ok(())
}
