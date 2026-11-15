//! Route selection and deployment health. State is local to one process.

use rand::Rng;
use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::catalog::Catalog;
use crate::config::{Config, Strategy};
use crate::provider::Endpoint;
use crate::state::{AppState, Provider};

#[derive(Clone)]
pub struct RouterTable {
    pub providers: Vec<Provider>,
    pub routes: Vec<Route>,
    pub settings: RouterSettings,
}

#[derive(Clone)]
pub struct RouterSettings {
    pub unknown_model_route: Option<String>,
    pub failure_threshold: usize,
    pub cooldown_secs: u64,
    pub max_attempts: usize,
    pub session_affinity_ttl_secs: u64,
    pub session_affinity_capacity: usize,
}

#[derive(Clone)]
pub struct Route {
    pub name: String,
    pub cache: crate::cache::Policy,
    pub strategy: Strategy,
    pub session_affinity: bool,
    pub deployments: Vec<Deployment>,
    pub fallbacks: Vec<String>,
}

#[derive(Clone)]
pub struct Deployment {
    pub provider: String,
    pub model: String,
    pub weight: u32,
}

impl RouterTable {
    /// Model discovery must not advance route rotation or weighted selection.
    pub fn reachable_providers(&self, catalog: &Catalog, name: &str) -> Vec<&Provider> {
        let mut names = Vec::new();
        if let Some(route) = self.routes.iter().find(|r| r.name == name) {
            names.extend(route.deployments.iter().map(|d| d.provider.as_str()));
            for fallback in &route.fallbacks {
                if let Some(route) = self.routes.iter().find(|r| &r.name == fallback) {
                    names.extend(route.deployments.iter().map(|d| d.provider.as_str()));
                } else if let Some(model) = catalog.resolve(fallback) {
                    names.push(&model.provider);
                }
            }
        } else if let Some(model) = catalog.resolve(name) {
            names.push(&model.provider);
        }
        self.providers
            .iter()
            .filter(|p| names.contains(&p.name.as_str()))
            .collect()
    }
    pub fn allowed(&self, catalog: &Catalog, allowed: &[String], model: Option<&str>) -> bool {
        let access = crate::resolve::Access {
            models: allowed,
            providers: &[],
        };
        let name = model.unwrap_or_default();
        crate::resolve::resolve(
            self,
            catalog,
            &crate::resolve::Index::default(),
            name,
            access,
        )
        .allowed(access, name, catalog)
    }

    pub fn from_config(cfg: &Config) -> Self {
        Self {
            providers: cfg
                .providers
                .iter()
                .map(|p| {
                    let keys = p.keys();
                    if keys.is_empty() && p.auth == crate::provider::ProviderAuth::ApiKey {
                        tracing::warn!(provider = %p.name, "no API key is set for this provider");
                    }
                    let mut provider = Provider::new(&p.name, p.kind, &p.base_url, keys);
                    provider.limits = p.limits.clone();
                    provider.auth = p.auth;
                    provider.default_model = p.default_model.clone();
                    provider.priority = p.priority;
                    provider.key_strategy = p.key_strategy;
                    provider.project_id = p.project_id.clone();
                    provider.region = p.region.clone();
                    provider.include_usage = p.include_usage;
                    provider.key_header = p
                        .key_header
                        .unwrap_or_else(|| crate::provider::KeyHeader::for_kind(p.kind));
                    provider
                })
                .collect(),
            routes: cfg
                .routes
                .iter()
                .map(|r| Route {
                    name: r.name.clone(),
                    cache: r.cache.clone(),
                    strategy: r.strategy,
                    session_affinity: r.session_affinity,
                    deployments: r
                        .deployments
                        .iter()
                        .map(|d| Deployment {
                            provider: d.provider.clone(),
                            model: d.model.clone(),
                            weight: d.weight,
                        })
                        .collect(),
                    fallbacks: r.fallbacks.clone(),
                })
                .collect(),
            settings: RouterSettings {
                unknown_model_route: cfg.router.unknown_model_route.clone(),
                failure_threshold: cfg.router.failure_threshold,
                cooldown_secs: cfg.router.cooldown_secs,
                max_attempts: cfg.router.max_attempts,
                session_affinity_ttl_secs: cfg.router.session_affinity_ttl_secs,
                session_affinity_capacity: cfg.router.session_affinity_capacity,
            },
        }
    }
}

