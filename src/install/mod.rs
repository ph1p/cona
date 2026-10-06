//! Installation, upgrade and agent integration.
//!
//! `install` — copy the binary into a bin dir, remember the source checkout,
//!             wire git hooks there so the binary rebuilds on code changes.
//! `upgrade` — rebuild from the source checkout when it is newer; otherwise
//!             check crates.io (≤1×/day, background) and update (git pull +
//!             rebuild, else prebuilt release binary, else cargo).
//! `agents`  — inject guide, skill, hooks and MCP entry into every agent
//!             config (`agents::AgentName`) — idempotent, marker-based.

use crate::ui;
use anyhow::{anyhow, bail, Result};
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};

pub const BLOCK_BEGIN: &str = "<!-- cona:begin -->";
pub const BLOCK_END: &str = "<!-- cona:end -->";

/// The semantic-resolve helper binary name. One source of truth for release
/// packaging, install/upgrade and the resolve module's discovery + auto-fetch.
pub const HELPER_EXE: &str = if cfg!(windows) {
    "cona-resolve-helper.exe"
} else {
    "cona-resolve-helper"
};

/// Release-artifact target triple for this platform, `None` without a prebuilt.
/// Single source for self-upgrade and the resolve helper auto-fetch.
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

/// GitHub repo of the release artifacts. Single source for self-upgrade and
/// the resolve helper auto-fetch — never hardcode a second copy.
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
/// `tmp`. Shared by self-upgrade and the resolve helper auto-fetch; callers
/// pick files out of `tmp` and clean it up.
pub fn fetch_release_archive(ver: &str, target: &str, tmp: &Path) -> Result<()> {
    let ext = release_ext();
    let url = format!(
        "https://github.com/{GITHUB_REPO}/releases/download/v{ver}/cona-v{ver}-{target}.{ext}"
    );
    let checksum_url = format!("{url}.sha256");
    std::fs::create_dir_all(tmp)?;
    let archive = tmp.join(format!("cona.{ext}"));
    let checksum = tmp.join("cona.sha256");
    let result = (|| {
        download_to(&url, &archive)?;
        // Verification is mandatory: this `cona` replaces the user's
        // executable. Every release publishes the sidecar (release.yml), and
        // the caller falls back to `install_via_cargo` on error. Fetch quietly
        // so a missing sidecar doesn't print a curl 404 first.
        download_quiet(&checksum_url, &checksum)
            .map_err(|_| anyhow!("no release checksum for v{ver} ({target}) — refusing to install an unverified binary"))?;
        verify_sha256(&archive, &checksum)?;
        verify_attestation(&archive)?;
        validate_archive_paths(&archive)?;
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
    })();
    if result.is_err() {
        // `tmp` is a caller-created, version-specific staging directory.
        let _ = std::fs::remove_dir_all(tmp);
    }
    result
}

fn download_to(url: &str, destination: &Path) -> Result<()> {
    let ok = std::process::Command::new("curl")
        .args(["-fsSL", "--max-time", "120", "-A", USER_AGENT, "-o"])
        .arg(destination)
        .arg(url)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        Ok(())
    } else {
        bail!("download failed: {url}");
    }
}

/// Like `download_to` but silent on HTTP/transport errors (`-fsS` → `-fs`), for
/// optional assets whose absence is expected and must not print a scary line.
fn download_quiet(url: &str, destination: &Path) -> Result<()> {
    let ok = std::process::Command::new("curl")
        .args(["-fsL", "--max-time", "120", "-A", USER_AGENT, "-o"])
        .arg(destination)
        .arg(url)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        Ok(())
    } else {
        bail!("download failed: {url}");
    }
}

/// Lowercase hex digest (sha2 0.11 dropped `LowerHex` on its output array).
fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn verify_sha256(archive: &Path, checksum_file: &Path) -> Result<()> {
    let text = std::fs::read_to_string(checksum_file)?;
    let expected = text
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("checksum metadata is empty"))?;
    if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("checksum metadata is malformed");
    }
    let bytes = std::fs::read(archive)?;
    let actual = sha256_hex(&bytes);
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("release archive checksum mismatch");
    }
    Ok(())
}

