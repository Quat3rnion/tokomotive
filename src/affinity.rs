//! Process-local session pins. Only scoped SHA-256 digests are retained.
use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    routing::{Candidate, RouterTable},
    state::AppState,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct SessionKey {
    route: String,
    digest: [u8; 32],
}
type DeploymentId = (String, String);
struct Pin {
    deployment: DeploymentId,
    account: String,
    used: Instant,
}
#[derive(Default)]
pub(crate) struct AffinityTable {
    pins: HashMap<SessionKey, Pin>,
    lru: BTreeSet<(Instant, SessionKey)>,
}
impl AffinityTable {
    pub(crate) fn configure(&mut self, table: &RouterTable) {
        let enabled: HashSet<_> = table
            .routes
            .iter()
            .filter(|r| {
                r.session_affinity
                    && (r.deployments.len() > 1
                        || r.deployments.iter().any(|d| {
                            table
                                .providers
                                .iter()
                                .any(|p| p.name == d.provider && p.keys.len() > 1)
                        }))
            })
            .map(|r| r.name.as_str())
            .collect();
        self.pins
            .retain(|key, _| enabled.contains(key.route.as_str()));
        self.lru.retain(|(_, key)| self.pins.contains_key(key));
        self.prune(
            Instant::now(),
            Duration::from_secs(table.settings.session_affinity_ttl_secs),
            table.settings.session_affinity_capacity,
        );
    }
    fn prune(&mut self, now: Instant, ttl: Duration, capacity: usize) {
        while let Some((used, _)) = self.lru.first() {
            if self.pins.len() <= capacity && now.saturating_duration_since(*used) < ttl {
                break;
            }
            let (_, key) = self.lru.pop_first().expect("oldest session");
            self.pins.remove(&key);
        }
    }
    fn get(&mut self, key: &SessionKey, now: Instant) -> Option<(DeploymentId, String)> {
        let pin = self.pins.get_mut(key)?;
        self.lru.remove(&(pin.used, key.clone()));
        pin.used = now;
        self.lru.insert((now, key.clone()));
        Some((pin.deployment.clone(), pin.account.clone()))
    }
    fn insert(
        &mut self,
        key: SessionKey,
        deployment: DeploymentId,
        account: String,
        now: Instant,
        ttl: Duration,
        capacity: usize,
    ) {
        if let Some(old) = self.pins.insert(
            key.clone(),
            Pin {
                deployment,
                account,
                used: now,
            },
        ) {
            self.lru.remove(&(old.used, key.clone()));
        }
        self.lru.insert((now, key));
        self.prune(now, ttl, capacity);
    }
}

fn digest(value: &Value) -> [u8; 32] {
    Sha256::digest(serde_json::to_vec(value).expect("JSON value")).into()
}

/// Adjacent text parts are joined without changing whitespace. Image data is
/// reduced to a SHA-256 digest; image URLs stay URLs. Other parts do not form
/// the first user prompt (for example tool results and thinking blocks).
fn content(value: &Value) -> Value {
    if let Some(text) = value.as_str() {
        return json!([text]);
    }
    let mut parts = Vec::new();
    let mut text = String::new();
    for part in value.as_array().into_iter().flatten() {
        if let Some(s) = part.get("text").and_then(Value::as_str) {
            text.push_str(s);
        } else if matches!(
            part["type"].as_str(),
            Some("image" | "image_url" | "input_image")
        ) {
            if !text.is_empty() {
                parts.push(json!(std::mem::take(&mut text)));
            }
            let source = part.get("source");
            let url = part["image_url"]
                .as_str()
                .or_else(|| part["image_url"]["url"].as_str())
                .or_else(|| source.and_then(|s| s["url"].as_str()));
            let image = if let Some(url) = url.filter(|u| !u.starts_with("data:")) {
                json!({"url":url})
            } else {
                json!({"sha256":hex::encode(digest(&json!([url, source])))})
            };
            parts.push(json!({"image":image}));
        }
    }
    if !text.is_empty() || parts.is_empty() {
        parts.push(json!(text));
    }
    json!(parts)
}

