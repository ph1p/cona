use cona::commands::*;
use cona::{dashboard, db, hook, indexer, install};

use anyhow::Result;
use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Parser, Subcommand};
use cona::ui;
use std::path::Path;
use std::time::Instant;

/// What `setup` configures — project, home configs, or both.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum SetupScope {
    Project,
    Global,
    All,
}

const HELP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::Green.on_default())
    .placeholder(AnsiColor::Yellow.on_default());

#[derive(Parser)]
#[command(
    name = "cona",
    version,
    about = "Token-efficient code navigation & editing for AI agents",
    long_about = "Token-efficient code navigation & editing for AI agents.\n\n\
        Indexes a repo with tree-sitter and lets you read ONE symbol instead of a\n\
        whole file, search code semantically, and edit with syntax verification —\n\
        a fraction of the tokens of Read + grep.",
    after_help = "\
QUICK START
  cona setup            First-time: index + git hooks + agent configs (interactive)
  cona tree --rank      Orient — symbols ranked by how often they're referenced
  cona outline <file>   List a file's symbols with line ranges
  cona show <Symbol>    Print just that symbol's source (not the whole file)

COMMON FLOWS
  Find a definition        cona find <Name>        (not grep)
  Find usages              cona refs <Name>        (semantic — skips strings/comments)
  Understand a symbol      cona context <Symbol>   (source + callees + call sites)
  Before an edit           cona impact <Symbol>    (refs + callers + tests + history)
  Review changes           cona diff [ref]         (changed symbols vs a git ref)

Run `cona <group> --help` (nav, inspect, code, history, project, maint) for the\n\
full command list, or `cona help <command>` for one command.",
    styles = HELP_STYLES
)]
struct Cli {
    /// Output machine-readable JSON where supported
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

/// install|uninstall for the `hooks` and `agents` commands.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum IntegrationAction {
    Install,
    Uninstall,
}

impl IntegrationAction {
    fn as_str(self) -> &'static str {
        match self {
            IntegrationAction::Install => "install",
            IntegrationAction::Uninstall => "uninstall",
        }
    }
}

// ── Per-command argument structs ──────────────────────────────────────────
// Args live here ONCE and are reused by both the flat aliases (`Cmd`) and the
// grouped, canonical subcommands (`Nav`/`Inspect`/`Edit`/…). Dispatch in
// `run()` reads them through these structs, so there is a single source of
// truth for every flag.

#[derive(clap::Args)]
struct IndexArgs {
    /// Suppress output (for hooks)
    #[arg(short, long)]
    quiet: bool,
    /// Keep running: watch the filesystem and reindex on change (debounced)
    #[arg(long)]
    watch: bool,
    /// Emit a SessionStart context block (repo orientation) as JSON on
    /// stdout after indexing. Wired to the SessionStart hook so a fresh
    /// session gets repo-specific orientation, not just a static guide.
    #[arg(long)]
    session_start: bool,
}

#[derive(clap::Args)]
struct TreeArgs {
    /// Approximate token budget for the output
    #[arg(long, default_value_t = defaults::TREE_BUDGET)]
    budget: i64,
    /// Only show files under this path prefix
    #[arg(long)]
    path: Option<String>,
    /// Rank symbols by reference fan-in (most-referenced first) instead of by path
    #[arg(long)]
    rank: bool,
}

#[derive(clap::Args)]
struct OutlineArgs {
    file: String,
    /// Show full signatures (default: kind + name + line range only)
    #[arg(long)]
    sig: bool,
}

#[derive(clap::Args)]
struct FindArgs {
    name: String,
    /// Filter by kind (fn, struct, class, method, ...)
    #[arg(long)]
    kind: Option<String>,
    #[arg(long, default_value_t = defaults::FIND_LIMIT)]
    limit: i64,
}

#[derive(clap::Args)]
struct ShowArgs {
    /// One or more symbol names — each printed in turn
    #[arg(required = true)]
    symbols: Vec<String>,
    /// Extra lines of context above and below
    #[arg(long, default_value_t = defaults::SHOW_CONTEXT)]
    context: usize,
    /// Narrow to a kind (fn, struct, impl, class, …) — resolves struct/impl same-name ambiguity
    #[arg(long)]
    kind: Option<String>,
    /// Print only the signature line(s), not the body — the leanest peek at a symbol
    #[arg(long)]
    sig: bool,
}

#[derive(clap::Args)]
struct RefsArgs {
    name: String,
    #[arg(long, default_value_t = defaults::REFS_LIMIT)]
    limit: usize,
}

#[derive(clap::Args)]
struct ContextArgs {
    /// Symbol (Name or Parent.Name)
    symbol: String,
    /// Token budget for the calls/called-by sections (the source is always printed in full)
    #[arg(long, default_value_t = defaults::CONTEXT_BUDGET)]
    budget: i64,
}

