use cona::commands::*;
use cona::{dashboard, db, hook, indexer, install};

use anyhow::Result;
use clap::Parser;
use cona::ui;
use std::path::Path;
use std::time::Instant;

mod cli;
use cli::*;

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    if cli.read_only {
        db::set_read_only(true);
        if !read_only_command(&cli.cmd) {
            anyhow::bail!(
                "`--read-only` only permits navigation and inspection commands; run without it to modify code, indexes, configuration, or statistics"
            );
        }
    }
    let root = db::project_root()?;
    let t0 = Instant::now();

    // Auto-update/tidy runs before every command except the install-lifecycle
    // ones (they manage the binary themselves) and the hook (latency-sensitive).
    if !cli.read_only
        && !matches!(
            &cli.cmd,
            Cmd::Maint(
                Maint::Install(_) | Maint::Upgrade(_) | Maint::Uninstall(_) | Maint::Hook(_)
            ) | Cmd::InstallFlat(_)
                | Cmd::UpgradeFlat(_)
                | Cmd::UninstallFlat(_)
                | Cmd::HookFlat(_)
        )
    {
        install::maybe_auto_update(&root);
        db::auto_tidy();
    }

    // Each operation has one body, shared by its grouped spelling (`nav show`)
    // and its flat alias (`show`) via an or-pattern — no separate dispatch enum.
    match &cli.cmd {
        Cmd::Project(Project::Index(a)) | Cmd::IndexFlat(a) => {
            let IndexArgs {
                quiet,
                watch,
                session_start,
            } = a;
            if db::is_home_or_fs_root(&root) {
                // A typed `cona index` in $HOME is a deliberate act: warn, then
                // do it. The SessionStart hook is not — it fires unattended in
                // whatever cwd the harness happens to have, and an agent app
                // launched from $HOME made every session walk the whole home
                // tree (several concurrent multi-hundred-MB walks). Refuse
                // there, quietly and with success: the hook is fail-open, a
                // session must never break over a missing index.
                if *session_start {
                    return Ok(());
                }
                if !quiet {
                    eprintln!(
                        "warning: indexing {} (home/filesystem root) walks a huge tree — \
                         consider running inside a project (or `git init`) instead",
                        root.display()
                    );
                }
            }
            let conn = db::open_project_db(&root)?;
            // Sessions opening together each fire this hook on the same tree.
            // Only one walk is useful: the loser reads the counts the winner is
            // writing and still emits its orientation block, so the session
            // gets its context without paying for a duplicate walk. Only the
            // hook dedupes — a typed `cona index` always indexes, because the
            // user asked for a walk, not for a warm index.
            let lock = session_start.then(|| db::IndexLock::acquire(&root));
            let skipped = matches!(lock, Some(None));
            let r = if skipped {
                indexer::counts(&conn).unwrap_or_default()
            } else {
                indexer::index_project(&root, &conn)?
            };
            let ms = t0.elapsed().as_millis() as i64;
            if !quiet {
                println!(
                    "indexed {} in {}ms — files: {} ({} parsed, {} removed), symbols: {}",
                    root.display(),
                    ms,
                    r.total_files,
                    r.parsed,
                    r.removed,
                    r.total_symbols
                );
            }
            if !skipped {
                db::log_usage(&root, "index", ms, r.total_symbols, 0, 0);
            }
            if *session_start {
                // The SessionStart hook runs `index --quiet --session-start`.
                // Beyond keeping the index warm, hand the agent repo-specific
                // orientation up front (the static guide alone was too easy to
                // skim past). Fail-open: any error → no context block, never a
                // broken session start.
                print!("{}", session_start_context(&root, &conn, &r));
            }
            if *watch {
                indexer::watch_project(&root, &conn)?;
            }
        }
        Cmd::Nav(Nav::Tree(a)) | Cmd::Tree(a) => {
            let TreeArgs { budget, path, rank } = a;
            queried(&root, t0, "tree", path.as_deref().unwrap_or(""), |conn| {
                if *rank {
                    cmd_tree_rank(&root, conn, *budget, path.as_deref(), cli.json)
                } else {
                    cmd_tree(&root, conn, *budget, path.as_deref(), cli.json)
                }
            })?;
        }
        Cmd::Nav(Nav::Outline(a)) | Cmd::Outline(a) => {
            let OutlineArgs { file, sig } = a;
            queried(&root, t0, "outline", file, |conn| {
                cmd_outline(&root, conn, file, *sig, cli.json)
            })?;
        }
        Cmd::Nav(Nav::Find(a)) | Cmd::Find(a) => {
            let FindArgs {
                name,
                kind,
                limit,
                path,
            } = a;
            queried(&root, t0, "find", name, |conn| {
                cmd_find(
                    &root,
                    conn,
                    name,
                    kind.as_deref(),
                    *limit,
                    path.as_deref(),
                    cli.json,
                )
            })?;
        }
        Cmd::Nav(Nav::Show(a)) | Cmd::Show(a) => {
            let ShowArgs {
                symbols,
                all,
                context,
                kind,
                sig,
            } = a;
            let conn = open_indexed(&root)?;
            let mut out = String::new();
            let mut baseline = 0i64;
            let mut resolved: Vec<&str> = Vec::new();
            for (i, symbol) in symbols.iter().enumerate() {
                match cmd_show(
                    &root,
                    &conn,
                    symbol,
                    ShowOpts {
                        context: *context,
                        kind: kind.as_deref(),
                        sig: *sig,
                        all: *all,
                    },
                    cli.json,
                ) {
                    Ok((o, b)) => {
                        if i > 0 && !cli.json {
                            out.push('\n');
                        }
                        out.push_str(&o);
                        baseline += b;
                        resolved.push(symbol);
                    }
                    // one bad name must not abort the batch — flag it and continue
                    Err(e) => out.push_str(&format!("error: {symbol}: {e}\n")),
                }
            }
            print!("{out}");
            // log only resolved names as stats targets — failed probes are not
            // "top targets"; and scripts checking $? must see batch failures
            if !resolved.is_empty() {
                finish(&root, "show", t0, &out, baseline, &resolved.join(","));
            }
            if resolved.len() < symbols.len() {
                std::process::exit(1);
            }
        }
        Cmd::Nav(Nav::Refs(a)) | Cmd::Refs(a) => {
            let RefsArgs { name, limit, path } = a;
            queried(&root, t0, "refs", name, |conn| {
                cmd_refs(&root, conn, name, *limit, path.as_deref(), cli.json)
            })?;
        }
        Cmd::Inspect(Inspect::Context(a)) | Cmd::ContextFlat(a) => {
            let ContextArgs {
                symbol,
                budget,
                no_tests,
            } = a;
            queried(&root, t0, "context", symbol, |conn| {
                cmd_context(&root, conn, symbol, *budget, *no_tests, cli.json)
            })?;
        }
        Cmd::Inspect(Inspect::Diff(a)) | Cmd::DiffFlat(a) => {
            let DiffArgs { r#ref } = a;
            queried(&root, t0, "diff", r#ref, |conn| {
                cmd_diff(&root, conn, r#ref, cli.json)
            })?;
        }
        Cmd::Nav(Nav::Grep(a)) | Cmd::Grep(a) => {
            let GrepArgs {
                pattern,
                ignore_case,
                regex,
                limit,
                path,
                include_deps,
            } = a;
            queried(&root, t0, "grep", pattern, |conn| {
                cmd_grep(
                    &root,
                    conn,
                    pattern,
                    GrepOpts {
                        ignore_case: *ignore_case,
                        regex: *regex,
                        limit: *limit,
                        path: path.as_deref(),
                        include_deps: *include_deps,
                    },
                    cli.json,
                )
            })?;
        }
        Cmd::Edit(EditCmd::Edit(a)) | Cmd::EditFlat(a) => {
            let EditArgs {
                symbol,
                file,
                range,
                force,
            } = a;
            queried(&root, t0, "edit", symbol, |conn| {
                let out = match range {
                    Some(r) => {
                        let (s, e) = parse_range(r)?;
                        let code = read_replacement(file.as_deref())?;
                        // with --range, `symbol` is the file path
                        cmd_edit_range(&root, conn, symbol, s, e, &code, *force)?
                    }
                    None => cmd_edit(&root, conn, symbol, file.as_deref(), *force)?,
                };
                Ok((out, 0))
            })?;
        }
        Cmd::Edit(EditCmd::Insert(a)) | Cmd::InsertFlat(a) => {
            let InsertArgs {
                symbol,
                after,
                at,
                file,
                force,
            } = a;
            let code = read_replacement(file.as_deref())?;
            let at_pos = match at {
                Some(v) => {
                    let line: usize = v[1].parse().map_err(|_| {
                        anyhow::anyhow!("--at LINE must be a number, got '{}'", v[1])
                    })?;
                    Some((v[0].clone(), line))
                }
                None => None,
            };
            let target = symbol.as_deref().unwrap_or("--at");
            queried(&root, t0, "edit", target, |conn| {
                Ok((
                    cmd_insert(
                        &root,
                        conn,
                        symbol.as_deref(),
                        *after,
                        at_pos,
                        &code,
                        *force,
                    )?,
                    0,
                ))
            })?;
        }
        Cmd::Edit(EditCmd::Check(a)) | Cmd::CheckFlat(a) => {
            let CheckArgs { file } = a;
            queried(&root, t0, "check", file.as_deref().unwrap_or("*"), |conn| {
                cmd_check(&root, conn, file.as_deref(), cli.json)
            })?;
        }
        Cmd::Inspect(Inspect::Impact(a)) | Cmd::ImpactFlat(a) => {
            let ImpactArgs { symbol } = a;
            queried(&root, t0, "impact", symbol, |conn| {
                cmd_impact(&root, conn, symbol, cli.json)
            })?;
        }
        Cmd::Inspect(Inspect::Entries(a)) | Cmd::EntriesFlat(a) => {
            let EntriesArgs { path, limit } = a;
            queried(
                &root,
                t0,
                "entries",
                path.as_deref().unwrap_or(""),
                |conn| cmd_entries(conn, path.as_deref(), *limit, cli.json),
            )?;
        }
        Cmd::Inspect(Inspect::Tests(a)) | Cmd::TestsFlat(a) => {
            let TestsArgs { symbol } = a;
            queried(&root, t0, "tests", symbol, |conn| {
                cmd_tests(&root, conn, symbol, cli.json)
            })?;
        }
        Cmd::History(History::Blame(a)) | Cmd::BlameFlat(a) => {
            let BlameArgs { symbol, limit } = a;
            queried(&root, t0, "blame", symbol, |conn| {
                cmd_blame(&root, conn, symbol, *limit, cli.json)
            })?;
        }
        Cmd::History(History::Hot(a)) | Cmd::HotFlat(a) => {
            let HotArgs { since, limit } = a;
            queried(&root, t0, "hot", "", |conn| {
                cmd_hot(&root, conn, since, *limit, cli.json)
            })?;
        }
        Cmd::History(History::Coupling(a)) | Cmd::CouplingFlat(a) => {
            let CouplingArgs { file, since, limit } = a;
            queried(&root, t0, "coupling", file, |conn| {
                cmd_coupling(&root, conn, file, since, *limit, cli.json)
            })?;
        }
        Cmd::Inspect(Inspect::Callers(a)) | Cmd::CallersFlat(a) => {
            let CallsArgs { symbol, depth } = a;
            queried(&root, t0, "callers", symbol, |conn| {
                cmd_calls(&root, conn, symbol, *depth, true, cli.json)
            })?;
        }
        Cmd::Inspect(Inspect::Callees(a)) | Cmd::CalleesFlat(a) => {
            let CallsArgs { symbol, depth } = a;
            queried(&root, t0, "callees", symbol, |conn| {
                cmd_calls(&root, conn, symbol, *depth, false, cli.json)
            })?;
        }
        Cmd::Inspect(Inspect::Path(a)) | Cmd::PathFlat(a) => {
            let PathArgs {
                from,
                to,
                max_depth,
            } = a;
            queried(&root, t0, "path", &format!("{from}→{to}"), |conn| {
                cmd_path(&root, conn, from, to, *max_depth, cli.json)
            })?;
        }
        Cmd::Edit(EditCmd::Note(a)) | Cmd::NoteFlat(a) => {
            let NoteArgs { symbol, text, rm } = a;
            queried(&root, t0, "note", symbol.as_deref().unwrap_or(""), |conn| {
                Ok((cmd_note(conn, symbol.as_deref(), text, *rm)?, 0))
            })?;
        }
        Cmd::Inspect(Inspect::Shape(a)) | Cmd::ShapeFlat(a) => {
            let ShapeArgs {
                symbol,
                budget,
                kind,
            } = a;
            queried(&root, t0, "shape", symbol, |conn| {
                cmd_shape(&root, conn, symbol, *budget, kind.as_deref(), cli.json)
            })?;
        }
        Cmd::Inspect(Inspect::Deps(a)) | Cmd::DepsFlat(a) => {
            let DepsArgs { path, path_pos } = a;
            let path = path.as_deref().or(path_pos.as_deref());
            queried(&root, t0, "deps", path.unwrap_or(""), |conn| {
                cmd_deps(&root, conn, path, cli.json)
            })?;
        }
        Cmd::Edit(EditCmd::Rename(a)) | Cmd::RenameFlat(a) => {
            let RenameArgs {
                symbol,
                new_name,
                force,
            } = a;
            queried(&root, t0, "rename", symbol, |conn| {
                Ok((cmd_rename(&root, conn, symbol, new_name, *force)?, 0))
            })?;
        }
        Cmd::Project(Project::Stats(a)) | Cmd::StatsFlat(a) => {
            let StatsArgs { project } = a;
            let out = if cli.json {
                cmd_stats_json(&root, *project)?
            } else {
                cmd_stats(&root, *project)?
            };
            print!("{out}");
        }
        Cmd::Project(Project::Ui) | Cmd::UiFlat => {
            dashboard::run(&root)?;
        }
        Cmd::Maint(Maint::Doctor) | Cmd::DoctorFlat => {
            install::cmd_doctor(&root, cli.json)?;
        }
        Cmd::Project(Project::Tidy(a)) | Cmd::TidyFlat(a) => {
            let TidyArgs { orphans } = a;
            let before = db::total_storage_bytes();
            let r = db::tidy(*orphans, true)?;
            println!(
                "tidy: pruned {} usage rows{}, storage {} → {} (reclaimed {})",
                r.usage_deleted,
                if *orphans {
                    format!(", removed {} orphaned index(es)", r.orphans_removed)
                } else {
                    String::new()
                },
                db::human_bytes(before),
                db::human_bytes(r.bytes_after),
                db::human_bytes(r.bytes_reclaimed()),
            );
            if !*orphans {
                println!("(pass --orphans to also drop indexes for projects whose folder is gone)");
            }
        }
        Cmd::Project(Project::Forget(a)) | Cmd::ForgetFlat(a) => {
            let ForgetArgs { path } = a;
            let target = match path {
                Some(p) => std::fs::canonicalize(p).unwrap_or_else(|_| std::path::PathBuf::from(p)),
                None => root.clone(),
            };
            let freed = db::forget_project(&target)?;
            println!(
                "forgot {} — reclaimed {}",
                target.display(),
                db::human_bytes(freed)
            );
        }
        Cmd::Project(Project::Reset(a)) | Cmd::ResetFlat(a) => {
            let ResetArgs { keep_stats } = a;
            let freed = db::remove_project_data(&root, *keep_stats)?;
            let conn = db::open_project_db(&root)?;
            let r = indexer::index_project(&root, &conn)?;
            println!(
                "reset {} — dropped {} of old data{}, reindexed {} files, {} symbols",
                root.display(),
                db::human_bytes(freed),
                if *keep_stats { " (stats kept)" } else { "" },
                r.total_files,
                r.total_symbols
            );
        }
        Cmd::Maint(Maint::Hook(a)) | Cmd::HookFlat(a) => {
            let HookArgs { event } = a;
            hook::run(event)?;
        }
        Cmd::Maint(Maint::Mcp) | Cmd::McpFlat => {
            cmd_mcp(&root)?;
        }
        Cmd::Project(Project::Projects) | Cmd::ProjectsFlat => {
            let g = db::open_global_db()?;
            let mut stmt = g.prepare(
                "SELECT path, files, symbols, last_indexed FROM projects ORDER BY last_indexed DESC",
            )?;
            let rows: Vec<(String, i64, i64, Option<i64>)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .flatten()
                .collect();
            if cli.json {
                let items: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|(p, f, s, li)| {
                        serde_json::json!({
                            "path": p,
                            "files": f,
                            "symbols": s,
                            "last_indexed": li,
                        })
                    })
                    .collect();
                println!("{}", serde_json::Value::Array(items));
            } else {
                if rows.is_empty() {
                    println!("no projects indexed yet — run `cona index` inside a project");
                }
                for (p, f, s, li) in rows {
                    let when = li.map(db::ago).unwrap_or_else(|| "never".into());
                    println!("{p}  ({f} files, {s} symbols, indexed {when})");
                }
            }
        }
        Cmd::Maint(Maint::Hooks(a)) | Cmd::HooksFlat(a) => {
            let HooksArgs { action } = a;
            install::cmd_hooks(&root, action.as_str())?;
        }
        Cmd::Maint(Maint::Skill) | Cmd::SkillFlat => {
            print!("{}", install::SKILL_MD);
        }
        Cmd::Maint(Maint::Setup(a)) | Cmd::SetupFlat(a) => {
            let SetupArgs { target, yes } = a;
            cmd_setup(&root, *target, *yes)?;
        }
        Cmd::Maint(Maint::Install(a)) | Cmd::InstallFlat(a) => {
            let InstallArgs { bin_dir } = a;
            install::cmd_install(bin_dir.as_deref())?;
        }
        Cmd::Maint(Maint::Upgrade(a)) | Cmd::UpgradeFlat(a) => {
            let UpgradeArgs { quiet } = a;
            install::cmd_upgrade(*quiet)?;
        }
        Cmd::Maint(Maint::Uninstall(a)) | Cmd::UninstallFlat(a) => {
            let UninstallArgs { purge, yes } = a;
            install::cmd_uninstall(*purge, *yes)?;
        }
        Cmd::Maint(Maint::Agents(a)) | Cmd::AgentsFlat(a) => {
            use std::io::IsTerminal;
            let AgentsArgs {
                action,
                names,
                all,
                global,
            } = a;
            match action {
                Some(AgentAction::Status) => install::cmd_agents_status(&root)?,
                Some(AgentAction::Install) => {
                    install::cmd_agents(&root, "install", names, *all, *global)?;
                }
                Some(AgentAction::Uninstall) => {
                    install::cmd_agents(&root, "uninstall", names, *all, *global)?;
                }
                // Bare `cona agents`: interactive checklist on a TTY, else status.
                // (clap requires an ACTION before any AGENT, so names/--all
                // never arrive here without a verb.)
                None if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() => {
                    install::cmd_agents_interactive(&root, *global)?;
                }
                None => install::cmd_agents_status(&root)?,
            }
        }
    }
    Ok(())
}

