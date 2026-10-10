//! `cona ui` — a live TUI of what cona is doing and how many tokens it saves.
//! Polls the SQLite databases (~1s). The only write it ever does is a reindex
//! the user asks for (`i`, or `a` = auto when files go stale) — the same
//! `index_project` + usage line as a typed `cona index`, on a worker thread so
//! the screen stays live.

use crate::{commands, db, indexer};
use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Gauge, List, ListItem, Paragraph, Row, Sparkline,
    Table,
};
use ratatui::Frame;
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

const VERSION: &str = env!("CARGO_PKG_VERSION");

// One palette, used everywhere — keeps the TUI reading as a single surface.
const ACCENT: Color = Color::Cyan; // headings, project name, targets
const SAVED: Color = Color::Green; // token-savings numbers
const MUTED: Color = Color::DarkGray; // secondary text, paths, timestamps
const FAILED: Color = Color::Red; // failed lookups, errors
const WARN: Color = Color::Yellow; // stale index, in-flight work

/// Days shown in the savings trend.
const TREND_DAYS: i64 = 14;
/// Window of the failures tab — same default as `cona learn`.
const FAIL_DAYS: i64 = 30;
/// Below this the layout cannot hold its panels — draw a notice instead.
const MIN_W: u16 = 60;
const MIN_H: u16 = 18;
/// A feed entry younger than this is highlighted as "just happened".
const FRESH_SECS: i64 = 10;
/// How long a finished reindex stays in the footer.
const JOB_TOAST: Duration = Duration::from_secs(6);
/// Auto-reindex waits this long after its last run — a file that stays
/// stale (unreadable, mid-write) must not turn into a reindex loop.
const AUTO_COOLDOWN: Duration = Duration::from_secs(10);

/// A bordered block with the shared rounded style and an accented title.
fn panel(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(MUTED))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
}

/// Savings tier → colour: red under 40%, yellow under 70%, green above.
fn tier_color(pct: f64) -> Color {
    if pct < 40.0 {
        Color::Red
    } else if pct < 70.0 {
        Color::Yellow
    } else {
        Color::Green
    }
}

fn sep() -> Span<'static> {
    Span::styled(" · ", Style::default().fg(MUTED))
}

fn muted(s: impl Into<String>) -> Span<'static> {
    Span::styled(s.into(), Style::default().fg(MUTED))
}

/// Fast-changing usage data, refreshed every second.
struct Snapshot {
    project_path: String,
    totals: db::Totals,
    per_cmd: Vec<db::CommandRow>,
    /// Failed calls per command (non-empty `outcome`).
    failures: HashMap<String, i64>,
    /// (hook hints fired, followed by a cona query) summed over all hooks.
    hooks: (i64, i64),
    /// Tokens saved per day, oldest first, `TREND_DAYS` long — gaps are 0.
    trend: Vec<u64>,
    top: Vec<(String, i64, i64)>,
    recent: Vec<db::RecentRow>,
    now: i64,
}

/// Slow-changing project index state (counts, staleness, composition). One
/// stat per indexed file — the caller throttles it.
#[derive(Default)]
struct IndexState {
    files: i64,
    symbols: i64,
    db_bytes: i64,
    last_indexed: Option<i64>,
    /// (path, deleted) for every indexed file whose mtime/size moved.
    stale: Vec<(String, bool)>,
    /// (language, files, symbols), most files first.
    langs: Vec<(String, i64, i64)>,
    /// (path, symbols) — where the code mass sits.
    dense: Vec<(String, i64)>,
}

/// Failed lookups of the last `FAIL_DAYS`, each with the `learn` fix ("" =
/// none). Only gathered while its tab is open: the fix needs the symbol pool.
type Failed = Vec<(db::FailureRow, String)>;

pub fn run(root: &Path) -> Result<()> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        anyhow::bail!("`cona ui` needs an interactive terminal — for scriptable output use `cona stats` (or `cona stats --json`)");
    }
    let mut terminal = ratatui::init();
    let res = event_loop(&mut terminal, root);
    ratatui::restore();
    res
}

/// Sort key for the "by command" table, cycled with `s`.
#[derive(Clone, Copy, PartialEq)]
enum SortKey {
    Saved,
    Calls,
    AvgMs,
}
impl SortKey {
    fn next(self) -> Self {
        match self {
            SortKey::Saved => SortKey::Calls,
            SortKey::Calls => SortKey::AvgMs,
            SortKey::AvgMs => SortKey::Saved,
        }
    }
    fn label(self) -> &'static str {
        match self {
            SortKey::Saved => "saved",
            SortKey::Calls => "calls",
            SortKey::AvgMs => "avg ms",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Overview,
    Index,
    Failures,
}
impl Tab {
    const ALL: [Tab; 3] = [Tab::Overview, Tab::Index, Tab::Failures];
    fn next(self) -> Self {
        match self {
            Tab::Overview => Tab::Index,
            Tab::Index => Tab::Failures,
            Tab::Failures => Tab::Overview,
        }
    }
    fn title(self) -> &'static str {
        match self {
            Tab::Overview => "overview",
            Tab::Index => "index",
            Tab::Failures => "failures",
        }
    }
}

/// View state the keys toggle; the snapshot is data, this is presentation.
#[derive(Clone, Copy)]
struct View {
    tab: Tab,
    /// true = only this project, false = global across all projects
    project_scope: bool,
    sort: SortKey,
    /// Frozen feed: polling stops so a burst can be read.
    paused: bool,
    /// Reindex on its own whenever indexed files go stale.
    auto: bool,
    help: bool,
}