#[derive(Clone)]
pub struct Candidate {
    pub provider: Provider,
    pub model: Option<String>,
    pub rewrite: bool,
    pub route: String,
}

impl Candidate {
    fn key(&self) -> (String, String) {
        (
            self.provider.name.clone(),
            self.model.clone().unwrap_or_default(),
        )
    }
}

#[derive(Default)]
pub(crate) struct Circuit {
    failures: usize,
    opened: Option<Instant>,
    probing: bool,
    probe_generation: u64,
}

impl Circuit {
    fn available(&self, now: Instant, cooldown: Duration) -> bool {
        !self.probing
            && self
                .opened
                .is_none_or(|t| now.duration_since(t) >= cooldown)
    }

    fn acquire(&mut self, now: Instant, cooldown: Duration, degraded: bool) -> bool {
        if self.probing || (!degraded && !self.available(now, cooldown)) {
            return false;
        }
        if self.opened.is_some() {
            self.probing = true;
            self.probe_generation = self.probe_generation.wrapping_add(1);
        }
        true
    }

    /// Returns true when this outcome opens or reopens the circuit.
    fn record(&mut self, healthy: bool, now: Instant, threshold: usize) -> bool {
        if healthy {
            self.failures = 0;
            self.opened = None;
            self.probing = false;
            return false;
        }
        self.failures = self.failures.saturating_add(1);
        if self.probing || self.failures >= threshold {
            self.opened = Some(now);
            self.probing = false;
            self.probe_generation = self.probe_generation.wrapping_add(1);
            return true;
        }
        false
    }
}

/// Releases an unfinished half-open probe if the request is cancelled.
pub struct DeploymentPermit {
    st: AppState,
    key: (String, String),
    threshold: usize,
    generation: u64,
    probe_generation: Option<u64>,
    single_probe: bool,
}

impl DeploymentPermit {
    pub fn is_probe(&self) -> bool {
        self.single_probe
    }

    pub fn record(&self, healthy: bool) {
        let mut circuits = self.st.circuits.lock().expect("circuits");
        let circuit = circuits.entry(self.key.clone()).or_default();
        if circuit.probe_generation == self.generation
            && circuit.record(healthy, Instant::now(), self.threshold)
        {
            metrics::counter!("llm_circuit_opens_total", "provider" => self.key.0.clone(), "model" => self.key.1.clone()).increment(1);
        }
    }
}

impl Drop for DeploymentPermit {
    fn drop(&mut self) {
        if let Some(c) = self
            .st
            .circuits
            .lock()
            .expect("circuits")
            .get_mut(&self.key)
            && self.probe_generation == Some(c.probe_generation)
        {
            c.probing = false;
        }
    }
}