/// The one shape shared by every query arm in `run()`: open the index, run the
/// body against it, print its output, log it via `finish`. `target` is the
/// stats detail (symbol/file) — the caller computes it up front, and a failed
/// body logs nothing. Mutations reuse it with a baseline of 0 (nothing was
/// "read instead").
fn queried(
    root: &Path,
    t0: Instant,
    cmd: &'static str,
    target: &str,
    body: impl FnOnce(&rusqlite::Connection) -> Result<(String, i64)>,
) -> Result<()> {
    let conn = open_indexed(root)?;
    let (out, baseline) = body(&conn)?;
    print!("{out}");
    finish(root, cmd, t0, &out, baseline, target);
    Ok(())
}

/// The command-level guard for `--read-only`. The database layer independently
/// opens SQLite read-only and suppresses telemetry, while this protects source
/// files and integration configuration from write-capable subcommands.
fn read_only_command(cmd: &Cmd) -> bool {
    matches!(
        cmd,
        Cmd::Nav(_)
            | Cmd::Inspect(_)
            | Cmd::History(_)
            | Cmd::Edit(EditCmd::Check(_))
            | Cmd::Tree(_)
            | Cmd::Outline(_)
            | Cmd::Find(_)
            | Cmd::Show(_)
            | Cmd::Refs(_)
            | Cmd::Grep(_)
            | Cmd::ContextFlat(_)
            | Cmd::DiffFlat(_)
            | Cmd::ImpactFlat(_)
            | Cmd::ShapeFlat(_)
            | Cmd::DepsFlat(_)
            | Cmd::EntriesFlat(_)
            | Cmd::TestsFlat(_)
            | Cmd::CallersFlat(_)
            | Cmd::CalleesFlat(_)
            | Cmd::PathFlat(_)
            | Cmd::CheckFlat(_)
            | Cmd::BlameFlat(_)
            | Cmd::HotFlat(_)
            | Cmd::CouplingFlat(_)
            // stats/projects only read: telemetry writes are already suppressed
            // by the read-only DB layer, and SKILL.md sells --read-only as safe
            // for all inspection
            | Cmd::Project(Project::Stats(_))
            | Cmd::Project(Project::Projects)
            | Cmd::StatsFlat(_)
            | Cmd::ProjectsFlat
    )
}

