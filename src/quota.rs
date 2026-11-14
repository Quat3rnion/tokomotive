//! Last observed account quotas. This module does not participate in routing.
use axum::http::HeaderMap;
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};

#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    pub window: String,
    pub used_percent: Option<f64>,
    pub remaining: Option<f64>,
    pub limit: Option<f64>,
    pub reset_at: Option<i64>,
}

/// A stable opaque ID, not the secret or its position in a rotating pool.
pub fn key_id(key: &str) -> String {
    format!("key-{}", &hex::encode(Sha256::digest(key.as_bytes()))[..16])
}

#[derive(Debug)]
pub struct Snapshot {
    pub quota: Window,
    pub observed_at: i64,
}
#[derive(Debug)]
pub struct Account {
    pub provider: String,
    pub key: String,
    pub state: String,
    pub until: Option<i64>,
    pub observed_at: i64,
    pub last_failure: Option<String>,
    pub failure_at: Option<i64>,
    pub windows: Vec<Snapshot>,
}

pub fn retry_at(headers: &HeaderMap, now: i64) -> Option<i64> {
    text(headers, "retry-after")
        .and_then(|v| {
            v.parse::<i64>()
                .ok()
                .filter(|s| *s >= 0)
                .and_then(|s| now.checked_add(s))
                .and_then(timestamp)
                .or_else(|| {
                    chrono::DateTime::parse_from_rfc2822(v)
                        .ok()
                        .map(|d| d.timestamp())
                })
        })
        .or_else(|| {
            let windows = parse_headers(headers, now);
            // A healthy weekly window must not mask a short exhausted window.
            windows
                .iter()
                .filter(|w| w.used_percent == Some(100.0) || w.remaining == Some(0.0))
                .filter_map(|w| w.reset_at)
                .filter(|t| *t >= now)
                .max()
                .or_else(|| {
                    windows
                        .iter()
                        .filter_map(|w| w.reset_at)
                        .filter(|t| *t >= now)
                        .min()
                })
        })
}

/// Persist only static failure reasons. Never inspect or store upstream bodies.
pub async fn record(
    db: &SqlitePool,
    provider: &str,
    account: &str,
    status: u16,
    headers: &HeaderMap,
    now: i64,
) -> anyhow::Result<()> {
    let (state, reason, until) = match status {
        401 => (
            "credentials_invalid",
            Some("Upstream rejected credentials (401)"),
            None,
        ),
        403 => (
            "credentials_invalid",
            Some("Upstream denied access (403)"),
            None,
        ),
        429 => (
            "rate_limited",
            Some("Upstream rate limit (429)"),
            retry_at(headers, now),
        ),
        200..=299 => ("ok", None, None),
        // Other outcomes can carry quotas, but do not clear a known failure.
        _ => ("ok", Some("Upstream request failed"), None),
    };
    let mut tx = db.begin().await?;
    sqlx::query("INSERT INTO provider_health(provider,account,state,until_at,observed_at,last_failure,failure_at) VALUES(?,?,?,?,?,?,?) ON CONFLICT(provider,account) DO UPDATE SET state=CASE WHEN ? THEN provider_health.state ELSE excluded.state END,until_at=CASE WHEN ? THEN provider_health.until_at ELSE excluded.until_at END,observed_at=excluded.observed_at,last_failure=COALESCE(excluded.last_failure,provider_health.last_failure),failure_at=COALESCE(excluded.failure_at,provider_health.failure_at)")
        .bind(provider).bind(account).bind(state).bind(until).bind(now).bind(reason).bind(reason.map(|_|now))
        .bind(!matches!(status,200..=299|401|403|429)).bind(!matches!(status,200..=299|401|403|429)).execute(&mut *tx).await?;
    for window in parse_headers(headers, now) {
        save_window(&mut tx, provider, account, &window, now).await?;
    }
    tx.commit().await?;
    Ok(())
}
async fn save_window(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    provider: &str,
    account: &str,
    window: &Window,
    now: i64,
) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO provider_quota(provider,account,window,used_percent,remaining,quota_limit,reset_at,observed_at) VALUES(?,?,?,?,?,?,?,?) ON CONFLICT(provider,account,window) DO UPDATE SET used_percent=excluded.used_percent,remaining=excluded.remaining,quota_limit=excluded.quota_limit,reset_at=excluded.reset_at,observed_at=excluded.observed_at WHERE excluded.observed_at>=provider_quota.observed_at")
        .bind(provider).bind(account).bind(&window.window).bind(window.used_percent).bind(window.remaining).bind(window.limit).bind(window.reset_at).bind(now).execute(&mut **tx).await?;
    Ok(())
}