#[derive(clap::Args)]
struct DiffArgs {
    /// Git ref to compare against
    #[arg(default_value = "HEAD")]
    r#ref: String,
}

#[derive(clap::Args)]
struct GrepArgs {
    pattern: String,
    /// Case-insensitive match
    #[arg(short = 'i', long)]
    ignore_case: bool,
    #[arg(long, default_value_t = defaults::GREP_LIMIT)]
    limit: usize,
}

#[derive(clap::Args)]
struct EditArgs {
    /// Symbol (Name or Parent.Name) — or, with --range, a file path
    symbol: String,
    /// File with replacement code (default: read from stdin)
    #[arg(long)]
    file: Option<String>,
    /// Replace absolute lines S-E of SYMBOL (treated as a file path) instead of a whole symbol body
    #[arg(long, value_name = "S-E")]
    range: Option<String>,
    /// Skip syntax verification
    #[arg(long)]
    force: bool,
}

#[derive(clap::Args)]
struct InsertArgs {
    /// Symbol to anchor the insertion at (omit when using --at)
    symbol: Option<String>,
    /// Insert AFTER the symbol (default: before)
    #[arg(long)]
    after: bool,
    /// Absolute position: FILE and LINE (0 = prepend, past EOF = append); works on files with no indexed symbol
    #[arg(long, num_args = 2, value_names = ["FILE", "LINE"])]
    at: Option<Vec<String>>,
    /// File with the code to insert (default: read from stdin)
    #[arg(long)]
    file: Option<String>,
    /// Skip syntax verification
    #[arg(long)]
    force: bool,
}

#[derive(clap::Args)]
struct CheckArgs {
    /// File to check (default: every file changed vs HEAD, incl. untracked)
    file: Option<String>,
}

#[derive(clap::Args)]
struct ImpactArgs {
    symbol: String,
}

#[derive(clap::Args)]
struct EntriesArgs {
    /// Only files under this path prefix
    #[arg(long)]
    path: Option<String>,
    /// Max rows per section
    #[arg(long, default_value_t = 40)]
    limit: usize,
}

#[derive(clap::Args)]
struct TestsArgs {
    symbol: String,
}

#[derive(clap::Args)]
struct BlameArgs {
    /// Symbol (Name or Parent.Name)
    symbol: String,
    #[arg(long, default_value_t = 10)]
    limit: usize,
}

#[derive(clap::Args)]
struct HotArgs {
    /// Git --since window
    #[arg(long, default_value = "6 months ago")]
    since: String,
    #[arg(long, default_value_t = 20)]
    limit: usize,
}

#[derive(clap::Args)]
struct CouplingArgs {
    file: String,
    #[arg(long, default_value = "1 year ago")]
    since: String,
    #[arg(long, default_value_t = 15)]
    limit: usize,
}

#[derive(clap::Args)]
struct CallsArgs {
    symbol: String,
    #[arg(long, default_value_t = defaults::CALLS_DEPTH)]
    depth: usize,
}

#[derive(clap::Args)]
struct PathArgs {
    from: String,
    to: String,
    #[arg(long, default_value_t = 8)]
    max_depth: usize,
}

#[derive(clap::Args)]
struct NoteArgs {
    /// Symbol or file the note attaches to
    symbol: Option<String>,
    /// Note text (omit to list notes on the symbol)
    text: Vec<String>,
    /// Remove a note by id
    #[arg(long)]
    rm: Option<i64>,
}

#[derive(clap::Args)]
struct ShapeArgs {
    symbol: String,
    #[arg(long, default_value_t = defaults::SHAPE_BUDGET)]
    budget: i64,
    /// Narrow to a kind (struct, class, …)
    #[arg(long)]
    kind: Option<String>,
}

#[derive(clap::Args)]
struct DepsArgs {
    /// Only edges from files under this path prefix
    path: Option<String>,
}

#[derive(clap::Args)]
struct RenameArgs {
    /// Symbol (Name or Parent.Name) — must be unambiguous
    symbol: String,
    new_name: String,
    /// Proceed despite name collisions or textual-fallback files
    #[arg(long)]
    force: bool,
}

#[derive(clap::Args)]
struct StatsArgs {
    /// Only current project
    #[arg(long)]
    project: bool,
}

#[derive(clap::Args)]
struct TidyArgs {
    /// Also remove indexes for projects whose directory no longer exists
    #[arg(long)]
    orphans: bool,
}

#[derive(clap::Args)]
struct ForgetArgs {
    /// Project path to forget (default: current project root)
    path: Option<String>,
}

