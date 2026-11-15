//! Shared, network-free model name resolution. Pricing keeps its own dated-ID lookup.

use crate::{catalog::Catalog, routing::RouterTable, state::Provider};
use std::collections::{BTreeMap, HashMap};

pub(crate) const MAX_OFFERS: usize = 50_000;

impl crate::state::AppState {
    /// Database and memory only. Never wait for a Provider model-list fetch.
    pub async fn refresh_resolution_index(&self) -> anyhow::Result<()> {
        let _guard = self.resolution_refresh.lock().await;
        let table = self.router_table();
        self.model_sources.prepare_connections(self).await;
        let providers = self.model_sources.connected_providers();
        let mut offers = self.model_sources.cached_models(&providers);
        for p in providers.iter().take(1000) {
            let remaining = MAX_OFFERS.saturating_sub(offers.len());
            if remaining == 0 {
                break;
            }
            if let Some(dev) = crate::models_dev::provider_for(&self.db, &p.name).await? {
                let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM models_dev_models WHERE provider=? AND length(id)<=200 ORDER BY id LIMIT ?")
                    .bind(dev).bind(remaining.min(5000) as i64).fetch_all(&self.db).await?;
                offers.extend(ids.into_iter().map(|id| (p.name.clone(), id)));
            }
        }
        let mut index = Index::default();
        for (provider, model) in offers {
            index.offers.entry(model).or_default().push(provider);
        }
        for (model, providers) in &mut index.offers {
            providers.sort();
            providers.dedup();
            index
                .folded
                .entry(model.to_ascii_lowercase())
                .or_default()
                .push(model.clone());
        }
        if std::sync::Arc::ptr_eq(&table, &self.router_table()) {
            *self.resolution_index.write().expect("resolution index") = index;
        }
        Ok(())
    }
}

#[derive(Clone, Default)]
pub struct Index {
    /// Bounded model IDs and their connected Providers, in deterministic order.
    offers: BTreeMap<String, Vec<String>>,
    folded: HashMap<String, Vec<String>>,
}

#[derive(Clone, Copy, Default)]
pub struct Access<'a> {
    pub models: &'a [String],
    pub providers: &'a [String],
}

impl Access<'_> {
    pub fn provider(&self, p: &Provider) -> bool {
        if self.providers.is_empty() {
            p.auth == crate::provider::ProviderAuth::ApiKey
        } else {
            self.providers.contains(&p.name)
        }
    }
}

#[derive(Clone, Debug)]
pub enum Target {
    Route(String),
    Model { provider: String, model: String },
}

#[derive(Clone, Debug)]
pub struct Resolution {
    pub target: Option<Target>,
    pub canonical: String,
    pub steps: Vec<String>,
    pub hint: Option<String>,
}

impl Resolution {
    pub fn owner(
        &self,
        table: &RouterTable,
        catalog: &Catalog,
        access: Access<'_>,
    ) -> Option<String> {
        match &self.target {
            Some(Target::Route(route))
                if table
                    .reachable_providers(catalog, route)
                    .iter()
                    .any(|p| access.provider(p)) =>
            {
                Some("router".into())
            }
            Some(Target::Model { provider, .. })
                if table
                    .providers
                    .iter()
                    .any(|p| &p.name == provider && access.provider(p)) =>
            {
                Some(provider.clone())
            }
            _ => None,
        }
    }
    pub fn allowed(&self, access: Access<'_>, requested: &str, catalog: &Catalog) -> bool {
        if access.models.is_empty() {
            return true;
        }
        access
            .models
            .iter()
            .any(|a| a == requested || a == &self.canonical)
            || match &self.target {
                Some(Target::Model { model, .. }) => catalog.allowed(access.models, Some(model)),
                _ => false,
            }
    }
}

