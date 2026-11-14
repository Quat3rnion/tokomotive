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

/// Money for totals and Budgets: 2 decimals from $1, 4 decimals below $1,
/// and "<$0.0001" for a non-zero amount below the display resolution.
pub fn money(micros: i64) -> String {
    if micros == 0 {
        "$0.00".into()
    } else if micros.unsigned_abs() >= 1_000_000 {
        let sign = if micros < 0 { "-" } else { "" };
        let cents = (micros.unsigned_abs() + 5_000) / 10_000;
        format!("{sign}${}.{:02}", int((cents / 100) as i64), cents % 100)
    } else {
        money_row(micros)
    }
}

/// Money for single Ledger Entries: always 4 decimals, and "<$0.0001" for a
/// non-zero amount below the display resolution.
pub fn money_row(micros: i64) -> String {
    let sign = if micros < 0 { "-" } else { "" };
    let m = micros.unsigned_abs();
    if m > 0 && m < 50 {
        return format!("{sign}<$0.0001");
    }
    let cents4 = (m + 50) / 100; // 1/10000 dollar
    format!(
        "{sign}${}.{:04}",
        int((cents4 / 10_000) as i64),
        cents4 % 10_000
    )
}

/// The full value of an amount, for a `title` attribute.
pub fn money_exact(micros: i64) -> String {
    let sign = if micros < 0 { "-" } else { "" };
    let m = micros.unsigned_abs();
    format!(
        "{sign}${}.{:06}",
        int((m / 1_000_000) as i64),
        m % 1_000_000
    )
}

/// An amount with its display text and its full value. It renders as the
/// display text; use `{{ x.exact }}` in a `title`.
#[derive(Clone)]
pub struct Money {
    pub text: String,
    pub exact: String,
}

impl Money {
    pub fn total(micros: i64) -> Money {
        Money {
            text: money(micros),
            exact: money_exact(micros),
        }
    }
}