impl AppState {
    fn route_deployments(&self, table: &RouterTable, route: &Route) -> Vec<Candidate> {
        let mut order: Vec<_> = route.deployments.iter().collect();
        if !order.is_empty() {
            match route.strategy {
                Strategy::Priority | Strategy::Quota => {}
                Strategy::RoundRobin => {
                    let mut next = self.route_next.lock().expect("route rotation");
                    let n = next.entry(route.name.clone()).or_default();
                    let start = *n % order.len();
                    *n = n.wrapping_add(1);
                    order.rotate_left(start);
                }
                Strategy::Weighted => {
                    order.sort_by_key(|d| std::cmp::Reverse(d.weight));
                    let total: u64 = order.iter().map(|d| u64::from(d.weight)).sum();
                    if total > 0 {
                        let mut pick = rand::rng().random_range(0..total);
                        let first = order
                            .iter()
                            .position(|d| {
                                let w = u64::from(d.weight);
                                if pick < w {
                                    true
                                } else {
                                    pick -= w;
                                    false
                                }
                            })
                            .expect("weighted deployment");
                        let chosen = order.remove(first);
                        order.insert(0, chosen);
                    }
                }
            }
        }
        order
            .into_iter()
            .filter_map(|d| {
                Some(Candidate {
                    provider: table
                        .providers
                        .iter()
                        .find(|p| p.name == d.provider)?
                        .clone(),
                    model: Some(d.model.clone()),
                    rewrite: true,
                    route: route.name.clone(),
                })
            })
            .collect()
    }

    /// Resolve explicit routes first, then catalog models. Fallbacks are one
    /// level deep and inherit the caller's permission to use the primary route.
    pub fn candidates(
        &self,
        table: &RouterTable,
        catalog: &Catalog,
        model: Option<&str>,
        _endpoint: Endpoint,
        _anthropic_hint: bool,
    ) -> Vec<Candidate> {
        let resolution = crate::resolve::resolve(
            table,
            catalog,
            &self.resolution_index.read().expect("resolution index"),
            model.unwrap_or_default(),
            crate::resolve::Access::default(),
        );
        self.resolved_candidates(table, catalog, &resolution)
    }

    pub fn resolved_candidates(
        &self,
        table: &RouterTable,
        catalog: &Catalog,
        resolution: &crate::resolve::Resolution,
    ) -> Vec<Candidate> {
        let mut out = Vec::new();
        if let Some(crate::resolve::Target::Route(name)) = &resolution.target {
            let Some(route) = table.routes.iter().find(|r| &r.name == name) else {
                return out;
            };
            out.extend(self.route_deployments(table, route));
            for name in &route.fallbacks {
                if name == &route.name {
                    continue;
                }
                if let Some(fallback) = table.routes.iter().find(|r| r.name == *name) {
                    out.extend(self.route_deployments(table, fallback));
                } else if let Some(m) = catalog.resolve(name) {
                    if let Some(provider) = table.providers.iter().find(|p| p.name == m.provider) {
                        out.push(Candidate {
                            provider: provider.clone(),
                            model: Some(m.name.clone()),
                            rewrite: true,
                            route: name.clone(),
                        });
                    }
                } else {
                    tracing::warn!(route = %route.name, fallback = %name, "unknown fallback; skipped");
                }
            }
        } else if let Some(crate::resolve::Target::Model { provider, model }) = &resolution.target
            && let Some(provider) = table.providers.iter().find(|p| &p.name == provider)
        {
            out.push(Candidate {
                provider: provider.clone(),
                model: Some(model.clone()),
                rewrite: true,
                route: resolution.canonical.clone(),
            });
        }
        let mut seen = HashSet::new();
        out.retain(|c| seen.insert(c.key()));
        out
    }

    pub fn deployment_available(&self, candidate: &Candidate, settings: &RouterSettings) -> bool {
        self.circuits
            .lock()
            .expect("circuits")
            .get(&candidate.key())
            .is_none_or(|c| {
                c.available(Instant::now(), Duration::from_secs(settings.cooldown_secs))
            })
    }