/// Prefer exact spelling. A folded match is usable only if its spelling is unique.
fn matching<'a>(names: impl Iterator<Item = &'a str>, input: &str, folded: bool) -> Vec<&'a str> {
    let mut found: Vec<_> = names
        .filter(|n| {
            if folded {
                n.eq_ignore_ascii_case(input)
            } else {
                *n == input
            }
        })
        .collect();
    found.sort_unstable();
    found.dedup();
    found
}

pub fn resolve(
    table: &RouterTable,
    catalog: &Catalog,
    index: &Index,
    input: &str,
    access: Access<'_>,
) -> Resolution {
    let mut out = Resolution {
        target: None,
        canonical: input.into(),
        steps: vec![],
        hint: None,
    };
    for folded in [false, true] {
        let mode = if folded {
            "unique case-insensitive"
        } else {
            "exact"
        };
        for step in 1..=5 {
            out.steps.push(format!(
                "{step}. {mode}: {}",
                match step {
                    1 => "Route",
                    2 => "catalog Model or alias",
                    3 => "provider/model",
                    4 => "connected Provider model ID",
                    _ => "Provider default model",
                }
            ));
            let names = match step {
                1 => matching(table.routes.iter().map(|r| r.name.as_str()), input, folded),
                2 => matching(
                    catalog.models.iter().flat_map(|m| {
                        std::iter::once(m.name.as_str()).chain(m.aliases.iter().map(String::as_str))
                    }),
                    input,
                    folded,
                ),
                3 => {
                    let Some((prefix, model)) =
                        input.split_once('/').filter(|(_, m)| !m.is_empty())
                    else {
                        continue;
                    };
                    let providers = matching(
                        table.providers.iter().map(|p| p.name.as_str()),
                        prefix,
                        folded,
                    );
                    if providers.len() > 1 {
                        out.hint = Some("Provider name is ambiguous. Use exact spelling.".into());
                        return out;
                    }
                    if let Some(p) = providers.first() {
                        out.canonical = format!("{p}/{model}");
                        out.target = Some(Target::Model {
                            provider: (*p).into(),
                            model: model.into(),
                        });
                        return out;
                    }
                    continue;
                }
                4 => {
                    let names: Vec<&str> = if folded {
                        index
                            .folded
                            .get(&input.to_ascii_lowercase())
                            .map(|names| names.iter().map(String::as_str).collect())
                            .unwrap_or_default()
                    } else {
                        index
                            .offers
                            .get_key_value(input)
                            .map(|(name, _)| vec![name.as_str()])
                            .unwrap_or_default()
                    };
                    names
                        .into_iter()
                        .filter(|m| {
                            index.offers[*m].iter().any(|name| {
                                table
                                    .providers
                                    .iter()
                                    .any(|p| &p.name == name && access.provider(p))
                            })
                        })
                        .collect()
                }
                _ => matching(
                    table.providers.iter().map(|p| p.name.as_str()),
                    input,
                    folded,
                ),
            };
            if names.len() > 1 {
                out.hint = Some("Model name is ambiguous. Use exact spelling.".into());
                return out;
            }
            let Some(name) = names.first().copied() else {
                continue;
            };
            out.canonical = name.into();
            match step {
                1 => out.target = Some(Target::Route(name.into())),
                2 => {
                    let m = if let Some(m) = catalog.models.iter().find(|m| m.name == name) {
                        m
                    } else {
                        let mut matches = catalog
                            .models
                            .iter()
                            .filter(|m| m.aliases.iter().any(|a| a == name));
                        let m = matches.next().expect("matched catalog Model");
                        if matches.next().is_some() {
                            out.hint = Some(
                                "Model alias is ambiguous. Use a canonical Model name.".into(),
                            );
                            return out;
                        }
                        m
                    };
                    out.canonical = m.name.clone();
                    out.target = Some(Target::Model {
                        provider: m.provider.clone(),
                        model: m.name.clone(),
                    });
                }
                4 => {
                    let mut providers: Vec<_> = table
                        .providers
                        .iter()
                        .filter(|p| access.provider(p) && index.offers[name].contains(&p.name))
                        .collect();
                    providers.sort_by(|a, b| {
                        a.priority
                            .unwrap_or(0)
                            .cmp(&b.priority.unwrap_or(0))
                            .then(
                                (a.auth != crate::provider::ProviderAuth::ApiKey)
                                    .cmp(&(b.auth != crate::provider::ProviderAuth::ApiKey)),
                            )
                            .then(a.name.cmp(&b.name))
                    });
                    let Some(chosen) = providers.first() else {
                        continue;
                    };
                    let skipped: Vec<_> =
                        providers.iter().skip(1).map(|p| p.name.as_str()).collect();
                    tracing::debug!(model = name, chosen = %chosen.name, ?skipped, "model resolution");
                    out.steps.push(format!(
                        "Chosen Provider: {}. Skipped Providers: {}.",
                        chosen.name,
                        skipped.join(", ")
                    ));
                    out.target = Some(Target::Model {
                        provider: chosen.name.clone(),
                        model: name.into(),
                    });
                }
                _ => {
                    let p = table
                        .providers
                        .iter()
                        .find(|p| p.name == name)
                        .expect("matched Provider");
                    if !access.provider(p) {
                        continue;
                    }
                    let Some(model) = &p.default_model else {
                        out.hint = Some(format!(
                            "Provider {name} has no Default model. Set one on Providers, or use an allowed provider/model name."
                        ));
                        return out;
                    };
                    out.target = Some(Target::Model {
                        provider: p.name.clone(),
                        model: model.clone(),
                    });
                }
            }
            return out;
        }
    }
    if let Some(name) = &table.settings.unknown_model_route {
        out.steps
            .push(format!("6. Unknown model names: Send to Route {name}"));
        if table.routes.iter().any(|r| &r.name == name) {
            out.canonical = name.clone();
            out.target = Some(Target::Route(name.clone()));
        }
    } else {
        out.steps
            .push("6. Unknown model names: Reject (404)".into());
    }
    out
}

