//! `learn`: mine the usage log for lookups that keep failing and say what
//! would have worked (closest name for misses, qualified forms for
//! ambiguities). Also feeds `learned_hints` + `hot_symbols` for SessionStart.

use super::{locate_candidates, Located};
use crate::{db, fuzzy};
use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

/// Commands whose `detail` is a symbol locator (grep/outline targets are
/// patterns/paths, so no suggestion is possible for them).
const SYMBOL_CMDS: &[&str] = &[
    "show", "find", "refs", "context", "impact", "callers", "callees", "tests", "shape", "edit",
    "insert", "rename", "path",
];

/// How far back the SessionStart hints (`learned_hints`, `hot_symbols`) look.
const HINT_WINDOW_SECS: i64 = 30 * 86_400;

/// What would have worked instead of a failed lookup.
#[derive(Debug, PartialEq)]
pub enum Fix {
    /// The name resolves today (indexed since, or the file was fixed).
    ResolvesNow,
    /// Closest indexed symbol: (qualified, path, line).
    Closest(String, String, i64),
    /// Qualified locators that each pick one definition, plus the total.
    Qualify(Vec<String>, usize),
    /// Nothing to suggest.
    None,
}

/// Symbol rows loaded once per report: (bare, qualified, path, line).
struct Pool(Vec<(String, String, String, i64)>);

