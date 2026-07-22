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
    Block, BorderType, Borders, Cell, Gauge, List, ListItem, Paragraph, Row, Sparkline, Table,
};
use ratatui::Frame;
use std::path::Path;
use std::time::{Duration, Instant};

const SPARK_BUCKETS: usize = 60;
const SPARK_SECS: i64 = 60; // one minute per bucket → last hour

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
    series: Vec<u64>,
}

pub fn run(root: &Path) -> Result<()> {
    let mut terminal = ratatui::init();
    let res = event_loop(&mut terminal, root);
    ratatui::restore();
    res
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, root: &Path) -> Result<()> {
    let root = root.to_path_buf();
    // scope: true = only this project, false = global across all projects
    let mut project_scope = true;
    let mut snap = gather(&root, project_scope)?;
    let mut last = Instant::now();

    loop {
        terminal.draw(|f| draw(f, &snap, project_scope))?;

        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(k) = event::read()? {
                if k.kind == KeyEventKind::Press {
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('p') | KeyCode::Tab => {
                            project_scope = !project_scope;
                            snap = gather(&root, project_scope)?;
                            last = Instant::now();
                        }
                        KeyCode::Char('c')
                            if k.modifiers
                                .contains(ratatui::crossterm::event::KeyModifiers::CONTROL) =>
                        {
                            break
                        }
                        _ => {}
                    }
                }
            }
        }
        if last.elapsed() >= Duration::from_secs(1) {
            snap = gather(&root, project_scope)?;
            last = Instant::now();
        }
    }
    Ok(())
}

fn gather(root: &Path, project_scope: bool) -> Result<Snapshot> {
    let g = db::open_global_db()?;
    let scope = project_scope.then(|| root.to_string_lossy().to_string());
    let scope_ref = scope.as_deref();

    // current-project index state (always shown in the header)
    let pconn = db::open_project_db(root)?;
    let files: i64 = pconn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap_or(0);
    let symbols: i64 = pconn
        .query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))
        .unwrap_or(0);
    let mut stale = 0i64;
    {
        let mut stmt = pconn.prepare("SELECT path FROM files")?;
        let paths: Vec<String> = stmt.query_map([], |r| r.get(0))?.flatten().collect();
        for p in paths {
            if indexer::is_stale(root, &pconn, &p) {
                stale += 1;
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

    Ok(Snapshot {
        project_path: root.to_string_lossy().to_string(),
        files,
        symbols,
        db_bytes: db::project_db_size(root),
        stale,
        last_indexed,
        totals: db::totals(&g, scope_ref)?,
        per_cmd: db::per_command(&g, scope_ref)?,
        top: db::top_targets(&g, scope_ref, 8)?,
        // live activity shows real queries only — index/edit/hook:* maintenance
        // carries no savings and is just noise in the feed
        recent: db::recent(&g, scope_ref, 40, true)?,
        series: db::savings_series(&g, scope_ref, SPARK_BUCKETS, SPARK_SECS)?,
    })
}

fn draw(f: &mut Frame, s: &Snapshot, project_scope: bool) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4), // header
            Constraint::Length(3), // savings gauge
            Constraint::Min(6),    // middle
            Constraint::Length(7), // sparkline
            Constraint::Length(1), // footer
        ])
        .split(f.area());

    draw_header(f, root[0], s);
    draw_gauge(f, root[1], s);
    draw_middle(f, root[2], s);
    draw_sparkline(f, root[3], s, project_scope);
    draw_footer(f, root[4], project_scope);
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
    let block =
        panel(&format!("cona v{VERSION} · live")).border_style(Style::default().fg(ACCENT));
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

fn draw_middle(f: &mut Frame, area: Rect, s: &Snapshot) {
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
    .block(panel("by command"));
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

fn draw_sparkline(f: &mut Frame, area: Rect, s: &Snapshot, project_scope: bool) {
    let total: u64 = s.series.iter().sum();
    let peak = s.series.iter().copied().max().unwrap_or(0);
    let scope = if project_scope {
        "this project"
    } else {
        "all projects"
    };
    let title = format!(
        " tokens saved per minute · {scope} · peak {}/min · total {} in last hour ",
        fmt_k(peak as i64),
        fmt_k(total as i64),
    );
    let block = panel(title.trim())
        .title_bottom(
            Line::from(Span::styled(" ◀ 60 min ago ", Style::default().fg(MUTED))).left_aligned(),
        )
        .title_bottom(
            Line::from(Span::styled(" now ▶ ", Style::default().fg(MUTED))).right_aligned(),
        );
    if total == 0 {
        f.render_widget(
            Paragraph::new("no tokens saved in the last hour — each bar is one minute of savings")
                .style(Style::default().fg(MUTED))
                .block(block),
            area,
        );
        return;
    }
    let spark = Sparkline::default()
        .block(block)
        .data(&s.series)
        .style(Style::default().fg(SAVED));
    f.render_widget(spark, area);
}

fn draw_footer(f: &mut Frame, area: Rect, project_scope: bool) {
    let scope = if project_scope { "project" } else { "global" };
    let key = Style::default().fg(Color::Black).bg(ACCENT);
    let line = Line::from(vec![
        Span::styled(" q ", key),
        Span::styled(" quit  ", Style::default().fg(MUTED)),
        Span::styled(" p ", key),
        Span::styled(format!(" scope: {scope}  "), Style::default().fg(MUTED)),
        Span::styled("· refreshing every 1s", Style::default().fg(MUTED)),
    ]);
    f.render_widget(Paragraph::new(line), area);
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
