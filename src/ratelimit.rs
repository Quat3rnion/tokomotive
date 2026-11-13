//! Sliding Rate Limits for one Proxy process.
//!
//! Call `admit` before forwarding with scopes `[Key(key_id), User(user_id)]`.
//! Record total tokens (all Token Classes) after the request finishes.
//! On rejection, respond with HTTP 429, type `rate_limit_error`, and a
//! `Retry-After` header from `retry_after_secs`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

const WINDOW_MS: i64 = 60_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Limits {
    pub rpm: Option<i64>,
    pub tpm: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    Key(i64),
    User(i64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limited {
    pub scope: Scope,
    pub which: &'static str,
    pub retry_after_secs: u64,
}

#[derive(Default)]
pub struct RateLimiter {
    inner: Mutex<Window>,
}

#[derive(Default)]
struct Window {
    // One bucket per millisecond, not per request: at most 60,000 buckets
    // per active scope. The sweep also removes scopes that are no longer used.
    scopes: HashMap<Scope, BTreeMap<i64, Counts>>,
    last_sweep: Option<i64>,
}

#[derive(Default)]
struct Counts {
    requests: i128,
    tokens: i128,
}

impl Window {
    fn prune(&mut self, now_ms: i64) {
        if self
            .last_sweep
            .is_none_or(|last| now_ms.saturating_sub(last) >= WINDOW_MS)
        {
            self.scopes.retain(|_, buckets| {
                buckets.retain(|&ts, _| ts > now_ms.saturating_sub(WINDOW_MS));
                !buckets.is_empty()
            });
            self.last_sweep = Some(now_ms);
        }
    }
}

fn retry_after(oldest_ms: Option<i64>, now_ms: i64) -> u64 {
    oldest_ms.map_or(1, |ts| {
        ((ts as i128 + WINDOW_MS as i128 - now_ms as i128).max(1) as u64).div_ceil(1000)
    })
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Admit one request in all scopes, or count nothing if any limit fails.
    /// Timestamps are milliseconds and must not decrease between calls.
    pub fn admit(&self, scopes: &[(Scope, Limits)], now_ms: i64) -> Result<(), Limited> {
        let mut window = self.inner.lock().expect("rate limiter mutex poisoned");
        window.prune(now_ms);
        for &(scope, limits) in scopes {
            let buckets = window.scopes.entry(scope).or_default();
            buckets.retain(|&ts, _| ts > now_ms.saturating_sub(WINDOW_MS));
            let requests: i128 = buckets.values().map(|b| b.requests).sum();
            if limits.rpm.is_some_and(|limit| requests >= limit as i128) {
                return Err(Limited {
                    scope,
                    which: "requests",
                    retry_after_secs: retry_after(
                        buckets
                            .iter()
                            .find(|(_, b)| b.requests > 0)
                            .map(|(&ts, _)| ts),
                        now_ms,
                    ),
                });
            }
            let tokens: i128 = buckets.values().map(|b| b.tokens).sum();
            if limits.tpm.is_some_and(|limit| tokens >= limit as i128) {
                return Err(Limited {
                    scope,
                    which: "tokens",
                    retry_after_secs: retry_after(
                        buckets
                            .iter()
                            .find(|(_, b)| b.tokens > 0)
                            .map(|(&ts, _)| ts),
                        now_ms,
                    ),
                });
            }
        }
        for &(scope, _) in scopes {
            window
                .scopes
                .entry(scope)
                .or_default()
                .entry(now_ms)
                .or_default()
                .requests += 1;
        }
        Ok(())
    }

    /// Record total completed tokens in each scope. Non-positive usage is ignored.
    pub fn record_tokens(&self, scopes: &[Scope], tokens: i64, now_ms: i64) {
        let mut window = self.inner.lock().expect("rate limiter mutex poisoned");
        window.prune(now_ms);
        if tokens <= 0 {
            return;
        }
        for &scope in scopes {
            let buckets = window.scopes.entry(scope).or_default();
            buckets.retain(|&ts, _| ts > now_ms.saturating_sub(WINDOW_MS));
            buckets.entry(now_ms).or_default().tokens += tokens as i128;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_slides_at_exact_millisecond_boundary() {
        let limiter = RateLimiter::new();
        let scopes = [(
            Scope::Key(1),
            Limits {
                rpm: Some(2),
                tpm: None,
            },
        )];
        assert_eq!(limiter.admit(&scopes, 123), Ok(()));
        assert_eq!(limiter.admit(&scopes, 1000), Ok(()));
        assert_eq!(
            limiter.admit(&scopes, 60_122).unwrap_err().retry_after_secs,
            1
        );
        assert_eq!(limiter.admit(&scopes, 60_123), Ok(()));
        assert_eq!(
            limiter.admit(&scopes, 60_124).unwrap_err().retry_after_secs,
            1
        );
        assert_eq!(limiter.admit(&scopes, 61_000), Ok(()));
    }

    #[test]
    fn admission_is_atomic_across_key_and_user() {
        let limiter = RateLimiter::new();
        let key = (
            Scope::Key(1),
            Limits {
                rpm: Some(1),
                tpm: None,
            },
        );
        let user = (
            Scope::User(1),
            Limits {
                rpm: None,
                tpm: Some(10),
            },
        );
        limiter.record_tokens(&[user.0], 10, 0);
        assert_eq!(limiter.admit(&[key, user], 1).unwrap_err().scope, user.0);
        assert_eq!(limiter.admit(&[key], 2), Ok(()));
        // The first failing scope is returned, even when both are limited.
        assert_eq!(limiter.admit(&[key, user], 3).unwrap_err().scope, key.0);
        let fresh = (
            Scope::User(2),
            Limits {
                rpm: Some(1),
                tpm: None,
            },
        );
        assert!(limiter.admit(&[fresh, key], 4).is_err());
        assert_eq!(limiter.admit(&[fresh], 5), Ok(()));
    }

    #[test]
    fn none_is_unlimited_and_zero_blocks_immediately() {
        let limiter = RateLimiter::new();
        let scopes = [
            (Scope::Key(1), Limits::default()),
            (Scope::User(1), Limits::default()),
        ];
        limiter.record_tokens(&[Scope::Key(1), Scope::User(1)], i64::MAX, 0);
        for now in 0..100 {
            assert_eq!(limiter.admit(&scopes, now), Ok(()));
        }
        for limits in [
            Limits {
                rpm: Some(0),
                tpm: None,
            },
            Limits {
                rpm: None,
                tpm: Some(0),
            },
        ] {
            assert_eq!(
                limiter
                    .admit(&[(Scope::Key(2), limits)], 100)
                    .unwrap_err()
                    .retry_after_secs,
                1
            );
        }
    }

    #[test]
    fn concurrent_admissions_share_one_limit() {
        let limiter = RateLimiter::new();
        std::thread::scope(|threads| {
            let joins: Vec<_> = (0..16)
                .map(|_| {
                    threads.spawn(|| {
                        limiter.admit(
                            &[(
                                Scope::Key(1),
                                Limits {
                                    rpm: Some(1),
                                    tpm: None,
                                },
                            )],
                            0,
                        )
                    })
                })
                .collect();
            assert_eq!(
                joins
                    .into_iter()
                    .filter_map(|j| j.join().unwrap().ok())
                    .count(),
                1
            );
        });
    }

    #[test]
    fn tpm_limit_uses_completed_tokens() {
        let limiter = RateLimiter::new();
        let scopes = [(
            Scope::User(1),
            Limits {
                rpm: None,
                tpm: Some(10),
            },
        )];
        assert_eq!(limiter.admit(&scopes, 0), Ok(()));
        limiter.record_tokens(&[Scope::User(1)], 9, 1000);
        assert_eq!(limiter.admit(&scopes, 1001), Ok(()));
        limiter.record_tokens(&[Scope::User(1)], 2, 2000);
        assert_eq!(
            limiter.admit(&scopes, 2001),
            Err(Limited {
                scope: Scope::User(1),
                which: "tokens",
                retry_after_secs: 59,
            })
        );
        assert_eq!(limiter.admit(&scopes, 61_000), Ok(()));
    }

    #[test]
    fn rpm_limit() {
        let limiter = RateLimiter::new();
        let scopes = [(
            Scope::Key(1),
            Limits {
                rpm: Some(1),
                tpm: None,
            },
        )];
        assert_eq!(limiter.admit(&scopes, 100), Ok(()));
        assert_eq!(
            limiter.admit(&scopes, 101),
            Err(Limited {
                scope: Scope::Key(1),
                which: "requests",
                retry_after_secs: 60,
            })
        );
    }
}
