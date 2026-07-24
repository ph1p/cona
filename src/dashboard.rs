//! `cona ui` — a live TUI showing what cona is doing and, crucially,
//! how many tokens it is saving the agent in real time. Read-only: it polls the
//! SQLite databases (~1s) and never mutates anything.

use crate::{db, indexer};
use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Gauge, List, ListItem, Paragraph, Row, Table,
};
use ratatui::Frame;
use rusqlite::Connection;
use std::path::Path;
use std::time::{Duration, Instant};

const VERSION: &str = env!("CARGO_PKG_VERSION");

// One palette, used everywhere — keeps the TUI reading as a single surface.
const ACCENT: Color = Color::Cyan; // headings, project name, targets
const SAVED: Color = Color::Green; // token-savings numbers
const MUTED: Color = Color::DarkGray; // secondary text, paths, timestamps

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

struct Snapshot {
    project_path: String,
    files: i64,
    symbols: i64,
    db_bytes: i64,
    stale: i64,
    last_indexed: Option<i64>,
    totals: db::Totals,
    per_cmd: Vec<(String, i64, f64, i64, i64)>,
    top: Vec<(String, i64, i64)>,
    recent: Vec<(i64, String, String, i64, i64)>,
}

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

fn event_loop(terminal: &mut ratatui::DefaultTerminal, root: &Path) -> Result<()> {
    let root = root.to_path_buf();
    // scope: true = only this project, false = global across all projects
    let mut project_scope = true;
    let mut sort = SortKey::Saved;
    // Open the DBs ONCE — reopening per tick re-runs PRAGMAs/migration probes.
    // WAL still makes external reindexes visible to these long-lived handles.
    let g = db::open_global_db()?;
    let pconn = db::open_project_db(&root)?;
    // The index-state scan (one fs stat per indexed file) is the expensive part;
    // it rarely changes, so recompute it at most every 5s while the cheap usage
    // stats refresh every 1s.
    let mut idx = gather_index_state(&g, &pconn, &root)?;
    let mut snap = gather(&g, &root, project_scope, sort, &idx)?;
    let mut last = Instant::now();
    let mut last_idx = Instant::now();

    let refresh =
        |project_scope, sort, idx: &IndexState| gather(&g, &root, project_scope, sort, idx);

    loop {
        terminal.draw(|f| draw(f, &snap, project_scope, sort))?;

        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Resize(..) => {
                    terminal.draw(|f| draw(f, &snap, project_scope, sort))?;
                }
                Event::Key(k) if k.kind == KeyEventKind::Press => match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('p') | KeyCode::Tab => {
                        project_scope = !project_scope;
                        snap = refresh(project_scope, sort, &idx)?;
                        last = Instant::now();
                    }
                    KeyCode::Char('s') => {
                        sort = sort.next();
                        snap = refresh(project_scope, sort, &idx)?;
                    }
                    KeyCode::Char('r') => {
                        idx = gather_index_state(&g, &pconn, &root)?;
                        snap = refresh(project_scope, sort, &idx)?;
                        last = Instant::now();
                        last_idx = Instant::now();
                    }
                    KeyCode::Char('c')
                        if k.modifiers
                            .contains(ratatui::crossterm::event::KeyModifiers::CONTROL) =>
                    {
                        break
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        if last_idx.elapsed() >= Duration::from_secs(5) {
            idx = gather_index_state(&g, &pconn, &root)?;
            last_idx = Instant::now();
        }
        if last.elapsed() >= Duration::from_secs(1) {
            snap = refresh(project_scope, sort, &idx)?;
            last = Instant::now();
        }
    }
    Ok(())
}

/// Slow-changing project index state (file/symbol counts, staleness). One fs
/// stat per indexed file — throttled by the caller so it doesn't run every tick.
struct IndexState {
    files: i64,
    symbols: i64,
    stale: i64,
    db_bytes: i64,
    last_indexed: Option<i64>,
}

fn gather_index_state(g: &Connection, pconn: &Connection, root: &Path) -> Result<IndexState> {
    let files: i64 = pconn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap_or(0);
    let symbols: i64 = pconn
        .query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))
        .unwrap_or(0);
    // Batch: pull (path, mtime, size) once and compare against a single fs stat
    // per file — was N SQL queries (one per path via is_stale) every scan.
    let mut stale = 0i64;
    {
        let mut stmt = pconn.prepare("SELECT path, mtime, size FROM files")?;
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
                _ => stale += 1,
            }
        }
    }
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
        stale,
        db_bytes: db::project_db_size(root),
        last_indexed,
    })
}