fn session_key(
    route: &str,
    user: i64,
    headers: &HeaderMap,
    body: Option<&Value>,
) -> Option<SessionKey> {
    let explicit = [
        headers.get("x-session-id").and_then(|h| h.to_str().ok()),
        body.and_then(|b| b["prompt_cache_key"].as_str()),
        body.and_then(|b| b["metadata"]["user_id"].as_str()),
        body.and_then(|b| b["user"].as_str()),
    ]
    .into_iter()
    .flatten()
    .find(|s| !s.is_empty());
    let source = if let Some(key) = explicit {
        json!(["explicit", key])
    } else {
        let b = body?;
        let mut system = vec![];
        for field in ["system", "instructions"] {
            if let Some(v) = b.get(field) {
                system.push(content(v));
            }
        }
        let messages = b.get("messages").or_else(|| b.get("input"));
        let first = if let Some(s) = messages.and_then(Value::as_str) {
            content(&json!(s))
        } else {
            let messages = messages?.as_array()?;
            // Only initial instructions belong to the stable prompt prefix.
            for m in messages.iter().take_while(|m| m["role"] != "user") {
                if matches!(m["role"].as_str(), Some("system" | "developer")) {
                    system.push(content(&m["content"]));
                }
            }
            content(&messages.iter().find(|m| m["role"] == "user")?["content"])
        };
        json!(["prefix", b["model"], system, first])
    };
    Some(SessionKey {
        route: route.into(),
        digest: digest(&json!(["tokomotive-session-v1", route, user, source])),
    })
}

fn id(candidate: &Candidate) -> DeploymentId {
    (
        candidate.provider.name.clone(),
        candidate.model.clone().unwrap_or_default(),
    )
}

/// Weighted rendezvous: an exponential race, smallest -ln(U)/weight wins.
/// Hash each identity independently so adding a Deployment changes no pair's order.
fn score(key: &SessionKey, deployment: &DeploymentId, weight: u32) -> f64 {
    let bytes = digest(&json!([
        "tokomotive-rendezvous-v1",
        hex::encode(key.digest),
        deployment
    ]));
    let n = u64::from_be_bytes(bytes[..8].try_into().expect("eight bytes")) >> 12;
    let uniform = (n as f64 + 0.5) / (1u64 << 52) as f64;
    -uniform.ln() / f64::from(weight.max(1))
}

pub(crate) struct Session {
    key: SessionKey,
    pinned: Option<DeploymentId>,
    first: DeploymentId,
    account: Option<String>,
    can_repin: bool,
}

impl Session {
    pub(crate) fn order_keys(
        &self,
        provider: &crate::state::Provider,
        order: &mut [Option<usize>],
    ) {
        if self.pinned.as_ref().is_some_and(|p| p.0 == provider.name)
            && let Some(position) = order.iter().position(|i| {
                self.account.as_ref() == Some(&crate::quota::account_id(provider, *i))
            })
        {
            order[..=position].rotate_right(1);
        }
    }
    pub(crate) fn unavailable(&mut self, provider: &crate::state::Provider, index: Option<usize>) {
        if self.pinned.as_ref().is_some_and(|p| p.0 == provider.name)
            && self.account.as_ref() == Some(&crate::quota::account_id(provider, index))
        {
            self.can_repin = true;
        }
    }
}

impl AppState {
    /// Called only after Provider Access and protocol compatibility filtering.
    pub(crate) fn session_order(
        &self,
        table: &RouterTable,
        model: Option<&str>,
        user: i64,
        headers: &HeaderMap,
        body: Option<&Value>,
        candidates: &mut [Candidate],
    ) -> Option<Session> {
        let route = table
            .routes
            .iter()
            .find(|r| Some(r.name.as_str()) == model)?;
        if !route.session_affinity
            || (route.deployments.len() < 2 && candidates.iter().all(|c| c.provider.keys.len() < 2))
        {
            return None;
        }
        let key = session_key(&route.name, user, headers, body)?;
        // Fallbacks stay behind the primary Route, with their existing order.
        let primary = candidates
            .iter()
            .take_while(|c| c.route == route.name)
            .count();
        let weight = |c: &Candidate| {
            route
                .deployments
                .iter()
                .find(|d| {
                    d.provider == c.provider.name && Some(d.model.as_str()) == c.model.as_deref()
                })
                .map_or(1, |d| d.weight)
        };
        if !matches!(route.strategy, crate::config::Strategy::Quota) {
            candidates[..primary].sort_by(|a, b| {
                score(&key, &id(a), weight(a))
                    .total_cmp(&score(&key, &id(b), weight(b)))
                    .then_with(|| id(a).cmp(&id(b)))
            });
        }
        let now = Instant::now();
        let mut pins = self.affinity.lock().expect("session affinity");
        pins.prune(
            now,
            Duration::from_secs(table.settings.session_affinity_ttl_secs),
            table.settings.session_affinity_capacity,
        );
        let (pinned, account) = pins
            .get(&key, now)
            .map_or((None, None), |(d, a)| (Some(d), Some(a)));
        // Removed Deployments and revoked Provider Access cannot retain a pin.
        let can_repin = pinned.is_some() && !candidates.iter().any(|c| Some(id(c)) == pinned);
        if let Some(position) = candidates.iter().position(|c| Some(id(c)) == pinned) {
            candidates[..=position].rotate_right(1);
        }
        Some(Session {
            key,
            pinned,
            first: id(candidates.first()?),
            account,
            can_repin,
        })
    }

