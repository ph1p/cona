//! Clap surface: every *Args struct and command enum lives HERE, once —
//! shared by the six command groups AND the hidden flat aliases.

use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{Parser, Subcommand};
use cona::commands::defaults;
use cona::install;

/// What `setup` configures — project, home configs, or both.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SetupScope {
    Project,
    Global,
    All,
}

pub const HELP_STYLES: Styles = Styles::styled()
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
pub struct Cli {
    /// Output machine-readable JSON where supported
    #[arg(long, global = true)]
    pub json: bool,
    /// Inspect an existing index without changing code, indexes, or usage statistics
    #[arg(long, global = true)]
    pub read_only: bool,
    #[command(subcommand)]
    pub cmd: Cmd,
}

/// install|uninstall for the `hooks` and `agents` commands.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum IntegrationAction {
    Install,
    Uninstall,
}

impl IntegrationAction {
    pub fn as_str(self) -> &'static str {
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
pub struct IndexArgs {
    /// Suppress output (for hooks)
    #[arg(short, long)]
    pub quiet: bool,
    /// Keep running: watch the filesystem and reindex on change (debounced)
    #[arg(long)]
    pub watch: bool,
    /// Emit a SessionStart context block (repo orientation) as JSON on
    /// stdout after indexing. Wired to the SessionStart hook so a fresh
    /// session gets repo-specific orientation, not just a static guide.
    #[arg(long)]
    pub session_start: bool,
}

#[derive(clap::Args)]
pub struct TreeArgs {
    /// Approximate token budget for the output
    #[arg(long, default_value_t = defaults::TREE_BUDGET)]
    pub budget: i64,
    /// Only show files under this path prefix
    #[arg(long)]
    pub path: Option<String>,
    /// Rank symbols by reference fan-in (most-referenced first) instead of by path
    #[arg(long)]
    pub rank: bool,
}

#[derive(clap::Args)]
pub struct OutlineArgs {
    pub file: String,
    /// Show full signatures (default: kind + name + line range only)
    #[arg(long)]
    pub sig: bool,
}

#[derive(clap::Args)]
pub struct FindArgs {
    pub name: String,
    /// Filter by kind (fn, struct, class, method, ...)
    #[arg(long)]
    pub kind: Option<String>,
    #[arg(long, default_value_t = defaults::FIND_LIMIT)]
    pub limit: usize,
    /// Only symbols in files under this path prefix (file or directory)
    #[arg(long)]
    pub path: Option<String>,
}

#[derive(clap::Args)]
pub struct ShowArgs {
    /// One or more symbol names — each printed in turn. A file path prints
    /// that file's outline instead.
    #[arg(required = true)]
    pub symbols: Vec<String>,
    /// On an ambiguous name, print every candidate instead of erroring
    #[arg(long)]
    pub all: bool,
    /// Extra lines of context above and below
    #[arg(long, default_value_t = defaults::SHOW_CONTEXT)]
    pub context: usize,
    /// Narrow to a kind (fn, struct, impl, class, …) — resolves struct/impl same-name ambiguity
    #[arg(long)]
    pub kind: Option<String>,
    /// Print only the signature line(s), not the body — the leanest peek at a symbol
    #[arg(long)]
    pub sig: bool,
}

#[derive(clap::Args)]
pub struct RefsArgs {
    pub name: String,
    #[arg(long, default_value_t = defaults::REFS_LIMIT)]
    pub limit: usize,
    /// Only references in files under this path prefix (file or directory)
    #[arg(long)]
    pub path: Option<String>,
}

#[derive(clap::Args)]
pub struct ContextArgs {
    /// Symbol (Name or Parent.Name)
    pub symbol: String,
    /// Token budget for the calls/called-by sections (the source is always printed in full)
    #[arg(long, default_value_t = defaults::CONTEXT_BUDGET)]
    pub budget: i64,
    /// Hide test call sites, so production callers aren't crowded out
    #[arg(long)]
    pub no_tests: bool,
}

#[derive(clap::Args)]
pub struct DiffArgs {
    /// Git ref to compare against
    #[arg(default_value = "HEAD")]
    pub r#ref: String,
}

#[derive(clap::Args)]
pub struct GrepArgs {
    /// Substring to search for — literal unless --regex
    pub pattern: String,
    /// Case-insensitive match
    #[arg(short = 'i', long)]
    pub ignore_case: bool,
    /// Treat the pattern as a regular expression (Rust regex syntax)
    #[arg(short = 'e', long)]
    pub regex: bool,
    #[arg(long, default_value_t = defaults::GREP_LIMIT)]
    pub limit: usize,
    /// Only search files under this path prefix (file or directory)
    #[arg(long)]
    pub path: Option<String>,
    /// Also search dependency dirs (node_modules, vendor, target, .venv, …),
    /// which are excluded from the index and hidden by default
    #[arg(long)]
    pub include_deps: bool,
}

#[derive(clap::Args)]
pub struct EditArgs {
    /// Symbol (Name or Parent.Name) — or, with --range, a file path
    pub symbol: String,
    /// File with replacement code (default: read from stdin)
    #[arg(long)]
    pub file: Option<String>,
    /// Replace absolute lines S-E of SYMBOL (treated as a file path) instead of a whole symbol body
    #[arg(long, value_name = "S-E")]
    pub range: Option<String>,
    /// Skip syntax verification
    #[arg(long)]
    pub force: bool,
}

#[derive(clap::Args)]
pub struct InsertArgs {
    /// Symbol to anchor the insertion at (omit when using --at)
    pub symbol: Option<String>,
    /// Insert AFTER the symbol (default: before)
    #[arg(long)]
    pub after: bool,
    /// Absolute position: FILE and LINE (0 = prepend, past EOF = append); works on files with no indexed symbol
    #[arg(long, num_args = 2, value_names = ["FILE", "LINE"])]
    pub at: Option<Vec<String>>,
    /// File with the code to insert (default: read from stdin)
    #[arg(long)]
    pub file: Option<String>,
    /// Skip syntax verification
    #[arg(long)]
    pub force: bool,
}

#[derive(clap::Args)]
pub struct CheckArgs {
    /// File to check (default: every file changed vs HEAD, incl. untracked)
    pub file: Option<String>,
}

#[derive(clap::Args)]
pub struct ImpactArgs {
    pub symbol: String,
}

#[derive(clap::Args)]
pub struct EntriesArgs {
    /// Only files under this path prefix
    #[arg(long)]
    pub path: Option<String>,
    /// Max rows per section
    #[arg(long, default_value_t = defaults::ENTRIES_LIMIT)]
    pub limit: usize,
}

#[derive(clap::Args)]
pub struct TestsArgs {
    pub symbol: String,
}

#[derive(clap::Args)]
pub struct BlameArgs {
    /// Symbol (Name or Parent.Name)
    pub symbol: String,
    #[arg(long, default_value_t = defaults::BLAME_LIMIT)]
    pub limit: usize,
}

#[derive(clap::Args)]
pub struct HotArgs {
    /// Git --since window
    #[arg(long, default_value = "6 months ago")]
    pub since: String,
    #[arg(long, default_value_t = defaults::HOT_LIMIT)]
    pub limit: usize,
}

#[derive(clap::Args)]
pub struct CouplingArgs {
    pub file: String,
    #[arg(long, default_value = "1 year ago")]
    pub since: String,
    #[arg(long, default_value_t = defaults::COUPLING_LIMIT)]
    pub limit: usize,
}

#[derive(clap::Args)]
pub struct CallsArgs {
    pub symbol: String,
    #[arg(long, default_value_t = defaults::CALLS_DEPTH)]
    pub depth: usize,
}

#[derive(clap::Args)]
pub struct PathArgs {
    pub from: String,
    pub to: String,
    #[arg(long, default_value_t = defaults::PATH_DEPTH)]
    pub max_depth: usize,
}

#[derive(clap::Args)]
pub struct NoteArgs {
    /// Symbol or file the note attaches to
    pub symbol: Option<String>,
    /// Note text (omit to list notes on the symbol)
    pub text: Vec<String>,
    /// Remove a note by id
    #[arg(long)]
    pub rm: Option<i64>,
}

#[derive(clap::Args)]
pub struct ShapeArgs {
    pub symbol: String,
    #[arg(long, default_value_t = defaults::SHAPE_BUDGET)]
    pub budget: i64,
    /// Narrow to a kind (struct, class, …)
    #[arg(long)]
    pub kind: Option<String>,
}

#[derive(clap::Args)]
pub struct DepsArgs {
    /// Only edges from files under this path prefix
    #[arg(long)]
    pub path: Option<String>,
    /// Positional fallback for --path (the pre-0.1 spelling)
    #[arg(value_name = "PATH", hide = true)]
    pub path_pos: Option<String>,
}

#[derive(clap::Args)]
pub struct RenameArgs {
    /// Symbol (Name or Parent.Name) — must be unambiguous
    pub symbol: String,
    pub new_name: String,
    /// Proceed despite name collisions or textual-fallback files
    #[arg(long)]
    pub force: bool,
}

#[derive(clap::Args)]
pub struct StatsArgs {
    /// Only current project
    #[arg(long)]
    pub project: bool,
}

#[derive(clap::Args)]
pub struct TidyArgs {
    /// Also remove indexes for projects whose directory no longer exists
    #[arg(long)]
    pub orphans: bool,
}

#[derive(clap::Args)]
pub struct ForgetArgs {
    /// Project path to forget (default: current project root)
    pub path: Option<String>,
}

#[derive(clap::Args)]
pub struct ResetArgs {
    /// Keep usage statistics (only rebuild the index)
    #[arg(long)]
    pub keep_stats: bool,
}

#[derive(clap::Args)]
pub struct HookArgs {
    /// Hook event name (e.g. PreToolUse)
    pub event: String,
}

#[derive(clap::Args)]
#[command(long_about = "\
Install or remove this project's git hooks. The hooks keep the index fresh\n\
automatically: post-commit, post-merge and post-checkout reindex what changed,\n\
so a commit, pull, rebase or branch switch never leaves stale symbols behind.\n\n\
Appends to existing hooks rather than replacing them — your own hook lines are\n\
preserved, and uninstall strips only cona's.\n\n\
EXAMPLES\n  \
cona hooks install        Wire up auto-reindexing in this repo\n  \
cona hooks uninstall      Remove cona's hook lines (keeps your own)")]
pub struct HooksArgs {
    /// Whether to add cona's hook lines or strip them out again
    #[arg(value_enum)]
    pub action: IntegrationAction,
}

#[derive(clap::Args)]
#[command(long_about = "\
One-shot setup. Indexes the project, installs git hooks (auto-reindex on commit),\n\
and wires cona into your agent configs.\n\n\
On a terminal with no arguments, shows ONE checklist of every agent in both\n\
scopes (project + home), pre-checked by what's detected on disk — toggle any,\n\
press enter. Piped/CI runs, an explicit scope, or --yes skip the prompt and\n\
install every detected agent.\n\n\
EXAMPLES\n  \
cona setup                Interactive checklist (project + global agents)\n  \
cona setup -y             Non-interactive: install every detected agent\n  \
cona setup project        This project only, autodetect agents (no prompt)\n  \
cona setup global         Home configs only (~/.claude, ~/.codex, …)\n  \
cona setup all            Both scopes, no prompt")]
pub struct SetupArgs {
    /// What to set up (omit for the interactive checklist)
    #[arg(value_enum)]
    pub target: Option<SetupScope>,
    /// Non-interactive: install every detected agent without prompting
    #[arg(short, long)]
    pub yes: bool,
}

#[derive(clap::Args)]
#[command(long_about = "\
Install the binary from a source checkout. Run it from inside the cona repo:\n\
it builds a release binary if the sources are newer, copies it to ~/.local/bin\n\
(or --bin-dir), installs the optional semantic-resolve helper beside it, and\n\
wires git hooks in the checkout so every commit rebuilds what you installed.\n\n\
Installing the binary does NOT wire your agents — run `cona setup` for that.\n\
Without a Rust toolchain, use install.sh, which downloads a prebuilt release.\n\n\
EXAMPLES\n  \
cona install                      Build + install to ~/.local/bin\n  \
cona install --bin-dir ~/bin      Install somewhere else\n  \
cona setup                        Then: index + wire your agents")]
pub struct InstallArgs {
    /// Target directory (default: ~/.local/bin)
    #[arg(long)]
    pub bin_dir: Option<String>,
}

#[derive(clap::Args)]
#[command(long_about = "\
Update cona in place. From a source checkout it pulls and rebuilds; otherwise\n\
it downloads the newest GitHub release binary (falling back to cargo install).\n\
Either way it then re-syncs the guide, skill and hooks for every agent already\n\
installed — globally and in each registered project — so they never lag the\n\
binary. Nothing new is added; only what you already installed is refreshed.\n\n\
This also runs on its own in the background, at most once a day, so you rarely\n\
need to call it by hand.\n\n\
EXAMPLES\n  \
cona upgrade              Update the binary + refresh agent configs\n  \
cona upgrade --quiet      Silent (what the background check runs)")]
pub struct UpgradeArgs {
    /// Print nothing unless something actually changed
    #[arg(short, long)]
    pub quiet: bool,
}

#[derive(clap::Args)]
#[command(long_about = "\
Remove cona: agent configs (every registered project + home), git hooks, and the\n\
installed binary.\n\n\
On a terminal with no flags, shows a checklist of what to remove — untick\n\
anything you want to keep. Piped/CI runs, or --yes, remove everything without\n\
prompting. --purge additionally deletes ~/.cona (all indexes + stats).\n\n\
EXAMPLES\n  \
cona uninstall            Interactive — pick what to remove\n  \
cona uninstall -y         Non-interactive: remove agents + binary\n  \
cona uninstall -y --purge Non-interactive: also delete ~/.cona")]
pub struct UninstallArgs {
    /// Also delete ~/.cona (all indexes + stats)
    #[arg(long)]
    pub purge: bool,
    /// Non-interactive: remove everything without the checklist
    #[arg(short, long)]
    pub yes: bool,
}

/// What `cona agents` should do. `add`/`remove` are friendly aliases for
/// `install`/`uninstall`; `status` lists what's configured; omitting the action
/// on a terminal opens an interactive checklist.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum AgentAction {
    /// Configure the given (or detected) agents
    #[value(alias = "add")]
    Install,
    /// Remove cona from the given (or all) agents
    #[value(alias = "remove")]
    Uninstall,
    /// Show what's configured per agent, per scope
    Status,
}