#[derive(clap::Args)]
struct ResetArgs {
    /// Keep usage statistics (only rebuild the index)
    #[arg(long)]
    keep_stats: bool,
}

#[derive(clap::Args)]
struct HookArgs {
    /// Hook event name (e.g. PreToolUse)
    event: String,
}

#[derive(clap::Args)]
struct HooksArgs {
    #[arg(value_enum)]
    action: IntegrationAction,
}

#[derive(clap::Args)]
#[command(long_about = "\
One-shot setup. Indexes the project, installs git hooks (auto-reindex on commit),\n\
and wires cona into your agent configs.\n\n\
With no scope argument on a terminal, prompts for project / global / both, then\n\
shows a checklist to pick which agents to configure (project pre-checks the ones\n\
detected on disk; global pre-checks none). Piped/CI runs skip the prompts and\n\
autodetect installed agents.\n\n\
EXAMPLES\n  \
cona setup                Interactive — choose scope + agents\n  \
cona setup project        This project only (autodetect agents when non-interactive)\n  \
cona setup global         Home configs only (~/.claude, ~/.codex, …)\n  \
cona setup all            Both, no scope prompt")]
struct SetupArgs {
    /// What to set up (omit for interactive chooser)
    #[arg(value_enum)]
    target: Option<SetupScope>,
}

#[derive(clap::Args)]
struct InstallArgs {
    /// Target directory (default: ~/.local/bin)
    #[arg(long)]
    bin_dir: Option<String>,
}

#[derive(clap::Args)]
struct UpgradeArgs {
    #[arg(short, long)]
    quiet: bool,
}

#[derive(clap::Args)]
struct UninstallArgs {
    /// Also delete ~/.cona (all indexes + stats)
    #[arg(long)]
    purge: bool,
}

#[derive(clap::Args)]
#[command(long_about = "\
Inject or remove the cona usage guide in agent configs. Idempotent and\n\
marker-based (`<!-- cona:begin/end -->`), so it never clobbers your content.\n\n\
Name one or more agents to target just those; pass none to autodetect the ones\n\
installed on this machine; `--all` targets every known agent regardless. Claude\n\
Code and (project) AGENTS.md are always configured on a bare install.\n\n\
EXAMPLES\n  \
cona agents install               Autodetect installed agents\n  \
cona agents install cursor gemini Just these two\n  \
cona agents install --all         Every known agent\n  \
cona agents install --global      Home configs (~/.claude, ~/.codex, …)\n  \
cona agents uninstall             Remove cona from all agent configs")]
struct AgentsArgs {
    #[arg(value_enum)]
    action: IntegrationAction,
    /// Agents to target. None = autodetect installed
    #[arg(value_enum, value_name = "AGENT", conflicts_with = "all")]
    names: Vec<install::AgentName>,
    /// Target every known agent, whether or not its config is detected
    #[arg(long)]
    all: bool,
    /// Target home configs (~/.claude, ~/.codex, …) instead of the project
    #[arg(long)]
    global: bool,
}

// ── Grouped, canonical subcommands ─────────────────────────────────────────
// These render in `--help` as six themed groups. Each variant carries a shared
// *Args struct. The flat top-level names (`cona show …`) remain as hidden
// aliases in `Cmd` for backward compatibility and shorter agent calls.

/// Navigate the code: locate and read symbols.
#[derive(Subcommand)]
enum Nav {
    /// Compact tree of files and their top-level symbols
    Tree(TreeArgs),
    /// All symbols of one file, with line ranges
    Outline(OutlineArgs),
    /// Find symbols by name (exact match first, then substring)
    Find(FindArgs),
    /// Print the source code of one or more symbols (Name or Parent.Name)
    Show(ShowArgs),
    /// Find references (identifier occurrences) across indexed files
    Refs(RefsArgs),
    /// Search indexed code files for a substring; hits show their enclosing symbol
    Grep(GrepArgs),
}

/// Understand the code: relationships, impact, dependencies, tests.
#[derive(Subcommand)]
enum Inspect {
    /// One context pack for a symbol: its source + callee signatures + call sites
    Context(ContextArgs),
    /// Changed symbols (not lines) vs a git ref — includes uncommitted + untracked
    Diff(DiffArgs),
    /// Blast radius before an edit: references + immediate callers + tests + recent history, in one pack
    Impact(ImpactArgs),
    /// A symbol's type shape: its source + the types it references, expanded one level
    Shape(ShapeArgs),
    /// File-level import graph over indexed files (+ cycle warnings, most-imported)
    Deps(DepsArgs),
    /// Entry points: mains, exported/public API, tests — where execution starts
    Entries(EntriesArgs),
    /// Which tests exercise a symbol (refs filtered to test code)
    Tests(TestsArgs),
    /// Transitive callers: who reaches this symbol (name-based, ambiguity marked)
    Callers(CallsArgs),
    /// Transitive callees: what this symbol reaches (name-based, ambiguity marked)
    Callees(CallsArgs),
    /// Shortest call chain between two symbols
    Path(PathArgs),
}