impl std::fmt::Display for Money {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

fn bp_percent(bp: i64) -> String {
    let s = format!("{:.2}", bp as f64 / 100.0);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// A point in time. It renders as `YYYY-MM-DD HH:MM UTC`; use
/// `<time datetime="{{ t.iso }}">{{ t }}</time>` in templates.
#[derive(Clone, Default)]
pub struct Stamp {
    pub text: String,
    /// Without the zone, for a column whose header says "(UTC)".
    pub short: String,
    pub iso: String,
}

impl std::fmt::Display for Stamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

fn stamp(ts_s: i64) -> Stamp {
    Utc.timestamp_opt(ts_s, 0)
        .single()
        .map(|t| Stamp {
            text: t.format("%Y-%m-%d %H:%M UTC").to_string(),
            short: t.format("%Y-%m-%d %H:%M").to_string(),
            iso: t.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        })
        .unwrap_or_default()
}

fn date(ts_s: i64) -> Stamp {
    stamp(ts_s)
}

fn opt_date(ts: Option<i64>) -> Stamp {
    ts.map(date).unwrap_or_else(|| Stamp {
        text: "never".into(),
        short: "never".into(),
        iso: String::new(),
    })
}

// ------------------------------------------------------------------- pages

/// Data that `base.html` needs on every page.
#[derive(Clone, Default)]
pub struct Base {
    pub simple: bool,
    /// Show the navigation. False on login, Setup and logged-out errors.
    pub logged_in: bool,
    /// One-time success message, shown in a `role="status"` box.
    pub flash: Option<String>,
    /// The Proxy base URL, for example `http://localhost:8080/v1`.
    pub proxy_url: String,
}

impl Base {
    pub fn new(st: &crate::state::AppState) -> Self {
        let ctx = super::ctx();
        Self {
            simple: st.settings().mode == "simple",
            logged_in: true,
            flash: ctx.flash,
            proxy_url: ctx.proxy_url,
        }
    }

    /// For pages outside a checked session: login, Setup and error pages.
    pub fn anonymous() -> Self {
        let ctx = super::ctx();
        Self {
            simple: false,
            logged_in: ctx.session,
            flash: ctx.flash,
            proxy_url: ctx.proxy_url,
        }
    }
}

#[derive(Template)]
#[template(path = "login.html")]
pub struct LoginPage {
    pub base: Base,
    pub error: Option<String>,
}

#[derive(Template)]
#[template(path = "error.html")]
pub struct ErrorPage {
    pub base: Base,
    pub heading: String,
    pub message: String,
    pub back_href: String,
    pub back_label: String,
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

/// How the range reads in sentences and chart captions.
impl Range {
    fn per(self) -> &'static str {
        match self {
            Range::Day => "per hour",
            Range::Week => "per 6 hours",
            Range::Month => "per day",
        }
    }
    fn words(self) -> &'static str {
        match self {
            Range::Day => "the last 24 hours",
            Range::Week => "the last 7 days",
            Range::Month => "the last 30 days",
        }
    }
}

pub struct Stat {
    pub label: &'static str,
    pub value: String,
    /// Full value for a `title`, empty when the value is exact.
    pub title: String,
    pub sub: String,
}

pub struct GroupView {
    pub key: String,
    pub requests: String,
    pub errors: String,
    pub error_rate: String,
    pub input: String,
    pub output: String,
    pub cache_read: String,
    pub cache_write: String,
    pub cache_write_title: String,
    pub image_input: String,
    pub image_output: String,
    pub attempts: String,
    pub retried: String,
    pub cost: Money,
    pub price: Money,
    pub profit: Money,
    pub margin: String,
    pub latency: String,
    pub ttft: String,
    pub tps: String,
}

/// One "By ..." table. Image columns show only when a row uses images.
pub struct GroupTable {
    pub all_href: String,
    pub id: &'static str,
    pub title: &'static str,
    pub column: &'static str,
    pub rows: Vec<GroupView>,
    pub images: bool,
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

fn all_tokens(t: &Totals) -> i64 {
    t.input
        + t.output
        + t.cache_read
        + t.cache_write_5m
        + t.cache_write_1h
        + t.image_input
        + t.image_output
}

impl GroupView {
    pub fn new(group: &Group) -> GroupView {
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
            error_rate: error_rate(t),
            input: int(t.input),
            output: int(t.output),
            cache_read: int(t.cache_read),
            cache_write: int(t.cache_write_5m + t.cache_write_1h),
            cache_write_title: format!(
                "Cache write 5m: {}, Cache write 1h: {}",
                int(t.cache_write_5m),
                int(t.cache_write_1h)
            ),
            image_input: int(t.image_input),
            image_output: int(t.image_output),
            attempts: int(group.attempts),
            retried: int(group.retried),
            cost: Money::total(t.cost_micros),
            price: Money::total(t.price_micros),
            profit: Money::total(t.price_micros - t.cost_micros),
            margin: margin(t),
            latency: avg_ms(t.latency_ms_sum, t.requests),
            ttft: avg_ms(t.ttft_ms_sum, t.ttft_count),
            tps: tps(t),
        }
    }
}

/// One step of the first-run checklist.
pub struct Step {
    pub id: &'static str,
    pub done: bool,
    pub title: &'static str,
    pub text: &'static str,
    pub links: Vec<(&'static str, &'static str)>,
}

/// What the first-run checklist needs from the database.
#[derive(Default, Clone, Copy)]
pub struct SetupState {
    pub providers: bool,
    pub models_or_routes: bool,
    pub users: bool,
    pub keys: bool,
    pub requests: bool,
}

impl SetupState {
    fn steps(self, simple: bool) -> Vec<Step> {
        vec![
            Step {
                id: "provider",
                done: self.providers,
                title: "Add a Provider",
                text: "The upstream LLM service, for example OpenAI or Anthropic, with its Provider Key or a subscription.",
                links: vec![("/admin/providers", "Providers")],
            },
            Step {
                id: "model",
                done: self.models_or_routes,
                title: "Add a Model or a Route (optional)",
                text: if simple {
                    "A Route gives one model name several Deployments and Fallbacks. Without one, clients send provider/model, for example openai/gpt-4o."
                } else {
                    "A Model holds the rates for usage accounting. A Route gives one model name several Deployments. Without either, clients send provider/model, for example openai/gpt-4o."
                },
                links: vec![("/admin/models", "Models"), ("/admin/routes", "Routes")],
            },
            Step {
                id: "user",
                done: self.users,
                title: "Add a User",
                text: "A User holds Virtual Keys. Usage is recorded per User.",
                links: vec![("/admin/users", "Users")],
            },
            Step {
                id: "key",
                done: self.keys,
                title: "Add a Virtual Key",
                text: "Give the key to the client. The dashboard shows it only once.",
                links: vec![("/admin/keys", "Keys")],
            },
            Step {
                id: "request",
                done: self.requests,
                title: "Send a first request",
                text: "Point the client at the Proxy base URL below and use the Virtual Key as its API key.",
                links: vec![("/admin/ledger", "Ledger")],
            },
        ]
    }

