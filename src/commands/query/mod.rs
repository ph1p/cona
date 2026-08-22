//! Read-only navigation commands: tree, outline, find, show, refs,
//! context, diff, grep. One file per command; `Matcher`/`grep_prefilter`
//! stay crate-visible for the shared rename prefilter.

mod context;
mod diff;
mod find;
mod grep;
mod outline;
mod refs;
mod show;
mod tree;

pub use context::cmd_context;
pub use diff::cmd_diff;
pub use find::{cmd_find, cmd_find_fuzzy};
pub use grep::cmd_grep;
pub(crate) use grep::{grep_prefilter, Matcher};
pub use outline::cmd_outline;
pub use refs::cmd_refs;
pub use show::cmd_show;
pub use tree::{cmd_tree, cmd_tree_rank};
