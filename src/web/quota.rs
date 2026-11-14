use super::views::{Stamp, stamp_opt};
use crate::quota;

pub struct AccountView {
    pub provider: String,
    pub key: String,
    pub state: String,
    pub reason: String,
    pub until: Option<Stamp>,
    pub skipped: bool,
    pub failure: Option<String>,
    pub failure_at: Option<Stamp>,
    pub windows: Vec<WindowView>,
}
pub struct WindowView {
    pub name: String,
    pub percent: Option<f64>,
    pub text: String,
    pub counts: String,
    pub reset: Option<Stamp>,
    pub relative: String,
    pub observed: Option<Stamp>,
}
pub async fn views(
    st: &crate::state::AppState,
    provider: Option<&str>,
    limit: i64,
) -> anyhow::Result<Vec<AccountView>> {
    let now = chrono::Utc::now().timestamp();
    let settings = st.settings();
    Ok(quota::accounts(&st.db, provider, limit)
        .await?
        .into_iter()
        .map(|a| {
            let skip = a.skip(
                now,
                settings.router.quota_exhausted_percent,
                settings.router.cooldown_secs,
            );
            AccountView {
                provider: a.provider,
                key: a.key,
                reason: match a.state.as_str() {
                    "rate_limited" => "Upstream rate limit",
                    "credentials_invalid" => "Upstream rejected credentials",
                    _ => "Last request succeeded",
                }
                .into(),
                state: a.state,
                until: skip.and_then(|s| s.until).and_then(stamp_opt),
                skipped: skip.is_some(),
                failure: a.last_failure,
                failure_at: a.failure_at.and_then(stamp_opt),
                windows: a
                    .windows
                    .into_iter()
                    .map(|s| {
                        let w = s.quota;
                        let seconds = w.reset_at.unwrap_or(now).saturating_sub(now).max(0);
                        WindowView {
                            name: w.window,
                            percent: w.used_percent,
                            text: w
                                .used_percent
                                .map(|p| format!("{p:.0}% used"))
                                .unwrap_or_else(|| "Usage not reported".into()),
                            counts: match (w.remaining, w.limit) {
                                (Some(r), Some(l)) => format!("{r} remaining of {l}"),
                                (Some(r), None) => format!("{r} remaining"),
                                _ => String::new(),
                            },
                            reset: w.reset_at.and_then(stamp_opt),
                            relative: if seconds == 0 {
                                "reset time reached".into()
                            } else if seconds < 60 {
                                format!("resets in {seconds}s")
                            } else if seconds < 3600 {
                                format!("resets in {}m", (seconds + 59) / 60)
                            } else {
                                format!("resets in {}h {}m", seconds / 3600, (seconds % 3600) / 60)
                            },
                            observed: stamp_opt(s.observed_at),
                        }
                    })
                    .collect(),
            }
        })
        .collect())
}
