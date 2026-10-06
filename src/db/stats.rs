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

/// `SUM(tokens_saved)` with the orientation cap applied to rows logged
/// before `ORIENT_BASELINE_CAP` existed, so old uncapped `tree` rows stop
/// dominating every total (one 500-file tree once claimed 700k).
pub(crate) const SAVED_SUM: &str = "COALESCE(SUM(CASE WHEN cmd IN ('tree','mcp:tree') \
     THEN MIN(tokens_saved, 20000) ELSE tokens_saved END),0)";

pub fn totals(g: &Connection, project: Option<&str>) -> Result<Totals> {
    let (where_, params) = scope_clause(project);
    let sql = format!(
        "SELECT COUNT(*), COALESCE(SUM(tokens_out),0), {SAVED_SUM},
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
        "SELECT cmd, COUNT(*), AVG(ms), COALESCE(SUM(tokens_out),0), {SAVED_SUM}
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
        "SELECT detail, COUNT(*), {SAVED_SUM}
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
        let filter = QUERY_FILTER;
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

/// SQL twin of `is_maintenance_cmd` — keep the two in lockstep.
const QUERY_FILTER: &str = "cmd NOT IN ('index','edit','rename','note') AND cmd NOT LIKE 'hook:%'";

fn and_clause(project: Option<&str>, extra: &str) -> (String, Vec<String>) {
    let (where_, params) = scope_clause(project);
    let w = if where_.is_empty() {
        format!(" WHERE {extra}")
    } else {
        format!("{where_} AND {extra}")
    };
    (w, params)
}

/// Failed queries per command: (cmd, failed calls), where failed = any
/// non-empty `outcome` (miss/ambiguous/empty/error).
pub fn failures_per_command(g: &Connection, project: Option<&str>) -> Result<Vec<(String, i64)>> {
    let (where_, params) = and_clause(project, "outcome <> ''");
    let sql = format!("SELECT cmd, COUNT(*) FROM usage{where_} GROUP BY cmd");
    let mut stmt = g.prepare(&sql)?;
    let p = rusqlite::params_from_iter(params.iter());
    let rows = stmt
        .query_map(p, |r| Ok((r.get(0)?, r.get(1)?)))?
        .flatten()
        .collect();
    Ok(rows)
}

/// Seconds after a hook hint in which a cona query counts as "followed".
pub const CONVERSION_WINDOW_SECS: i64 = 120;

/// Hook conversion row: (hook cmd, fired, followed by a cona query).
pub type ConversionRow = (String, i64, i64);

/// How often each hook outcome was followed by a cona query in the same
/// project within `CONVERSION_WINDOW_SECS`. Hooks log with a session id the
/// CLI never sees, so this is time-correlated, not session-exact: two
/// sessions in one repo can credit each other. It is a trend metric — a hint
/// that converts 5% of the time is noise the agent pays tokens to read.
pub fn hook_conversion(g: &Connection, project: Option<&str>) -> Result<Vec<ConversionRow>> {
    let (where_, params) = scope_clause(project);
    let where_ = where_.replace("project", "h.project");
    let hook = if where_.is_empty() {
        " WHERE h.cmd LIKE 'hook:%'".to_string()
    } else {
        format!("{where_} AND h.cmd LIKE 'hook:%'")
    };
    let follow = QUERY_FILTER.replace("cmd", "q.cmd");
    let sql = format!(
        "SELECT h.cmd, COUNT(*),
                SUM(EXISTS(SELECT 1 FROM usage q
                           WHERE q.project = h.project
                             AND q.ts BETWEEN h.ts AND h.ts + {CONVERSION_WINDOW_SECS}
                             AND q.id > h.id AND {follow}))
         FROM usage h{hook} GROUP BY h.cmd ORDER BY COUNT(*) DESC"
    );
    let mut stmt = g.prepare(&sql)?;
    let p = rusqlite::params_from_iter(params.iter());
    let rows = stmt
        .query_map(p, |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .flatten()
        .collect();
    Ok(rows)
}

/// Failed-query row for `cona learn`: (cmd, detail, outcome, count, last ts).
pub type FailureRow = (String, String, String, i64, i64);

/// Recurring failed lookups, most frequent first — `cona learn`'s input.
pub fn failed_queries(
    g: &Connection,
    project: Option<&str>,
    since_ts: i64,
    limit: i64,
) -> Result<Vec<FailureRow>> {
    let (where_, params) = and_clause(
        project,
        &format!("outcome <> '' AND detail <> '' AND ts >= {since_ts}"),
    );
    let sql = format!(
        "SELECT REPLACE(cmd, 'mcp:', ''), detail, outcome, COUNT(*), MAX(ts)
         FROM usage{where_} GROUP BY 1, 2, 3 ORDER BY 4 DESC, 5 DESC LIMIT {limit}"
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
