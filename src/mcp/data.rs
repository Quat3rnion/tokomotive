use crate::{
    db::{Db, KeyAuth},
    state::AppState,
};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Server {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub auth: String,
    pub header_name: String,
    pub secret: Option<String>,
    pub allow_private: bool,
    pub enabled: bool,
}
// Tool names: https://modelcontextprotocol.io/specification/2025-11-25/server/tools#tool-names
pub fn valid_name(name: &str, max: usize) -> bool {
    !name.is_empty()
        && name.len() <= max
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
}
pub fn valid_server_name(name: &str) -> bool {
    valid_name(name, 32)
        && !name.bytes().all(|b| b == b'.')
        && !name.starts_with('_')
        && !name.ends_with('_')
        && !name.contains("__")
}
pub async fn servers(db: &Db) -> anyhow::Result<Vec<Server>> {
    let mut servers: Vec<Server> = sqlx::query_as("SELECT id,name,url,auth,header_name,secret,allow_private,enabled FROM mcp_servers ORDER BY name LIMIT 65").fetch_all(db).await?;
    servers.retain(|s| {
        if valid_server_name(&s.name) {
            true
        } else {
            tracing::warn!(
                server_id = s.id,
                error_class = "invalid_server_name",
                "MCP server omitted"
            );
            false
        }
    });
    Ok(servers)
}
#[derive(Default)]
pub struct Access {
    pub all: bool,
    pub patterns: Vec<String>,
}
impl Access {
    pub fn permits(&self, server: &str, tool: &str) -> bool {
        self.all
            || self
                .patterns
                .iter()
                .any(|p| p == &format!("{server}/*") || p == &format!("{server}/{tool}"))
    }
    pub fn server(&self, server: &str) -> bool {
        self.all
            || self
                .patterns
                .iter()
                .any(|p| p.strip_prefix(server).is_some_and(|p| p.starts_with('/')))
    }
    pub fn display(&self) -> String {
        self.patterns.join(",")
    }
}
pub async fn access(db: &Db, kind: &str, id: i64) -> anyhow::Result<Access> {
    let table = if kind == "teams" {
        "teams"
    } else {
        "virtual_keys"
    };
    let (all, patterns): (bool, String) =
        sqlx::query_as(&format!("SELECT mcp_all,mcp_allow FROM {table} WHERE id=?"))
            .bind(id)
            .fetch_one(db)
            .await?;
    Ok(Access {
        all,
        patterns: serde_json::from_str(&patterns)?,
    })
}
pub struct Permissions {
    key: Access,
    team: Option<Access>,
}
impl Permissions {
    pub async fn load(st: &AppState, auth: &KeyAuth) -> anyhow::Result<Self> {
        Ok(Self {
            key: access(&st.db, "keys", auth.key_id).await?,
            team: match auth.team_id {
                Some(id) => Some(access(&st.db, "teams", id).await?),
                None => None,
            },
        })
    }
    pub fn permits(&self, server: &str, tool: &str) -> bool {
        self.key.permits(server, tool) && self.team.as_ref().is_none_or(|t| t.permits(server, tool))
    }
    pub fn server(&self, server: &str) -> bool {
        self.key.server(server) && self.team.as_ref().is_none_or(|t| t.server(server))
    }
}
pub async fn cache_tools(st: &AppState, id: i64, tools: &[Value]) -> anyhow::Result<()> {
    sqlx::query("UPDATE mcp_servers SET tools_json=? WHERE id=?")
        .bind(serde_json::to_string(tools)?)
        .bind(id)
        .execute(&st.db)
        .await?;
    Ok(())
}

#[derive(Clone, Copy)]
pub struct LogRetention {
    pub days: i64,
    pub max_rows: i64,
}
impl LogRetention {
    pub async fn load(db: &Db) -> anyhow::Result<Self> {
        Ok(Self {
            days: crate::settings::value(db, "mcp_retention_days")
                .await?
                .and_then(|v| v.parse().ok())
                .unwrap_or(30)
                .clamp(1, 3650),
            max_rows: crate::settings::value(db, "mcp_max_rows")
                .await?
                .and_then(|v| v.parse().ok())
                .unwrap_or(100_000)
                .clamp(1, 1_000_000),
        })
    }
}
/// Delete at most 500 rows per tick, including incomplete calls.
pub async fn purge_calls(db: &Db, now_ms: i64) -> anyhow::Result<u64> {
    let retention = LogRetention::load(db).await?;
    let mut tx = crate::db::begin_purge(db, "mcp_call_purge").await?;
    let count = sqlx::query("DELETE FROM mcp_calls WHERE id IN (SELECT id FROM mcp_calls WHERE ts_ms<? OR id<=(SELECT id FROM mcp_calls ORDER BY id DESC LIMIT 1 OFFSET ?) ORDER BY id LIMIT 500)")
        .bind(now_ms - retention.days * 86_400_000).bind(retention.max_rows)
        .execute(&mut *tx).await?.rows_affected();
    tx.commit().await?;
    Ok(count)
}