/// The background reindex, as the footer shows it.
enum Job {
    Idle,
    Running { since: Instant, auto: bool },
    Done { at: Instant, ok: bool, msg: String },
}

/// Run one reindex on a worker thread; the receiver yields its one-line
/// summary. Its own connection — rusqlite handles are not shared across
/// threads, and WAL lets the UI's readers keep going meanwhile.
fn spawn_reindex(root: PathBuf, auto: bool) -> Receiver<Result<String, String>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let res = (|| -> Result<String> {
            // Auto is unattended — it defers to a walk already in flight, the
            // SessionStart rule. A keypress is a typed `cona index`: it holds
            // the marker if free (so others defer to IT) but always walks.
            let lock = db::IndexLock::acquire(&root);
            if auto && lock.is_none() {
                return Ok("another cona process is indexing — skipped".into());
            }
            let t0 = Instant::now();
            let conn = db::open_project_db(&root)?;
            let r = indexer::index_project(&root, &conn)?;
            let ms = t0.elapsed().as_millis() as i64;
            db::log_usage(&root, "index", ms, r.total_symbols, 0, 0);
            Ok(format!(
                "indexed · {} parsed · {} removed · {}ms",
                r.parsed, r.removed, ms
            ))
        })();
        let _ = tx.send(res.map_err(|e| format!("{e:#}")));
    });
    rx
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, root: &Path) -> Result<()> {
    let root = root.to_path_buf();
    let mut v = View {
        tab: Tab::Overview,
        project_scope: true,
        sort: SortKey::Saved,
        paused: false,
        auto: false,
        help: false,
    };
    // Open the DBs ONCE — reopening per tick re-runs PRAGMAs/migration probes.
    // WAL still makes external reindexes visible to these long-lived handles.
    let g = db::open_global_db()?;
    let pconn = db::open_project_db(&root)?;
    // The index-state scan (one stat per indexed file) is the expensive part
    // and rarely changes: every 5s, while usage stats refresh every 1s.
    let mut idx = gather_index_state(&g, &pconn, &root)?;
    let mut failed: Failed = vec![];
    let mut snap = gather(&g, &root, v.project_scope, v.sort)?;
    let mut last = Instant::now();
    let mut last_idx = Instant::now();
    let mut job = Job::Idle;
    let mut job_rx: Option<Receiver<Result<String, String>>> = None;
    let mut last_auto: Option<Instant> = None;

    let refresh = |v: &View| gather(&g, &root, v.project_scope, v.sort);
    let refresh_failed = |v: &View| -> Result<Failed> {
        if v.tab != Tab::Failures {
            return Ok(vec![]);
        }
        gather_failed(&g, &pconn, &root, v.project_scope)
    };

    loop {
        terminal.draw(|f| draw(f, &snap, &idx, &failed, v, &job))?;

        let mut start_job = None;
        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Resize(..) => {}
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    let ctrl_c =
                        k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL);
                    // help is modal: any key closes it, only q/ctrl-c still quit
                    if v.help && !ctrl_c && k.code != KeyCode::Char('q') {
                        v.help = false;
                        continue;
                    }
                    let before = v.tab;
                    match k.code {
                        _ if ctrl_c => break,
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('?') | KeyCode::Char('h') => v.help = true,
                        KeyCode::Char(' ') => v.paused = !v.paused,
                        KeyCode::Tab | KeyCode::Right => v.tab = v.tab.next(),
                        KeyCode::BackTab | KeyCode::Left => {
                            v.tab = v.tab.next().next();
                        }
                        KeyCode::Char(c @ '1'..='3') => {
                            v.tab = Tab::ALL[c as usize - '1' as usize];
                        }
                        KeyCode::Char('p') => {
                            v.project_scope = !v.project_scope;
                            snap = refresh(&v)?;
                            failed = refresh_failed(&v)?;
                            last = Instant::now();
                        }
                        KeyCode::Char('s') => {
                            v.sort = v.sort.next();
                            snap = refresh(&v)?;
                        }
                        KeyCode::Char('r') => {
                            idx = gather_index_state(&g, &pconn, &root)?;
                            snap = refresh(&v)?;
                            failed = refresh_failed(&v)?;
                            last = Instant::now();
                            last_idx = Instant::now();
                        }
                        KeyCode::Char('i') => start_job = Some(false),
                        KeyCode::Char('a') => v.auto = !v.auto,
                        _ => {}
                    }
                    if v.tab != before && v.tab == Tab::Failures {
                        failed = refresh_failed(&v)?;
                    }
                }
                _ => {}
            }
        }

        // the job outlives `paused`: a pause freezes the view, not the work
        if let Some(rx) = &job_rx {
            let done = match rx.try_recv() {
                Ok(res) => Some(res),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err("indexer thread died".into())),
            };
            if let Some(res) = done {
                let (ok, msg) = match res {
                    Ok(m) => (true, m),
                    Err(e) => (false, e),
                };
                job = Job::Done {
                    at: Instant::now(),
                    ok,
                    msg,
                };
                job_rx = None;
                idx = gather_index_state(&g, &pconn, &root)?;
                failed = refresh_failed(&v)?;
                last_idx = Instant::now();
            }
        }
        if let Job::Done { at, .. } = job {
            if at.elapsed() >= JOB_TOAST {
                job = Job::Idle;
            }
        }
        let auto_due = v.auto
            && !idx.stale.is_empty()
            && last_auto.is_none_or(|t| t.elapsed() >= AUTO_COOLDOWN);
        if start_job.is_none() && auto_due {
            start_job = Some(true);
        }
        if let (Some(auto), None) = (start_job, &job_rx) {
            job_rx = Some(spawn_reindex(root.clone(), auto));
            job = Job::Running {
                since: Instant::now(),
                auto,
            };
            if auto {
                last_auto = Some(Instant::now());
            }
        }

        if v.paused {
            continue;
        }
        if last_idx.elapsed() >= Duration::from_secs(5) {
            idx = gather_index_state(&g, &pconn, &root)?;
            failed = refresh_failed(&v)?;
            last_idx = Instant::now();
        }
        if last.elapsed() >= Duration::from_secs(1) {
            snap = refresh(&v)?;
            last = Instant::now();
        }
    }
    // A reindex in flight finishes before we exit: killing it mid-walk would
    // leave the IndexLock marker behind until it ages out.
    if let Some(rx) = job_rx {
        let _ = rx.recv_timeout(Duration::from_secs(30));
    }
    Ok(())
}