    pub(crate) fn pin_session(
        &self,
        table: &RouterTable,
        session: &Session,
        candidate: &Candidate,
        index: Option<usize>,
    ) -> &'static str {
        let deployment = id(candidate);
        let account = crate::quota::account_id(&candidate.provider, index);
        if session.pinned.is_some()
            && !session.can_repin
            && (session.pinned.as_ref() != Some(&deployment)
                || session.account.as_ref() != Some(&account))
        {
            return "pinned";
        }
        let result = match &session.pinned {
            Some(old) if old == &deployment && session.account.as_ref() == Some(&account) => "hit",
            Some(_) => "repinned",
            None if session.first == deployment => "new",
            None => "repinned",
        };
        self.affinity.lock().expect("session affinity").insert(
            session.key.clone(),
            deployment,
            account,
            Instant::now(),
            Duration::from_secs(table.settings.session_affinity_ttl_secs),
            table.settings.session_affinity_capacity,
        );
        metrics::counter!("llm_session_affinity_total", "route" => session.key.route.clone(), "result" => result).increment(1);
        match result {
            "hit" => "pinned",
            "repinned" => "re-pinned",
            _ => "new",
        }
    }

    pub fn active_sessions(&self) -> HashMap<String, usize> {
        let table = self.router_table();
        let mut pins = self.affinity.lock().expect("session affinity");
        pins.configure(&table);
        let mut counts = HashMap::new();
        for key in pins.pins.keys() {
            *counts.entry(key.route.clone()).or_default() += 1;
        }
        counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(body: Value) -> SessionKey {
        session_key("halogen", 1, &HeaderMap::new(), Some(&body)).unwrap()
    }

    #[test]
    fn empty_explicit_sources_do_not_hide_a_later_key() {
        let b =
            json!({"prompt_cache_key":"", "metadata":{"user_id":"conversation"}, "user":"ignored"});
        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", "".parse().unwrap());
        assert_eq!(
            session_key("halogen", 1, &headers, Some(&b)),
            Some(key(json!({"user":"conversation"})))
        );
    }

    #[test]
    fn explicit_source_order_is_header_cache_key_metadata_then_user() {
        let b =
            json!({"prompt_cache_key":"cache", "metadata":{"user_id":"metadata"}, "user":"user"});
        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", "header".parse().unwrap());
        assert_eq!(
            session_key("halogen", 1, &headers, Some(&b)),
            Some(key(json!({"user":"header"})))
        );
        assert_eq!(key(b), key(json!({"user":"cache"})));
        assert_eq!(
            key(json!({"metadata":{"user_id":"metadata"},"user":"user"})),
            key(json!({"user":"metadata"}))
        );
        // SHA-256 reference from Python hashlib, with compact JSON framing.
        assert_eq!(
            hex::encode(key(json!({"user":"conversation"})).digest),
            "66f848cf533bae27cea6c5b04646dcdc4125b0af6d4e398fa771e7b75a810d7a"
        );
    }

    #[test]
    fn prefix_joins_text_parts_keeps_initial_instructions_and_ignores_later_turns() {
        let b = json!({"model":"halogen", "messages":[{"role":"system","content":"Help"},{"role":"user","content":"Hello there"}]});
        let mut parts = b.clone();
        parts["messages"][1]["content"] =
            json!([{"type":"text","text":"Hello "},{"type":"text","text":"there"}]);
        parts["messages"].as_array_mut().unwrap().extend([
            json!({"role":"assistant","content":"Hi"}),
            json!({"role":"user","content":"Next turn"}),
        ]);
        assert_eq!(key(b.clone()), key(parts));
        for field in ["model", "system", "instructions"] {
            let mut changed = b.clone();
            changed[field] = json!("different");
            assert_ne!(key(b.clone()), key(changed));
        }
        let mut changed = b.clone();
        changed["messages"][1]["content"] = json!("Different first user");
        assert_ne!(key(b), key(changed));
    }

    #[test]
    fn responses_and_anthropic_client_bodies_have_stable_prefixes() {
        for mut b in [
            json!({"model":"halogen","instructions":"Help","input":[{"role":"user","content":[{"type":"input_text","text":"Hi"}]}]}),
            json!({"model":"halogen","system":[{"type":"text","text":"Help"}],"messages":[{"role":"user","content":[{"type":"text","text":"Hi"}]}]}),
        ] {
            let first = key(b.clone());
            let messages = if b.get("input").is_some() {
                "input"
            } else {
                "messages"
            };
            b[messages]
                .as_array_mut()
                .unwrap()
                .push(json!({"role":"user","content":"Second turn"}));
            assert_eq!(first, key(b));
        }
        assert_eq!(
            key(json!({"model":"halogen","instructions":"Help","input":"Hi"})),
            key(
                json!({"model":"halogen","instructions":"Help","input":[{"role":"user","content":"Hi"}]})
            )
        );
        assert!(
            session_key(
                "halogen",
                1,
                &HeaderMap::new(),
                Some(&json!({"model":"halogen"}))
            )
            .is_none()
        );
    }

    #[test]
    fn images_use_urls_or_data_hashes_not_volatile_image_options() {
        let a =
            json!([{"type":"image_url","image_url":{"url":"https://image.test/a","detail":"low"}}]);
        let b = json!([{"type":"image_url","image_url":{"url":"https://image.test/a","detail":"high"}}]);
        assert_eq!(content(&a), content(&b));
        assert_ne!(
            content(&a),
            content(&json!([{"type":"input_image","image_url":"https://image.test/b"}]))
        );
        let image = content(
            &json!([{"type":"image","source":{"type":"base64","media_type":"image/png","data":"private-image-data"}}]),
        );
        assert!(image.to_string().contains("sha256"));
        assert!(!image.to_string().contains("private-image-data"));
    }

    #[test]
    fn session_hashes_are_scoped_to_both_route_and_user() {
        let b = json!({"user":"same-client-key"});
        let headers = HeaderMap::new();
        let first = session_key("route-a", 1, &headers, Some(&b));
        assert_ne!(first, session_key("route-b", 1, &headers, Some(&b)));
        assert_ne!(first, session_key("route-a", 2, &headers, Some(&b)));
    }

    #[test]
    fn rendezvous_is_weighted_and_membership_only_moves_necessary_sessions() {
        let deployments = [
            ("a".into(), "model".into()),
            ("b".into(), "model".into()),
            ("c".into(), "model".into()),
        ];
        let weights = [1, 3, 1];
        let mut heavy = 0;
        let mut moved = 0;
        for n in 0..10_000 {
            let key = key(json!({"user":format!("s{n}")}));
            let mut order = [0, 1, 2];
            order.sort_by(|&a, &b| {
                score(&key, &deployments[a], weights[a]).total_cmp(&score(
                    &key,
                    &deployments[b],
                    weights[b],
                ))
            });
            let old = if score(&key, &deployments[0], 1) < score(&key, &deployments[1], 3) {
                0
            } else {
                1
            };
            heavy += usize::from(old == 1);
            if old != order[0] {
                assert_eq!(order[0], 2);
                moved += 1;
            }
            // Removing c restores the prior winner without moving a to b.
            assert_eq!(*order.iter().find(|&&n| n != 2).unwrap(), old);
        }
        assert!((7200..=7800).contains(&heavy), "weight 3 won {heavy}/10000");
        assert!(
            (1700..=2300).contains(&moved),
            "new weight 1 won {moved}/10000"
        );
    }

    #[test]
    fn ttl_refresh_and_lru_are_deterministic_at_the_boundary() {
        let mut table = AffinityTable::default();
        let now = Instant::now();
        let ttl = Duration::from_secs(10);
        let a = key(json!({"user":"a"}));
        let b = key(json!({"user":"b"}));
        let c = key(json!({"user":"c"}));
        let deployment = ("p".into(), "m".into());
        table.insert(a.clone(), deployment.clone(), String::new(), now, ttl, 2);
        table.insert(
            b.clone(),
            deployment.clone(),
            String::new(),
            now + Duration::from_secs(1),
            ttl,
            2,
        );
        assert!(table.get(&a, now + Duration::from_secs(2)).is_some());
        table.insert(
            c.clone(),
            deployment,
            String::new(),
            now + Duration::from_secs(3),
            ttl,
            2,
        );
        assert!(!table.pins.contains_key(&b));
        table.prune(now + Duration::from_secs(11), ttl, 2);
        assert_eq!(table.pins.len(), 2);
        table.prune(now + Duration::from_secs(12), ttl, 2);
        assert!(!table.pins.contains_key(&a));
        assert!(table.pins.contains_key(&c));
        table.prune(now + Duration::from_secs(13), ttl, 2);
        assert!(table.pins.is_empty() && table.lru.is_empty());
    }
}