    fn complete(self) -> bool {
        self.providers && self.users && self.keys && self.requests
    }
}

#[derive(Template)]
#[template(path = "overview.html")]
pub struct OverviewPage {
    pub quota: Vec<super::quota::AccountView>,
    pub model_price_alerts: Vec<super::models_dev::AlertView>,
    pub base: Base,
    pub range: &'static str,
    pub range_words: &'static str,
    /// Empty when Setup is complete.
    pub steps: Vec<Step>,
    pub stats: Vec<Stat>,
    pub more: Vec<(&'static str, String)>,
    pub money_chart: String,
    pub requests_chart: String,
    pub tokens_chart: String,
    pub groups: Vec<GroupTable>,
    pub unpriced: i64,
    pub estimated: i64,
}

pub struct OverviewData<'a> {
    pub totals: &'a Totals,
    pub series: &'a [Bucket],
    pub by_model: &'a [Group],
    pub by_provider: &'a [Group],
    pub by_user: &'a [Group],
    pub by_team: &'a [Group],
    pub unpriced: i64,
    pub estimated: i64,
    pub setup: SetupState,
}

impl OverviewPage {
    pub fn build(base: Base, range: Range, d: OverviewData) -> OverviewPage {
        let t = d.totals;
        let hours = range.span() as f64 / 3600.0;
        let attempts: i64 = d.by_model.iter().map(|g| g.attempts).sum();
        let retried: i64 = d.by_model.iter().map(|g| g.retried).sum();
        let mut stats = vec![
            Stat {
                label: "Requests",
                value: int(t.requests),
                title: String::new(),
                sub: format!("{} errors, error rate {}", int(t.errors), error_rate(t)),
            },
            Stat {
                label: "Tokens",
                value: int(all_tokens(t)),
                title: String::new(),
                sub: format!("input {}, output {}", int(t.input), int(t.output)),
            },
        ];
        if !base.simple {
            stats.push(Stat {
                label: "Price",
                value: money(t.price_micros),
                title: money_exact(t.price_micros),
                sub: format!(
                    "Cost {}, Profit {} ({})",
                    money(t.cost_micros),
                    money(t.price_micros - t.cost_micros),
                    margin(t)
                ),
            });
        }
        stats.push(Stat {
            label: "Latency",
            value: avg_ms(t.latency_ms_sum, t.requests),
            title: String::new(),
            sub: format!(
                "TTFT {}, {} output tokens/s",
                avg_ms(t.ttft_ms_sum, t.ttft_count),
                tps(t)
            ),
        });
        let mut more = vec![
            (
                "Requests per hour",
                format!("{:.1}", t.requests as f64 / hours),
            ),
            ("Attempts", int(attempts)),
            ("Retried requests", int(retried)),
            ("Cache read", int(t.cache_read)),
            ("Cache write 5m", int(t.cache_write_5m)),
            ("Cache write 1h", int(t.cache_write_1h)),
        ];
        if t.image_input != 0 || t.image_output != 0 {
            more.push(("Image input", int(t.image_input)));
            more.push(("Image output", int(t.image_output)));
        }

        // Fill empty buckets so the time axis is continuous.
        let b = range.bucket();
        let now = Utc::now().timestamp();
        let mut labels = vec![];
        let mut axis = vec![];
        let (mut cost, mut price, mut profit, mut reqs, mut toks) =
            (vec![], vec![], vec![], vec![], vec![]);
        let mut it = d.series.iter().peekable();
        let mut at = range.since() / b * b;
        while at <= now {
            let zero = Totals::default();
            let tt = match it.peek() {
                Some(x) if x.bucket == at => &it.next().expect("peeked").t,
                _ => &zero,
            };
            let (fmt, short) = match range {
                Range::Day => ("%m-%d %H:%M", "%H:%M"),
                Range::Week => ("%m-%d %H:%M", "%m-%d"),
                Range::Month => ("%m-%d", "%m-%d"),
            };
            let when = Utc.timestamp_opt(at, 0).single();
            labels.push(when.map(|d| d.format(fmt).to_string()).unwrap_or_default());
            axis.push(
                when.map(|d| d.format(short).to_string())
                    .unwrap_or_default(),
            );
            cost.push(tt.cost_micros as f64 / 1e6);
            price.push(tt.price_micros as f64 / 1e6);
            profit.push((tt.price_micros - tt.cost_micros) as f64 / 1e6);
            reqs.push(tt.requests as f64);
            toks.push(all_tokens(tt) as f64);
            at += b;
        }
        let per = range.per();
        let words = range.words();
        let money_chart = chart::line(
            &chart::Chart {
                id: "chart-money",
                title: format!("Price, Cost and Profit {per} (USD)"),
                range: words,
                unit: chart::Unit::Money,
            },
            &labels,
            &axis,
            &[
                Series {
                    name: "Price",
                    color: "#60a5fa",
                    dash: "",
                    values: price,
                },
                Series {
                    name: "Cost",
                    color: "#f87171",
                    dash: "6 4",
                    values: cost,
                },
                Series {
                    name: "Profit",
                    color: "#34d399",
                    dash: "2 3",
                    values: profit,
                },
            ],
        );
        let requests_chart = chart::line(
            &chart::Chart {
                id: "chart-requests",
                title: format!("Requests {per}"),
                range: words,
                unit: chart::Unit::Count,
            },
            &labels,
            &axis,
            &[Series {
                name: "Requests",
                color: "#a78bfa",
                dash: "",
                values: reqs,
            }],
        );
        let tokens_chart = chart::line(
            &chart::Chart {
                id: "chart-tokens",
                title: format!("Tokens {per}"),
                range: words,
                unit: chart::Unit::Count,
            },
            &labels,
            &axis,
            &[Series {
                name: "Tokens",
                color: "#fbbf24",
                dash: "",
                values: toks,
            }],
        );
        let table = |id, title, column, rows: &[Group]| GroupTable {
            all_href: format!(
                "/admin/usage?group={}&range={}",
                match column {
                    "User" => "user",
                    "Team" => "team",
                    "Provider" => "provider",
                    _ => "model",
                },
                range.name()
            ),
            id,
            title,
            column,
            images: rows
                .iter()
                .any(|g| g.t.image_input != 0 || g.t.image_output != 0),
            rows: rows.iter().map(GroupView::new).collect(),
        };
        OverviewPage {
            quota: vec![],
            model_price_alerts: vec![],
            range: range.name(),
            range_words: words,
            steps: if d.setup.complete() {
                vec![]
            } else {
                d.setup.steps(base.simple)
            },
            base,
            stats,
            more,
            money_chart,
            requests_chart,
            tokens_chart,
            groups: vec![
                table("by-model", "By Model", "Model", d.by_model),
                table("by-provider", "By Provider", "Provider", d.by_provider),
                table("by-user", "By User", "User", d.by_user),
                table("by-team", "By Team", "Team", d.by_team),
            ],
            unpriced: d.unpriced,
            estimated: d.estimated,
        }
    }
}

// ------------------------------------------------------------------- users

#[derive(Default)]
pub struct BudgetView {
    pub amount: String,
    pub period: String,
    pub summary: String,
    pub resets: String,
}
impl BudgetView {
    pub async fn load(
        st: &crate::state::AppState,
        subject: crate::budgets::Subject,
    ) -> anyhow::Result<Self> {
        let b = crate::budgets::current(&st.db, subject, crate::db::now_s()).await?;
        Ok(Self {
            amount: b.limit.map(crate::pricing::fmt_rate).unwrap_or_default(),
            period: b.period,
            summary: format!(
                "{} / {}",
                money(b.spent),
                b.limit.map(money).unwrap_or_else(|| "No Budget".into())
            ),
            resets: b
                .end
                .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
                .unwrap_or_else(|| "Never (lifetime)".into()),
        })
    }
}

pub struct UserView {
    pub budget_view: BudgetView,
    pub team_name: String,
    pub id: i64,
    pub name: String,
    pub team: String,
    /// The Budget for the form, for example "12.5".
    pub budget: String,
    pub disabled: bool,
    pub used: String,
    pub over: bool,
    pub keys: i64,
    pub limits: String,
    pub rpm_limit: String,
    pub tpm_limit: String,
}

#[derive(Template)]
#[template(path = "users.html")]
pub struct UsersPage {
    pub team_choices: Option<Vec<crate::db::ControlChoice>>,
    pub pager: crate::pagination::Pager,
    pub base: Base,
    pub users: Vec<UserView>,
}

impl UsersPage {
    pub fn build(base: Base, users: &[UserRow]) -> UsersPage {
        let users = users
            .iter()
            .map(|u| UserView {
                budget_view: Default::default(),
                team_name: u.team.clone().unwrap_or_default(),
                id: u.id,
                name: u.name.clone(),
                team: u.team.clone().unwrap_or_else(|| "No Team".into()),
                budget: u
                    .budget_micros
                    .map(crate::pricing::fmt_rate)
                    .unwrap_or_default(),
                disabled: u.disabled,
                used: match u.budget_micros {
                    Some(b) if b > 0 => format!("{:.1}%", u.period_spend as f64 * 100.0 / b as f64),
                    _ => "-".into(),
                },
                over: u.budget_micros.is_some_and(|b| u.period_spend >= b),
                keys: u.keys,
                limits: if u.rpm_limit.is_none() && u.tpm_limit.is_none() {
                    "Unlimited".into()
                } else {
                    format!(
                        "RPM {}, TPM {}",
                        limit_text(u.rpm_limit),
                        limit_text(u.tpm_limit)
                    )
                },
                rpm_limit: u.rpm_limit.map(|v| v.to_string()).unwrap_or_default(),
                tpm_limit: u.tpm_limit.map(|v| v.to_string()).unwrap_or_default(),
            })
            .collect();
        UsersPage {
            team_choices: None,
            pager: Default::default(),
            base,
            users,
        }
    }
}

pub struct TeamView {
    pub budget_view: BudgetView,
    pub id: i64,
    pub name: String,
    pub user_count: i64,
    pub override_count: usize,
}

#[derive(Template)]
#[template(path = "teams.html")]
pub struct TeamsPage {
    pub pager: crate::pagination::Pager,
    pub base: Base,
    pub teams: Vec<TeamView>,
}

impl TeamsPage {
    pub fn build(base: Base, teams: &[TeamRow], cat: &Catalog) -> TeamsPage {
        TeamsPage {
            pager: Default::default(),
            base,
            teams: teams
                .iter()
                .map(|t| TeamView {
                    budget_view: Default::default(),
                    id: t.id,
                    name: t.name.clone(),
                    user_count: t.users,
                    override_count: cat
                        .models
                        .iter()
                        .filter(|m| m.team_price.contains_key(&t.id))
                        .count(),
                })
                .collect(),
        }
    }
}

// -------------------------------------------------------------------- keys

pub struct KeyView {
    pub capture_bodies: bool,
    pub budget_view: BudgetView,
    pub expires: String,
    pub expired: bool,
    pub provider_names: String,
    pub id: i64,
    pub user: String,
    pub prefix: String,
    pub label: String,
    /// "All", or the allowed names.
    pub models: String,
    /// "All API-key Providers", or the allowed names.
    pub access: String,
    /// "RPM 60, TPM unlimited", or "Unlimited".
    pub limits: String,
    pub created: Stamp,
    pub last_used: Stamp,
    pub revoked: Stamp,
    pub rpm_limit: String,
    pub tpm_limit: String,
}

#[derive(Template)]
#[template(path = "keys.html")]
pub struct KeysPage {
    pub provider_choices: Option<Vec<crate::db::ControlChoice>>,
    pub user_choices: Option<Vec<crate::db::ControlChoice>>,
    pub revoked_open: bool,
    pub has_users: bool,
    pub pager: crate::pagination::Pager,
    pub revoked_pager: crate::pagination::Pager,
    pub base: Base,
    pub keys: Vec<KeyView>,
    pub revoked: Vec<KeyView>,
    pub new_key: Option<String>,
    /// The User name when the list shows the keys of one User.
    pub filter_user: Option<String>,
}

pub struct KeysData<'a> {
    pub keys: &'a [KeyRow],
    pub new_key: Option<String>,
}