fn gather_index_state(g: &Connection, pconn: &Connection, root: &Path) -> Result<IndexState> {
    let files: i64 = pconn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap_or(0);
    let symbols: i64 = pconn
        .query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))
        .unwrap_or(0);
    // Batch: pull (path, mtime, size) once, then one stat per file — not one
    // SQL query per path via is_stale.
    let mut stale = vec![];
    {
        let mut stmt = pconn.prepare("SELECT path, mtime, size FROM files ORDER BY path")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows.flatten() {
            let (path, m, s) = row;
            match std::fs::metadata(root.join(&path)) {
                Ok(meta) if indexer::meta_matches(&meta, m, s) => {}
                Ok(_) => stale.push((path, false)),
                Err(_) => stale.push((path, true)),
            }
        }
    }
    let langs = pconn
        .prepare(
            "SELECT f.lang, COUNT(DISTINCT f.id), COUNT(s.id)
             FROM files f LEFT JOIN symbols s ON s.file_id = f.id
             GROUP BY f.lang ORDER BY 2 DESC, 3 DESC",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .flatten()
        .collect();
    let dense = pconn
        .prepare(
            "SELECT f.path, COUNT(*) FROM symbols s JOIN files f ON f.id = s.file_id
             GROUP BY f.id ORDER BY 2 DESC LIMIT 20",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .flatten()
        .collect();
    let last_indexed: Option<i64> = g
        .query_row(
            "SELECT last_indexed FROM projects WHERE hash = ?1",
            [db::project_hash(root)],
            |r| r.get(0),
        )
        .ok()
        .flatten();
    Ok(IndexState {
        files,
        symbols,
        db_bytes: db::project_db_size(root),
        last_indexed,
        stale,
        langs,
        dense,
    })
}

/// Fixes come from THIS project's index, so the global scope lists the
/// failures without them rather than suggesting another repo's symbols.
fn gather_failed(
    g: &Connection,
    pconn: &Connection,
    root: &Path,
    project_scope: bool,
) -> Result<Failed> {
    let scope = project_scope.then(|| root.to_string_lossy().to_string());
    let rows = db::failed_queries(g, scope.as_deref(), db::now() - FAIL_DAYS * 86_400, 50)?;
    let fixes = if project_scope {
        commands::suggest_fixes(pconn, &rows)?
    } else {
        vec![String::new(); rows.len()]
    };
    Ok(rows.into_iter().zip(fixes).collect())
}

/// The last `TREND_DAYS` local-calendar days, oldest first, as the same
/// `%Y-%m-%d` keys `savings_series` buckets by — so days without usage show
/// as 0 instead of silently collapsing the axis.
fn trend_days(g: &Connection) -> Vec<String> {
    (0..TREND_DAYS)
        .rev()
        .filter_map(|d| {
            g.query_row(
                "SELECT strftime('%Y-%m-%d', 'now', 'localtime', ?1)",
                [format!("-{d} days")],
                |r| r.get(0),
            )
            .ok()
        })
        .collect()
}

fn gather(g: &Connection, root: &Path, project_scope: bool, sort: SortKey) -> Result<Snapshot> {
    let scope = project_scope.then(|| root.to_string_lossy().to_string());
    let scope_ref = scope.as_deref();

    let mut per_cmd = db::per_command(g, scope_ref)?;
    // sort only the query rows; maintenance is folded out in draw_overview anyway
    per_cmd.sort_by(|a, b| match sort {
        SortKey::Saved => b.4.cmp(&a.4),
        SortKey::Calls => b.1.cmp(&a.1),
        SortKey::AvgMs => b.2.total_cmp(&a.2),
    });

    let hooks = db::hook_conversion(g, scope_ref)?
        .into_iter()
        .fold((0, 0), |(n, f), (_, fired, followed)| {
            (n + fired, f + followed)
        });

    let by_day: HashMap<String, i64> =
        db::savings_series(g, scope_ref, db::Bucket::Day, TREND_DAYS)?
            .into_iter()
            .map(|(day, _, _, saved)| (day, saved))
            .collect();
    let trend = trend_days(g)
        .iter()
        .map(|d| by_day.get(d).copied().unwrap_or(0).max(0) as u64)
        .collect();

    Ok(Snapshot {
        project_path: root.to_string_lossy().to_string(),
        totals: db::totals(g, scope_ref)?,
        per_cmd,
        failures: db::failures_per_command(g, scope_ref)?
            .into_iter()
            .collect(),
        hooks,
        trend,
        top: db::top_targets(g, scope_ref, 8)?,
        // live activity shows real queries only — maintenance is feed noise
        recent: db::recent(g, scope_ref, 40, true)?,
        now: db::now(),
    })
}

fn draw(f: &mut Frame, s: &Snapshot, idx: &IndexState, failed: &Failed, v: View, job: &Job) {
    let area = f.area();
    if area.width < MIN_W || area.height < MIN_H {
        let msg = vec![
            Line::from(Span::styled(
                "terminal too small",
                Style::default().fg(WARN).add_modifier(Modifier::BOLD),
            )),
            Line::from(muted(format!(
                "{}×{} — need {MIN_W}×{MIN_H}",
                area.width, area.height
            ))),
            Line::from(muted("q quits")),
        ];
        let y = area.height.saturating_sub(3) / 2;
        let r = Rect::new(area.x, area.y + y, area.width, area.height.min(3));
        f.render_widget(Paragraph::new(msg).alignment(Alignment::Center), r);
        return;
    }

    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4), // header
            Constraint::Length(1), // tab bar
            Constraint::Min(6),    // body
            Constraint::Length(1), // footer
        ])
        .split(area);

    draw_header(f, root[0], s, idx);
    draw_tabs(f, root[1], s, idx, v);
    match v.tab {
        Tab::Overview => draw_overview(f, root[2], s, v.sort),
        Tab::Index => draw_index(f, root[2], idx, job),
        Tab::Failures => draw_failures(f, root[2], failed, v.project_scope),
    }
    draw_footer(f, root[3], v, job);
    if v.help {
        draw_help(f, area);
    }
}