impl Pool {
    fn load(conn: &Connection) -> Result<Self> {
        let mut stmt = conn.prepare(
            "SELECT s.name, s.qualified, f.path, s.start_line
             FROM symbols s JOIN files f ON f.id = s.file_id",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .flatten()
            .collect();
        Ok(Pool(rows))
    }

    fn closest(&self, name: &str) -> Option<(String, String, i64)> {
        let ranked = fuzzy::rank(
            name,
            self.0
                .iter()
                .enumerate()
                .map(|(i, (bare, q, ..))| (i, bare.as_str(), q.as_str())),
            1,
        );
        ranked.first().map(|&(_, i)| {
            let (_, q, p, l) = &self.0[i];
            (q.clone(), p.clone(), *l)
        })
    }
}

/// The qualified locator for one candidate: `Parent.Name` when that is
/// unique among the candidates, else `path:Name` (invariant 4's hatches).
fn qualify(c: &Located, all: &[Located]) -> String {
    let (path, _, _, q) = c;
    let unique_q = all.iter().filter(|o| &o.3 == q).count() == 1;
    if q.contains('.') && unique_q {
        q.clone()
    } else {
        format!("{path}:{}", db::name_tail(q))
    }
}

fn fix_for(conn: &Connection, pool: &Pool, cmd: &str, detail: &str, outcome: &str) -> Fix {
    if !SYMBOL_CMDS.contains(&cmd) {
        return Fix::None;
    }
    // an empty pool is a miss (locate_candidates no longer errors on one)
    match locate_candidates(conn, detail, None)
        .ok()
        .filter(|c| !c.is_empty())
    {
        Some(c) if c.len() == 1 => Fix::ResolvesNow,
        Some(c) if outcome == "ambiguous" || c.len() > 1 => {
            let mut forms: Vec<String> = c.iter().map(|x| qualify(x, &c)).collect();
            forms.dedup();
            let total = forms.len();
            forms.truncate(3);
            Fix::Qualify(forms, total)
        }
        Some(_) => Fix::ResolvesNow,
        None => {
            let bare = super::split_locator(detail).map_or(detail, |(_, n)| n);
            match pool.closest(bare) {
                Some((q, p, l)) => Fix::Closest(q, p, l),
                None => Fix::None,
            }
        }
    }
}

fn render_fix(f: &Fix) -> String {
    match f {
        Fix::ResolvesNow => "resolves now".into(),
        Fix::Closest(q, p, l) => format!("closest: {q}  {p}:{l}"),
        Fix::Qualify(forms, total) => {
            let more = if *total > forms.len() {
                format!(" (+{})", total - forms.len())
            } else {
                String::new()
            };
            format!("qualify: {}{more}", forms.join(" · "))
        }
        Fix::None => String::new(),
    }
}

/// `cona learn`: recurring failed lookups for this project (or all, without
/// suggestions — other projects' indexes are not open here).
/// One rendered fix per failed-query row (`""` = nothing to suggest). Loads
/// the symbol pool ONCE — shared by `cmd_learn` and the `ui` failures tab.
pub fn suggest_fixes(conn: &Connection, rows: &[db::FailureRow]) -> Result<Vec<String>> {
    let pool = Pool::load(conn)?;
    Ok(rows
        .iter()
        .map(|(cmd, detail, outcome, ..)| render_fix(&fix_for(conn, &pool, cmd, detail, outcome)))
        .collect())
}

pub fn cmd_learn(
    root: &Path,
    conn: Option<&Connection>,
    days: i64,
    limit: i64,
    json: bool,
) -> Result<String> {
    let g = db::open_global_db()?;
    let scope = conn.map(|_| root.to_string_lossy().to_string());
    let since = db::now() - days * 86_400;
    let rows = db::failed_queries(&g, scope.as_deref(), since, limit)?;
    let fixes = match conn {
        Some(c) => suggest_fixes(c, &rows)?,
        None => vec![String::new(); rows.len()],
    };

    if json {
        let items: Vec<_> = rows
            .iter()
            .zip(&fixes)
            .map(|((cmd, detail, outcome, n, last), f)| {
                serde_json::json!({"cmd": cmd, "target": detail, "outcome": outcome,
                    "count": n, "last": last, "fix": f})
            })
            .collect();
        return Ok(format!("{}\n", serde_json::Value::from(items)));
    }

    let label = if scope.is_some() {
        "this project"
    } else {
        "all projects"
    };
    let mut out = format!("── learn · {label} · last {days}d ──\n");
    if rows.is_empty() {
        out.push_str("  no failed lookups recorded — nothing to learn yet\n");
        return Ok(out);
    }
    let w = rows
        .iter()
        .map(|r| r.1.chars().count())
        .max()
        .unwrap_or(0)
        .min(36);
    for ((cmd, detail, outcome, n, _), fix) in rows.iter().zip(&fixes) {
        let arrow = if fix.is_empty() {
            String::new()
        } else {
            format!("  → {fix}")
        };
        out.push_str(&format!(
            "  {n:>3}×  {cmd:<8} {:<w$}  {outcome}{arrow}\n",
            clip_str(detail, 36),
        ));
    }
    if scope.is_none() {
        out.push_str("  (suggestions need the project's index — run without --all inside it)\n");
    }
    Ok(out)
}

pub fn clip_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}

/// At most `max` SessionStart lines: names that failed ≥2× in 30 days and now
/// have a concrete fix. Rides every session start, so empty unless worth it.
pub fn learned_hints(g: &Connection, root: &Path, conn: &Connection, max: usize) -> Vec<String> {
    let since = db::now() - HINT_WINDOW_SECS;
    let Ok(rows) = db::failed_queries(g, root.to_str(), since, 20) else {
        return vec![];
    };
    let Ok(pool) = Pool::load(conn) else {
        return vec![];
    };
    rows.iter()
        .filter(|r| r.3 >= 2)
        .filter_map(
            |(cmd, detail, outcome, ..)| match fix_for(conn, &pool, cmd, detail, outcome) {
                Fix::Closest(q, p, _) => Some(format!(
                    "`{detail}` is not a symbol here — closest is `{q}` ({p})"
                )),
                Fix::Qualify(forms, _) => {
                    Some(format!("`{detail}` is ambiguous — use e.g. `{}`", forms[0]))
                }
                _ => None,
            },
        )
        .take(max)
        .collect()
}