/// Change the code: edit, insert, rename, annotate, verify.
#[derive(Subcommand)]
enum EditCmd {
    /// Replace the body of a symbol; verifies syntax, rolls back on error
    Edit(EditArgs),
    /// Insert new code without touching a body — next to a symbol (--after) or --at <file> <line>
    Insert(InsertArgs),
    /// Rename a symbol project-wide: identifier-exact, syntax-verified, all-or-nothing
    Rename(RenameArgs),
    /// Attach a persistent note to a symbol (no text: list; --rm <id>: delete)
    Note(NoteArgs),
    /// Syntax-check a file (tree-sitter parse only, NOT a compiler); no FILE = all changed vs HEAD
    Check(CheckArgs),
}

/// Mine git history: who/when/why, churn, coupling.
#[derive(Subcommand)]
enum History {
    /// Symbol-level git history: who changed these lines, when, why
    Blame(BlameArgs),
    /// Churn hotspots: indexed files ranked by commit frequency
    Hot(HotArgs),
    /// Files that historically change together with FILE (hidden coupling)
    Coupling(CouplingArgs),
}

/// Manage the index + this project's data.
#[derive(Subcommand)]
enum Project {
    /// Index (or incrementally update) the current project
    Index(IndexArgs),
    /// Usage statistics (per-project + global, savings, top targets, recent)
    Stats(StatsArgs),
    /// List all registered projects
    Projects,
    /// Reset the current project: wipe index, notes + stats, then reindex fresh
    Reset(ResetArgs),
    /// Delete a project's index + stats (default: current project)
    Forget(ForgetArgs),
    /// Tidy the databases: prune old usage, drop orphaned indexes, reclaim space
    #[command(alias = "gc")]
    Tidy(TidyArgs),
    /// Live TUI dashboard: index state + token savings in real time
    Ui,
}

/// Install, upgrade, and wire cona into agents/hooks.
#[derive(Subcommand)]
enum Maint {
    /// Diagnose install + agent integration (binary, hooks, skill, index, storage)
    Doctor,
    /// One-shot setup: index + git hooks + agent integration (no arg → interactive chooser)
    Setup(SetupArgs),
    /// Install the binary from this source checkout (+ upgrade git hooks)
    Install(InstallArgs),
    /// Rebuild when the source checkout changed, else update to the newest release
    Upgrade(UpgradeArgs),
    /// Remove binary, upgrade hooks and global agent files
    Uninstall(UninstallArgs),
    /// Inject/remove cona into agent configs (Claude Code, AGENTS.md, Cursor, Gemini)
    Agents(AgentsArgs),
    /// Install/uninstall git hooks for automatic re-indexing
    Hooks(HooksArgs),
    /// Print the agent skill (usage guide for AI agents)
    Skill,
    /// MCP server over stdio: exposes the query + edit commands as tools
    Mcp,
    /// Agent tool-call hook (reads JSON on stdin) — wired into settings.json
    Hook(HookArgs),
}

#[derive(Subcommand)]
enum Cmd {
    // ── Canonical, grouped commands (shown in --help) ──
    /// Navigate: locate & read symbols (tree, outline, find, show, refs, grep)
    #[command(subcommand)]
    Nav(Nav),
    /// Inspect: relationships, impact, deps, tests (context, diff, callers, …)
    #[command(subcommand)]
    Inspect(Inspect),
    /// Code: change code (edit, insert, rename, note, check)
    #[command(subcommand, name = "code")]
    Edit(EditCmd),
    /// History: git mining (blame, hot, coupling)
    #[command(subcommand)]
    History(History),
    /// Project: index lifecycle & stats (index, stats, reset, tidy, ui, …)
    #[command(subcommand)]
    Project(Project),
    /// Maint: install & agent integration (doctor, setup, upgrade, agents, mcp, …)
    #[command(subcommand)]
    Maint(Maint),