/// Opt-in SLSA provenance check, same knob as install.sh:
/// `CONA_VERIFY_ATTESTATION=1` verifies via `gh` that this repo's release
/// workflow built the archive. Off by default (needs an authenticated `gh`),
/// but once asked for, failure is fatal — a silent downgrade would defeat it.
fn verify_attestation(archive: &Path) -> Result<()> {
    if std::env::var("CONA_VERIFY_ATTESTATION").as_deref() != Ok("1") {
        return Ok(());
    }
    let status = std::process::Command::new("gh")
        .args(["attestation", "verify"])
        .arg(archive)
        .args(["--repo", GITHUB_REPO])
        .status()
        .map_err(|e| anyhow!("CONA_VERIFY_ATTESTATION=1 needs the gh CLI ({e})"))?;
    if !status.success() {
        bail!("attestation verification failed for {}", archive.display());
    }
    Ok(())
}

fn validate_archive_paths(archive: &Path) -> Result<()> {
    let output = std::process::Command::new("tar")
        .arg("-tf")
        .arg(archive)
        .output()?;
    if !output.status.success() {
        bail!("could not inspect archive: {}", archive.display());
    }
    for raw in String::from_utf8_lossy(&output.stdout).lines() {
        let normalized = raw.replace('\\', "/");
        let path = Path::new(&normalized);
        if path.is_absolute()
            || path.components().any(|c| {
                matches!(
                    c,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            bail!("archive contains unsafe path: {raw}");
        }
    }
    Ok(())
}

/// The Claude Code skill — single source: the repo-root SKILL.md, embedded
/// at build time so the three published copies can never drift.
pub const SKILL_MD: &str = include_str!("../../SKILL.md");

pub mod agents;
pub mod doctor;
pub mod mcp_config;
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

/// Single-quote a value for a shell command line (git hooks, settings.json
/// hook commands): a path with spaces or quotes would otherwise break a line
/// that fires on every commit / tool call.
pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Write `content` to `path` only if it differs; creates parent dirs.
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

/// One recorded status line, kept as DATA, not pre-rendered text: callers
/// ask questions ("did anything move?", "group by label") by reading fields,
/// never by parsing colored strings. `render` runs only at print time, so a
/// quiet run pays nothing for display.
pub(crate) struct Mark {
    pub label: &'static str,
    pub verb: &'static str,
    /// Why ("plugin has it") — rendered after the path; such a row always
    /// gets its own line.
    pub why: Option<&'static str>,
    pub path: PathBuf,
}

/// Width of `render`'s label column. A longer label breaks the row alignment,
/// so it constrains target names — `agents::label_widths_fit_the_column` pins it.
pub(crate) const LABEL_COL: usize = 14;

impl Mark {
    /// Does this denote a real change, vs an already-current no-op?
    pub fn changed(&self) -> bool {
        matches!(self.verb, "created" | "updated" | "installed" | "removed")
    }

    /// `label   verb   path` — the one status-line format.
    /// Pad BEFORE coloring — ANSI escapes would break the column width.
    pub fn render(&self) -> String {
        let padded = format!("{:<9}", self.verb);
        let verb_col = match self.verb {
            "created" | "updated" | "installed" => ui::green(&padded),
            "removed" => ui::yellow(&padded),
            _ => ui::dim(&padded),
        };
        // The reason trails the path so a long one keeps the columns aligned.
        let tail = self
            .why
            .map(|r| format!("  {}", ui::dim(&format!("· {r}"))))
            .unwrap_or_default();
        format!(
            "{:<LABEL_COL$} {verb_col} {}{tail}",
            self.label,
            short_path(&self.path)
        )
    }
}

/// Shorten `path` for display: under the cwd → `./…`, under `$HOME` → `~/…`,
/// else unchanged — long temp/checkout prefixes push the useful tail off screen.
///
/// Symlink-tolerant, three attempts, cheapest first:
/// 1. as spelled — the common case (path built by joining onto the anchor),
///    zero syscalls;
/// 2. both sides resolved — macOS reports cwd as `/private/tmp/x` while a
///    path built from args is `/tmp/x` (or vice versa);
/// 3. as spelled under the resolved anchor — the anchor is a symlink and its
///    subtree holds a *further* symlink, so (2) leaves the subtree.
///
/// A missing path (just removed) fails `canonicalize`, collapsing (2) and (3).
/// Falls back to the absolute path — never to a wrong relative one.
pub(crate) fn short_path(path: &Path) -> String {
    let rel_to = |base: &Path| -> Option<String> {
        let strip = |p: &Path, b: &Path| {
            p.strip_prefix(b)
                .ok()
                .filter(|rel| !rel.as_os_str().is_empty())
                .map(|rel| rel.display().to_string())
        };
        if let Some(rel) = strip(path, base) {
            return Some(rel);
        }
        let real = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let base_real = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
        strip(&real, &base_real).or_else(|| strip(path, &base_real))
    };
    // The base itself (a project heading for the cwd, `~` for home) shortens
    // too — `rel_to` alone refuses an empty remainder.
    let is = |base: &Path| {
        path == base
            || matches!((path.canonicalize(), base.canonicalize()), (Ok(a), Ok(b)) if a == b)
    };
    if let Ok(cwd) = std::env::current_dir() {
        if is(&cwd) {
            return ".".into();
        }
        if let Some(rel) = rel_to(&cwd) {
            return format!("./{rel}");
        }
    }
    if let Some(home) = dirs::home_dir() {
        if is(&home) {
            return "~".into();
        }
        if let Some(rel) = rel_to(&home) {
            return format!("~/{rel}");
        }
    }
    path.display().to_string()
}

/// Record what happened to one target. Pure; display is `Mark::render`'s job.
pub(crate) fn mark(done: &mut Vec<Mark>, label: &'static str, verb: &'static str, path: &Path) {
    mark_why(done, label, verb, None, path);
}

/// `mark` with a reason the row renders after its path.
pub(crate) fn mark_why(
    done: &mut Vec<Mark>,
    label: &'static str,
    verb: &'static str,
    why: Option<&'static str>,
    path: &Path,
) {
    done.push(Mark {
        label,
        verb,
        why,
        path: path.to_path_buf(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_path_shortens_home_and_leaves_foreign_paths_absolute() {
        let home = dirs::home_dir().expect("home");
        assert_eq!(
            short_path(&home.join("some/where.md")),
            "~/some/where.md",
            "a path under $HOME should render as ~/…"
        );
        // The anchors themselves render bare (`~`, `.`), never as "~/" / "./".
        assert_eq!(short_path(&home), "~");
        assert_eq!(short_path(&std::env::current_dir().unwrap()), ".");
        // A path under neither anchor stays fully qualified.
        let foreign = Path::new("/definitely/not/here/x.md");
        assert_eq!(short_path(foreign), "/definitely/not/here/x.md");
    }

    #[test]
    fn short_path_prefers_cwd_over_home() {
        // cwd is the more specific anchor: ./… wins even under $HOME.
        let cwd = std::env::current_dir().expect("cwd");
        let got = short_path(&cwd.join("Cargo.toml"));
        assert_eq!(got, "./Cargo.toml", "cwd-relative should win, got {got}");
    }

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

    #[test]
    fn checksum_verification_rejects_tampering() {
        let dir = std::env::temp_dir().join(format!("cona-checksum-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("archive");
        let checksum = dir.join("archive.sha256");
        std::fs::write(&archive, b"release bytes").unwrap();
        let digest = sha256_hex(b"release bytes");
        std::fs::write(&checksum, format!("{digest}  archive\n")).unwrap();
        assert!(verify_sha256(&archive, &checksum).is_ok());
        std::fs::write(&archive, b"tampered").unwrap();
        assert!(verify_sha256(&archive, &checksum).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
