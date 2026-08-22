//! Stats aggregation (shared by `cona stats` and `cona ui`).

use super::*;
use anyhow::Result;
use rusqlite::Connection;

/// Headline totals over the usage table, optionally scoped to one project.
#[derive(Default, Clone)]
pub struct Totals {
    pub calls: i64,
    pub tokens_out: i64,
    pub tokens_saved: i64,
    pub reads_blocked: i64,
    pub total_ms: i64,
}

impl Totals {
    /// Tokens the agent would have spent reading files wholesale.
    pub fn baseline(&self) -> i64 {
        self.tokens_out + self.tokens_saved
    }
    /// Percentage of the baseline that cona avoided (0..=100).
    pub fn pct_saved(&self) -> f64 {
        let b = self.baseline();
        if b <= 0 {
            0.0
        } else {
            (self.tokens_saved as f64 / b as f64) * 100.0
        }
    }
}

fn scope_clause(project: Option<&str>) -> (String, Vec<String>) {
    match project {
        Some(p) => (" WHERE project = ?1".into(), vec![p.to_string()]),
        None => (String::new(), vec![]),
    }
}

pub fn totals(g: &Connection, project: Option<&str>) -> Result<Totals> {
    let (where_, params) = scope_clause(project);
    let sql = format!(
        "SELECT COUNT(*), COALESCE(SUM(tokens_out),0), COALESCE(SUM(tokens_saved),0),
                COALESCE(SUM(CASE WHEN cmd LIKE 'hook:%-block' THEN 1 ELSE 0 END),0),
                COALESCE(SUM(ms),0)
         FROM usage{where_}"
    );
    let p = rusqlite::params_from_iter(params.iter());
    let t = g.query_row(&sql, p, |r| {
        Ok(Totals {
            calls: r.get(0)?,
            tokens_out: r.get(1)?,
            tokens_saved: r.get(2)?,
            reads_blocked: r.get(3)?,
            total_ms: r.get(4)?,
        })
    })?;
    Ok(t)
}

/// Per-command aggregate row: (cmd, calls, avg_ms, tokens_out, tokens_saved).
pub type CommandRow = (String, i64, f64, i64, i64);

/// Per-command aggregate, most-called first.
pub fn per_command(g: &Connection, project: Option<&str>) -> Result<Vec<CommandRow>> {
    let (where_, params) = scope_clause(project);
    let sql = format!(
        "SELECT cmd, COUNT(*), AVG(ms), COALESCE(SUM(tokens_out),0), COALESCE(SUM(tokens_saved),0)
         FROM usage{where_} GROUP BY cmd ORDER BY COUNT(*) DESC"
    );
    let mut stmt = g.prepare(&sql)?;
    let p = rusqlite::params_from_iter(params.iter());
    let rows = stmt
        .query_map(p, |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .flatten()
        .collect();
    Ok(rows)
}

/// Most frequent query targets: (detail, count, tokens_saved).
pub fn top_targets(
    g: &Connection,
    project: Option<&str>,
    limit: i64,
) -> Result<Vec<(String, i64, i64)>> {
    let (mut where_, params) = scope_clause(project);
    if where_.is_empty() {
        where_ = " WHERE detail <> ''".into();
    } else {
        where_.push_str(" AND detail <> ''");
    }
    let sql = format!(
        "SELECT detail, COUNT(*), COALESCE(SUM(tokens_saved),0)
         FROM usage{where_} GROUP BY detail ORDER BY COUNT(*) DESC, 3 DESC LIMIT {limit}"
    );
    let mut stmt = g.prepare(&sql)?;
    let p = rusqlite::params_from_iter(params.iter());
    let rows = stmt
        .query_map(p, |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .flatten()
        .collect();
    Ok(rows)
}

/// Recent-query row: (ts, cmd, detail, tokens_saved, ms).
pub type RecentRow = (i64, String, String, i64, i64);

/// Recent queries, newest first. With `queries_only`, maintenance commands
/// (index/edit/rename/note/hook:* — see `is_maintenance_cmd`) are dropped;
/// they carry no savings and are just noise in an activity feed.
pub fn recent(
    g: &Connection,
    project: Option<&str>,
    limit: i64,
    queries_only: bool,
) -> Result<Vec<RecentRow>> {
    let (mut where_, params) = scope_clause(project);
    if queries_only {
        // keep in lockstep with is_maintenance_cmd
        let filter = "cmd NOT IN ('index','edit','rename','note') AND cmd NOT LIKE 'hook:%'";
        where_ = if where_.is_empty() {
            format!(" WHERE {filter}")
        } else {
            format!("{where_} AND {filter}")
        };
    }
    let sql = format!(
        "SELECT ts, cmd, detail, tokens_saved, ms FROM usage{where_} ORDER BY id DESC LIMIT {limit}"
    );
    let mut stmt = g.prepare(&sql)?;
    let p = rusqlite::params_from_iter(params.iter());
    let rows = stmt
        .query_map(p, |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .flatten()
        .collect();
    Ok(rows)
}

/// Human-friendly relative time, e.g. "3m ago", "just now".
pub fn ago(ts: i64) -> String {
    let d = (now() - ts).max(0);
    if d < 5 {
        "just now".into()
    } else if d < 60 {
        format!("{d}s ago")
    } else if d < 3600 {
        format!("{}m ago", d / 60)
    } else if d < 86400 {
        format!("{}h ago", d / 3600)
    } else {
        format!("{}d ago", d / 86400)
    }
}

/// Human-friendly byte size.
pub fn human_bytes(n: i64) -> String {
    const U: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}