/// Parse an "S-E" line range (1-based, inclusive) for `edit --range`.
fn parse_range(s: &str) -> Result<(usize, usize)> {
    let bad = || anyhow::anyhow!("--range must be S-E with 1-based S ≤ E, e.g. 42-48");
    let (a, b) = s.split_once('-').ok_or_else(bad)?;
    let start: usize = a.trim().parse().map_err(|_| bad())?;
    let end: usize = b.trim().parse().map_err(|_| bad())?;
    if start == 0 || end < start {
        return Err(bad());
    }
    Ok((start, end))
}

/// Read replacement/insert source from a file, or stdin when no file is given.
fn read_replacement(file: Option<&str>) -> Result<String> {
    use std::io::Read;
    match file {
        Some(f) => Ok(std::fs::read_to_string(f)?),
        None => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            Ok(buf)
        }
    }
}

/// One-shot setup. Indexes the project, then wires agent integration for the
/// project and/or the global home configs. No scope → interactive chooser on
/// a terminal, both otherwise.
/// Thousands-separated count for human-facing tallies (e.g. `1234567` → `1,234,567`).
fn fmt_count(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    if n < 0 {
        format!("-{out}")
    } else {
        out
    }
}

/// Build the SessionStart context block the agent sees at the top of a session.
///
/// For an indexed project: a short reference-ranked symbol map (the fastest
/// orientation cona offers) plus the one-line habit. Failure to render the
/// map degrades to just the habit line — never an error. Emitted as Claude's
/// `hookSpecificOutput.additionalContext` JSON so the text lands in context
/// without a user-visible message.
fn session_start_context(
    root: &Path,
    conn: &rusqlite::Connection,
    report: &indexer::IndexReport,
) -> String {
    // A tight budget: enough to orient, not so much it floods the session.
    let map = cmd_tree_rank(root, conn, 900, None, false)
        .map(|(out, _)| out)
        .unwrap_or_default();
    let mut ctx = String::new();
    ctx.push_str(&format!(
        "cona has this project indexed ({} files, {} symbols). Before you Read a \
         whole code file or Grep for a name, reach for cona: `cona outline <file>` \
         \u{2192} `cona show <Symbol>` reads one symbol, `cona grep`/`refs <Name>` \
         searches code semantically.\n",
        report.total_files, report.total_symbols
    ));
    // One statement is enough for current models — the "standing rule for the
    // WHOLE session" reinforcement paragraph that used to follow here was
    // repeated-instruction noise (same rule as the line above and the agent
    // guide). The PreToolUse redirect remains the backstop for an actual
    // wrong Read/Grep.
    // A little social proof: surface what the habit has already bought on this
    // project. Cheap SELECT, fully fail-open — no tally, no line.
    if let Some(saved) = db::open_global_db()
        .and_then(|g| db::totals(&g, root.to_str()))
        .map(|t| t.tokens_saved)
        .ok()
        .filter(|s| *s > 0)
    {
        ctx.push_str(&format!(
            "So far cona has saved ~{} tokens on this project (`cona stats` for the breakdown).\n",
            fmt_count(saved)
        ));
    }
    ctx.push_str("\nMost-referenced symbols (your orientation map):\n\n");
    ctx.push_str(&map);

    let payload = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": ctx,
        }
    });
    match serde_json::to_string(&payload) {
        Ok(s) => format!("{s}\n"),
        Err(_) => String::new(),
    }
}