fn limit_text(v: Option<i64>) -> String {
    v.map(int).unwrap_or_else(|| "unlimited".into())
}

impl KeysPage {
    pub fn build(base: Base, d: KeysData) -> KeysPage {
        let (mut keys, mut revoked) = (vec![], vec![]);
        for k in d.keys {
            let selected: Vec<String> = k
                .allowed_providers
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();
            let view = KeyView {
                capture_bodies: k.capture_bodies,
                budget_view: Default::default(),
                expires: String::new(),
                expired: false,
                provider_names: selected.join(", "),
                id: k.id,
                user: k.user.clone(),
                prefix: k.prefix.clone(),
                label: k.label.clone(),
                access: if selected.is_empty() {
                    "All API-key Providers".into()
                } else {
                    selected.join(", ")
                },
                models: k
                    .allowed_models
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
                    .filter(|v| !v.is_empty())
                    .map(|v| v.join(", "))
                    .unwrap_or_else(|| "All".into()),
                limits: if k.rpm_limit.is_none() && k.tpm_limit.is_none() {
                    "Unlimited".into()
                } else {
                    format!(
                        "RPM {}, TPM {}",
                        limit_text(k.rpm_limit),
                        limit_text(k.tpm_limit)
                    )
                },
                created: date(k.created_at),
                last_used: opt_date(k.last_used_at),
                revoked: k.revoked_at.map(date).unwrap_or_default(),
                rpm_limit: k.rpm_limit.map(|v| v.to_string()).unwrap_or_default(),
                tpm_limit: k.tpm_limit.map(|v| v.to_string()).unwrap_or_default(),
            };
            if k.revoked_at.is_some() {
                revoked.push(view);
            } else {
                keys.push(view);
            }
        }
        KeysPage {
            provider_choices: None,
            user_choices: None,
            revoked_open: false,
            has_users: true,
            pager: Default::default(),
            revoked_pager: Default::default(),
            base,
            keys,
            revoked,
            new_key: d.new_key,
            filter_user: None,
        }
    }
}

