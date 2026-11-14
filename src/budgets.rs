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
}

impl Period {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "none" => Self::None,
            "daily" => Self::Daily,
            "weekly" => Self::Weekly,
            "monthly" => Self::Monthly,
            _ => anyhow::bail!("Choose none, daily, weekly or monthly."),
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
            Self::Monthly => {
                let (y, m) = if dt.month() == 12 {
                    (dt.year() + 1, 1)
                } else {
                    (dt.year(), dt.month() + 1)
                };
                (
                    Utc.with_ymd_and_hms(dt.year(), dt.month(), 1, 0, 0, 0)
                        .unwrap()
                        .timestamp(),
                    Some(Utc.with_ymd_and_hms(y, m, 1, 0, 0, 0).unwrap().timestamp()),
                )
            }
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
    Ok(sqlx::query_scalar(&format!("SELECT IFNULL(SUM(price_micros),0) FROM ledger INDEXED BY {index} WHERE {col}=? AND ts_ms>=? AND ts_ms<?"))
        .bind(id).bind(start.saturating_mul(1000)).bind(end.map_or(i64::MAX, |e|e.saturating_mul(1000))).fetch_one(db).await?)
}

#[derive(Debug)]
pub struct Budget {
    pub subject: Subject,
    pub limit: Option<i64>,
    pub period: String,
    pub start: i64,
    pub end: Option<i64>,
    pub spent: i64,
}
pub async fn current(db: &Db, subject: Subject, now: i64) -> Result<Budget> {
    load(db, subject, now, true).await
}
async fn load(db: &Db, subject: Subject, now: i64, show_unlimited: bool) -> Result<Budget> {
    let (table, id) = subject.table();
    let (limit, period): (Option<i64>, String) = sqlx::query_as(&format!(
        "SELECT budget_micros,budget_period FROM {table} WHERE id=?"
    ))
    .bind(id)
    .fetch_one(db)
    .await?;
    let (start, end) = Period::parse(&period)?.bounds(now);
    Ok(Budget {
        subject,
        limit,
        period,
        start,
        end,
        spent: if limit.is_some() || show_unlimited {
            spend(db, subject, start, end).await?
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
            budgets.push(budget);
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
    let _guard = st.budget_check.lock().await;
    let budgets = applicable(&st.db, auth, now).await?;
    Ok(st.reserve_budgets(&budgets, amount))
}
