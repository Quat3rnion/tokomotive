//! UTC Budget periods and indexed, immutable Ledger spend.
use crate::{
    db::{Db, KeyAuth},
    state::{AppState, Reservation},
};
use anyhow::Result;
use chrono::{Datelike, Days, TimeZone, Utc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    None,
    Daily,
    Weekly,
    Monthly,
    Quarterly,
    Yearly,
}

impl Period {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "none" => Self::None,
            "daily" => Self::Daily,
            "weekly" => Self::Weekly,
            "monthly" => Self::Monthly,
            "quarterly" => Self::Quarterly,
            "yearly" => Self::Yearly,
            _ => anyhow::bail!("Choose none, daily, weekly, monthly, quarterly or yearly."),
        })
    }
    pub fn bounds(self, now: i64) -> (i64, Option<i64>) {
        let dt = Utc.timestamp_opt(now, 0).single().expect("valid date");
        let day = dt.date_naive();
        let midnight = |d: chrono::NaiveDate| d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp();
        match self {
            Self::None => (0, None),
            Self::Daily => (
                midnight(day),
                Some(midnight(day.checked_add_days(Days::new(1)).unwrap())),
            ),
            Self::Weekly => {
                let monday = day
                    .checked_sub_days(Days::new(day.weekday().num_days_from_monday() as u64))
                    .unwrap();
                (
                    midnight(monday),
                    Some(midnight(monday.checked_add_days(Days::new(7)).unwrap())),
                )
            }
            Self::Monthly | Self::Quarterly | Self::Yearly => {
                let months = match self {
                    Self::Quarterly => 3,
                    Self::Yearly => 12,
                    _ => 1,
                };
                let month = (dt.month0() / months) * months + 1;
                let (y, m) = if month + months > 12 {
                    (dt.year() + 1, 1)
                } else {
                    (dt.year(), month + months)
                };
                (
                    Utc.with_ymd_and_hms(dt.year(), month, 1, 0, 0, 0)
                        .unwrap()
                        .timestamp(),
                    Some(Utc.with_ymd_and_hms(y, m, 1, 0, 0, 0).unwrap().timestamp()),
                )
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    Calendar,
    Rolling(u16),
}
impl Window {
    pub fn parse(kind: &str, days: i64) -> Result<Self> {
        anyhow::ensure!(
            (1..=366).contains(&days),
            "Enter rolling days from 1 to 366."
        );
        match kind {
            "calendar" => Ok(Self::Calendar),
            "rolling" => Ok(Self::Rolling(days as u16)),
            _ => anyhow::bail!("Choose a calendar or rolling Budget window."),
        }
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum Subject {
    User(i64),
    Team(i64),
    Key(i64),
}
impl Subject {
    pub fn name(self) -> String {
        match self {
            Self::User(id) => format!("user:{id}"),
            Self::Team(id) => format!("team:{id}"),
            Self::Key(id) => format!("key:{id}"),
        }
    }
    fn columns(self) -> (&'static str, &'static str, i64) {
        match self {
            Self::User(id) => ("user_id", "ledger_user_ts", id),
            Self::Team(id) => ("team_id", "ledger_team_ts", id),
            Self::Key(id) => ("key_id", "ledger_key_ts", id),
        }
    }
    fn table(self) -> (&'static str, i64) {
        match self {
            Self::User(id) => ("users", id),
            Self::Team(id) => ("teams", id),
            Self::Key(id) => ("virtual_keys", id),
        }
    }
}

pub async fn spend(db: &Db, subject: Subject, start: i64, end: Option<i64>) -> Result<i64> {
    let (col, index, id) = subject.columns();
    let start = start.saturating_mul(1000);
    let end = end.map_or(i64::MAX, |e| e.saturating_mul(1000));
    // Replay commits and removes the pending charge under the write side of
    // this lock. The Ledger snapshot and pending total cannot miss a handoff.
    let mut conn = db.acquire().await?;
    let pending = db.spool.pending.read().await;
    let ledger: i64 = sqlx::query_scalar(&format!("SELECT IFNULL(SUM(price_micros),0) FROM ledger INDEXED BY {index} WHERE {col}=? AND ts_ms>=? AND ts_ms<?"))
        .bind(id).bind(start).bind(end).fetch_one(&mut *conn).await?;
    Ok(pending.values().fold(ledger, |total, charge| {
        total.saturating_add(charge.spend(subject, start, end))
    }))
}

#[derive(Debug)]
pub struct Budget {
    pub subject: Subject,
    pub limit: Option<i64>,
    pub period: String,
    pub window: Window,
    pub days: i64,
    pub start: i64,
    pub end: Option<i64>,
    pub spent: i64,
}
impl Budget {
    /// Rolling windows have no reset. Keep their live holds across clock ticks.
    pub fn reservation_start(&self) -> i64 {
        match self.window {
            Window::Calendar => self.start,
            Window::Rolling(days) => -i64::from(days),
        }
    }
}
pub async fn current(db: &Db, subject: Subject, now: i64) -> Result<Budget> {
    load(db, subject, now, true).await
}
async fn load(db: &Db, subject: Subject, now: i64, show_unlimited: bool) -> Result<Budget> {
    let (table, id) = subject.table();
    let (limit, period, kind, days): (Option<i64>, String, String, i64) = sqlx::query_as(&format!(
        "SELECT budget_micros,budget_period,budget_window,budget_days FROM {table} WHERE id=?"
    ))
    .bind(id)
    .fetch_one(db)
    .await?;
    let window = Window::parse(&kind, days)?;
    let (start, end) = match window {
        Window::Calendar => Period::parse(&period)?.bounds(now),
        Window::Rolling(days) => (now.saturating_sub(i64::from(days) * 86400), None),
    };
    // `now` has second precision. Include that whole second, but not the next.
    let spend_end = match window {
        Window::Calendar => end,
        Window::Rolling(_) => Some(now.saturating_add(1)),
    };
    Ok(Budget {
        subject,
        limit,
        period,
        window,
        days,
        start,
        end,
        spent: if limit.is_some() || show_unlimited {
            spend(db, subject, start, spend_end).await?
        } else {
            0
        },
    })
}
pub fn subjects(auth: &KeyAuth) -> Vec<Subject> {
    let mut subjects = vec![Subject::User(auth.user_id), Subject::Key(auth.key_id)];
    if let Some(id) = auth.team_id {
        subjects.push(Subject::Team(id));
    }
    subjects
}
pub async fn applicable(db: &Db, auth: &KeyAuth, now: i64) -> Result<Vec<Budget>> {
    let mut budgets = vec![];
    for subject in subjects(auth) {
        let budget = load(db, subject, now, false).await?;
        if budget.limit.is_some() {
            let (column, _, id) = subject.columns();
            let held: i64 = sqlx::query_scalar(&format!(
                "SELECT IFNULL(SUM(held_micros),0) FROM batch_jobs WHERE {column}=? AND accounted=0"
            ))
            .bind(id)
            .fetch_one(db)
            .await?;
            budgets.push(Budget {
                spent: budget.spent.saturating_add(held),
                ..budget
            });
        }
    }
    Ok(budgets)
}
pub async fn reserve(
    st: &AppState,
    auth: &KeyAuth,
    amount: i64,
    now: i64,
) -> Result<Option<Reservation>> {
    let (budgets, reservation) = {
        let _guard = st.budget_check.lock().await;
        let budgets = applicable(&st.db, auth, now).await?;
        let reservation = st.reserve_budgets(&budgets, amount);
        (budgets, reservation)
    };
    // An idle rolling window can fall below its alert re-arm level before a
    // new request. Check the existing snapshot before its new charge arrives.
    // Alert storage is best-effort and never holds the Budget lock.
    if st.settings().mode != "simple" {
        for budget in &budgets {
            if let Err(error) = crate::alerts::rearm_rolling(&st.db, budget).await {
                tracing::warn!(%error, "rolling Budget alert re-arm failed");
            }
        }
    }
    Ok(reservation)
}
