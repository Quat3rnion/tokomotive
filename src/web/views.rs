//! Template structs and the code that turns database rows into display text.

use askama::Template;
use chrono::{TimeZone, Utc};

use super::chart::{self, Series};
use crate::catalog::{Catalog, ModelEntry};
use crate::db::{Bucket, CardHistoryRow, Group, KeyRow, LedgerRow, TeamRow, Totals, UserRow};
use crate::pricing::{CardRates, PriceSpec, Rates, fmt_rate};

// --------------------------------------------------------------- formatting

pub fn int(n: i64) -> String {
    let s = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// Money for display: thousands separators and 4 decimals.
pub fn money(micros: i64) -> String {
    let sign = if micros < 0 { "-" } else { "" };
    let m = micros.unsigned_abs();
    let cents4 = (m + 50) / 100; // 1/10000 dollar
    format!(
        "{sign}${}.{:04}",
        int((cents4 / 10_000) as i64),
        cents4 % 10_000
    )
}

fn bp_percent(bp: i64) -> String {
    let s = format!("{:.2}", bp as f64 / 100.0);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn date(ts_s: i64) -> String {
    Utc.timestamp_opt(ts_s, 0)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

fn opt_date(ts: Option<i64>) -> String {
    ts.map(date).unwrap_or_else(|| "never".into())
}

#[derive(Clone)]
pub struct Opt {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

fn team_opts(teams: &[TeamRow], selected: Option<i64>) -> Vec<Opt> {
    let mut v = vec![Opt {
        value: String::new(),
        label: "(no team)".into(),
        selected: selected.is_none(),
    }];
    v.extend(teams.iter().map(|t| Opt {
        value: t.id.to_string(),
        label: t.name.clone(),
        selected: Some(t.id) == selected,
    }));
    v
}

// ------------------------------------------------------------------- pages

#[derive(Clone, Copy, Default)]
pub struct Base {
    pub simple: bool,
}

impl Base {
    pub fn new(st: &crate::state::AppState) -> Self {
        Self {
            simple: st.settings().mode == "simple",
        }
    }
}

#[derive(Template)]
#[template(path = "login.html")]
pub struct LoginPage {
    pub error: Option<String>,
}

#[derive(Template)]
#[template(path = "error.html")]
pub struct ErrorPage {
    pub base: Base,
    pub message: String,
}

// ---------------------------------------------------------------- overview

#[derive(Clone, Copy)]
pub enum Range {
    Day,
    Week,
    Month,
}

impl Range {
    pub fn parse(s: Option<&str>) -> Range {
        match s {
            Some("7d") => Range::Week,
            Some("30d") => Range::Month,
            _ => Range::Day,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Range::Day => "24h",
            Range::Week => "7d",
            Range::Month => "30d",
        }
    }
    fn span(self) -> i64 {
        match self {
            Range::Day => 86_400,
            Range::Week => 7 * 86_400,
            Range::Month => 30 * 86_400,
        }
    }
    pub fn bucket(self) -> i64 {
        match self {
            Range::Day => 3_600,
            Range::Week => 6 * 3_600,
            Range::Month => 86_400,
        }
    }
    pub fn since(self) -> i64 {
        let now = Utc::now().timestamp();
        let b = self.bucket();
        (now - self.span()) / b * b + b
    }
}

pub struct Stat {
    pub label: &'static str,
    pub value: String,
    pub sub: String,
}

pub struct GroupView {
    pub key: String,
    pub requests: String,
    pub errors: String,
    pub input: String,
    pub output: String,
    pub cache_read: String,
    pub cache_write_5m: String,
    pub cache_write_1h: String,
    pub image_input: String,
    pub image_output: String,
    pub attempts: String,
    pub retried: String,
    pub error_rate: String,
    pub cost: String,
    pub price: String,
    pub profit: String,
    pub margin: String,
    pub latency: String,
    pub ttft: String,
    pub tps: String,
}

fn margin(t: &Totals) -> String {
    if t.price_micros == 0 {
        "-".into()
    } else {
        format!(
            "{:.1}%",
            (t.price_micros - t.cost_micros) as f64 * 100.0 / t.price_micros as f64
        )
    }
}

fn error_rate(t: &Totals) -> String {
    if t.requests == 0 {
        "-".into()
    } else {
        format!("{:.1}%", t.errors as f64 * 100.0 / t.requests as f64)
    }
}

fn avg_ms(sum: i64, n: i64) -> String {
    if n == 0 {
        "-".into()
    } else {
        format!("{} ms", int(sum / n))
    }
}

fn tps(t: &Totals) -> String {
    // Below one second of total latency the ratio is noise.
    if t.latency_ms_sum < 1000 {
        "-".into()
    } else {
        format!("{:.1}", t.output as f64 * 1000.0 / t.latency_ms_sum as f64)
    }
}

impl GroupView {
    fn new(group: &Group) -> GroupView {
        let key = &group.key;
        let t = &group.t;
        GroupView {
            key: if key.is_empty() {
                "(unknown)".into()
            } else {
                key.to_owned()
            },
            requests: int(t.requests),
            errors: int(t.errors),
            input: int(t.input),
            output: int(t.output),
            cache_read: int(t.cache_read),
            cache_write_5m: int(t.cache_write_5m),
            cache_write_1h: int(t.cache_write_1h),
            image_input: int(t.image_input),
            image_output: int(t.image_output),
            attempts: int(group.attempts),
            retried: int(group.retried),
            error_rate: error_rate(t),
            cost: money(t.cost_micros),
            price: money(t.price_micros),
            profit: money(t.price_micros - t.cost_micros),
            margin: margin(t),
            latency: avg_ms(t.latency_ms_sum, t.requests),
            ttft: avg_ms(t.ttft_ms_sum, t.ttft_count),
            tps: tps(t),
        }
    }
}

#[derive(Template)]
#[template(path = "overview.html")]
pub struct OverviewPage {
    pub base: Base,
    pub range: &'static str,
    pub stats: Vec<Stat>,
    pub money_chart: String,
    pub traffic_chart: String,
    pub by_model: Vec<GroupView>,
    pub by_provider: Vec<GroupView>,
    pub by_user: Vec<GroupView>,
    pub by_team: Vec<GroupView>,
    pub unpriced: i64,
    pub estimated: i64,
}

impl OverviewPage {
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        base: Base,
        range: Range,
        t: &Totals,
        series: &[Bucket],
        by_model: &[Group],
        by_provider: &[Group],
        by_user: &[Group],
        by_team: &[Group],
        unpriced: i64,
        estimated: i64,
    ) -> OverviewPage {
        let tokens = t.input
            + t.output
            + t.cache_read
            + t.cache_write_5m
            + t.cache_write_1h
            + t.image_input
            + t.image_output;
        let hours = range.span() as f64 / 3600.0;
        let mut stats = vec![
            Stat {
                label: "Requests",
                value: int(t.requests),
                sub: format!(
                    "{} errors, {:.1}/h",
                    int(t.errors),
                    t.requests as f64 / hours
                ),
            },
            Stat {
                label: "Tokens",
                value: int(tokens),
                sub: format!(
                    "in {} / out {} / cache r {} w5m {} w1h {} / image in {} out {}",
                    int(t.input),
                    int(t.output),
                    int(t.cache_read),
                    int(t.cache_write_5m),
                    int(t.cache_write_1h),
                    int(t.image_input),
                    int(t.image_output)
                ),
            },
            Stat {
                label: "Price (billed)",
                value: money(t.price_micros),
                sub: "what users owe".into(),
            },
            Stat {
                label: "Cost (provider)",
                value: money(t.cost_micros),
                sub: "what you owe".into(),
            },
            Stat {
                label: "Profit",
                value: money(t.price_micros - t.cost_micros),
                sub: format!("margin {}", margin(t)),
            },
            Stat {
                label: "Latency",
                value: avg_ms(t.latency_ms_sum, t.requests),
                sub: format!(
                    "TTFT {}, {} out tok/s",
                    avg_ms(t.ttft_ms_sum, t.ttft_count),
                    tps(t)
                ),
            },
        ];
        if base.simple {
            stats.retain(|s| !matches!(s.label, "Price (billed)" | "Cost (provider)" | "Profit"));
        }
        stats.push(Stat {
            label: "Error rate",
            value: error_rate(t),
            sub: format!("{} errors", int(t.errors)),
        });
        stats.push(Stat {
            label: "Attempts",
            value: int(by_model.iter().map(|g| g.attempts).sum()),
            sub: format!(
                "{} retried requests",
                int(by_model.iter().map(|g| g.retried).sum())
            ),
        });

        // Fill empty buckets so the time axis is continuous.
        let b = range.bucket();
        let now = Utc::now().timestamp();
        let mut labels = vec![];
        let (mut cost, mut price, mut profit, mut reqs, mut toks) =
            (vec![], vec![], vec![], vec![], vec![]);
        let mut it = series.iter().peekable();
        let mut at = range.since() / b * b;
        while at <= now {
            let zero = Totals::default();
            let tt = match it.peek() {
                Some(x) if x.bucket == at => &it.next().expect("peeked").t,
                _ => &zero,
            };
            let fmt = if b >= 86_400 { "%m-%d" } else { "%m-%d %H:%M" };
            labels.push(
                Utc.timestamp_opt(at, 0)
                    .single()
                    .map(|d| d.format(fmt).to_string())
                    .unwrap_or_default(),
            );
            cost.push(tt.cost_micros as f64 / 1e6);
            price.push(tt.price_micros as f64 / 1e6);
            profit.push((tt.price_micros - tt.cost_micros) as f64 / 1e6);
            reqs.push(tt.requests as f64);
            toks.push(
                (tt.input
                    + tt.output
                    + tt.cache_read
                    + tt.cache_write_5m
                    + tt.cache_write_1h
                    + tt.image_input
                    + tt.image_output) as f64
                    / 1000.0,
            );
            at += b;
        }
        let money_chart = chart::line(
            &labels,
            &[
                Series {
                    name: "Price",
                    color: "#3b82f6",
                    values: price,
                },
                Series {
                    name: "Cost",
                    color: "#ef4444",
                    values: cost,
                },
                Series {
                    name: "Profit",
                    color: "#10b981",
                    values: profit,
                },
            ],
            &|v| format!("${v:.2}"),
        );
        let traffic_chart = chart::line(
            &labels,
            &[
                Series {
                    name: "Requests",
                    color: "#8b5cf6",
                    values: reqs,
                },
                Series {
                    name: "Tokens (k)",
                    color: "#f59e0b",
                    values: toks,
                },
            ],
            &|v| format!("{v:.0}"),
        );
        let g = |rows: &[Group]| rows.iter().map(GroupView::new).collect();
        OverviewPage {
            base,
            range: range.name(),
            stats,
            money_chart,
            traffic_chart,
            by_model: g(by_model),
            by_provider: g(by_provider),
            by_user: g(by_user),
            by_team: g(by_team),
            unpriced,
            estimated,
        }
    }
}

// ------------------------------------------------------------------- users

pub struct UserView {
    pub id: i64,
    pub name: String,
    pub team_opts: Vec<Opt>,
    pub budget: String,
    pub monthly: bool,
    pub disabled: bool,
    pub spend: String,
    pub used: String,
    pub over: bool,
    pub keys: i64,
    pub rpm_limit: String,
    pub tpm_limit: String,
}

#[derive(Template)]
#[template(path = "users.html")]
pub struct UsersPage {
    pub base: Base,
    pub users: Vec<UserView>,
    pub team_opts: Vec<Opt>,
}

impl UsersPage {
    pub fn build(base: Base, users: &[UserRow], teams: &[TeamRow]) -> UsersPage {
        let users = users
            .iter()
            .map(|u| UserView {
                id: u.id,
                name: u.name.clone(),
                team_opts: team_opts(teams, u.team_id),
                budget: u
                    .budget_micros
                    .map(crate::pricing::fmt_rate)
                    .unwrap_or_default(),
                monthly: u.budget_monthly,
                disabled: u.disabled,
                spend: money(u.period_spend),
                used: match u.budget_micros {
                    Some(b) if b > 0 => format!("{:.1}%", u.period_spend as f64 * 100.0 / b as f64),
                    _ => "-".into(),
                },
                over: u.budget_micros.is_some_and(|b| u.period_spend >= b),
                keys: u.keys,
                rpm_limit: u.rpm_limit.map(|v| v.to_string()).unwrap_or_default(),
                tpm_limit: u.tpm_limit.map(|v| v.to_string()).unwrap_or_default(),
            })
            .collect();
        UsersPage {
            base,
            users,
            team_opts: team_opts(teams, None),
        }
    }
}

#[derive(Template)]
#[template(path = "teams.html")]
pub struct TeamsPage {
    pub base: Base,
    pub teams: Vec<TeamRow>,
}

// -------------------------------------------------------------------- keys

pub struct KeyView {
    pub id: i64,
    pub user: String,
    pub prefix: String,
    pub label: String,
    pub models: String,
    pub providers: Vec<Opt>,
    pub created: String,
    pub last_used: String,
    pub revoked: bool,
    pub rpm_limit: String,
    pub tpm_limit: String,
}

#[derive(Template)]
#[template(path = "keys.html")]
pub struct KeysPage {
    pub base: Base,
    pub keys: Vec<KeyView>,
    pub users: Vec<Opt>,
    pub new_key: Option<String>,
    pub providers: Vec<String>,
}

impl KeysPage {
    pub fn build(
        base: Base,
        keys: &[KeyRow],
        users: &[UserRow],
        new_key: Option<String>,
        providers: &[String],
    ) -> KeysPage {
        KeysPage {
            base,
            providers: providers.to_vec(),
            keys: keys
                .iter()
                .map(|k| KeyView {
                    id: k.id,
                    user: k.user.clone(),
                    prefix: k.prefix.clone(),
                    label: k.label.clone(),
                    providers: {
                        let selected: Vec<String> = k
                            .allowed_providers
                            .as_deref()
                            .and_then(|s| serde_json::from_str(s).ok())
                            .unwrap_or_default();
                        providers
                            .iter()
                            .map(|p| Opt {
                                value: p.clone(),
                                label: p.clone(),
                                selected: selected.contains(p),
                            })
                            .collect()
                    },
                    models: k
                        .allowed_models
                        .as_deref()
                        .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
                        .map(|v| v.join(", "))
                        .unwrap_or_else(|| "all".into()),
                    created: date(k.created_at),
                    last_used: opt_date(k.last_used_at),
                    revoked: k.revoked_at.is_some(),
                    rpm_limit: k.rpm_limit.map(|v| v.to_string()).unwrap_or_default(),
                    tpm_limit: k.tpm_limit.map(|v| v.to_string()).unwrap_or_default(),
                })
                .collect(),
            users: users
                .iter()
                .map(|u| Opt {
                    value: u.id.to_string(),
                    label: u.name.clone(),
                    selected: false,
                })
                .collect(),
            new_key,
        }
    }
}

// ------------------------------------------------------------------ models

pub struct RatesView {
    pub input: String,
    pub output: String,
    pub cache_read: String,
    pub cache_write_5m: String,
    pub cache_write_1h: String,
    pub image_input: String,
    pub image_output: String,
    pub tiers: String,
    pub tier_count: usize,
    has_image: bool,
}

impl RatesView {
    fn new(c: &CardRates) -> RatesView {
        let r = |x: &Rates| {
            [
                x.input,
                x.output,
                x.cache_read,
                x.cache_write_5m,
                x.cache_write_1h,
                x.image_input,
                x.image_output,
            ]
            .map(fmt_rate)
        };
        let b = r(&c.base);
        RatesView {
            input: b[0].clone(),
            output: b[1].clone(),
            cache_read: b[2].clone(),
            cache_write_5m: b[3].clone(),
            cache_write_1h: b[4].clone(),
            image_input: b[5].clone(),
            image_output: b[6].clone(),
            has_image: std::iter::once(&c.base)
                .chain(c.tiers.iter().map(|t| &t.rates))
                .any(|x| x.image_input != 0 || x.image_output != 0),
            tiers: c
                .tiers
                .iter()
                .map(|t| {
                    let v = r(&t.rates);
                    // Leave out the image rates when they are zero.
                    let n = if t.rates.image_input == 0 && t.rates.image_output == 0 {
                        5
                    } else {
                        7
                    };
                    format!("{} {}", t.above_input_tokens, v[..n].join(" "))
                })
                .collect::<Vec<_>>()
                .join("\n"),
            tier_count: c.tiers.len(),
        }
    }

    pub fn summary(&self) -> String {
        let mut s = format!(
            "in {} / out {} / cache r {} w5m {} w1h {}",
            self.input, self.output, self.cache_read, self.cache_write_5m, self.cache_write_1h
        );
        if self.has_image {
            s += &format!(" / image in {} out {}", self.image_input, self.image_output);
        }
        s
    }
}

fn describe_price(spec: Option<&PriceSpec>, default_bp: i64) -> String {
    match spec {
        None => format!("default markup {}%", bp_percent(default_bp)),
        Some(PriceSpec::MarkupBp(bp)) => format!("markup {}%", bp_percent(*bp)),
        Some(PriceSpec::Rates(r)) => {
            format!("explicit: {}", RatesView::new(r).summary())
        }
    }
}

pub struct ModelView {
    pub id: i64,
    pub name: String,
    pub provider: String,
    pub aliases: String,
    pub cost: Option<RatesView>,
    pub price: String,
    pub overrides: usize,
}

#[derive(Template)]
#[template(path = "models.html")]
pub struct ModelsPage {
    pub base: Base,
    pub models: Vec<ModelView>,
    pub providers: Vec<String>,
    pub default_markup: String,
}

impl ModelsPage {
    pub fn build(base: Base, cat: &Catalog, providers: Vec<String>) -> ModelsPage {
        ModelsPage {
            base,
            models: cat
                .models
                .iter()
                .map(|m| ModelView {
                    id: m.id,
                    name: m.name.clone(),
                    provider: m.provider.clone(),
                    aliases: m.aliases.join(", "),
                    cost: m.cost.as_ref().map(|(_, c)| RatesView::new(c)),
                    price: describe_price(m.price.as_ref().map(|(_, p)| p), cat.default_markup_bp),
                    overrides: m.team_price.len(),
                })
                .collect(),
            providers,
            default_markup: bp_percent(cat.default_markup_bp),
        }
    }
}

pub struct OverrideView {
    pub team_id: i64,
    pub team: String,
    pub desc: String,
}

pub struct HistView {
    pub id: i64,
    pub scope: String,
    pub kind: String,
    pub desc: String,
    pub created: String,
}

#[derive(Template)]
#[template(path = "model.html")]
pub struct ModelPage {
    pub base: Base,
    pub id: i64,
    pub name: String,
    pub provider_opts: Vec<Opt>,
    pub aliases: String,
    pub cost: RatesView,
    pub price_mode: &'static str,
    pub price_markup: String,
    pub price_rates: RatesView,
    pub price_desc: String,
    pub default_markup: String,
    pub overrides: Vec<OverrideView>,
    pub team_opts: Vec<Opt>,
    pub history: Vec<HistView>,
}

impl ModelPage {
    pub fn build(
        base: Base,
        m: &ModelEntry,
        default_bp: i64,
        history: &[CardHistoryRow],
        teams: &[TeamRow],
        providers: Vec<String>,
    ) -> ModelPage {
        let cost = m.cost.as_ref().map(|(_, c)| c.clone()).unwrap_or_default();
        let price = m.price.as_ref().map(|(_, p)| p);
        let (price_mode, price_markup, price_rates) = match price {
            None => ("default", String::new(), cost.clone()),
            Some(PriceSpec::MarkupBp(bp)) => ("markup", bp_percent(*bp), cost.clone()),
            Some(PriceSpec::Rates(r)) => ("rates", String::new(), r.clone()),
        };
        let mut overrides: Vec<OverrideView> = m
            .team_price
            .iter()
            .map(|(tid, (_, spec))| OverrideView {
                team_id: *tid,
                team: teams
                    .iter()
                    .find(|t| t.id == *tid)
                    .map(|t| t.name.clone())
                    .unwrap_or_else(|| format!("team {tid}")),
                desc: describe_price(Some(spec), default_bp),
            })
            .collect();
        overrides.sort_by(|a, b| a.team.cmp(&b.team));
        let history = history
            .iter()
            .map(|h| HistView {
                id: h.id,
                scope: h.team.clone().unwrap_or_else(|| "model".into()),
                kind: h.kind.clone(),
                desc: if h.kind == "cost" {
                    serde_json::from_str::<CardRates>(&h.spec)
                        .map(|c| {
                            let v = RatesView::new(&c);
                            format!("{}; {} tier(s)", v.summary(), v.tier_count)
                        })
                        .unwrap_or_else(|_| h.spec.clone())
                } else {
                    match serde_json::from_str::<Option<PriceSpec>>(&h.spec) {
                        Ok(None) => "removed".into(),
                        Ok(Some(p)) => describe_price(Some(&p), default_bp),
                        Err(_) => h.spec.clone(),
                    }
                },
                created: date(h.created_at),
            })
            .collect();
        ModelPage {
            base,
            id: m.id,
            name: m.name.clone(),
            provider_opts: providers
                .into_iter()
                .map(|p| Opt {
                    selected: p == m.provider,
                    label: p.clone(),
                    value: p,
                })
                .collect(),
            aliases: m.aliases.join(", "),
            cost: RatesView::new(&cost),
            price_mode,
            price_markup,
            price_rates: RatesView::new(&price_rates),
            price_desc: describe_price(price, default_bp),
            default_markup: bp_percent(default_bp),
            overrides,
            team_opts: teams
                .iter()
                .map(|t| Opt {
                    value: t.id.to_string(),
                    label: t.name.clone(),
                    selected: false,
                })
                .collect(),
            history,
        }
    }
}

// ------------------------------------------------------------------ ledger

pub struct LedgerView {
    pub id: i64,
    pub time: String,
    pub user: String,
    pub provider: String,
    pub endpoint: String,
    pub model: String,
    pub status: i64,
    pub streamed: bool,
    pub input: String,
    pub output: String,
    pub cache_read: String,
    pub cache_write_5m: String,
    pub cache_write_1h: String,
    pub image_input: String,
    pub image_output: String,
    pub attempts: i64,
    pub key: String,
    pub cost: String,
    pub price: String,
    pub unpriced: bool,
    pub estimated: bool,
    pub aborted: bool,
    pub latency: String,
    pub ttft: String,
}

#[derive(Template)]
#[template(path = "ledger.html")]
pub struct LedgerPage {
    pub base: Base,
    pub rows: Vec<LedgerView>,
    pub filter: &'static str,
    pub notice: Option<String>,
}

impl LedgerPage {
    pub fn build(
        base: Base,
        rows: &[LedgerRow],
        filter: &'static str,
        notice: Option<String>,
    ) -> LedgerPage {
        LedgerPage {
            base,
            rows: rows
                .iter()
                .map(|r| LedgerView {
                    id: r.id,
                    time: Utc
                        .timestamp_millis_opt(r.ts_ms)
                        .single()
                        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
                        .unwrap_or_default(),
                    user: r.user.clone(),
                    provider: r.provider.clone(),
                    endpoint: r.endpoint.clone(),
                    model: r
                        .model
                        .clone()
                        .or_else(|| r.requested_model.clone())
                        .unwrap_or_else(|| "-".into()),
                    status: r.status,
                    streamed: r.streamed,
                    input: int(r.input),
                    output: int(r.output),
                    cache_read: int(r.cache_read),
                    cache_write_5m: int(r.cache_write_5m),
                    cache_write_1h: int(r.cache_write_1h),
                    image_input: int(r.image_input),
                    image_output: int(r.image_output),
                    attempts: r.attempts,
                    key: match (r.provider_key, r.attempts) {
                        (None, _) => "-".into(),
                        (Some(k), a) if a > 1 => format!("#{k} ({a} tries)"),
                        (Some(k), _) => format!("#{k}"),
                    },
                    cost: money(r.cost_micros),
                    price: money(r.price_micros),
                    unpriced: r.unpriced,
                    estimated: r.estimated,
                    aborted: r.client_aborted,
                    latency: format!("{} ms", int(r.latency_ms)),
                    ttft: r
                        .ttft_ms
                        .map(|t| format!("{} ms", int(t)))
                        .unwrap_or_else(|| "-".into()),
                })
                .collect(),
            filter,
            notice,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_formatting() {
        assert_eq!(int(1_234_567), "1,234,567");
        assert_eq!(int(-1000), "-1,000");
        assert_eq!(money(1_234_567_890), "$1,234.5679");
        assert_eq!(money(-50), "-$0.0001");
        assert_eq!(bp_percent(2_500), "25");
        assert_eq!(bp_percent(1_250), "12.5");
    }
}
