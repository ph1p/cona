//! Read-only navigation commands: tree, outline, find, show, refs,
//! context, diff, grep. One file per command; `Matcher`/`grep_prefilter`
//! stay crate-visible for the shared rename prefilter.

mod context;
mod diff;
mod find;
mod grep;
mod outline;
mod refs;
mod search;
mod show;
mod tree;

pub use context::cmd_context;
pub use context::cmd_context_edit;
pub use diff::cmd_diff;
pub use diff::cmd_diff_impact;
pub use find::{cmd_find, cmd_find_fuzzy};
pub use grep::{cmd_grep, split_positionals};
pub(crate) use grep::{prefilter_indexed, Matcher};
pub use outline::cmd_outline;
pub use refs::{cmd_refs, RefsShape};
pub use search::cmd_search;
pub use show::cmd_show;
pub use tree::{cmd_tree, cmd_tree_rank};