    pub fn acquire_deployment(
        &self,
        candidate: &Candidate,
        settings: &RouterSettings,
        degraded: bool,
    ) -> Option<DeploymentPermit> {
        let key = candidate.key();
        let mut circuits = self.circuits.lock().expect("circuits");
        let c = circuits.entry(key.clone()).or_default();
        if !c.acquire(
            Instant::now(),
            Duration::from_secs(settings.cooldown_secs),
            degraded,
        ) {
            return None;
        }
        Some(DeploymentPermit {
            st: self.clone(),
            key,
            threshold: settings.failure_threshold,
            generation: c.probe_generation,
            probe_generation: c.probing.then_some(c.probe_generation),
            single_probe: c.probing && !degraded,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderKind;
    use std::time::{Duration, Instant};

    #[test]
    fn circuit_cooldown_allows_one_probe_and_success_closes() {
        let now = Instant::now();
        let cooldown = Duration::from_secs(30);
        let mut c = Circuit::default();
        assert!(c.acquire(now, cooldown, false));
        assert!(!c.record(false, now, 2));
        assert!(c.acquire(now, cooldown, false));
        assert!(c.record(false, now, 2));
        assert!(!c.acquire(now, cooldown, false));
        let later = now + cooldown;
        assert!(c.acquire(later, cooldown, false));
        assert!(!c.acquire(later, cooldown, false));
        assert!(c.record(false, later, 2));
        assert!(!c.acquire(later, cooldown, false));
        assert!(c.acquire(later + cooldown, cooldown, false));
        assert!(!c.record(true, later + cooldown, 2));
        assert!(c.acquire(later + cooldown, cooldown, false));
    }

    #[tokio::test]
    async fn stale_success_cannot_clear_a_new_half_open_probe() {
        let cfg: Config = toml::from_str("[server]\ndatabase = 'sqlite::memory:'").unwrap();
        let st = crate::init(&cfg, None).await.unwrap();
        let candidate = Candidate {
            provider: Provider::new("test", ProviderKind::Openai, "http://localhost", vec![]),
            model: Some("test".into()),
            rewrite: true,
            route: "test".into(),
        };
        let settings = RouterSettings {
            unknown_model_route: None,
            failure_threshold: 1,
            cooldown_secs: 30,
            max_attempts: 8,
            session_affinity_ttl_secs: 1800,
            session_affinity_capacity: 100_000,
        };
        let old = st.acquire_deployment(&candidate, &settings, false).unwrap();
        let failed = st.acquire_deployment(&candidate, &settings, false).unwrap();
        failed.record(false);
        // Advance the circuit's clock without waiting for the cooldown.
        st.circuits
            .lock()
            .unwrap()
            .get_mut(&candidate.key())
            .unwrap()
            .opened = Some(Instant::now() - Duration::from_secs(31));
        let probe = st.acquire_deployment(&candidate, &settings, false).unwrap();
        old.record(true);
        assert!(
            st.acquire_deployment(&candidate, &settings, false)
                .is_none(),
            "stale success admitted traffic beside the active probe"
        );
        probe.record(false);
        old.record(true);
        assert!(
            !st.deployment_available(&candidate, &settings),
            "stale success closed the newer open circuit"
        );
    }

    #[tokio::test]
    async fn old_permit_cannot_release_another_requests_half_open_probe() {
        let cfg: Config = toml::from_str("[server]\ndatabase = 'sqlite::memory:'").unwrap();
        let st = crate::init(&cfg, None).await.unwrap();
        let candidate = Candidate {
            provider: Provider::new("test", ProviderKind::Openai, "http://localhost", vec![]),
            model: Some("test".into()),
            rewrite: true,
            route: "test".into(),
        };
        let settings = RouterSettings {
            unknown_model_route: None,
            failure_threshold: 1,
            cooldown_secs: 0,
            max_attempts: 8,
            session_affinity_ttl_secs: 1800,
            session_affinity_capacity: 100_000,
        };
        let old = st.acquire_deployment(&candidate, &settings, false).unwrap();
        let failed = st.acquire_deployment(&candidate, &settings, false).unwrap();
        failed.record(false);
        let probe = st.acquire_deployment(&candidate, &settings, false).unwrap();
        drop(old);
        drop(failed);
        assert!(st.acquire_deployment(&candidate, &settings, true).is_none());
        // Cancellation releases only this probe, so a later request can try.
        drop(probe);
        assert!(
            st.acquire_deployment(&candidate, &settings, false)
                .is_some()
        );
    }
}