fn gather(
    g: &Connection,
    root: &Path,
    project_scope: bool,
    sort: SortKey,
    idx: &IndexState,
) -> Result<Snapshot> {
    let scope = project_scope.then(|| root.to_string_lossy().to_string());
    let scope_ref = scope.as_deref();

    let mut per_cmd = db::per_command(g, scope_ref)?;
    // sort only the query rows; maintenance is folded out in draw_middle anyway
    per_cmd.sort_by(|a, b| match sort {
        SortKey::Saved => b.4.cmp(&a.4),
        SortKey::Calls => b.1.cmp(&a.1),
        SortKey::AvgMs => b.2.total_cmp(&a.2),
    });

    Ok(Snapshot {
        project_path: root.to_string_lossy().to_string(),
        files: idx.files,
        symbols: idx.symbols,
        db_bytes: idx.db_bytes,
        stale: idx.stale,
        last_indexed: idx.last_indexed,
        totals: db::totals(g, scope_ref)?,
        per_cmd,
        top: db::top_targets(g, scope_ref, 8)?,
        // live activity shows real queries only — index/edit/hook:* maintenance
        // carries no savings and is just noise in the feed
        recent: db::recent(g, scope_ref, 40, true)?,
    })
}

fn draw(f: &mut Frame, s: &Snapshot, project_scope: bool, sort: SortKey) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4), // header
            Constraint::Length(3), // savings gauge
            Constraint::Min(6),    // middle (queries, top targets, live activity)
            Constraint::Length(1), // footer
        ])
        .split(f.area());

    draw_header(f, root[0], s);
    draw_gauge(f, root[1], s);
    draw_middle(f, root[2], s, sort);
    draw_footer(f, root[3], project_scope, sort);
}