#[derive(clap::Args)]
#[command(long_about = "\
Add, remove, or inspect cona's integration in your agent configs. Idempotent\n\
and marker-based (`<!-- cona:begin/end -->`), so it never clobbers your content.\n\n\
Run bare on a terminal for an interactive checklist (check = configured); toggle\n\
any agent on or off and confirm. Or target agents by name for a one-shot change.\n\
`add`/`remove` are aliases for `install`/`uninstall`. Claude Code and (project)\n\
AGENTS.md are always configured on a bare install.\n\n\
EXAMPLES\n  \
cona agents                       Interactive checklist — toggle any agent\n  \
cona agents status                What's configured, where, + how to change it\n  \
cona agents add cursor            Configure one agent (this project)\n  \
cona agents add gemini --global   Configure one agent (home configs)\n  \
cona agents remove cursor         Remove one agent\n  \
cona agents install               Autodetect + configure installed agents\n  \
cona agents install --all         Every known agent\n  \
cona agents uninstall             Remove cona from all agent configs")]
pub struct AgentsArgs {
    /// Action (omit for an interactive checklist on a terminal)
    #[arg(value_enum)]
    pub action: Option<AgentAction>,
    /// Agents to target. None = autodetect installed
    #[arg(value_enum, value_name = "AGENT", conflicts_with = "all")]
    pub names: Vec<install::AgentName>,
    /// Target every known agent, whether or not its config is detected
    #[arg(long)]
    pub all: bool,
    /// Target home configs (~/.claude, ~/.codex, …) instead of the project
    #[arg(long)]
    pub global: bool,
}