// ------------------------------------------------------------------ models

/// The display text of one set of rates. `RatesView::default()` is an empty
/// form: no value in any field.
#[derive(Clone, Default)]
pub struct RatesView {
    pub batch_multiplier_bp: String,
    pub service_units: String,
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
                x.service_units,
            ]
            .map(fmt_rate)
        };
        let b = r(&c.base);
        RatesView {
            batch_multiplier_bp: c
                .batch_multiplier_bp
                .map(|v| v.to_string())
                .unwrap_or_default(),
            input: b[0].clone(),
            output: b[1].clone(),
            cache_read: b[2].clone(),
            cache_write_5m: b[3].clone(),
            cache_write_1h: b[4].clone(),
            image_input: b[5].clone(),
            image_output: b[6].clone(),
            service_units: b[7].clone(),
            has_image: std::iter::once(&c.base)
                .chain(c.tiers.iter().map(|t| &t.rates))
                .any(|x| x.image_input != 0 || x.image_output != 0),
            tiers: c
                .tiers
                .iter()
                .map(|t| {
                    let v = r(&t.rates);
                    // Leave out the image rates when they are zero.
                    let n = if t.rates.service_units != 0 {
                        8
                    } else if t.rates.image_input == 0 && t.rates.image_output == 0 {
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

    /// Label and value pairs for a compact rates list: Input and Output
    /// always, the other Token Classes when they are not zero.
    pub fn pairs(&self) -> Vec<(&'static str, String)> {
        let mut v = vec![
            ("Input", self.input.clone()),
            ("Output", self.output.clone()),
        ];
        for (label, value) in [
            ("Cache read", &self.cache_read),
            ("Cache write 5m", &self.cache_write_5m),
            ("Cache write 1h", &self.cache_write_1h),
        ] {
            if value != "0.00" {
                v.push((label, value.clone()));
            }
        }
        if self.has_image {
            v.push(("Image input", self.image_input.clone()));
            v.push(("Image output", self.image_output.clone()));
        }
        if self.service_units != "0.00" {
            v.push(("Service units", self.service_units.clone()));
        }
        if self.tier_count > 0 {
            v.push(("Context Tiers", self.tier_count.to_string()));
        }
        v
    }
}

/// A percentage with its sign, for example "+20%" or "-5%".
fn signed_percent(bp: i64) -> String {
    let sign = if bp < 0 { "-" } else { "+" };
    format!("{sign}{}%", bp_percent(bp.abs()))
}

fn describe_price(spec: Option<&PriceSpec>, default_bp: i64) -> String {
    match spec {
        None => format!("Default Markup ({})", signed_percent(default_bp)),
        Some(PriceSpec::MarkupBp(bp)) => format!("Markup {}", signed_percent(*bp)),
        Some(PriceSpec::Rates(_)) => "Explicit rates".into(),
    }
}

/// "Input 0.12, Output 0.60" for the base rates.
fn rates_line(r: &Rates) -> String {
    format!("Input {}, Output {}", fmt_rate(r.input), fmt_rate(r.output))
}

pub struct ModelView {
    pub id: i64,
    pub name: String,
    pub provider: String,
    pub aliases: String,
    pub cost: Option<RatesView>,
    /// A short tag: "+20% default", "+25%" or "Explicit rates".
    pub price_tag: String,
    /// The full Price description, for the tag's `title`.
    pub price_title: String,
    pub overrides: usize,
}

#[derive(Template)]
#[template(path = "models.html")]
pub struct ModelsPage {
    pub provider_choices: Option<Vec<crate::db::ControlChoice>>,
    /// The Provider that "Add a Model" preselects (`?provider=`).
    pub new_provider: String,
    pub pager: crate::pagination::Pager,
    pub base: Base,
    pub models: Vec<ModelView>,
    pub providers: Vec<String>,
    pub default_markup: String,
    /// An empty rates form for "Add model".
    pub blank: RatesView,
}

impl ModelsPage {
    pub fn build(
        base: Base,
        models: &[&ModelEntry],
        default_bp: i64,
        providers: Vec<String>,
    ) -> ModelsPage {
        ModelsPage {
            provider_choices: None,
            new_provider: String::new(),
            pager: Default::default(),
            base,
            models: models
                .iter()
                .map(|m| {
                    let price = m.price.as_ref().map(|(_, p)| p);
                    ModelView {
                        id: m.id,
                        name: m.name.clone(),
                        provider: m.provider.clone(),
                        aliases: m.aliases.join(", "),
                        cost: m.cost.as_ref().map(|(_, c)| RatesView::new(c)),
                        price_tag: match price {
                            None => format!("{} default", signed_percent(default_bp)),
                            Some(PriceSpec::MarkupBp(bp)) => signed_percent(*bp),
                            Some(PriceSpec::Rates(_)) => "Explicit rates".into(),
                        },
                        price_title: match price {
                            Some(PriceSpec::Rates(r)) => {
                                format!("Explicit rates: {}", rates_line(&r.base))
                            }
                            _ => describe_price(price, default_bp),
                        },
                        overrides: m.team_price.len(),
                    }
                })
                .collect(),
            providers,
            default_markup: bp_percent(default_bp),
            blank: RatesView::default(),
        }
    }
}

pub struct OverrideView {
    pub team_id: i64,
    pub team: String,
    pub desc: String,
}

pub struct HistView {
    pub source: String,
    pub id: i64,
    /// "Cost Card" or "Price Card".
    pub kind: String,
    /// "Model" or "Team: research".
    pub scope: String,
    /// The rates of a Cost Card or an explicit Price Card.
    pub rates: Option<RatesView>,
    /// The text for a Markup or a removed card.
    pub desc: String,
    pub created: Stamp,
}

#[derive(Template)]
#[template(path = "model.html")]
pub struct ModelPage {
    pub models_dev: super::models_dev::DetailView,
    pub provider_choices: Option<Vec<crate::db::ControlChoice>>,
    pub team_choices: Option<Vec<crate::db::ControlChoice>>,
    pub provider_name: String,
    pub pager: crate::pagination::Pager,
    pub override_pager: crate::pagination::Pager,
    pub base: Base,
    pub id: i64,
    pub name: String,
    pub aliases: String,
    /// The current Cost Card, or an empty form when there is none.
    pub cost: RatesView,
    pub has_cost: bool,
    pub price_mode: &'static str,
    pub price_markup: String,
    /// The explicit Price Card rates, only when the Price Card has rates.
    /// Never the Cost Card: a copy must be an explicit action.
    pub price_rates: RatesView,
    pub price_desc: String,
    /// What Users pay now (base rates), when the Model has a Cost Card.
    pub price_now: Option<String>,
    pub default_markup: String,
    /// The Cost Card input and output rates in dollars, for the Markup
    /// preview script.
    pub cost_in: String,
    pub cost_out: String,
    /// An empty rates form for the Team override.
    pub blank: RatesView,
    pub overrides: Vec<OverrideView>,
    pub history: Vec<HistView>,
}

impl ModelPage {
    pub fn build(
        base: Base,
        m: &ModelEntry,
        default_bp: i64,
        history: &[CardHistoryRow],
        teams: &[TeamRow],
    ) -> ModelPage {
        let cost = m.cost.as_ref().map(|(_, c)| c);
        let price = m.price.as_ref().map(|(_, p)| p);
        let (price_mode, price_markup, price_rates) = match price {
            None => ("default", String::new(), RatesView::default()),
            Some(PriceSpec::MarkupBp(bp)) => ("markup", bp_percent(*bp), RatesView::default()),
            Some(PriceSpec::Rates(r)) => ("rates", String::new(), RatesView::new(r)),
        };
        let price_now = cost.map(|c| {
            let now = match price {
                Some(p) => p.resolve(c),
                None => c.marked_up(default_bp),
            };
            rates_line(&now.base)
        });
        let overrides: Vec<OverrideView> = teams
            .iter()
            .filter_map(|t| m.team_price.get(&t.id).map(|(_, spec)| (t, spec)))
            .map(|(team, spec)| OverrideView {
                team_id: team.id,
                team: team.name.clone(),
                desc: match spec {
                    PriceSpec::Rates(r) => format!("Explicit rates: {}", rates_line(&r.base)),
                    _ => describe_price(Some(spec), default_bp),
                },
            })
            .collect();
        let history = history
            .iter()
            .map(|h| {
                let (rates, desc) = if h.kind == "cost" {
                    match serde_json::from_str::<CardRates>(&h.spec) {
                        Ok(c) => (Some(RatesView::new(&c)), String::new()),
                        Err(_) => (None, h.spec.clone()),
                    }
                } else {
                    match serde_json::from_str::<Option<PriceSpec>>(&h.spec) {
                        Ok(None) if h.team.is_some() => {
                            (None, "Removed: the Team pays the Model Price".into())
                        }
                        Ok(None) => (None, "Removed: Default Markup".into()),
                        Ok(Some(PriceSpec::Rates(r))) => (Some(RatesView::new(&r)), String::new()),
                        Ok(Some(p)) => (None, describe_price(Some(&p), default_bp)),
                        Err(_) => (None, h.spec.clone()),
                    }
                };
                HistView {
                    source: h.source.clone(),
                    id: h.id,
                    kind: if h.kind == "cost" {
                        "Cost Card".into()
                    } else {
                        "Price Card".into()
                    },
                    scope: h
                        .team
                        .as_ref()
                        .map(|t| format!("Team: {t}"))
                        .unwrap_or_else(|| "Model".into()),
                    rates,
                    desc,
                    created: date(h.created_at),
                }
            })
            .collect();
        let dollars = |micros: i64| format!("{}", micros as f64 / 1e6);
        ModelPage {
            models_dev: Default::default(),
            provider_choices: None,
            team_choices: None,
            provider_name: m.provider.clone(),
            pager: Default::default(),
            override_pager: Default::default(),
            base,
            id: m.id,
            name: m.name.clone(),
            aliases: m.aliases.join(", "),
            cost: cost.map(RatesView::new).unwrap_or_default(),
            has_cost: cost.is_some(),
            price_mode,
            price_markup,
            price_rates,
            price_desc: describe_price(price, default_bp),
            price_now,
            default_markup: bp_percent(default_bp),
            cost_in: cost.map(|c| dollars(c.base.input)).unwrap_or_default(),
            cost_out: cost.map(|c| dollars(c.base.output)).unwrap_or_default(),
            blank: RatesView::default(),
            overrides,
            history,
        }
    }
}

// ------------------------------------------------------------------ ledger

/// A point in time from Unix seconds, or None when out of range.
pub fn stamp_opt(ts_s: i64) -> Option<Stamp> {
    Some(stamp(ts_s)).filter(|s| !s.iso.is_empty())
}

pub struct LedgerView {
    pub captured: bool,
    pub service_units: String,
    pub id: i64,
    pub time: Stamp,
    pub user: String,
    pub provider: String,
    pub endpoint: String,
    pub model: String,
    pub status: i64,
    pub streamed: bool,
    /// "input / output", with the full breakdown in `tokens_title`.
    pub tokens: String,
    pub tokens_title: String,
    pub input: String,
    pub output: String,
    pub cache_read: String,
    pub cache_write_5m: String,
    pub cache_write_1h: String,
    pub image_input: String,
    pub image_output: String,
    pub attempts: i64,
    pub session_affinity: Option<String>,
    pub key: String,
    pub cost: Money,
    pub reported_cost: Option<Money>,
    pub price: Money,
    pub unpriced: bool,
    pub estimated: bool,
    pub aborted: bool,
    pub latency: String,
    pub ttft: String,
}

/// The values of the Ledger filter bar.
#[derive(Default)]
pub struct LedgerFilterView {
    pub status: String,
    pub error_class: String,
    pub provider_key: String,
    pub has_retries: String,
    pub provider_choices: Option<Vec<crate::db::ControlChoice>>,
    pub user_choices: Option<Vec<crate::db::ControlChoice>>,
    pub provider: String,
    pub user: String,
    pub model: String,
    pub from: String,
    pub to: String,
    /// True when a filter other than the tab is active.
    pub active: bool,
}

#[derive(Template)]
#[template(path = "ledger.html")]
pub struct LedgerPage {
    pub detail_id: Option<i64>,
    pub attempts: Vec<crate::observability::Attempt>,
    pub bodies: Option<crate::observability::Bodies>,
    pub pager: crate::pagination::Pager,
    pub base: Base,
    pub rows: Vec<LedgerView>,
    pub filter: &'static str,
    pub filters: LedgerFilterView,
    /// Links for the tabs: the current filters with another tab.
    pub tab_all: String,
    pub tab_unpriced: String,
    pub tab_estimated: String,
    pub tab_errors: String,
    /// A message about the filter values, for example a bad date.
    pub error: Option<String>,
    pub page_size: i64,
    pub unpriced_models: Vec<String>,
}

fn endpoint_label(e: &str) -> String {
    match e {
        "chat_completions" => "Chat Completions".into(),
        "messages" => "Messages".into(),
        "embeddings" => "Embeddings".into(),
        "responses" => "Responses".into(),
        "completions" => "Completions".into(),
        "other" => "Other".into(),
        e => e.replace('_', " "),
    }
}

impl LedgerView {
    pub fn build(r: &LedgerRow, key_label: &dyn Fn(&str, i64) -> Option<String>) -> LedgerView {
        let time = Utc
            .timestamp_millis_opt(r.ts_ms)
            .single()
            .map(|t| Stamp {
                text: t.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
                short: t.format("%Y-%m-%d %H:%M:%S").to_string(),
                iso: t.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            })
            .unwrap_or_default();
        LedgerView {
            captured: r.captured,
            service_units: int(r.service_units),
            id: r.id,
            time,
            user: r.user.clone(),
            provider: r.provider.clone(),
            endpoint: endpoint_label(&r.endpoint),
            model: r
                .model
                .clone()
                .or_else(|| r.requested_model.clone())
                .unwrap_or_else(|| "-".into()),
            status: r.status,
            streamed: r.streamed,
            tokens: format!("{} / {}", int(r.input), int(r.output)),
            tokens_title: format!(
                "Input {}, output {}, cache read {}, cache write 5m {}, cache write 1h {}, image input {}, image output {}",
                int(r.input),
                int(r.output),
                int(r.cache_read),
                int(r.cache_write_5m),
                int(r.cache_write_1h),
                int(r.image_input),
                int(r.image_output)
            ),
            input: int(r.input),
            output: int(r.output),
            cache_read: int(r.cache_read),
            cache_write_5m: int(r.cache_write_5m),
            cache_write_1h: int(r.cache_write_1h),
            image_input: int(r.image_input),
            image_output: int(r.image_output),
            attempts: r.attempts,
            session_affinity: r.session_affinity.clone(),
            key: r
                .provider_key
                .map(|k| key_label(&r.provider, k).unwrap_or_else(|| format!("Key {}", k + 1)))
                .unwrap_or_else(|| "-".into()),
            cost: Money {
                text: money_row(r.cost_micros),
                exact: money_exact(r.cost_micros),
            },
            reported_cost: r.reported_cost_micros.map(|value| Money {
                text: money_row(value),
                exact: money_exact(value),
            }),
            price: Money {
                text: money_row(r.price_micros),
                exact: money_exact(r.price_micros),
            },
            unpriced: r.unpriced,
            estimated: r.estimated,
            aborted: r.client_aborted,
            latency: format!("{} ms", int(r.latency_ms)),
            ttft: r
                .ttft_ms
                .map(|t| format!("{} ms", int(t)))
                .unwrap_or_else(|| "-".into()),
        }
    }
}

impl LedgerPage {
    /// Column count of the table, for the `colspan` of full-width rows.
    pub fn columns(&self) -> usize {
        if self.base.simple { 8 } else { 10 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_formatting() {
        assert_eq!(int(1_234_567), "1,234,567");
        assert_eq!(int(-1000), "-1,000");
        assert_eq!(money(1_234_567_890), "$1,234.57");
        assert_eq!(money(999_999), "$1.0000");
        assert_eq!(money(4_500), "$0.0045");
        assert_eq!(money(-50), "-$0.0001");
        assert_eq!(money(0), "$0.00");
        assert_eq!(money_row(1_234_567_890), "$1,234.5679");
        assert_eq!(money_row(30), "<$0.0001");
        assert_eq!(money_row(-30), "-<$0.0001");
        assert_eq!(money_exact(1_234_567), "$1.234567");
        assert_eq!(stamp(0).text, "1970-01-01 00:00 UTC");
        assert_eq!(bp_percent(2_500), "25");
        assert_eq!(bp_percent(1_250), "12.5");
    }
}