fn draw_header(f: &mut Frame, area: Rect, s: &Snapshot) {
    let name = Path::new(&s.project_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| s.project_path.clone());
    let last = s
        .last_indexed
        .map(db::ago)
        .unwrap_or_else(|| "never".into());
    let stale = if s.stale == 0 {
        Span::styled("fresh", Style::default().fg(SAVED))
    } else {
        Span::styled(
            format!("{} stale", s.stale),
            Style::default().fg(Color::Yellow),
        )
    };
    let lines = vec![
        Line::from(vec![
            Span::styled(
                name,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(s.project_path.clone(), Style::default().fg(MUTED)),
        ]),
        Line::from(vec![
            Span::styled(format!("{} files", s.files), Style::default().fg(ACCENT)),
            Span::styled(" · ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{} symbols", s.symbols),
                Style::default().fg(ACCENT),
            ),
            Span::styled(" · ", Style::default().fg(MUTED)),
            Span::raw(format!("db {}", db::human_bytes(s.db_bytes))),
            Span::styled(" · ", Style::default().fg(MUTED)),
            stale,
            Span::styled(format!(" · indexed {last}"), Style::default().fg(MUTED)),
        ]),
    ];
    let block = panel(&format!("cona v{VERSION} · live")).border_style(Style::default().fg(ACCENT));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_gauge(f: &mut Frame, area: Rect, s: &Snapshot) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    let pct = s.totals.pct_saved();
    let gauge = Gauge::default()
        .block(panel("tokens saved"))
        .gauge_style(Style::default().fg(tier_color(pct)).bg(Color::Black))
        .ratio((pct / 100.0).clamp(0.0, 1.0))
        .label(format!("{pct:.0}% of reads avoided"));
    f.render_widget(gauge, cols[0]);

    let t = &s.totals;
    let info = Line::from(vec![
        Span::styled(
            fmt_k(t.tokens_saved).to_string(),
            Style::default().fg(SAVED).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" saved", Style::default().fg(SAVED)),
        Span::styled("  ·  ", Style::default().fg(MUTED)),
        Span::raw(format!("{} used", fmt_k(t.tokens_out))),
        Span::styled("  ·  ", Style::default().fg(MUTED)),
        Span::styled(
            format!("{} would-read", fmt_k(t.baseline())),
            Style::default().fg(MUTED),
        ),
    ]);
    let info2 = Line::from(vec![
        Span::styled(format!("{} queries", t.calls), Style::default().fg(ACCENT)),
        Span::styled("  ·  ", Style::default().fg(MUTED)),
        Span::styled(
            format!("{} big reads intercepted", t.reads_blocked),
            Style::default().fg(Color::Yellow),
        ),
    ]);
    f.render_widget(
        Paragraph::new(vec![info, info2]).block(panel("totals")),
        cols[1],
    );
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

    // per-command table — queries only; maintenance rows (index/edit/hook:*)
    // never carry savings and are folded into one dim line below the table.
    let (queries, maint): (Vec<_>, Vec<_>) = s
        .per_cmd
        .iter()
        .partition(|(cmd, ..)| !db::is_maintenance_cmd(cmd));
    let table_area = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(left[0]);
    let rows = queries.iter().map(|(cmd, n, ms, out, saved)| {
        Row::new(vec![
            Cell::from(cmd.clone()),
            Cell::from(n.to_string()),
            Cell::from(format!("{ms:.0}")),
            Cell::from(fmt_k(*out)),
            Cell::from(Span::styled(fmt_k(*saved), Style::default().fg(SAVED))),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(16),
            Constraint::Length(6),
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Length(8),
        ],
    )
    .header(
        Row::new(vec!["cmd", "calls", "avg ms", "out", "saved"])
            .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
    )
    .block(panel(&format!("by command · ↓{}", sort.label())));
    f.render_widget(table, table_area[0]);
    if !maint.is_empty() {
        let parts: Vec<String> = maint
            .iter()
            .map(|(cmd, n, ..)| format!("{cmd} {n}×"))
            .collect();
        f.render_widget(
            Paragraph::new(format!(" maintenance {}", parts.join(" · ")))
                .style(Style::default().fg(MUTED)),
            table_area[1],
        );
    }

    // top targets
    let items: Vec<ListItem> = s
        .top
        .iter()
        .map(|(d, n, saved)| {
            ListItem::new(Line::from(vec![
                Span::styled(format!("{n:>3}× "), Style::default().fg(MUTED)),
                Span::styled(trunc(d, 26), Style::default().fg(ACCENT)),
                Span::styled(format!("  {}", fmt_k(*saved)), Style::default().fg(SAVED)),
            ]))
        })
        .collect();
    f.render_widget(List::new(items).block(panel("top targets")), left[1]);

    // activity feed
    let feed: Vec<ListItem> = s
        .recent
        .iter()
        .map(|(ts, cmd, detail, saved, ms)| {
            // feed is queries-only (maintenance/hook:* filtered in db::recent)
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:<10}", db::ago(*ts)), Style::default().fg(MUTED)),
                Span::styled(format!("{cmd:<16}"), Style::default().fg(Color::White)),
                Span::raw(trunc(detail, 22)),
                Span::styled(format!("  +{}", fmt_k(*saved)), Style::default().fg(SAVED)),
                Span::styled(format!(" {ms}ms"), Style::default().fg(MUTED)),
            ]))
        })
        .collect();
    f.render_widget(List::new(feed).block(panel("live activity")), cols[1]);
}

fn draw_footer(f: &mut Frame, area: Rect, project_scope: bool, sort: SortKey) {
    let scope = if project_scope { "project" } else { "global" };
    let key = Style::default().fg(Color::Black).bg(ACCENT);
    let spans = vec![
        Span::styled(" q ", key),
        Span::styled(" quit  ", Style::default().fg(MUTED)),
        Span::styled(" p ", key),
        Span::styled(format!(" scope:{scope}  "), Style::default().fg(MUTED)),
        Span::styled(" s ", key),
        Span::styled(
            format!(" sort:{}  ", sort.label()),
            Style::default().fg(MUTED),
        ),
        Span::styled(" r ", key),
        Span::styled(" refresh  ", Style::default().fg(MUTED)),
        Span::styled("· live", Style::default().fg(MUTED)),
    ];
    f.render_widget(Paragraph::new(Line::from(spans)), area);
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