/// A plan endpoint updates quotas but must not clear inference failures.
pub async fn save_plan(
    db: &SqlitePool,
    provider: &str,
    account: &str,
    usage: &crate::oauth::PlanUsage,
    now: i64,
) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query("INSERT INTO provider_health(provider,account,state,observed_at) VALUES(?,?,'ok',?) ON CONFLICT(provider,account) DO NOTHING")
        .bind(provider).bind(account).bind(now).execute(&mut *tx).await?;
    for w in &usage.windows {
        save_window(
            &mut tx,
            provider,
            account,
            &Window {
                window: w.window.clone(),
                used_percent: Some(w.used_percent),
                remaining: w.remaining,
                limit: w.limit,
                reset_at: w.resets_at.map(|t| t.div_euclid(1000)),
            },
            now,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn accounts(
    db: &SqlitePool,
    provider: Option<&str>,
    limit: i64,
) -> anyhow::Result<Vec<Account>> {
    let rows = sqlx::query("SELECT * FROM provider_health WHERE (? IS NULL OR provider=?) ORDER BY provider,account LIMIT ?")
        .bind(provider).bind(provider).bind(limit.clamp(1,1000)).fetch_all(db).await?;
    let mut out = Vec::new();
    for row in rows {
        let provider: String = row.get("provider");
        let key: String = row.get("account");
        let windows = sqlx::query(
            "SELECT * FROM provider_quota WHERE provider=? AND account=? ORDER BY window",
        )
        .bind(&provider)
        .bind(&key)
        .fetch_all(db)
        .await?
        .into_iter()
        .map(|r| Snapshot {
            quota: Window {
                window: r.get("window"),
                used_percent: r.get("used_percent"),
                remaining: r.get("remaining"),
                limit: r.get("quota_limit"),
                reset_at: r.get("reset_at"),
            },
            observed_at: r.get("observed_at"),
        })
        .collect();
        out.push(Account {
            provider,
            key,
            state: row.get("state"),
            until: row.get("until_at"),
            observed_at: row.get("observed_at"),
            last_failure: row.get("last_failure"),
            failure_at: row.get("failure_at"),
            windows,
        });
    }
    Ok(out)
}

/// Inference must not fail when observability storage is unavailable.
pub async fn observe(
    db: &SqlitePool,
    provider: &crate::state::Provider,
    index: Option<usize>,
    result: &Result<reqwest::Response, reqwest::Error>,
) {
    if let Ok(response) = result {
        let account = if crate::oauth::Flow::from_auth(provider.auth).is_some() {
            "signin".into()
        } else {
            index
                .map(|i| key_id(&provider.keys[i]))
                .unwrap_or_else(|| "default".into())
        };
        if record(
            db,
            &provider.name,
            &account,
            response.status().as_u16(),
            response.headers(),
            chrono::Utc::now().timestamp(),
        )
        .await
        .is_err()
        {
            tracing::warn!("Could not store upstream quota snapshot");
        }
    }
}

/// Render persisted gauges on scrape, so they survive process restart and do
/// not retain obsolete label sets in the process-global recorder.
pub async fn metrics(db: &SqlitePool) -> anyhow::Result<String> {
    use std::fmt::Write;
    fn escape(s: &str) -> String {
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    }
    let mut out = String::from(
        "# TYPE tokomotive_provider_quota_used_ratio gauge\n# TYPE tokomotive_provider_quota_reset_seconds gauge\n# TYPE tokomotive_provider_health gauge\n",
    );
    // Streaming rows avoids an account-count cap that would hide valid gauges.
    use futures_util::TryStreamExt;
    let mut rows = sqlx::query("SELECT h.provider,h.account,h.state,q.window,q.used_percent,q.reset_at FROM provider_health h LEFT JOIN provider_quota q ON q.provider=h.provider AND q.account=h.account ORDER BY h.provider,h.account,q.window").fetch(db);
    let mut previous = None;
    while let Some(r) = rows.try_next().await? {
        let provider: String = r.get("provider");
        let key: String = r.get("account");
        let labels = format!(
            "provider=\"{}\",key=\"{}\"",
            escape(&provider),
            escape(&key)
        );
        if previous.as_ref() != Some(&labels) {
            let state: &str = r.get("state");
            for candidate in ["ok", "rate_limited", "credentials_invalid"] {
                writeln!(
                    out,
                    "tokomotive_provider_health{{{labels},state=\"{candidate}\"}} {}",
                    u8::from(state == candidate)
                )?;
            }
            previous = Some(labels.clone());
        }
        if let Some(window) = r.get::<Option<String>, _>("window") {
            let labels = format!("{labels},window=\"{}\"", escape(&window));
            if let Some(percent) = r.get::<Option<f64>, _>("used_percent") {
                writeln!(
                    out,
                    "tokomotive_provider_quota_used_ratio{{{labels}}} {}",
                    percent / 100.0
                )?;
            }
            if let Some(reset) = r.get::<Option<i64>, _>("reset_at") {
                writeln!(
                    out,
                    "tokomotive_provider_quota_reset_seconds{{{labels}}} {reset}"
                )?;
            }
        }
    }
    Ok(out)
}

fn text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}
fn number(headers: &HeaderMap, name: &str) -> Option<f64> {
    text(headers, name)?
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite() && *n >= 0.0)
}
fn timestamp(value: i64) -> Option<i64> {
    (value > 0 && chrono::DateTime::from_timestamp(value, 0).is_some()).then_some(value)
}
fn duration(value: &str) -> Option<i64> {
    if value.is_empty() {
        return None;
    }
    let mut total = 0.0;
    let mut rest = value;
    while !rest.is_empty() {
        let end = rest.find(|c: char| !c.is_ascii_digit() && c != '.')?;
        let n: f64 = rest[..end].parse().ok()?;
        rest = &rest[end..];
        let (unit, scale) = [
            ("ms", 0.001),
            ("s", 1.0),
            ("m", 60.0),
            ("h", 3600.0),
            ("d", 86400.0),
        ]
        .into_iter()
        .find(|(unit, _)| rest.starts_with(unit))?;
        total += n * scale;
        rest = &rest[unit.len()..];
    }
    (total.is_finite() && total >= 0.0 && total < i64::MAX as f64).then_some(total.ceil() as i64)
}
fn reset(value: &str, now: i64) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|d| d.timestamp())
        .or_else(|| value.parse::<i64>().ok().and_then(timestamp))
        .or_else(|| {
            duration(value)
                .and_then(|s| now.checked_add(s))
                .and_then(timestamp)
        })
}

