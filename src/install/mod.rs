//! Installation, upgrade and agent integration.
//!
//! `install` — copy the binary into a bin dir, remember the source
//!             checkout, wire git hooks in the source repo so the
//!             installed binary rebuilds itself on code changes.
//! `upgrade` — rebuild from the recorded source checkout when the
//!             sources are newer than the installed binary; otherwise
//!             check crates.io (≤1×/day in the background) and update
//!             to the newest release (git pull + rebuild for source
//!             installs, prebuilt release binary else, cargo fallback).
//! `agents`  — inject the usage guide into agent configs (Claude Code
//!             skills + hooks + CLAUDE.md, AGENTS.md, Cursor, Gemini) —
//!             idempotent, marker-based, uninstallable.

use crate::ui;
use anyhow::Result;
use std::path::Path;

pub const BLOCK_BEGIN: &str = "<!-- cona:begin -->";
pub const BLOCK_END: &str = "<!-- cona:end -->";

/// The semantic-resolve helper binary name (`.exe`-suffixed on Windows). One
/// source of truth for the release packaging, install/upgrade, and the resolve
/// module's discovery + auto-fetch.
pub const HELPER_EXE: &str = if cfg!(windows) {
    "cona-resolve-helper.exe"
} else {
    "cona-resolve-helper"
};

/// Release-artifact target triple for this platform, or `None` if there is no
/// prebuilt for it. The single source used by both the self-upgrade download
/// and the resolve helper auto-fetch — keep them from drifting.
pub fn release_target() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        _ => return None,
    })
}

/// GitHub repo the release artifacts live under. The single source for both
/// the self-upgrade download and the resolve helper auto-fetch — a rename
/// here must not leave a second hardcoded copy behind.
pub const GITHUB_REPO: &str = "ph1p/cona";
pub const USER_AGENT: &str = concat!(
    "cona/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/ph1p/cona)"
);

/// Release archive extension for this platform.
pub fn release_ext() -> &'static str {
    if cfg!(windows) {
        "zip"
    } else {
        "tar.gz"
    }
}

/// Download the `v{ver}` release archive for `target` and extract it into
/// `tmp` (created if missing). The one download/extract path shared by
/// self-upgrade and the resolve helper auto-fetch; callers pick files out of
/// `tmp` and clean it up themselves.
pub fn fetch_release_archive(ver: &str, target: &str, tmp: &Path) -> Result<()> {
    use anyhow::bail;
    let ext = release_ext();
    let url = format!(
        "https://github.com/{GITHUB_REPO}/releases/download/v{ver}/cona-v{ver}-{target}.{ext}"
    );
    std::fs::create_dir_all(tmp)?;
    let archive = tmp.join(format!("cona.{ext}"));
    let ok = std::process::Command::new("curl")
        .args(["-fsSL", "--max-time", "120", "-A", USER_AGENT, "-o"])
        .arg(&archive)
        .arg(&url)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        bail!("download failed: {url}");
    }
    // bsdtar (macOS, Windows 10+) and GNU tar both handle .tar.gz; bsdtar
    // also extracts the Windows .zip.
    let ok = std::process::Command::new("tar")
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(tmp)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        bail!("extract failed: {}", archive.display());
    }
    Ok(())
}

/// The Claude Code skill — single source: the repo-root SKILL.md, embedded
/// at build time so the three published copies can never drift.
pub const SKILL_MD: &str = include_str!("../../SKILL.md");

pub mod agents;
pub mod doctor;
pub mod upgrade;

pub use agents::*;
pub use doctor::*;
pub use upgrade::*;

// ---------------------------------------------------------------------------
// marker-block injection (pure, tested)
// ---------------------------------------------------------------------------

/// Insert or replace the cona marker block in `content`. Idempotent.
pub fn upsert_block(content: &str, body: &str) -> String {
    let block = format!("{BLOCK_BEGIN}\n{}\n{BLOCK_END}\n", body.trim_end());
    match (content.find(BLOCK_BEGIN), content.find(BLOCK_END)) {
        (Some(b), Some(e)) if e >= b => {
            let after = e + BLOCK_END.len();
            // swallow one trailing newline of the old block
            let rest = content[after..]
                .strip_prefix('\n')
                .unwrap_or(&content[after..]);
            format!("{}{}{}", &content[..b], block, rest)
        }
        _ => {
            if content.trim().is_empty() {
                block
            } else {
                format!("{}\n{}", content.trim_end(), block)
            }
        }
    }
}

