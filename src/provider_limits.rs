//! Provider overrides. Missing values inherit the live Server defaults.
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub request_timeout_ms: Option<u64>,
    pub connect_timeout_ms: Option<u64>,
    pub stream_idle_timeout_ms: Option<u64>,
    pub max_concurrent: Option<usize>,
    pub max_wait_ms: Option<u64>,
    pub max_retries: Option<usize>,
    pub backoff_base_ms: Option<u64>,
    pub backoff_max_ms: Option<u64>,
    pub backoff_jitter: Option<bool>,
}

/// A resizable admission semaphore. A lower limit does not cancel holders or
/// replace the semaphore: releases pay down the active count before admission.
#[derive(Debug, Default)]
pub struct Gate {
    counts: Mutex<(usize, usize)>,
    changed: Notify,
}
impl Gate {
    pub fn resize(&self, limit: usize) {
        self.counts.lock().expect("provider gate").1 = limit;
        self.changed.notify_waiters();
    }
    pub async fn acquire(self: &Arc<Self>, provider: &str, wait_ms: u64) -> Option<Permit> {
        let acquire = async {
            let mut waited = false;
            loop {
                let notified = self.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                {
                    let mut counts = self.counts.lock().expect("provider gate");
                    if counts.1 == 0 || counts.0 < counts.1 {
                        counts.0 += 1;
                        metrics::gauge!("tokomotive_provider_inflight", "provider" => provider.to_owned()).increment(1.0);
                        return Some(Permit {
                            gate: self.clone(),
                            provider: provider.to_owned(),
                        });
                    }
                }
                if wait_ms == 0 {
                    return None;
                }
                if !waited {
                    metrics::counter!("tokomotive_provider_limited_total", "provider" => provider.to_owned(), "kind" => "wait").increment(1);
                    waited = true;
                }
                notified.await;
            }
        };
        let duration = std::time::Duration::from_millis(wait_ms);
        match tokio::time::timeout(duration, acquire).await.ok().flatten() {
            Some(permit) => Some(permit),
            None => {
                metrics::counter!("tokomotive_provider_limited_total", "provider" => provider.to_owned(), "kind" => if wait_ms == 0 { "rejected" } else { "wait_timeout" }).increment(1);
                None
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("request_timeout")]
pub(crate) struct RequestTimeout;

#[derive(Debug)]
pub struct Permit {
    gate: Arc<Gate>,
    provider: String,
}
impl Drop for Permit {
    fn drop(&mut self) {
        self.gate.counts.lock().expect("provider gate").0 -= 1;
        metrics::gauge!("tokomotive_provider_inflight", "provider" => self.provider.clone())
            .decrement(1.0);
        self.gate.changed.notify_waiters();
    }
}

impl Limits {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.request_timeout_ms != Some(0),
            "Request timeout must be 1 ms or more."
        );
        for n in [
            self.request_timeout_ms,
            self.connect_timeout_ms,
            self.stream_idle_timeout_ms,
            self.max_wait_ms,
            self.backoff_base_ms,
            self.backoff_max_ms,
        ]
        .into_iter()
        .flatten()
        {
            anyhow::ensure!(
                n <= 86_400_000,
                "Timeouts, waits and backoff must not exceed 86400000 ms (one day)."
            );
        }
        anyhow::ensure!(
            self.max_retries.is_none_or(|n| n <= 10_000),
            "Maximum Provider retries must not exceed 10000."
        );
        anyhow::ensure!(
            self.max_concurrent.is_none_or(|n| n <= 1_000_000),
            "Maximum concurrent requests must not exceed 1000000."
        );
        Ok(())
    }
    pub(crate) async fn backoff(&self, retry: usize) {
        let max = self.backoff_max_ms.unwrap_or(30_000);
        let delay = self
            .backoff_base_ms
            .unwrap_or(0)
            .saturating_mul(
                1u64.checked_shl(retry.saturating_sub(1).min(63) as u32)
                    .unwrap_or(u64::MAX),
            )
            .min(max);
        let delay = if self.backoff_jitter.unwrap_or(false) && delay > 0 {
            use rand::Rng;
            rand::rng().random_range(0..=delay)
        } else {
            delay
        };
        if delay > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }
    }
    pub fn inherit(&self, defaults: &Self) -> Self {
        Self {
            request_timeout_ms: self.request_timeout_ms.or(defaults.request_timeout_ms),
            connect_timeout_ms: self.connect_timeout_ms.or(defaults.connect_timeout_ms),
            stream_idle_timeout_ms: self
                .stream_idle_timeout_ms
                .or(defaults.stream_idle_timeout_ms),
            max_concurrent: self.max_concurrent.or(defaults.max_concurrent),
            max_wait_ms: self.max_wait_ms.or(defaults.max_wait_ms),
            max_retries: self.max_retries.or(defaults.max_retries),
            backoff_base_ms: self.backoff_base_ms.or(defaults.backoff_base_ms),
            backoff_max_ms: self.backoff_max_ms.or(defaults.backoff_max_ms),
            backoff_jitter: self.backoff_jitter.or(defaults.backoff_jitter),
        }
    }
}

pub(crate) async fn next_chunk<S: futures_util::Stream + Unpin>(
    stream: &mut S,
    idle_ms: Option<u64>,
) -> Result<Option<S::Item>, tokio::time::error::Elapsed> {
    use futures_util::StreamExt;
    match idle_ms.filter(|v| *v > 0) {
        Some(ms) => tokio::time::timeout(std::time::Duration::from_millis(ms), stream.next()).await,
        None => Ok(stream.next().await),
    }
}
