//! Process-local adaptive routing. Scores and random state reset on restart.
use crate::{
    config::RouterCfg,
    routing::{Candidate, RouterTable},
};
use rand::{Rng, SeedableRng, rngs::StdRng};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const SUCCESS_PRIOR: f64 = 0.9;
const LATENCY_PRIOR: f64 = 1000.0;

#[derive(Clone, Copy)]
pub struct Pick {
    pub score: f64,
    pub exploration: bool,
}
#[derive(Clone, Copy)]
struct Sample {
    success: f64,
    latency: [f64; 2],
    last: Duration,
}
impl Sample {
    fn prior(now: Duration) -> Self {
        Self {
            success: SUCCESS_PRIOR,
            latency: [LATENCY_PRIOR; 2],
            last: now,
        }
    }
    fn decayed(mut self, now: Duration, cfg: &RouterCfg) -> Self {
        let periods =
            now.saturating_sub(self.last).as_secs_f64() / (cfg.adaptive_prior_decay_minutes * 60.0);
        // Keep fresh estimates unchanged. After one idle interval, decay to
        // the neutral prior with that same interval as the half-life.
        let retain = 2_f64.powf(-(periods - 1.0).max(0.0));
        self.success = SUCCESS_PRIOR + (self.success - SUCCESS_PRIOR) * retain;
        for l in &mut self.latency {
            *l = LATENCY_PRIOR + (*l - LATENCY_PRIOR) * retain;
        }
        self
    }
}
struct Inner {
    samples: HashMap<(String, String), Sample>,
    rng: StdRng,
    clock: Arc<dyn Fn() -> Duration + Send + Sync>,
}
pub struct Adaptive(Mutex<Inner>);
impl Default for Adaptive {
    fn default() -> Self {
        let start = Instant::now();
        Self(Mutex::new(Inner {
            samples: HashMap::new(),
            rng: StdRng::from_os_rng(),
            clock: Arc::new(move || start.elapsed()),
        }))
    }
}
impl Adaptive {
    /// Inject deterministic randomness and a monotonic clock before requests.
    pub fn set_sources(&self, seed: u64, clock: impl Fn() -> Duration + Send + Sync + 'static) {
        let mut s = self.0.lock().expect("adaptive state");
        s.rng = StdRng::seed_from_u64(seed);
        s.clock = Arc::new(clock);
        s.samples.clear();
    }
    pub fn now(&self) -> Duration {
        (self.0.lock().expect("adaptive state").clock)()
    }
    pub fn observe(
        &self,
        provider: &str,
        model: &str,
        healthy: bool,
        stream: bool,
        latency: f64,
        cfg: &RouterCfg,
    ) {
        let mut s = self.0.lock().expect("adaptive state");
        let now = (s.clock)();
        let sample = s
            .samples
            .entry((provider.into(), model.into()))
            .or_insert_with(|| Sample::prior(now));
        *sample = sample.decayed(now, cfg);
        let alpha = 1.0 - 2_f64.powf(-1.0 / cfg.adaptive_half_life);
        sample.success += alpha * (f64::from(healthy) - sample.success);
        if healthy && latency.is_finite() {
            let l = &mut sample.latency[usize::from(stream)];
            *l += alpha * (latency.max(0.1) - *l);
        }
        sample.last = now;
    }
    pub fn scores(
        &self,
        route: &crate::routing::Route,
        cfg: &RouterCfg,
        stream: bool,
    ) -> Vec<Score> {
        let s = self.0.lock().expect("adaptive state");
        let now = (s.clock)();
        route
            .deployments
            .iter()
            .map(|d| {
                let sample = s
                    .samples
                    .get(&(d.provider.clone(), d.model.clone()))
                    .copied()
                    .unwrap_or_else(|| Sample::prior(now))
                    .decayed(now, cfg);
                let latency = sample.latency[usize::from(stream)];
                Score {
                    provider: d.provider.clone(),
                    model: d.model.clone(),
                    success: sample.success,
                    latency,
                    score: f64::from(d.weight) * sample.success.powi(2) / latency,
                }
            })
            .collect()
    }
    pub fn order(
        &self,
        table: &RouterTable,
        candidates: &mut [Candidate],
        cfg: &RouterCfg,
        stream: bool,
        eligible: impl Fn(&Candidate) -> bool,
    ) {
        for route in table
            .routes
            .iter()
            .filter(|r| matches!(r.strategy, crate::config::Strategy::Adaptive))
        {
            let scores = self.scores(route, cfg, stream);
            let mut available: Vec<_> = candidates
                .iter()
                .enumerate()
                .filter(|(_, c)| c.route == route.name && eligible(c))
                .map(|(i, c)| {
                    let score = scores
                        .iter()
                        .find(|s| {
                            s.provider == c.provider.name && Some(&s.model) == c.model.as_ref()
                        })
                        .map_or(0.0, |s| s.score);
                    (i, score)
                })
                .collect();
            let slots: Vec<_> = available.iter().map(|(i, _)| *i).collect();
            let mut ordered = Vec::new();
            let mut s = self.0.lock().expect("adaptive state");
            let explore = s.rng.random::<f64>() < cfg.adaptive_epsilon;
            while !available.is_empty() {
                let exploration = explore && ordered.is_empty();
                let total: f64 = available.iter().map(|(_, score)| score).sum();
                let pos = if exploration || total <= 0.0 {
                    s.rng.random_range(0..available.len())
                } else {
                    let mut pick = s.rng.random::<f64>() * total;
                    available
                        .iter()
                        .position(|(_, score)| {
                            pick -= score;
                            pick < 0.0
                        })
                        .unwrap_or(available.len() - 1)
                };
                let (i, score) = available.remove(pos);
                let mut c = candidates[i].clone();
                c.adaptive = Some(Pick { score, exploration });
                ordered.push(c);
            }
            for (i, next) in slots.into_iter().zip(ordered) {
                candidates[i] = next;
            }
        }
        self.publish(table, cfg);
    }
    pub fn publish(&self, table: &RouterTable, cfg: &RouterCfg) {
        for route in table
            .routes
            .iter()
            .filter(|r| matches!(r.strategy, crate::config::Strategy::Adaptive))
        {
            for score in self.scores(route, cfg, false) {
                metrics::gauge!("tokomotive_deployment_score", "route" => route.name.clone(), "provider" => score.provider, "model" => score.model).set(score.score);
            }
        }
    }
}
pub struct Score {
    pub provider: String,
    pub model: String,
    pub success: f64,
    pub latency: f64,
    pub score: f64,
}