/// Symbols agents kept returning to here (answered lookups on ≥2 days in the
/// last 30), resolved against the CURRENT index: a deleted or renamed target
/// drops out on its own, an ambiguous one is skipped (never pick, inv. 4).
/// Lines are `q  path:start-end  (N days)`.
pub fn hot_symbols(g: &Connection, root: &Path, conn: &Connection, max: usize) -> Vec<String> {
    let since = db::now() - HINT_WINDOW_SECS;
    // rows arrive best-first; a few spares cover targets that no longer resolve
    let Ok(rows) = db::hot_targets(g, root.to_str(), since, (max * 4) as i64) else {
        return vec![];
    };
    // the candidate pool, not locate_symbol: an ambiguity is skipped here, so
    // building its error text would be wasted
    let unique = |t: &str| {
        locate_candidates(conn, t, None)
            .ok()
            .filter(|c| c.len() == 1)
            .and_then(|mut c| c.pop())
    };
    rank_hot(&rows, unique)
        .into_iter()
        .take(max)
        .map(|((p, s, e, q), days)| format!("{q}  {p}:{s}-{e}  ({days} days)"))
        .collect()
}

/// Split comma-joined details, resolve each, merge targets that name the same
/// definition (`Foo` and `a.rs:Foo`) keeping their best day count; rank by days.
fn rank_hot(rows: &[db::HotRow], resolve: impl Fn(&str) -> Option<Located>) -> Vec<(Located, i64)> {
    let mut out: Vec<(Located, i64)> = Vec::new();
    for (detail, days) in rows {
        for t in detail.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            let Some(loc) = resolve(t) else { continue };
            match out.iter_mut().find(|(l, _)| l.0 == loc.0 && l.3 == loc.3) {
                Some((_, d)) => *d = (*d).max(*days),
                None => out.push((loc, *days)),
            }
        }
    }
    out.sort_by_key(|(_, d)| std::cmp::Reverse(*d));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(path: &str, q: &str) -> Located {
        (path.into(), 1, 2, q.into())
    }

    #[test]
    fn qualify_prefers_parent_then_path() {
        let all = [
            loc("a.rs", "A.run"),
            loc("b.rs", "B.run"),
            loc("c.rs", "run"),
        ];
        assert_eq!(qualify(&all[0], &all), "A.run");
        // a bare name has no parent to qualify by → the path form
        assert_eq!(qualify(&all[2], &all), "c.rs:run");
        // the same Parent.Name in two files is not unique → the path form
        let dup = [loc("a.rs", "A.run"), loc("b.rs", "A.run")];
        assert_eq!(qualify(&dup[1], &dup), "b.rs:run");
    }

    #[test]
    fn fix_rendering() {
        assert_eq!(render_fix(&Fix::ResolvesNow), "resolves now");
        assert_eq!(
            render_fix(&Fix::Closest("cmd_show".into(), "a.rs".into(), 7)),
            "closest: cmd_show  a.rs:7"
        );
        assert_eq!(
            render_fix(&Fix::Qualify(vec!["A.run".into(), "B.run".into()], 4)),
            "qualify: A.run · B.run (+2)"
        );
        assert_eq!(clip_str("abcdef", 4), "abc…");
    }

    #[test]
    fn hot_targets_split_merge_and_drop_unresolved() {
        let rows = vec![
            ("Foo".to_string(), 2),
            ("a.rs:Foo,Bar".to_string(), 4), // same Foo, more days
            ("Gone".to_string(), 6),         // deleted since → dropped
        ];
        let ranked = rank_hot(&rows, |t| match t {
            "Foo" | "a.rs:Foo" => Some(loc("a.rs", "Foo")),
            "Bar" => Some(loc("b.rs", "Bar")),
            _ => None,
        });
        let got: Vec<(&str, i64)> = ranked.iter().map(|(l, d)| (l.3.as_str(), *d)).collect();
        assert_eq!(got, [("Foo", 4), ("Bar", 4)]);
    }
}