/// Discoverable names must pass the same resolution and access rules as requests.
pub fn names(
    table: &RouterTable,
    catalog: &Catalog,
    index: &Index,
    access: Access<'_>,
) -> Vec<(String, String)> {
    let mut names: Vec<_> = table
        .routes
        .iter()
        .map(|r| r.name.clone())
        .chain(
            catalog
                .models
                .iter()
                .flat_map(|m| std::iter::once(m.name.clone()).chain(m.aliases.clone())),
        )
        .chain(index.offers.iter().flat_map(|(m, providers)| {
            std::iter::once(m.clone()).chain(providers.iter().map(move |p| format!("{p}/{m}")))
        }))
        .chain(
            table
                .providers
                .iter()
                .filter(|p| p.default_model.is_some())
                .map(|p| p.name.clone()),
        )
        .collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .filter_map(|name| {
            let r = resolve(table, catalog, index, &name, access);
            if !r.allowed(access, &name, catalog) {
                return None;
            }
            let owner = r.owner(table, catalog, access)?;
            Some((name, owner))
        })
        .collect()
}

pub fn message(
    table: &RouterTable,
    catalog: &Catalog,
    index: &Index,
    input: &str,
    access: Access<'_>,
    resolution: &Resolution,
) -> String {
    let mut names = names(table, catalog, index, access);
    names.sort_by(|(a, _), (b, _)| {
        crate::model_search::score(input, a, a)
            .cmp(&crate::model_search::score(input, b, b))
            .then(a.len().cmp(&b.len()))
            .then(a.cmp(b))
    });
    let suggestions = names
        .into_iter()
        .take(5)
        .map(|(n, _)| n)
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Model {input:?} was not found. {}{}",
        resolution.hint.as_deref().unwrap_or(""),
        if suggestions.is_empty() {
            String::new()
        } else {
            format!(" Try: {suggestions}.")
        }
    )
}