/// Remove the cona marker block from `content`. Returns None if absent.
pub fn remove_block(content: &str) -> Option<String> {
    let b = content.find(BLOCK_BEGIN)?;
    let e = content.find(BLOCK_END)?;
    if e < b {
        return None;
    }
    let after = e + BLOCK_END.len();
    let rest = content[after..]
        .strip_prefix('\n')
        .unwrap_or(&content[after..]);
    Some(format!("{}{}", content[..b].trim_end_matches('\n'), {
        if rest.trim().is_empty() {
            String::from("\n")
        } else {
            format!("\n\n{rest}")
        }
    }))
}

pub(crate) fn upsert_block_file(path: &Path, body: &str) -> Result<Change> {
    let existed = path.exists();
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let updated = upsert_block(&existing, body);
    if updated == existing {
        return Ok(Change::Unchanged);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, updated)?;
    Ok(if existed {
        Change::Updated
    } else {
        Change::Created
    })
}

pub(crate) fn remove_block_file(path: &Path) -> Result<bool> {
    let Ok(existing) = std::fs::read_to_string(path) else {
        return Ok(false);
    };
    match remove_block(&existing) {
        Some(updated) => {
            if updated.trim().is_empty() {
                std::fs::remove_file(path)?;
            } else {
                std::fs::write(path, updated)?;
            }
            Ok(true)
        }
        None => Ok(false),
    }
}

// ---------------------------------------------------------------------------
// idempotent file writes (never rewrite identical content)
// ---------------------------------------------------------------------------

/// Outcome of an idempotent write, so callers can report precisely.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Change {
    Created,
    Updated,
    Unchanged,
}

impl Change {
    fn verb(self) -> &'static str {
        match self {
            Change::Created => "created",
            Change::Updated => "updated",
            Change::Unchanged => "unchanged",
        }
    }
}

/// Write `content` to `path` only if it differs from what's already there.
/// Creates parent directories as needed. Returns what actually happened.
pub(crate) fn write_if_changed(path: &Path, content: &str) -> Result<Change> {
    let existed = path.exists();
    if existed {
        if let Ok(old) = std::fs::read_to_string(path) {
            if old == content {
                return Ok(Change::Unchanged);
            }
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, content)?;
    Ok(if existed {
        Change::Updated
    } else {
        Change::Created
    })
}

/// One recorded status line: its rendered text plus whether it denotes a real
/// change (created/updated/installed/removed) vs an already-current no-op. The
/// flag lets callers decide "did anything move?" without re-parsing the text.
pub(crate) struct Mark {
    pub line: String,
    pub changed: bool,
}

/// Append a uniformly formatted status line: `label   verb   path`.
/// Pad BEFORE coloring — ANSI escapes would break the column width.
pub(crate) fn mark(done: &mut Vec<Mark>, label: &str, verb: &str, path: &Path) {
    let padded = format!("{verb:<9}");
    let (verb_col, changed) = match verb {
        "created" | "updated" | "installed" => (ui::green(&padded), true),
        "removed" => (ui::yellow(&padded), true),
        _ => (ui::dim(&padded), false),
    };
    done.push(Mark {
        line: format!("{label:<14} {verb_col} {}", path.display()),
        changed,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_appends_then_replaces() {
        let v1 = upsert_block("# My file\n", "guide v1");
        assert!(v1.contains("guide v1"));
        assert!(v1.starts_with("# My file"));
        let v2 = upsert_block(&v1, "guide v2");
        assert!(v2.contains("guide v2"));
        assert!(!v2.contains("guide v1"));
        assert_eq!(v2.matches(BLOCK_BEGIN).count(), 1);
        // idempotent
        assert_eq!(upsert_block(&v2, "guide v2"), v2);
    }

    #[test]
    fn remove_restores_original() {
        let orig = "# My file\n\nsome text\n";
        let with = upsert_block(orig, "guide");
        let without = remove_block(&with).unwrap();
        assert!(!without.contains(BLOCK_BEGIN));
        assert!(without.contains("# My file"));
        assert!(remove_block(orig).is_none());
    }

    #[test]
    fn upsert_into_empty() {
        let v = upsert_block("", "guide");
        assert!(v.starts_with(BLOCK_BEGIN));
        assert!(v.ends_with(&format!("{BLOCK_END}\n")));
    }
}
