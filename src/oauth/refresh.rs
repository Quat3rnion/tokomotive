//! Persisted refresh recovery. No issuer text or credentials enter diagnostics.
use super::*;

const RETRYING: &str = "Sign-in refresh is retrying";
pub(super) const UNKNOWN: &str = "Refresh token state is unknown; sign in again";
#[derive(Debug)]
pub struct RefreshRetrying;
impl std::fmt::Display for RefreshRetrying {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(RETRYING)
    }
}
impl std::error::Error for RefreshRetrying {}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct RefreshState {
    pub next_refresh_at: Option<i64>,
    pub next_retry_at: Option<i64>,
    pub last_error: Option<String>,
    attempts: u32,
    // The credential retains the old refresh token until the replacement is saved.
    pub pending: bool,
    retry_used: bool,
    in_doubt_since: Option<i64>,
    in_doubt_attempts: u32,
}
enum Failure {
    Transient,
    Ambiguous,
    Rejected,
    AlreadyUsed,
    Invalid,
}

impl OAuthManager {
    pub(super) async fn refresh_event(&self, name: &str, flow: Flow, recovered: bool) {
        let mut event = crate::alerts::Event::new(
            if recovered {
                "oauth_recovered"
            } else {
                "oauth_disconnected"
            },
            name,
            if recovered {
                "Sign-in refresh recovered"
            } else {
                "Sign-in disconnected; sign in again"
            },
        );
        event.details = json!({"flow":flow.as_str()});
        self.refresh_events
            .lock()
            .await
            .insert((name.to_owned(), flow, recovered), event);
    }
    pub async fn emit_refresh_events(&self, st: &crate::state::AppState) {
        for (_, event) in self.refresh_events.lock().await.drain() {
            crate::alerts::emit(st, event);
        }
    }
    /// A manual request shares the same flight lock and never bypasses backoff.
    pub async fn refresh_now(&self, name: &str, flow: Flow) -> Result<()> {
        let saved = self.load(name, flow).await?;
        let token = saved
            .credential
            .as_ref()
            .context("Subscription disconnected; sign in again")?
            .access_token
            .clone();
        self.token(name, flow, Some(&token)).await?;
        Ok(())
    }
    /// One periodic pass. The injected manager clock controls eligibility.
    /// At most four token endpoint calls run at once, also across slow Providers.
    pub async fn refresh_due_credentials(&self) -> Result<()> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT provider,flow FROM oauth_credentials ORDER BY provider,flow")
                .fetch_all(&self.db)
                .await?;
        let mut work = tokio::task::JoinSet::new();
        for (name, id) in rows {
            let Some(flow) = [
                Flow::Chatgpt,
                Flow::Claude,
                Flow::Codex,
                Flow::HuggingFace,
                Flow::Entra,
                Flow::GoogleVertex,
                Flow::GithubCopilot,
                Flow::QwenCode,
                Flow::KimiCode,
                Flow::GeminiCli,
            ]
            .into_iter()
            .find(|f| f.as_str() == id) else {
                continue;
            };
            let saved = self.load(&name, flow).await?;
            if let Some(c) = &saved.credential {
                self.expiry_metric(&name, flow, c.expires_at);
            }
            if saved.last_error.is_some()
                || saved.credential.is_none()
                || !self.refresh_due(flow, &saved)
                || saved
                    .refresh
                    .next_retry_at
                    .is_some_and(|at| at > self.now())
                    && !self.doubt_exhausted(&saved)
                || (saved.service_account.is_none()
                    && self.config(flow, &saved)?.refresh == RefreshRule::None)
            {
                continue;
            }
            if work.len() == 4 {
                work.join_next().await;
            }
            let auth = self.clone();
            work.spawn(async move {
                let _ = auth.token(&name, flow, None).await;
            });
        }
        while work.join_next().await.is_some() {}
        Ok(())
    }
    pub(super) fn expiry_metric(&self, name: &str, flow: Flow, expiry: i64) {
        metrics::gauge!("tokomotive_oauth_access_expiry_seconds", "provider" => name.to_owned(), "flow" => flow.as_str())
            .set(((expiry.saturating_sub(self.now())).max(0) / 1000) as f64);
    }
    fn refresh_metric(name: &str, flow: Flow, result: &'static str) {
        metrics::counter!("tokomotive_oauth_refresh_total", "provider" => name.to_owned(), "flow" => flow.as_str(), "result" => result).increment(1);
        tracing::info!(
            provider = name,
            flow = flow.as_str(),
            result,
            "OAuth refresh finished"
        );
    }
    pub(super) fn refresh_due(&self, flow: Flow, saved: &Saved) -> bool {
        saved.refresh.pending
            || saved.refresh.next_retry_at.is_some()
            || saved.credential.as_ref().is_some_and(|c| {
                saved.refresh.next_refresh_at.unwrap_or_else(|| {
                    let due = c.expires_at.saturating_sub(600_000);
                    if Self::idle_keepalive(flow, saved) {
                        due.min(c.last_refresh.unwrap_or(0).saturating_add(30 * 86_400_000))
                    } else {
                        due
                    }
                }) <= self.now()
            })
    }
    pub(super) fn doubt_exhausted(&self, saved: &Saved) -> bool {
        saved.refresh.pending
            && (saved.refresh.in_doubt_attempts >= 20
                || saved
                    .refresh
                    .in_doubt_since
                    .is_some_and(|since| self.now().saturating_sub(since) >= 24 * 60 * 60 * 1000))
    }
    fn idle_keepalive(flow: Flow, saved: &Saved) -> bool {
        matches!(flow, Flow::GeminiCli | Flow::Entra)
            || (flow == Flow::GoogleVertex && saved.service_account.is_none())
    }
    pub(super) fn schedule_refresh(&self, flow: Flow, saved: &mut Saved) {
        if let Some(c) = &saved.credential {
            let life = c.expires_at.saturating_sub(self.now());
            if c.expires_at == i64::MAX {
                saved.refresh.next_refresh_at = None;
                return;
            }
            let mut delay = (life / 5 * 4).min(life.saturating_sub(600_000)).max(0);
            if Self::idle_keepalive(flow, saved) {
                delay = delay.min(30 * 86_400_000);
            }
            // Refresh early, never beyond the 80%/ten-minute boundary.
            let jitter = i64::from(rand::rng().next_u32() % 30_001).min(delay);
            saved.refresh.next_refresh_at = Some(self.now() + delay - jitter);
        }
    }
    pub(super) fn usable_or_retrying(
        &self,
        saved: &Saved,
        rejected: Option<&str>,
    ) -> Result<String> {
        if let Some(c) = &saved.credential
            && rejected.is_none()
            && c.expires_at > self.now()
        {
            return Ok(c.access_token.clone());
        }
        Err(RefreshRetrying.into())
    }
    pub(super) async fn refresh_disconnect<T>(
        &self,
        name: &str,
        flow: Flow,
        saved: &mut Saved,
        message: &'static str,
    ) -> Result<T> {
        saved.last_error = Some(message.into());
        saved.refresh.pending = false;
        saved.refresh.next_retry_at = None;
        saved.refresh.next_refresh_at = None;
        Self::refresh_metric(name, flow, "disconnected");
        self.save(name, flow, saved).await?;
        self.refresh_event(name, flow, false).await;
        bail!(message)
    }
    pub(super) async fn refresh_succeeded(
        &self,
        name: &str,
        flow: Flow,
        saved: &mut Saved,
    ) -> Result<()> {
        let recovered = saved.refresh.last_error.is_some() || saved.last_error.is_some();
        saved.refresh = RefreshState::default();
        self.schedule_refresh(flow, saved);
        self.save(name, flow, saved).await?;
        if recovered {
            self.refresh_event(name, flow, true).await;
        }
        Self::refresh_metric(name, flow, "success");
        if let Some(c) = &saved.credential {
            self.expiry_metric(name, flow, c.expires_at);
        }
        Ok(())
    }
    async fn refresh_response(
        &self,
        name: &str,
        config: &FlowConfig,
        body: Value,
    ) -> std::result::Result<Tokens, Failure> {
        let req = self
            .oauth_request(name, config, &config.token, body)
            .await
            .map_err(|_| Failure::Transient)?;
        let response = req
            .timeout(self.refresh_timeout)
            .send()
            .await
            .map_err(|e| {
                self.egress.connect_error(name, &e);
                if e.is_connect() {
                    Failure::Transient
                } else {
                    Failure::Ambiguous
                }
            })?;
        let status = response.status();
        if status.is_server_error() || status.as_u16() == 429 {
            return Err(Failure::Transient);
        }
        let value = bounded_json(response).await.map_err(|_| {
            if status.is_success() {
                Failure::Invalid
            } else {
                Failure::Transient
            }
        })?;
        if !status.is_success() {
            if matches!(status.as_u16(), 400 | 401) {
                let code = value["error"]
                    .as_str()
                    .or_else(|| value["error"]["code"].as_str())
                    .unwrap_or_default();
                if matches!(code, "refresh_token_reused" | "refresh_token_already_used") {
                    return Err(Failure::AlreadyUsed);
                }
                if matches!(
                    code,
                    "invalid_grant" | "invalid_token" | "unauthorized_client"
                ) {
                    let description = value["error_description"]
                        .as_str()
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    return Err(
                        if description.contains("already used")
                            || description.contains("already been used")
                            || description.contains("reuse")
                        {
                            Failure::AlreadyUsed
                        } else {
                            Failure::Rejected
                        },
                    );
                }
            }
            return Err(Failure::Transient);
        }
        serde_json::from_value(value).map_err(|_| Failure::Invalid)
    }
    pub(super) async fn refresh_tokens(
        &self,
        name: &str,
        flow: Flow,
        config: &FlowConfig,
        saved: &mut Saved,
        mut body: Value,
    ) -> Result<Option<Tokens>> {
        // Codex can omit a replacement but can issue single-use rotating tokens.
        let rotating = config.refresh == RefreshRule::Rotating || flow == Flow::Codex;
        let retry = saved.refresh.pending;
        if rotating {
            if retry && saved.refresh.retry_used {
                return self.refresh_disconnect(name, flow, saved, UNKNOWN).await;
            }
            saved.refresh.pending = true;
            saved.refresh.retry_used = retry;
            if !retry {
                saved.refresh.in_doubt_since = Some(self.now());
                saved.refresh.in_doubt_attempts = 0;
            }
            saved.refresh.in_doubt_since.get_or_insert(self.now());
            saved.refresh.in_doubt_attempts = saved.refresh.in_doubt_attempts.saturating_add(1);
            // Save before send. A restart can make one recovery attempt, not a replay loop.
            self.save(name, flow, saved).await?;
        }
        if flow != Flow::Chatgpt {
            body.as_object_mut()
                .expect("token object")
                .remove("resource");
        }
        let response = self
            .refresh_response(name, config, body)
            .await
            .and_then(|t| {
                Self::check_tokens(flow, &t).map_err(|_| Failure::Invalid)?;
                Ok(t)
            });
        match response {
            Ok(t) => Ok(Some(t)),
            Err(Failure::Rejected) => {
                self.refresh_disconnect(name, flow, saved, "Refresh token rejected; sign in again")
                    .await
            }
            Err(Failure::AlreadyUsed) => {
                self.refresh_disconnect(
                    name,
                    flow,
                    saved,
                    "Refresh token was already used; sign in again",
                )
                .await
            }
            Err(Failure::Invalid) if rotating => {
                self.refresh_disconnect(
                    name,
                    flow,
                    saved,
                    "Invalid refresh response; sign in again",
                )
                .await
            }
            Err(failure) => {
                let ambiguous =
                    matches!(failure, Failure::Ambiguous | Failure::Invalid) && rotating;
                if ambiguous && retry {
                    return self.refresh_disconnect(name, flow, saved, UNKNOWN).await;
                }
                saved.refresh.pending = ambiguous || retry;
                if !saved.refresh.pending {
                    saved.refresh.in_doubt_since = None;
                    saved.refresh.in_doubt_attempts = 0;
                }
                if self.doubt_exhausted(saved) {
                    return self.refresh_disconnect(name, flow, saved, UNKNOWN).await;
                }
                // An explicit transient response did not consume the recovery attempt.
                saved.refresh.retry_used = false;
                saved.refresh.attempts = saved.refresh.attempts.saturating_add(1);
                let delay = (30_000_i64
                    * (1_i64 << saved.refresh.attempts.min(6).saturating_sub(1)))
                .min(900_000);
                let jitter = i64::from(rand::rng().next_u32() % 15_001);
                saved.refresh.next_retry_at = Some(
                    self.now()
                        + if delay == 900_000 {
                            delay - jitter
                        } else {
                            delay + jitter
                        },
                );
                saved.refresh.last_error = Some(RETRYING.into());
                Self::refresh_metric(name, flow, if ambiguous { "in_doubt" } else { "transient" });
                self.save(name, flow, saved).await?;
                Ok(None)
            }
        }
    }
}

// The periodic worker is added through the same token single-flight path.
pub fn start_background(st: crate::state::AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let state = st.clone();
            let (done, wait) = tokio::sync::oneshot::channel();
            st.spawn_tracked(async move {
                if state.oauth.refresh_due_credentials().await.is_err() {
                    tracing::warn!("OAuth refresh scan failed");
                }
                state.oauth.emit_refresh_events(&state).await;
                crate::quota::emit_health_events(&state).await;
                let _ = done.send(());
            });
            let _ = wait.await;
        }
    })
}