    // ── Flat aliases (hidden): backward-compatible short forms ──
    #[command(hide = true)]
    Tree(TreeArgs),
    #[command(hide = true)]
    Outline(OutlineArgs),
    #[command(hide = true)]
    Find(FindArgs),
    #[command(hide = true)]
    Show(ShowArgs),
    #[command(hide = true)]
    Refs(RefsArgs),
    #[command(hide = true)]
    Grep(GrepArgs),
    #[command(name = "context", hide = true)]
    ContextFlat(ContextArgs),
    #[command(name = "diff", hide = true)]
    DiffFlat(DiffArgs),
    #[command(name = "impact", hide = true)]
    ImpactFlat(ImpactArgs),
    #[command(name = "shape", hide = true)]
    ShapeFlat(ShapeArgs),
    #[command(name = "deps", hide = true)]
    DepsFlat(DepsArgs),
    #[command(name = "entries", hide = true)]
    EntriesFlat(EntriesArgs),
    #[command(name = "tests", hide = true)]
    TestsFlat(TestsArgs),
    #[command(name = "callers", hide = true)]
    CallersFlat(CallsArgs),
    #[command(name = "callees", hide = true)]
    CalleesFlat(CallsArgs),
    #[command(name = "path", hide = true)]
    PathFlat(PathArgs),
    #[command(name = "edit", hide = true)]
    EditFlat(EditArgs),
    #[command(name = "insert", hide = true)]
    InsertFlat(InsertArgs),
    #[command(name = "rename", hide = true)]
    RenameFlat(RenameArgs),
    #[command(name = "note", hide = true)]
    NoteFlat(NoteArgs),
    #[command(name = "check", hide = true)]
    CheckFlat(CheckArgs),
    #[command(name = "blame", hide = true)]
    BlameFlat(BlameArgs),
    #[command(name = "hot", hide = true)]
    HotFlat(HotArgs),
    #[command(name = "coupling", hide = true)]
    CouplingFlat(CouplingArgs),
    #[command(name = "index", hide = true)]
    IndexFlat(IndexArgs),
    #[command(name = "stats", hide = true)]
    StatsFlat(StatsArgs),
    #[command(name = "projects", hide = true)]
    ProjectsFlat,
    #[command(name = "reset", hide = true)]
    ResetFlat(ResetArgs),
    #[command(name = "forget", hide = true)]
    ForgetFlat(ForgetArgs),
    #[command(name = "tidy", alias = "gc", hide = true)]
    TidyFlat(TidyArgs),
    #[command(name = "ui", hide = true)]
    UiFlat,
    #[command(name = "doctor", hide = true)]
    DoctorFlat,
    #[command(name = "setup", hide = true)]
    SetupFlat(SetupArgs),
    #[command(name = "install", hide = true)]
    InstallFlat(InstallArgs),
    #[command(name = "upgrade", hide = true)]
    UpgradeFlat(UpgradeArgs),
    #[command(name = "uninstall", hide = true)]
    UninstallFlat(UninstallArgs),
    #[command(name = "agents", hide = true)]
    AgentsFlat(AgentsArgs),
    #[command(name = "hooks", hide = true)]
    HooksFlat(HooksArgs),
    #[command(name = "skill", hide = true)]
    SkillFlat,
    #[command(name = "mcp", hide = true)]
    McpFlat,
    #[command(name = "hook", hide = true)]
    HookFlat(HookArgs),
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let root = db::project_root()?;
    let t0 = Instant::now();