fn draw_header(f: &mut Frame, area: Rect, s: &Snapshot, idx: &IndexState) {
    let name = Path::new(&s.project_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| s.project_path.clone());
    let last = idx
        .last_indexed
        .map(db::ago)
        .unwrap_or_else(|| "never".into());
    let stale = if idx.stale.is_empty() {
        Span::styled("✓ fresh", Style::default().fg(SAVED))
    } else {
        Span::styled(
            format!("⚠ {} stale (i reindexes)", idx.stale.len()),
            Style::default().fg(WARN),
        )
    };
    // path is the least important header item: it gets what the name leaves
    let inner = area.width.saturating_sub(2) as usize;
    let path_room = inner.saturating_sub(name.chars().count() + 2);
    let lines = vec![
        Line::from(vec![
            Span::styled(
                name,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            muted(trunc(&s.project_path, path_room).trim_end()),
        ]),
        Line::from(vec![
            Span::styled(format!("{} files", idx.files), Style::default().fg(ACCENT)),
            sep(),
            Span::styled(
                format!("{} symbols", idx.symbols),
                Style::default().fg(ACCENT),
            ),
            sep(),
            Span::raw(format!("db {}", db::human_bytes(idx.db_bytes))),
            sep(),
            stale,
            sep(),
            muted(format!("indexed {last}")),
        ]),
    ];
    let block = panel(&format!("cona v{VERSION}")).border_style(Style::default().fg(ACCENT));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// Tab strip; each tab carries the count that makes it worth opening.
fn draw_tabs(f: &mut Frame, area: Rect, s: &Snapshot, idx: &IndexState, v: View) {
    let failed: i64 = s.failures.values().sum();
    let mut spans = vec![Span::raw(" ")];
    for (i, t) in Tab::ALL.into_iter().enumerate() {
        let badge = match t {
            Tab::Index if !idx.stale.is_empty() => Some((idx.stale.len() as i64, WARN)),
            Tab::Failures if failed > 0 => Some((failed, FAILED)),
            _ => None,
        };
        let style = if t == v.tab {
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        spans.push(Span::styled(format!(" {} {} ", i + 1, t.title()), style));
        if let Some((n, c)) = badge {
            spans.push(Span::styled(format!("{n} "), Style::default().fg(c)));
        }
        spans.push(Span::raw(" "));
    }
    let scope = if v.project_scope {
        "this project"
    } else {
        "all projects"
    };
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(16)])
        .split(area);
    f.render_widget(Paragraph::new(Line::from(spans)), cols[0]);
    f.render_widget(
        Paragraph::new(muted(format!("{scope} "))).alignment(Alignment::Right),
        cols[1],
    );
}

fn draw_overview(f: &mut Frame, area: Rect, s: &Snapshot, sort: SortKey) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(0)])
        .split(area);
    draw_gauge(f, rows[0], s);
    draw_middle(f, rows[1], s, sort);
}

fn draw_gauge(f: &mut Frame, area: Rect, s: &Snapshot) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(30),
            Constraint::Percentage(40),
            Constraint::Percentage(30),
        ])
        .split(area);

    let pct = s.totals.pct_saved();
    let gauge = Gauge::default()
        .block(panel("tokens saved"))
        .gauge_style(Style::default().fg(tier_color(pct)).bg(Color::Black))
        .ratio((pct / 100.0).clamp(0.0, 1.0))
        .label(Span::styled(
            format!("{pct:.0}% of reads avoided"),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ));
    f.render_widget(gauge, cols[0]);

    let t = &s.totals;
    let failed: i64 = s.failures.values().sum();
    let info = Line::from(vec![
        Span::styled(
            fmt_k(t.tokens_saved),
            Style::default().fg(SAVED).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" saved", Style::default().fg(SAVED)),
        sep(),
        Span::raw(format!("{} used", fmt_k(t.tokens_out))),
        sep(),
        muted(format!("{} would-read", fmt_k(t.baseline()))),
    ]);
    let info2 = Line::from(vec![
        Span::styled(format!("{} queries", t.calls), Style::default().fg(ACCENT)),
        sep(),
        Span::styled(
            format!("{} reads intercepted", t.reads_blocked),
            Style::default().fg(WARN),
        ),
    ]);
    // quality line: what went wrong and whether hook hints land
    let mut info3 = vec![];
    if failed > 0 {
        info3.push(Span::styled(
            format!("{failed} failed"),
            Style::default().fg(FAILED),
        ));
    }
    if s.hooks.0 > 0 {
        if !info3.is_empty() {
            info3.push(sep());
        }
        info3.push(muted(format!(
            "hints → {}% followed",
            s.hooks.1 * 100 / s.hooks.0
        )));
    }
    f.render_widget(
        Paragraph::new(vec![info, info2, Line::from(info3)]).block(panel("totals")),
        cols[1],
    );

    let today = s.trend.last().copied().unwrap_or(0);
    let spark = Sparkline::default()
        .block(panel(&format!(
            "{TREND_DAYS}d · today {}",
            fmt_k(today as i64)
        )))
        .data(stretch(&s.trend, cols[2].width.saturating_sub(2) as usize))
        .style(Style::default().fg(SAVED));
    f.render_widget(spark, cols[2]);
}

