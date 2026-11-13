//! Shared bounds, keyset cursors and bookmarkable pager links.
use serde::Deserialize;
use sqlx::{QueryBuilder, Sqlite};

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PageQuery {
    #[serde(default, deserialize_with = "number")]
    pub after: Option<i64>,
    #[serde(default, deserialize_with = "number")]
    pub before: Option<i64>,
    #[serde(default, deserialize_with = "number")]
    pub limit: Option<i64>,
    #[serde(default)]
    pub q: String,
    #[serde(default, deserialize_with = "number")]
    pub page: Option<i64>,
}

// Flattened query maps contain strings, even for numeric fields.
fn number<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    let s = Option::<String>::deserialize(d)?;
    s.filter(|s| !s.is_empty())
        .map(|s| s.parse().map_err(serde::de::Error::custom))
        .transpose()
}

impl PageQuery {
    pub fn limit(&self) -> i64 {
        self.limit.unwrap_or(50).clamp(1, 200)
    }
    pub fn offset(&self) -> i64 {
        self.page
            .unwrap_or(1)
            .max(1)
            .saturating_sub(1)
            .saturating_mul(self.limit())
    }
    pub fn keyset(&self, b: &mut QueryBuilder<'_, Sqlite>, id: &str) {
        self.keyset_order(b, id, false);
    }
    pub fn keyset_desc(&self, b: &mut QueryBuilder<'_, Sqlite>, id: &str) {
        self.keyset_order(b, id, true);
    }
    fn keyset_order(&self, b: &mut QueryBuilder<'_, Sqlite>, id: &str, desc: bool) {
        if let Some(v) = self.before {
            b.push(format!(" AND {id} {} ", if desc { ">" } else { "<" }))
                .push_bind(v);
        } else if let Some(v) = self.after {
            b.push(format!(" AND {id} {} ", if desc { "<" } else { ">" }))
                .push_bind(v);
        }
        b.push(format!(
            " ORDER BY {id} {} LIMIT ",
            if self.before.is_some() != desc {
                "DESC"
            } else {
                "ASC"
            }
        ))
        .push_bind(self.limit() + 1);
    }
}

pub struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<i64>,
    pub prev: Option<i64>,
}

impl<T> Page<T> {
    pub fn new(mut items: Vec<T>, q: &PageQuery, id: impl Fn(&T) -> i64) -> Self {
        let more = items.len() as i64 > q.limit();
        items.truncate(q.limit() as usize);
        let reverse = q.before.is_some();
        if reverse {
            items.reverse();
        }
        let next = if (reverse && q.before.is_some()) || (!reverse && more) {
            items.last().map(&id)
        } else {
            None
        };
        let prev = if (reverse && more) || (!reverse && q.after.is_some()) {
            items.first().map(id)
        } else {
            None
        };
        Self { items, next, prev }
    }
}

#[derive(Default)]
pub struct Pager {
    pub label: String,
    pub next: Option<String>,
    pub prev: Option<String>,
    pub shown: usize,
    pub search: String,
    pub filters: Vec<(String, String)>,
}

/// Preserve every filter, including parameters used by another paged section.
pub fn link(uri: &str, changes: &[(&str, Option<String>)]) -> String {
    let mut url = reqwest::Url::parse(&format!("http://local{uri}")).expect("request URI");
    let pairs: Vec<_> = url
        .query_pairs()
        .filter(|(k, _)| !changes.iter().any(|(key, _)| k == *key))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    url.set_query(None);
    {
        let mut q = url.query_pairs_mut();
        q.extend_pairs(pairs);
        for (k, v) in changes {
            if let Some(v) = v {
                q.append_pair(k, v);
            }
        }
    }
    format!(
        "{}{}",
        url.path(),
        url.query()
            .filter(|q| !q.is_empty())
            .map(|q| format!("?{q}"))
            .unwrap_or_default()
    )
}

impl Pager {
    fn filters(uri: &str) -> Vec<(String, String)> {
        reqwest::Url::parse(&format!("http://local{uri}"))
            .expect("request URI")
            .query_pairs()
            .filter(|(k, _)| {
                matches!(
                    k.as_ref(),
                    "team" | "user" | "limit" | "range" | "group" | "sort" | "direction"
                )
            })
            .take(20)
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }
    pub fn keyset<T>(uri: &str, q: &PageQuery, page: &Page<T>) -> Self {
        Self {
            label: "Pages".into(),
            next: page
                .next
                .map(|id| link(uri, &[("before", None), ("after", Some(id.to_string()))])),
            prev: page
                .prev
                .map(|id| link(uri, &[("after", None), ("before", Some(id.to_string()))])),
            shown: page.items.len(),
            search: q.q.clone(),
            filters: Self::filters(uri),
        }
    }
    /// Offset pages are only for small configuration tables and aggregate groups,
    /// whose rank changes when their settings or totals change, not append logs.
    pub fn offset(uri: &str, q: &PageQuery, fetched: usize) -> Self {
        let n = q.page.unwrap_or(1).max(1);
        Self {
            label: "Pages".into(),
            next: (fetched as i64 > q.limit())
                .then(|| link(uri, &[("page", Some(n.saturating_add(1).to_string()))])),
            prev: (n > 1).then(|| link(uri, &[("page", Some((n - 1).to_string()))])),
            shown: fetched.min(q.limit() as usize),
            search: q.q.clone(),
            filters: Self::filters(uri),
        }
    }
}