// ── Grouped, canonical subcommands ─────────────────────────────────────────
// These render in `--help` as six themed groups. Each variant carries a shared
// *Args struct. The flat top-level names (`cona show …`) remain as hidden
// aliases in `Cmd` for backward compatibility and shorter agent calls.

/// Navigate the code: locate and read symbols.
#[derive(Subcommand)]
pub enum Nav {
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
pub enum Inspect {
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
pub enum EditCmd {
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
pub enum History {
    /// Symbol-level git history: who changed these lines, when, why
    Blame(BlameArgs),
    /// Churn hotspots: indexed files ranked by commit frequency
    Hot(HotArgs),
    /// Files that historically change together with FILE (hidden coupling)
    Coupling(CouplingArgs),
}

/// Manage the index + this project's data.
#[derive(Subcommand)]
pub enum Project {
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

/// Long help for `doctor`. A const rather than an inline attribute because
/// `doctor` takes no arguments: commands with an `*Args` struct carry their
/// `long_about` on that struct, so the grouped and flat spellings inherit one
/// copy, but a bare variant has no such shared home and would otherwise need
/// the prose written out on both `Maint::Doctor` and `Cmd::DoctorFlat`.
pub const DOCTOR_ABOUT: &str = "\
Health check for the whole installation. Reports the binary and whether it is on\n\
your PATH, git hooks, the guide/skill in each scope, this project's index, config\n\
freshness per scope, the optional semantic-resolve helper, and every harness the\n\
MCP server is registered with.\n\n\
Read-only — it changes nothing. Anything it flags is fixed by `cona setup`\n\
(wire agents), `cona index` (build the index), or `cona upgrade` (stale config).\n\n\
EXAMPLES\n  \
cona doctor               Check this project + your global install\n  \
cona doctor --json        Machine-readable report";

/// Install, upgrade, and wire cona into agents/hooks.
#[derive(Subcommand)]
pub enum Maint {
    /// Diagnose install + agent integration (binary, hooks, skill, index, storage)
    #[command(long_about = DOCTOR_ABOUT)]
    Doctor,
    /// One-shot setup: index + git hooks + agent integration (no arg → interactive chooser)
    Setup(SetupArgs),
    /// Install the binary from this source checkout (+ upgrade git hooks)
    Install(InstallArgs),
    /// Rebuild when the source checkout changed, else update to the newest release
    Upgrade(UpgradeArgs),
    /// Remove the binary, git hooks and every agent config cona wrote
    Uninstall(UninstallArgs),
    /// Inject/remove cona into agent configs (guides, skills and MCP entries)
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
pub enum Cmd {
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
    #[command(name = "doctor", hide = true, long_about = DOCTOR_ABOUT)]
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