fn draw_middle(f: &mut Frame, area: Rect, s: &Snapshot, sort: SortKey) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(cols[0]);

    // per-command table — queries only; maintenance (no savings) is folded
    // into one dim line below.
    let (queries, maint): (Vec<_>, Vec<_>) = s
        .per_cmd
        .iter()
        .partition(|(cmd, ..)| !db::is_maintenance_cmd(cmd));
    let table_area = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(left[0]);
    let block = panel(&format!("by command · ↓{}", sort.label()));
    if queries.is_empty() {
        empty(
            f,
            table_area[0],
            block,
            "no queries yet — try `cona show <Symbol>`",
        );
    } else {
        let rows = queries.iter().map(|(cmd, n, ms, out, saved)| {
            let fails = s.failures.get(cmd).copied().unwrap_or(0);
            let fail_cell = if fails > 0 {
                Cell::from(Span::styled(fails.to_string(), Style::default().fg(FAILED)))
            } else {
                Cell::from(muted("·"))
            };
            Row::new(vec![
                Cell::from(cmd.clone()),
                Cell::from(n.to_string()),
                fail_cell,
                Cell::from(format!("{ms:.0}")),
                Cell::from(fmt_k(*out)),
                Cell::from(Span::styled(fmt_k(*saved), Style::default().fg(SAVED))),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Min(10),
                Constraint::Length(6),
                Constraint::Length(5),
                Constraint::Length(7),
                Constraint::Length(7),
                Constraint::Length(7),
            ],
        )
        .header(header_row(&[
            "cmd", "calls", "fail", "avg ms", "out", "saved",
        ]))
        .block(block);
        f.render_widget(table, table_area[0]);
    }
    if !maint.is_empty() {
        let parts: Vec<String> = maint
            .iter()
            .map(|(cmd, n, ..)| format!("{cmd} {n}×"))
            .collect();
        let line = format!(" maintenance {}", parts.join(" · "));
        f.render_widget(
            Paragraph::new(trunc(&line, table_area[1].width as usize))
                .style(Style::default().fg(MUTED)),
            table_area[1],
        );
    }

    // top targets — target column takes whatever the counts leave
    let block = panel("top targets");
    if s.top.is_empty() {
        empty(f, left[1], block, "symbols you query most show up here");
    } else {
        let room = (left[1].width as usize).saturating_sub(2 + 5 + 8);
        let items: Vec<ListItem> = s
            .top
            .iter()
            .map(|(d, n, saved)| {
                ListItem::new(Line::from(vec![
                    muted(format!("{n:>3}× ")),
                    Span::styled(trunc(d, room), Style::default().fg(ACCENT)),
                    Span::styled(format!(" {:>7}", fmt_k(*saved)), Style::default().fg(SAVED)),
                ]))
            })
            .collect();
        f.render_widget(List::new(items).block(block), left[1]);
    }

    // activity feed — detail column takes whatever the fixed columns leave
    let block = panel("live activity");
    if s.recent.is_empty() {
        empty(f, cols[1], block, "waiting for the first query…");
        return;
    }
    let cmd_w = s
        .recent
        .iter()
        .map(|r| r.1.chars().count())
        .max()
        .unwrap_or(0)
        .min(16);
    let room = (cols[1].width as usize).saturating_sub(2 + 9 + cmd_w + 1 + 8 + 8);
    let feed: Vec<ListItem> = s
        .recent
        .iter()
        .map(|(ts, cmd, detail, saved, ms)| {
            // feed is queries-only (maintenance/hook:* filtered in db::recent)
            let fresh = s.now - ts < FRESH_SECS;
            let when = if fresh {
                Style::default().fg(SAVED).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(MUTED)
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:<9}", trunc(&db::ago(*ts), 8).trim_end()), when),
                Span::styled(
                    format!("{:<cmd_w$} ", trunc(cmd, cmd_w).trim_end()),
                    Style::default().fg(Color::White),
                ),
                Span::raw(trunc(detail, room)),
                Span::styled(
                    format!(" {:>7}", format!("+{}", fmt_k(*saved))),
                    Style::default().fg(SAVED),
                ),
                muted(format!(" {ms:>5}ms")),
            ]))
        })
        .collect();
    f.render_widget(List::new(feed).block(block), cols[1]);
}