/// Only fixed window names enter storage or metric labels.
pub fn parse_headers(headers: &HeaderMap, now: i64) -> Vec<Window> {
    let mut out = Vec::new();
    for window in ["requests", "tokens"] {
        let limit = number(headers, &format!("x-ratelimit-limit-{window}"));
        let remaining = number(headers, &format!("x-ratelimit-remaining-{window}"));
        let reset_at =
            text(headers, &format!("x-ratelimit-reset-{window}")).and_then(|v| reset(v, now));
        if limit.is_some() || remaining.is_some() || reset_at.is_some() {
            out.push(Window {
                window: window.into(),
                used_percent: limit
                    .zip(remaining)
                    .filter(|(l, _)| *l > 0.0)
                    .map(|(l, r)| ((l - r) / l * 100.0).clamp(0.0, 100.0)),
                remaining,
                limit,
                reset_at,
            });
        }
    }
    for window in ["requests", "tokens", "input-tokens", "output-tokens"] {
        let prefix = format!("anthropic-ratelimit-{window}");
        let limit = number(headers, &format!("{prefix}-limit"));
        let remaining = number(headers, &format!("{prefix}-remaining"));
        let reset_at = text(headers, &format!("{prefix}-reset")).and_then(|v| reset(v, now));
        if limit.is_some() || remaining.is_some() || reset_at.is_some() {
            out.push(Window {
                window: window.into(),
                used_percent: limit
                    .zip(remaining)
                    .filter(|(l, _)| *l > 0.0)
                    .map(|(l, r)| ((l - r) / l * 100.0).clamp(0.0, 100.0)),
                remaining,
                limit,
                reset_at,
            });
        }
    }
    // Observed Claude subscription names and fractional utilization semantics:
    // https://github.com/router-for-me/CLIProxyAPI/blob/main/internal/runtime/executor/helps/claude_ratelimit.go
    // Status is an enum, never free text. Only rejected implies exhaustion.
    for window in ["5h", "7d", "7d_oi", "overage", "unified"] {
        let prefix = if window == "unified" {
            "anthropic-ratelimit-unified".into()
        } else {
            format!("anthropic-ratelimit-unified-{window}")
        };
        let status = text(headers, &format!("{prefix}-status"));
        let used_percent = number(headers, &format!("{prefix}-utilization"))
            .map(|n| (n * 100.0).clamp(0.0, 100.0))
            .or_else(|| (status == Some("rejected")).then_some(100.0));
        let reset_at = text(headers, &format!("{prefix}-reset")).and_then(|v| reset(v, now));
        if used_percent.is_some()
            || reset_at.is_some()
            || matches!(status, Some("allowed" | "allowed_warning" | "rejected"))
        {
            out.push(Window {
                window: window.into(),
                used_percent,
                remaining: None,
                limit: None,
                reset_at,
            });
        }
    }
    // Codex primary/secondary header contract (percent, Unix reset or seconds):
    // https://github.com/router-for-me/CLIProxyAPI/blob/main/internal/runtime/executor/helps/codex_quota.go
    // Keep primary/secondary stable even when window-minutes changes with the plan.
    for window in ["primary", "secondary"] {
        let prefix = format!("x-codex-{window}");
        let used_percent =
            number(headers, &format!("{prefix}-used-percent")).map(|n| n.clamp(0.0, 100.0));
        let reset_at = text(headers, &format!("{prefix}-reset-at"))
            .and_then(|v| reset(v, now))
            .or_else(|| {
                text(headers, &format!("{prefix}-reset-after-seconds"))?
                    .parse::<i64>()
                    .ok()
                    .filter(|s| *s >= 0)
                    .and_then(|s| now.checked_add(s))
                    .and_then(timestamp)
            });
        if used_percent.is_some() || reset_at.is_some() {
            out.push(Window {
                window: format!("codex-{window}"),
                used_percent,
                remaining: None,
                limit: None,
                reset_at,
            });
        }
    }
    out
}
