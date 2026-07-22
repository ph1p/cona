use anyhow::{bail, Result};
use std::path::Path;
use std::process::Command;

/// One commit as mined from `git log --numstat`.
#[derive(Debug, Clone)]
pub struct Commit {
    pub hash: String,
    pub author: String,
    pub ts: i64,
    pub subject: String,
    /// (adds, dels, path) — adds/dels are -1 for binary files
    pub files: Vec<(i64, i64, String)>,
}

pub fn run_git(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").args(args).current_dir(root).output()?;
    if !out.status.success() {
        // first stderr line only — git's multi-line CLI advice ("use '--' to
        // separate paths…") doesn't apply to cona's interface
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            stderr.lines().next().unwrap_or("").trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Header marker: \x01 never occurs in normal git output, so parsers can
/// split commit headers from numstat/diff lines without ambiguity.
const LOG_FORMAT: &str = "\u{1}%H\u{9}%an\u{9}%at\u{9}%s";

/// `git log --numstat` over the whole history since `since` — the invocation
/// half of the `parse_numstat` contract, so LOG_FORMAT never leaves gitmap.
pub fn log_numstat(root: &Path, since: &str) -> Result<Vec<Commit>> {
    let fmt = format!("--format={LOG_FORMAT}");
    let since_arg = format!("--since={since}");
    let raw = run_git(root, &["log", &fmt, &since_arg, "--numstat"])?;
    Ok(parse_numstat(&raw))
}

/// One parsed commit header: (hash, author, timestamp, subject).
pub type LogHeader = (String, String, i64, String);

/// `git log -L` over one symbol's line range — the invocation half of the
/// `parse_log_headers` contract. Also returns the raw output length for the
/// caller's savings baseline.
pub fn log_symbol_range(
    root: &Path,
    path: &str,
    start: i64,
    end: i64,
    limit: usize,
) -> Result<(Vec<LogHeader>, usize)> {
    let fmt = format!("--format={LOG_FORMAT}");
    let range = format!("{start},{end}:{path}");
    let raw = run_git(root, &["log", &fmt, "-n", &limit.to_string(), "-L", &range])?;
    Ok((parse_log_headers(&raw), raw.len()))
}

/// Decode one LOG_FORMAT header (after the \x01 marker) — the single place
/// that knows the field order.
fn parse_header(rest: &str) -> (String, String, i64, String) {
    let mut it = rest.splitn(4, '\t');
    (
        it.next().unwrap_or("").to_string(),
        it.next().unwrap_or("").to_string(),
        it.next().unwrap_or("0").parse().unwrap_or(0),
        it.next().unwrap_or("").to_string(),
    )
}

/// Parse `git log --format=LOG_FORMAT --numstat` output into commits.
/// Pure — tested against captured output shapes.
pub fn parse_numstat(out: &str) -> Vec<Commit> {
    let mut commits: Vec<Commit> = Vec::new();
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix('\u{1}') {
            let (hash, author, ts, subject) = parse_header(rest);
            commits.push(Commit {
                hash,
                author,
                ts,
                subject,
                files: Vec::new(),
            });
            continue;
        }
        let mut it = line.splitn(3, '\t');
        let (Some(a), Some(d), Some(p)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        if let Some(c) = commits.last_mut() {
            let adds = a.parse().unwrap_or(-1); // "-" for binary
            let dels = d.parse().unwrap_or(-1);
            c.files.push((adds, dels, normalize_rename(p)));
        }
    }
    commits
}

/// numstat rename syntax: `src/{old => new}.rs` or `old => new` — keep the
/// new name so churn attaches to the file that exists today.
fn normalize_rename(p: &str) -> String {
    if let (Some(open), Some(close)) = (p.find('{'), p.find('}')) {
        if let Some(arrow) = p[open..close].find(" => ") {
            let new_part = &p[open + arrow + 4..close];
            let mut s = format!("{}{}{}", &p[..open], new_part, &p[close + 1..]);
            s = s.replace("//", "/");
            return s;
        }
    }
    if let Some(arrow) = p.find(" => ") {
        return p[arrow + 4..].to_string();
    }
    p.to_string()
}

/// Per-file churn: (path, commit count, lines touched, last author, last ts),
/// sorted by commit count desc then lines desc.
pub fn churn(commits: &[Commit]) -> Vec<(String, i64, i64, String, i64)> {
    use std::collections::HashMap;
    // path → (commits, lines, last_author, last_ts); log is newest-first,
    // so the first sighting of a path carries its most recent author.
    let mut m: HashMap<String, (i64, i64, String, i64)> = HashMap::new();
    for c in commits {
        for (a, d, p) in &c.files {
            let e = m.entry(p.clone()).or_insert((0, 0, c.author.clone(), c.ts));
            e.0 += 1;
            e.1 += a.max(&0) + d.max(&0);
        }
    }
    let mut v: Vec<_> = m
        .into_iter()
        .map(|(p, (n, l, au, ts))| (p, n, l, au, ts))
        .collect();
    v.sort_by(|a, b| (b.1, b.2).cmp(&(a.1, a.2)).then(a.0.cmp(&b.0)));
    v
}

/// Co-change coupling: of the commits touching `target`, which other files
/// ride along and how often? Returns (path, together, target_total) sorted
/// by together desc. Files that co-change constantly are hidden dependencies.
pub fn co_change(commits: &[Commit], target: &str) -> (i64, Vec<(String, i64)>) {
    use std::collections::HashMap;
    let mut total = 0i64;
    let mut m: HashMap<String, i64> = HashMap::new();
    for c in commits {
        if !c.files.iter().any(|(_, _, p)| p == target) {
            continue;
        }
        total += 1;
        for (_, _, p) in &c.files {
            if p != target {
                *m.entry(p.clone()).or_insert(0) += 1;
            }
        }
    }
    let mut v: Vec<_> = m.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    (total, v)
}

/// Parse `git log -L …` output — only the \x01-marked header lines matter,
/// the interleaved diff hunks are skipped. Pure.
pub fn parse_log_headers(out: &str) -> Vec<(String, String, i64, String)> {
    out.lines()
        .filter_map(|l| l.strip_prefix('\u{1}'))
        .map(parse_header)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\u{1}abc123\tAlice\t1700000000\tadd feature\n5\t2\tsrc/a.rs\n1\t0\tsrc/b.rs\n\n\u{1}def456\tBob\t1690000000\tfix bug\n3\t3\tsrc/a.rs\n-\t-\tassets/logo.png\n";

    #[test]
    fn numstat_parses_commits_and_files() {
        let cs = parse_numstat(SAMPLE);
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].author, "Alice");
        assert_eq!(cs[0].files.len(), 2);
        assert_eq!(cs[1].files[1], (-1, -1, "assets/logo.png".into()));
    }

    #[test]
    fn churn_ranks_by_commits_and_keeps_latest_author() {
        let cs = parse_numstat(SAMPLE);
        let ch = churn(&cs);
        assert_eq!(ch[0].0, "src/a.rs");
        assert_eq!(ch[0].1, 2); // two commits
        assert_eq!(ch[0].2, 13); // 5+2+3+3
        assert_eq!(ch[0].3, "Alice"); // newest-first log → latest author
    }

    #[test]
    fn co_change_counts_riders() {
        let cs = parse_numstat(SAMPLE);
        let (total, riders) = co_change(&cs, "src/a.rs");
        assert_eq!(total, 2);
        assert!(riders.contains(&("src/b.rs".into(), 1)));
        assert!(riders.contains(&("assets/logo.png".into(), 1)));
    }

    #[test]
    fn rename_syntax_normalized() {
        assert_eq!(normalize_rename("src/{old => new}.rs"), "src/new.rs");
        assert_eq!(normalize_rename("old.rs => new.rs"), "new.rs");
        assert_eq!(normalize_rename("src/plain.rs"), "src/plain.rs");
    }

    #[test]
    fn log_l_headers_skip_diff_noise() {
        let out = "\u{1}abc\tAlice\t1700000000\tsubject line\ndiff --git a/x b/x\n@@ -1,2 +1,2 @@\n-old\n+new\n\u{1}def\tBob\t1690000000\tother\n";
        let h = parse_log_headers(out);
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].3, "subject line");
    }
}