fn cmd_setup(root: &Path, scope: Option<SetupScope>, yes: bool) -> Result<()> {
    use std::io::IsTerminal;
    println!("{}", ui::banner("cona setup"));

    // An explicit scope arg or `--yes` forces a non-interactive run; otherwise
    // a terminal gets the checklist. Scope decides which sections are in play.
    let explicit = scope.is_some();
    let scope = scope.unwrap_or(SetupScope::All);
    let do_project = scope != SetupScope::Global;
    let do_global = scope != SetupScope::Project;
    let interactive =
        !yes && !explicit && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();

    // Record where this binary lives if `install` never did (prebuilt-binary
    // users install via curl/wget, not from a source checkout) — otherwise
    // `cona upgrade` / auto-update have no target path to replace.
    if db::meta_get("install_path")?.is_none() {
        if let Ok(exe) = std::env::current_exe() {
            db::meta_set("install_path", &exe.to_string_lossy())?;
        }
    }

    // --- 1. index (always) -------------------------------------------------
    println!("{}", ui::heading("index"));
    let conn = db::open_project_db(root)?;
    let r = indexer::index_project(root, &conn)?;
    println!(
        "{}",
        ui::ok(&format!(
            "indexed {} files, {} symbols",
            r.total_files, r.total_symbols
        ))
    );

    // --- 2. git hooks (project scope only) ---------------------------------
    if do_project {
        println!();
        if root.join(".git").exists() {
            install::cmd_hooks(root, "install")?;
        } else {
            println!("{}", ui::heading("git hooks"));
            println!(
                "{}",
                ui::warn("no .git — skipped git hooks (run `cona hooks install` later)")
            );
        }
    }

    // --- 3. agents — pick per scope (interactive) or take every detected ---
    // `(project_agents, global_agents)`.
    let (proj_plan, glob_plan) = if interactive {
        match install::pick_agents(root, do_project, do_global)? {
            Some(p) => p,
            None => {
                println!("{}", ui::dim("setup cancelled — nothing changed"));
                return Ok(());
            }
        }
    } else {
        // non-interactive: install every detected agent in the active scopes.
        // Nothing is removed — an unattended run never takes integrations away.
        let home = dirs::home_dir().unwrap_or_default();
        let detected = |on: bool, global: bool| install::ScopePlan {
            add: if on {
                install::detected_agents(root, &home, global)
            } else {
                Vec::new()
            },
            remove: Vec::new(),
        };
        (detected(do_project, false), detected(do_global, true))
    };

    let (mut configured, mut removed) = (0usize, 0usize);
    for (global, plan, label) in [
        (false, &proj_plan, "project"),
        (true, &glob_plan, "home configs"),
    ] {
        if plan.add.is_empty() && plan.remove.is_empty() {
            continue;
        }
        println!("\n{}", ui::heading(&format!("agents — {label}")));
        // Remove first: an agent can only be in one of the two lists, but
        // removing before installing keeps the printed order readable.
        if !plan.remove.is_empty() {
            install::cmd_agents(root, "uninstall", &plan.remove, false, global)?;
            removed += plan.remove.len();
        }
        if !plan.add.is_empty() {
            install::cmd_agents(root, "install", &plan.add, false, global)?;
            configured += plan.add.len();
        }
    }

    // --- 4. summary --------------------------------------------------------
    let mut summary = format!(
        "setup complete — {configured} agent{} configured",
        if configured == 1 { "" } else { "s" }
    );
    if removed > 0 {
        summary.push_str(&format!(", {removed} removed"));
    }
    println!("\n{}", ui::ok(&ui::bold(&summary)));

    // Lead with what to DO now (the payoff), then how to manage what was just
    // wired. Aligned table, same shape as `install`'s next-steps block.
    println!("\n{}", ui::heading("try it"));
    print!(
        "{}",
        ui::cmd_table(&[
            (
                "cona tree --rank",
                "orient — symbols by how often referenced"
            ),
            ("cona show <Symbol>", "read one symbol, not the whole file"),
            ("cona agents status", "what's configured, per scope"),
            ("cona agents", "interactive checklist — toggle any agent"),
            ("cona doctor", "verify the installation any time"),
        ])
    );
    Ok(())
}