    // Auto-update/tidy runs before every command except the install-lifecycle
    // ones (they manage the binary themselves) and the hook (latency-sensitive).
    if !matches!(
        &cli.cmd,
        Cmd::Maint(Maint::Install(_) | Maint::Upgrade(_) | Maint::Uninstall(_) | Maint::Hook(_))
            | Cmd::InstallFlat(_)
            | Cmd::UpgradeFlat(_)
            | Cmd::UninstallFlat(_)
            | Cmd::HookFlat(_)
    ) {
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
            if db::is_home_or_fs_root(&root) && !quiet {
                eprintln!(
                    "warning: indexing {} (home/filesystem root) walks a huge tree — \
                     consider running inside a project (or `git init`) instead",
                    root.display()
                );
            }
            let conn = db::open_project_db(&root)?;
            let r = indexer::index_project(&root, &conn)?;
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
            db::log_usage(&root, "index", ms, r.total_symbols, 0, 0);
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
            let conn = open_indexed(&root)?;
            let (out, baseline) = if *rank {
                cmd_tree_rank(&root, &conn, *budget, path.as_deref(), cli.json)?
            } else {
                cmd_tree(&conn, *budget, path.as_deref(), cli.json)?
            };
            print!("{out}");
            finish(
                &root,
                "tree",
                t0,
                &out,
                baseline,
                path.as_deref().unwrap_or(""),
            );
        }
        Cmd::Nav(Nav::Outline(a)) | Cmd::Outline(a) => {
            let OutlineArgs { file, sig } = a;
            let conn = open_indexed(&root)?;
            let (out, baseline) = cmd_outline(&conn, file, *sig, cli.json)?;
            print!("{out}");
            finish(&root, "outline", t0, &out, baseline, file);
        }
        Cmd::Nav(Nav::Find(a)) | Cmd::Find(a) => {
            let FindArgs { name, kind, limit } = a;
            let conn = open_indexed(&root)?;
            let (out, baseline) = cmd_find(&conn, name, kind.as_deref(), *limit, cli.json)?;
            print!("{out}");
            finish(&root, "find", t0, &out, baseline, name);
        }
        Cmd::Nav(Nav::Show(a)) | Cmd::Show(a) => {
            let ShowArgs {
                symbols,
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
                    *context,
                    kind.as_deref(),
                    *sig,
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
            let RefsArgs { name, limit } = a;
            let conn = open_indexed(&root)?;
            let (out, baseline) = cmd_refs(&root, &conn, name, *limit, cli.json)?;
            print!("{out}");
            finish(&root, "refs", t0, &out, baseline, name);
        }
        Cmd::Inspect(Inspect::Context(a)) | Cmd::ContextFlat(a) => {
            let ContextArgs { symbol, budget } = a;
            let conn = open_indexed(&root)?;
            let (out, baseline) = cmd_context(&root, &conn, symbol, *budget, cli.json)?;
            print!("{out}");
            finish(&root, "context", t0, &out, baseline, symbol);
        }
        Cmd::Inspect(Inspect::Diff(a)) | Cmd::DiffFlat(a) => {
            let DiffArgs { r#ref } = a;
            let conn = open_indexed(&root)?;
            let (out, baseline) = cmd_diff(&root, &conn, r#ref, cli.json)?;
            print!("{out}");
            finish(&root, "diff", t0, &out, baseline, r#ref);
        }
        Cmd::Nav(Nav::Grep(a)) | Cmd::Grep(a) => {
            let GrepArgs {
                pattern,
                ignore_case,
                limit,
            } = a;
            let conn = open_indexed(&root)?;
            let (out, baseline) = cmd_grep(&root, &conn, pattern, *ignore_case, *limit, cli.json)?;
            print!("{out}");
            finish(&root, "grep", t0, &out, baseline, pattern);
        }
        Cmd::Edit(EditCmd::Edit(a)) | Cmd::EditFlat(a) => {
            let EditArgs {
                symbol,
                file,
                range,
                force,
            } = a;
            let conn = open_indexed(&root)?;
            let out = match range {
                Some(r) => {
                    let (s, e) = parse_range(r)?;
                    let code = read_replacement(file.as_deref())?;
                    // with --range, `symbol` is the file path
                    cmd_edit_range(&root, &conn, symbol, s, e, &code, *force)?
                }
                None => cmd_edit(&root, &conn, symbol, file.as_deref(), *force)?,
            };
            print!("{out}");
            finish(&root, "edit", t0, &out, 0, symbol);
        }
        Cmd::Edit(EditCmd::Insert(a)) | Cmd::InsertFlat(a) => {
            let InsertArgs {
                symbol,
                after,
                at,
                file,
                force,
            } = a;
            let conn = open_indexed(&root)?;
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
            let out = cmd_insert(
                &root,
                &conn,
                symbol.as_deref(),
                *after,
                at_pos,
                &code,
                *force,
            )?;
            print!("{out}");
            finish(
                &root,
                "edit",
                t0,
                &out,
                0,
                symbol.as_deref().unwrap_or("--at"),
            );
        }
        Cmd::Edit(EditCmd::Check(a)) | Cmd::CheckFlat(a) => {
            let CheckArgs { file } = a;
            let conn = open_indexed(&root)?;
            let (out, baseline) = cmd_check(&root, &conn, file.as_deref(), cli.json)?;
            print!("{out}");
            finish(
                &root,
                "check",
                t0,
                &out,
                baseline,
                file.as_deref().unwrap_or("*"),
            );
        }
        Cmd::Inspect(Inspect::Impact(a)) | Cmd::ImpactFlat(a) => {
            let ImpactArgs { symbol } = a;
            let conn = open_indexed(&root)?;
            let (out, baseline) = cmd_impact(&root, &conn, symbol, cli.json)?;
            print!("{out}");
            finish(&root, "impact", t0, &out, baseline, symbol);
        }
        Cmd::Inspect(Inspect::Entries(a)) | Cmd::EntriesFlat(a) => {
            let EntriesArgs { path, limit } = a;
            let conn = open_indexed(&root)?;
            let (out, baseline) = cmd_entries(&conn, path.as_deref(), *limit, cli.json)?;
            print!("{out}");
            finish(
                &root,
                "entries",
                t0,
                &out,
                baseline,
                path.as_deref().unwrap_or(""),
            );
        }
        Cmd::Inspect(Inspect::Tests(a)) | Cmd::TestsFlat(a) => {
            let TestsArgs { symbol } = a;
            let conn = open_indexed(&root)?;
            let (out, baseline) = cmd_tests(&root, &conn, symbol, cli.json)?;
            print!("{out}");
            finish(&root, "tests", t0, &out, baseline, symbol);
        }
        Cmd::History(History::Blame(a)) | Cmd::BlameFlat(a) => {
            let BlameArgs { symbol, limit } = a;
            let conn = open_indexed(&root)?;
            let (out, baseline) = cmd_blame(&root, &conn, symbol, *limit, cli.json)?;
            print!("{out}");
            finish(&root, "blame", t0, &out, baseline, symbol);
        }
        Cmd::History(History::Hot(a)) | Cmd::HotFlat(a) => {
            let HotArgs { since, limit } = a;
            let conn = open_indexed(&root)?;
            let (out, baseline) = cmd_hot(&root, &conn, since, *limit, cli.json)?;
            print!("{out}");
            finish(&root, "hot", t0, &out, baseline, "");
        }
        Cmd::History(History::Coupling(a)) | Cmd::CouplingFlat(a) => {
            let CouplingArgs { file, since, limit } = a;
            let conn = open_indexed(&root)?;
            let (out, baseline) = cmd_coupling(&root, &conn, file, since, *limit, cli.json)?;
            print!("{out}");
            finish(&root, "coupling", t0, &out, baseline, file);
        }
        Cmd::Inspect(Inspect::Callers(a)) | Cmd::CallersFlat(a) => {
            let CallsArgs { symbol, depth } = a;
            let conn = open_indexed(&root)?;
            let (out, baseline) = cmd_calls(&root, &conn, symbol, *depth, true, cli.json)?;
            print!("{out}");
            finish(&root, "callers", t0, &out, baseline, symbol);
        }
        Cmd::Inspect(Inspect::Callees(a)) | Cmd::CalleesFlat(a) => {
            let CallsArgs { symbol, depth } = a;
            let conn = open_indexed(&root)?;
            let (out, baseline) = cmd_calls(&root, &conn, symbol, *depth, false, cli.json)?;
            print!("{out}");
            finish(&root, "callees", t0, &out, baseline, symbol);
        }
        Cmd::Inspect(Inspect::Path(a)) | Cmd::PathFlat(a) => {
            let PathArgs {
                from,
                to,
                max_depth,
            } = a;
            let conn = open_indexed(&root)?;
            let (out, baseline) = cmd_path(&root, &conn, from, to, *max_depth, cli.json)?;
            print!("{out}");
            finish(&root, "path", t0, &out, baseline, &format!("{from}→{to}"));
        }
        Cmd::Edit(EditCmd::Note(a)) | Cmd::NoteFlat(a) => {
            let NoteArgs { symbol, text, rm } = a;
            let conn = open_indexed(&root)?;
            let out = cmd_note(&conn, symbol.as_deref(), text, *rm)?;
            print!("{out}");
            finish(&root, "note", t0, &out, 0, symbol.as_deref().unwrap_or(""));
        }
        Cmd::Inspect(Inspect::Shape(a)) | Cmd::ShapeFlat(a) => {
            let ShapeArgs {
                symbol,
                budget,
                kind,
            } = a;
            let conn = open_indexed(&root)?;
            let (out, baseline) =
                cmd_shape(&root, &conn, symbol, *budget, kind.as_deref(), cli.json)?;
            print!("{out}");
            finish(&root, "shape", t0, &out, baseline, symbol);
        }
        Cmd::Inspect(Inspect::Deps(a)) | Cmd::DepsFlat(a) => {
            let DepsArgs { path } = a;
            let conn = open_indexed(&root)?;
            let (out, baseline) = cmd_deps(&root, &conn, path.as_deref(), cli.json)?;
            print!("{out}");
            finish(
                &root,
                "deps",
                t0,
                &out,
                baseline,
                path.as_deref().unwrap_or(""),
            );
        }
        Cmd::Edit(EditCmd::Rename(a)) | Cmd::RenameFlat(a) => {
            let RenameArgs {
                symbol,
                new_name,
                force,
            } = a;
            let conn = open_indexed(&root)?;
            let out = cmd_rename(&root, &conn, symbol, new_name, *force)?;
            print!("{out}");
            finish(&root, "rename", t0, &out, 0, symbol);
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
            install::cmd_doctor(&root)?;
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
            if rows.is_empty() {
                println!("no projects indexed yet — run `cona index` inside a project");
            }
            for (p, f, s, _) in rows {
                println!("{p}  ({f} files, {s} symbols)");
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
            let SetupArgs { target } = a;
            cmd_setup(&root, *target)?;
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
            let UninstallArgs { purge } = a;
            install::cmd_uninstall(*purge)?;
        }
        Cmd::Maint(Maint::Agents(a)) | Cmd::AgentsFlat(a) => {
            let AgentsArgs {
                action,
                names,
                all,
                global,
            } = a;
            install::cmd_agents(&root, action.as_str(), names, *all, *global)?;
        }
    }
    Ok(())
}

/// Parse an "S-E" line range (1-based, inclusive) for `edit --range`.
fn parse_range(s: &str) -> Result<(usize, usize)> {
    let (a, b) = s
        .split_once('-')
        .ok_or_else(|| anyhow::anyhow!("--range must be S-E, e.g. 42-48"))?;
    Ok((a.trim().parse()?, b.trim().parse()?))
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
         searches code semantically. Most-referenced symbols (your orientation map):\n\n",
        report.total_files, report.total_symbols
    ));
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

fn cmd_setup(root: &Path, scope: Option<SetupScope>) -> Result<()> {
    use std::io::IsTerminal;
    println!("{}", ui::banner("cona setup"));
    let scope = match scope {
        Some(s) => s,
        None if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() => {
            const ITEMS: [(&str, &str); 3] = [
                ("all", "project + global — recommended"),
                (
                    "project",
                    "this project only: index, git hooks, agent files",
                ),
                ("global", "home configs only: ~/.claude, ~/.codex, …"),
            ];
            match ui::select("what should cona set up?", &ITEMS)? {
                Some(1) => SetupScope::Project,
                Some(2) => SetupScope::Global,
                Some(_) => SetupScope::All,
                None => {
                    println!("{}", ui::dim("setup cancelled"));
                    return Ok(());
                }
            }
        }
        None => SetupScope::All,
    };
    let do_project = scope != SetupScope::Global;
    let do_global = scope != SetupScope::Project;

    // Record where this binary lives if `install` never did (prebuilt-binary
    // users install via curl/wget, not from a source checkout) — otherwise
    // `cona upgrade` / auto-update have no target path to replace.
    if db::meta_get("install_path")?.is_none() {
        if let Ok(exe) = std::env::current_exe() {
            db::meta_set("install_path", &exe.to_string_lossy())?;
        }
    }

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

    let mut agents_configured = 0usize;
    if do_project {
        println!(
            "\n{}",
            ui::heading(&format!("project setup — {}", root.display()))
        );
        if root.join(".git").exists() {
            install::cmd_hooks(root, "install")?;
        } else {
            println!(
                "{}",
                ui::warn("no .git — skipped git hooks (run `cona hooks install` later)")
            );
        }
        match pick_agents(root, false)? {
            Some(agents) if !agents.is_empty() => {
                agents_configured += agents.len();
                install::cmd_agents(root, "install", &agents, false, false)?;
            }
            Some(_) => println!("{}", ui::dim("no agents selected")),
            None => {}
        }
    }
    if do_global {
        println!(
            "\n{}",
            ui::heading("global setup — home configs (~/.claude, ~/.codex, …)")
        );
        match pick_agents(root, true)? {
            Some(agents) if !agents.is_empty() => {
                agents_configured += agents.len();
                install::cmd_agents(root, "install", &agents, false, true)?;
            }
            Some(_) => println!("{}", ui::dim("no agents selected")),
            None => {}
        }
    }
    println!(
        "\n{}",
        ui::ok(&ui::bold(&format!(
            "setup complete — {agents_configured} agent{} configured",
            if agents_configured == 1 { "" } else { "s" }
        )))
    );
    println!(
        "{}",
        ui::dim("run `cona doctor` any time to verify the installation")
    );
    Ok(())
}

/// Choose which agents to configure for one scope. Interactively → a
/// multi-select checklist, pre-checked with the detected agents (project
/// pre-checks Claude Code + AGENTS.md + anything found; global pre-checks only
/// what is actually installed). Non-interactively → the detected set, no
/// prompt. `Ok(None)` means the user cancelled (skip this scope).
fn pick_agents(root: &Path, global: bool) -> Result<Option<Vec<install::AgentName>>> {
    use std::io::IsTerminal;
    let home = dirs::home_dir().unwrap_or_default();
    let detected = install::detected_agents(root, &home, global);

    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        return Ok(Some(detected)); // non-TTY: autodetect, no prompt
    }

    let items: Vec<(&str, &str, bool)> = install::AgentName::ALL
        .iter()
        .map(|a| (a.slug(), a.desc(), detected.contains(a)))
        .collect();
    match ui::multiselect("select agents to configure", &items)? {
        Some(idxs) => Ok(Some(
            idxs.into_iter()
                .map(|i| install::AgentName::ALL[i])
                .collect(),
        )),
        None => {
            println!("{}", ui::dim("agent setup skipped"));
            Ok(None)
        }
    }
}