/// Index tab: what the index holds (languages, densest files) and where it
/// lags the working tree (stale files — the list `i` would fix).
fn draw_index(f: &mut Frame, area: Rect, idx: &IndexState, job: &Job) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(cols[0]);

    // languages — share bar scaled to the biggest language
    let block = panel("languages");
    if idx.langs.is_empty() {
        empty(f, left[0], block, "nothing indexed — press i");
    } else {
        let max = idx.langs.iter().map(|l| l.1).max().unwrap_or(1).max(1);
        let bar_w = (left[0].width as usize)
            .saturating_sub(2 + 12 + 7 + 9 + 2)
            .min(30);
        let rows = idx.langs.iter().map(|(lang, files, syms)| {
            let n = (*files as usize * bar_w).div_ceil(max as usize);
            Row::new(vec![
                Cell::from(Span::styled(lang.clone(), Style::default().fg(ACCENT))),
                Cell::from(files.to_string()),
                Cell::from(syms.to_string()),
                Cell::from(Span::styled("▇".repeat(n), Style::default().fg(SAVED))),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(12),
                Constraint::Length(6),
                Constraint::Length(8),
                Constraint::Min(0),
            ],
        )
        .header(header_row(&["lang", "files", "symbols", ""]))
        .block(block);
        f.render_widget(table, left[0]);
    }

    // densest files
    let block = panel("most symbols");
    if idx.dense.is_empty() {
        empty(f, left[1], block, "no symbols indexed yet");
    } else {
        let room = (left[1].width as usize).saturating_sub(2 + 6);
        let items: Vec<ListItem> = idx
            .dense
            .iter()
            .map(|(p, n)| {
                ListItem::new(Line::from(vec![
                    muted(format!("{n:>5} ")),
                    Span::raw(trunc(p, room)),
                ]))
            })
            .collect();
        f.render_widget(List::new(items).block(block), left[1]);
    }

    // stale files — the right column is the actionable one
    let running = matches!(job, Job::Running { .. });
    let title = match (idx.stale.len(), running) {
        (_, true) => "stale files · reindexing…".to_string(),
        (0, _) => "stale files".to_string(),
        (n, _) => format!("stale files · {n} · i reindexes"),
    };
    let block = panel(&title);
    if idx.stale.is_empty() {
        empty(f, cols[1], block, "✓ index matches the working tree");
        return;
    }
    let room = (cols[1].width as usize).saturating_sub(2 + 10);
    let items: Vec<ListItem> = idx
        .stale
        .iter()
        .map(|(p, deleted)| {
            let (tag, c) = if *deleted {
                ("deleted  ", FAILED)
            } else {
                ("modified ", WARN)
            };
            ListItem::new(Line::from(vec![
                Span::styled(tag, Style::default().fg(c)),
                Span::raw(trunc(p, room)),
            ]))
        })
        .collect();
    f.render_widget(List::new(items).block(block), cols[1]);
}

/// Failures tab: `cona learn`, live — what keeps failing and what would work.
fn draw_failures(f: &mut Frame, area: Rect, failed: &Failed, project_scope: bool) {
    let title = format!("failed lookups · last {FAIL_DAYS}d");
    let block = panel(&title);
    if failed.is_empty() {
        empty(f, area, block, "✓ no failed lookups in this window");
        return;
    }
    let inner_w = area.width.saturating_sub(2) as usize;
    let target_w = failed
        .iter()
        .map(|r| r.0 .1.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(8, (inner_w / 3).max(8));
    let rows = failed.iter().map(|((cmd, detail, outcome, n, last), fix)| {
        let oc = match outcome.as_str() {
            "ambiguous" => WARN,
            "error" => FAILED,
            _ => Color::Magenta,
        };
        let fix_cell = if fix.is_empty() {
            Cell::from(muted("—"))
        } else if fix == "resolves now" {
            Cell::from(Span::styled("✓ resolves now", Style::default().fg(SAVED)))
        } else {
            Cell::from(Span::styled(format!("→ {fix}"), Style::default().fg(SAVED)))
        };
        Row::new(vec![
            Cell::from(muted(format!("{n:>3}×"))),
            Cell::from(cmd.clone()),
            Cell::from(Span::styled(
                trunc(detail, target_w),
                Style::default().fg(ACCENT),
            )),
            Cell::from(Span::styled(outcome.clone(), Style::default().fg(oc))),
            Cell::from(muted(db::ago(*last))),
            fix_cell,
        ])
    });
    let mut block = block;
    if !project_scope {
        block = block.title_bottom(muted(
            " fixes need this project's index — p for project scope ",
        ));
    } else {
        block = block.title_bottom(muted(" full list: cona learn "));
    }
    let table = Table::new(
        rows,
        [
            Constraint::Length(5),
            Constraint::Length(8),
            Constraint::Length(target_w as u16),
            Constraint::Length(10),
            Constraint::Length(9),
            Constraint::Min(10),
        ],
    )
    .header(header_row(&["", "cmd", "target", "outcome", "last", "fix"]))
    .block(block);
    f.render_widget(table, area);
}

fn header_row(cols: &[&'static str]) -> Row<'static> {
    Row::new(cols.to_vec()).style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
}

/// Widen each bar to fill `width` columns — one column per day leaves most
/// of the panel blank on a normal terminal.
fn stretch(data: &[u64], width: usize) -> Vec<u64> {
    let per = (width / data.len().max(1)).max(1);
    data.iter()
        .flat_map(|&v| std::iter::repeat_n(v, per))
        .collect()
}

/// An empty panel with one centred, dim hint — a blank box reads as broken.
fn empty(f: &mut Frame, area: Rect, block: Block<'static>, hint: &str) {
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    let r = Rect::new(inner.x, inner.y + inner.height / 2, inner.width, 1);
    f.render_widget(Paragraph::new(muted(hint)).alignment(Alignment::Center), r);
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Right side of the footer: the reindex job wins over the live/paused badge
/// — it is the thing the user is waiting on.
fn status_span(v: View, job: &Job) -> Span<'static> {
    match job {
        Job::Running { since, auto } => {
            let el = since.elapsed();
            let frame = SPINNER[(el.as_millis() / 100) as usize % SPINNER.len()];
            let who = if *auto {
                "auto-reindexing"
            } else {
                "reindexing"
            };
            Span::styled(
                format!("{frame} {who}… {:.1}s ", el.as_secs_f64()),
                Style::default().fg(WARN),
            )
        }
        Job::Done { ok: true, msg, .. } => {
            Span::styled(format!("✓ {msg} "), Style::default().fg(SAVED))
        }
        Job::Done { ok: false, msg, .. } => {
            Span::styled(format!("✗ {msg} "), Style::default().fg(FAILED))
        }
        Job::Idle if v.paused => {
            Span::styled(" ⏸ paused ", Style::default().fg(Color::Black).bg(WARN))
        }
        Job::Idle => Span::styled("● live ", Style::default().fg(SAVED)),
    }
}

fn draw_footer(f: &mut Frame, area: Rect, v: View, job: &Job) {
    let scope = if v.project_scope { "project" } else { "global" };
    let key = Style::default().fg(Color::Black).bg(ACCENT);
    let mut keys = vec![
        ("q", "quit".to_string()),
        ("p", scope.to_string()),
        ("i", "reindex".to_string()),
        ("a", format!("auto:{}", if v.auto { "on" } else { "off" })),
    ];
    if v.tab == Tab::Overview {
        keys.push(("s", format!("sort:{}", v.sort.label())));
    }
    keys.push(("␣", if v.paused { "resume" } else { "pause" }.into()));
    keys.push(("?", "help".into()));
    let mut spans = vec![];
    for (k, l) in keys {
        spans.push(Span::styled(format!(" {k} "), key));
        let style = if k == "a" && v.auto {
            Style::default().fg(SAVED)
        } else {
            Style::default().fg(MUTED)
        };
        spans.push(Span::styled(format!(" {l}  "), style));
    }
    let status = status_span(v, job);
    let status_w = (status.width() as u16).min(area.width / 2);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(status_w)])
        .split(area);
    f.render_widget(Paragraph::new(Line::from(spans)), cols[0]);
    f.render_widget(Paragraph::new(status).alignment(Alignment::Right), cols[1]);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let key = |k: &str| {
        Span::styled(
            format!(" {k:<7}"),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )
    };
    let txt = |t: &str| Span::raw(t.to_string());
    let dim = |t: &str| Line::from(muted(t));
    let lines = vec![
        Line::from(vec![
            key("1 2 3"),
            txt("overview · index · failures (tab/←/→)"),
        ]),
        Line::from(vec![
            key("i"),
            txt("reindex now (background, like `cona index`)"),
        ]),
        Line::from(vec![
            key("a"),
            txt("auto-reindex whenever indexed files go stale"),
        ]),
        Line::from(vec![key("r"), txt("rescan freshness now (else every 5s)")]),
        Line::from(vec![key("p"), txt("scope: this project ↔ all projects")]),
        Line::from(vec![key("s"), txt("sort commands: saved → calls → avg ms")]),
        Line::from(vec![key("space"), txt("pause/resume live refresh")]),
        Line::from(vec![key("q"), txt("quit (also esc, ctrl-c)")]),
        Line::from(""),
        dim(" saved = grep-then-Read baseline − cona output."),
        dim(" fail  = lookups that missed, were ambiguous or empty;"),
        dim("         the failures tab shows what would have worked."),
        dim(" hints → % = hook hints followed by a cona query"),
        dim("             within 2 minutes."),
        Line::from(""),
        dim(" any key closes"),
    ];
    let w = 62.min(area.width);
    let h = (lines.len() as u16 + 2).min(area.height);
    let r = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(lines).block(panel("keys").border_style(Style::default().fg(ACCENT))),
        r,
    );
}

fn fmt_k(n: i64) -> String {
    if n.abs() >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n.abs() >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Fit `s` into exactly `max` columns: pad when short, keep the TAIL with a
/// leading `…` when long (the tail of a path/symbol is the telling part).
fn trunc(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        format!("{s:<max$}")
    } else {
        let cut: String = s
            .chars()
            .rev()
            .take(max - 1)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("…{cut}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn snap(with_data: bool) -> Snapshot {
        let now = db::now();
        Snapshot {
            project_path: "/home/u/dev/some/deeply/nested/project/cona".into(),
            totals: db::Totals {
                calls: if with_data { 42 } else { 0 },
                tokens_out: if with_data { 9_000 } else { 0 },
                tokens_saved: if with_data { 51_000 } else { 0 },
                reads_blocked: if with_data { 3 } else { 0 },
                total_ms: 0,
            },
            per_cmd: if with_data {
                vec![
                    ("show".into(), 30, 4.2, 6_000, 40_000),
                    ("refs".into(), 12, 9.0, 3_000, 11_000),
                    ("index".into(), 2, 900.0, 0, 0),
                ]
            } else {
                vec![]
            },
            failures: if with_data {
                [("refs".to_string(), 2)].into_iter().collect()
            } else {
                HashMap::new()
            },
            hooks: if with_data { (10, 7) } else { (0, 0) },
            trend: if with_data {
                (0..TREND_DAYS as u64).map(|d| d * 100).collect()
            } else {
                vec![0; TREND_DAYS as usize]
            },
            top: if with_data {
                vec![(
                    "src/commands/query/show.rs:cmd_show_with_a_long_name".into(),
                    9,
                    12_000,
                )]
            } else {
                vec![]
            },
            recent: if with_data {
                vec![(now - 2, "show".into(), "locate_fresh".into(), 1_200, 3)]
            } else {
                vec![]
            },
            now,
        }
    }

    fn idx(with_data: bool) -> IndexState {
        if !with_data {
            return IndexState::default();
        }
        IndexState {
            files: 85,
            symbols: 1314,
            db_bytes: 2_400_000,
            last_indexed: Some(db::now() - 60),
            stale: vec![("src/a.rs".into(), false), ("src/gone.rs".into(), true)],
            langs: vec![("rust".into(), 80, 1300), ("toml".into(), 5, 14)],
            dense: vec![("src/cli.rs".into(), 120)],
        }
    }

    fn failed() -> Failed {
        vec![(
            (
                "show".into(),
                "locate_fresch".into(),
                "miss".into(),
                3,
                db::now() - 600,
            ),
            "closest: locate_fresh  src/commands/mod.rs:406".into(),
        )]
    }

    fn view(tab: Tab) -> View {
        View {
            tab,
            project_scope: true,
            sort: SortKey::Saved,
            paused: false,
            auto: false,
            help: false,
        }
    }

    fn render_with(
        s: &Snapshot,
        i: &IndexState,
        fl: &Failed,
        v: View,
        job: &Job,
        w: u16,
        h: u16,
    ) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, s, i, fl, v, job)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render(with_data: bool, v: View, w: u16, h: u16) -> String {
        render_with(
            &snap(with_data),
            &idx(with_data),
            &if with_data { failed() } else { vec![] },
            v,
            &Job::Idle,
            w,
            h,
        )
    }

    #[test]
    fn overview_renders_data_failures_and_hints() {
        let out = render(true, view(Tab::Overview), 120, 32);
        assert!(out.contains("85% of reads avoided"), "{out}");
        assert!(out.contains("2 failed"), "{out}");
        assert!(out.contains("hints → 70% followed"), "{out}");
        assert!(out.contains("2 stale"));
        assert!(out.contains("locate_fresh"));
        assert!(out.contains("maintenance index 2×"));
    }

    #[test]
    fn tab_bar_badges_stale_and_failed_counts() {
        let out = render(true, view(Tab::Overview), 120, 32);
        let bar = out.lines().nth(4).unwrap();
        assert!(bar.contains("2 index 2"), "{bar}");
        assert!(bar.contains("3 failures 2"), "{bar}");
    }

    #[test]
    fn empty_panels_explain_themselves() {
        let out = render(false, view(Tab::Overview), 120, 32);
        assert!(out.contains("no queries yet"), "{out}");
        assert!(out.contains("waiting for the first query"));
        assert!(!out.contains("failed"));
        let out = render(false, view(Tab::Index), 120, 32);
        assert!(out.contains("nothing indexed — press i"), "{out}");
        assert!(out.contains("index matches the working tree"));
    }

    #[test]
    fn index_tab_lists_languages_and_stale_files() {
        let out = render(true, view(Tab::Index), 120, 32);
        assert!(out.contains("rust"), "{out}");
        assert!(out.contains("modified src/a.rs"));
        assert!(out.contains("deleted  src/gone.rs"));
        assert!(out.contains("i reindexes"));
        assert!(out.contains("src/cli.rs"));
    }

    #[test]
    fn failures_tab_shows_the_learn_fix() {
        let out = render(true, view(Tab::Failures), 140, 32);
        assert!(out.contains("locate_fresch"), "{out}");
        assert!(out.contains("→ closest: locate_fresh"), "{out}");
        assert!(out.contains("cona learn"));
    }

    #[test]
    fn footer_shows_reindex_progress_and_result() {
        let s = snap(true);
        let i = idx(true);
        let running = Job::Running {
            since: Instant::now(),
            auto: true,
        };
        let out = render_with(&s, &i, &vec![], view(Tab::Index), &running, 120, 32);
        assert!(
            out.lines().last().unwrap().contains("auto-reindexing…"),
            "{out}"
        );
        assert!(out.contains("stale files · reindexing…"));
        let done = Job::Done {
            at: Instant::now(),
            ok: false,
            msg: "boom".into(),
        };
        let out = render_with(&s, &i, &vec![], view(Tab::Overview), &done, 120, 32);
        assert!(out.lines().last().unwrap().contains("✗ boom"), "{out}");
    }

    #[test]
    fn tiny_terminal_gets_a_notice_not_a_mangled_layout() {
        let out = render(true, view(Tab::Overview), 40, 10);
        assert!(out.contains("terminal too small"), "{out}");
    }

    #[test]
    fn minimum_size_renders_every_tab() {
        for t in Tab::ALL {
            let out = render(true, view(t), MIN_W, MIN_H);
            assert!(!out.contains("terminal too small"));
            assert!(out.lines().all(|l| l.chars().count() == MIN_W as usize));
        }
    }

    #[test]
    fn help_overlay_lists_keys() {
        let mut v = view(Tab::Overview);
        v.help = true;
        let out = render(true, v, 100, 32);
        assert!(out.contains("reindex now"), "{out}");
    }

    #[test]
    fn stretch_widens_bars_evenly() {
        assert_eq!(stretch(&[1, 2], 5), vec![1, 1, 2, 2]);
        assert_eq!(stretch(&[1, 2, 3], 2), vec![1, 2, 3]);
    }

    #[test]
    fn trunc_keeps_tail_and_pads() {
        assert_eq!(trunc("abc", 5), "abc  ");
        assert_eq!(trunc("abcdef", 4), "…def");
        assert_eq!(trunc("x", 0), "");
    }
}
