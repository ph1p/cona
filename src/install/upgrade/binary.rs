//! Binary placement: build, replace-in-place, release download, cargo fallback.

use crate::install::{fetch_release_archive, release_target, Change, HELPER_EXE};
use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};

/// Byte-for-byte equality of two files (false if either can't be read).
pub(super) fn files_identical(a: &Path, b: &Path) -> bool {
    match (std::fs::read(a), std::fs::read(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

pub(super) fn default_bin_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
    Ok(home.join(".local").join("bin"))
}

/// Is `dir` a cona source checkout?
pub(super) fn is_source_dir(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("Cargo.toml"))
        .map(|t| t.contains("name = \"cona\""))
        .unwrap_or(false)
}

/// Newest mtime (secs) of anything that affects the build.
pub(super) fn source_mtime(dir: &Path) -> i64 {
    let mut newest = 0i64;
    let mut stamp = |p: &Path| {
        if let Ok(md) = std::fs::metadata(p) {
            if let Ok(m) = md.modified() {
                if let Ok(d) = m.duration_since(std::time::UNIX_EPOCH) {
                    newest = newest.max(d.as_secs() as i64);
                }
            }
        }
    };
    stamp(&dir.join("Cargo.toml"));
    stamp(&dir.join("Cargo.lock"));
    if let Ok(rd) = std::fs::read_dir(dir.join("src")) {
        for entry in rd.flatten() {
            stamp(&entry.path());
        }
    }
    newest
}

/// Whole-second mtime — deliberately coarser than `indexer::file_mtime`
/// (nanoseconds, needed for sub-second staleness); comparing binary vs source
/// checkout ages never needs that precision. Keep the two separate.
pub(super) fn mtime_secs(p: &Path) -> i64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Atomically place `src` at `dst` (copy to temp sibling, rename over).
pub(super) fn replace_binary(src: &Path, dst: &Path) -> Result<Change> {
    sweep_old_husks(dst);
    let existed = dst.exists();
    if existed && files_identical(src, dst) {
        return Ok(Change::Unchanged);
    }
    if let Some(dir) = dst.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // per-process temp name: auto-update can run concurrently (background
    // `upgrade --quiet` + git-hook upgrade); a shared name lets two copiers
    // interleave into the same inode and rename a truncated binary into place
    let tmp = dst.with_extension(format!("tmp-update.{}", std::process::id()));
    std::fs::copy(src, &tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    // Windows refuses to rename over a running exe (self-upgrade replaces the
    // very binary that spawned us): move the old file aside first — renaming a
    // running exe TO a new name is allowed — then best-effort-delete the husk.
    #[cfg(windows)]
    let aside = if existed {
        let aside = dst.with_extension(format!("old.{}", std::process::id()));
        std::fs::rename(dst, &aside)?;
        Some(aside)
    } else {
        None
    };
    std::fs::rename(&tmp, dst)?;
    #[cfg(windows)]
    if let Some(aside) = aside {
        let _ = std::fs::remove_file(aside);
    }
    Ok(if existed {
        Change::Updated
    } else {
        Change::Created
    })
}

/// Collect the previous upgrade's leavings next to `dst`. On Windows the
/// rename-aside husk (`cona.old.<pid>`) cannot be deleted while it IS the
/// running process — self-upgrade, the main caller — so each upgrade sweeps
/// the stale ones the last upgrade had to leave behind. Best-effort: a husk
/// that is still running stays locked and survives until the next sweep.
/// Deliberately does NOT touch `tmp-update.*` — a concurrent upgrade may be
/// mid-copy into its own tmp file.
pub(super) fn sweep_old_husks(dst: &Path) {
    let (Some(dir), Some(stem)) = (dst.parent(), dst.file_stem().and_then(|s| s.to_str())) else {
        return;
    };
    let prefix = format!("{stem}.old.");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        if e.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Update `dst` to release `ver`: prebuilt GitHub-release binary first,
/// `cargo install` from crates.io as fallback (private repo, exotic platform).
pub(super) fn install_release_binary(ver: &str, dst: &Path) -> Result<Change> {
    match download_release_binary(ver, dst) {
        Ok(ch) => Ok(ch),
        Err(e) => install_via_cargo(ver, dst)
            .map_err(|e2| anyhow!("binary download failed ({e}); cargo fallback failed ({e2})")),
    }
}

/// Download the GitHub-release archive for `ver`, extract, replace `dst`.
pub(super) fn download_release_binary(ver: &str, dst: &Path) -> Result<Change> {
    let target = release_target().ok_or_else(|| anyhow!("no prebuilt binary for this platform"))?;
    let tmp = std::env::temp_dir().join(format!("cona-update-{ver}"));
    fetch_release_archive(ver, target, &tmp)?;
    let bin = tmp.join(if cfg!(windows) { "cona.exe" } else { "cona" });
    let ch = replace_binary(&bin, dst)?;
    // the tarball bundles the optional resolve helper on targets where it
    // builds — install it beside cona too (best-effort, absence is fine)
    let helper = tmp.join(HELPER_EXE);
    if helper.is_file() {
        if let Some(dir) = dst.parent() {
            let _ = replace_binary(&helper, &dir.join(HELPER_EXE));
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(ch)
}

/// `cargo install cona@ver` into a temp root, then move the binary into
/// place — keeps the recorded install path instead of ~/.cargo/bin.
pub(super) fn install_via_cargo(ver: &str, dst: &Path) -> Result<Change> {
    let root = std::env::temp_dir().join(format!("cona-cargo-{ver}"));
    let ok = std::process::Command::new("cargo")
        .args(["install", "--locked", "--root"])
        .arg(&root)
        .arg(format!("cona@{ver}"))
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        bail!("cargo install cona@{ver} failed");
    }
    let bin = root
        .join("bin")
        .join(if cfg!(windows) { "cona.exe" } else { "cona" });
    let ch = replace_binary(&bin, dst)?;
    let _ = std::fs::remove_dir_all(&root);
    Ok(ch)
}

pub(super) fn cargo_build(dir: &Path) -> Result<()> {
    let status = std::process::Command::new("cargo")
        .args(["build", "--release"])
        .current_dir(dir)
        .status()
        .context("failed to run cargo — is Rust installed?")?;
    if !status.success() {
        bail!("cargo build failed");
    }
    Ok(())
}

/// Build the standalone `resolve-helper` crate and install its binary beside
/// cona. Returns the installed path, or `Ok(None)` if the crate isn't in the
/// checkout. Errors (build failed, no cargo) are surfaced to the caller as
/// non-fatal — the helper is optional. Only rebuilds when the binary is stale.
pub(super) fn install_resolve_helper(src_root: &Path, bin_dir: &Path) -> Result<Option<PathBuf>> {
    let crate_dir = src_root.join("src/resolve-helper");
    if !crate_dir.join("Cargo.toml").is_file() {
        return Ok(None); // not present in this checkout
    }
    let built = crate_dir.join("target/release").join(HELPER_EXE);
    if !built.exists() || mtime_secs(&built) < source_mtime(&crate_dir) {
        println!("building resolve helper (semantic resolution) …");
        cargo_build(&crate_dir)?;
    }
    if !built.exists() {
        bail!("helper binary not produced");
    }
    let dst = bin_dir.join(HELPER_EXE);
    replace_binary(&built, &dst)?;
    Ok(Some(dst))
}

pub(crate) fn shellexpand_home(p: &str) -> Result<String> {
    if let Some(rest) = p.strip_prefix("~/") {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
        return Ok(home.join(rest).to_string_lossy().to_string());
    }
    Ok(p.to_string())
}

pub(crate) fn on_path(dir: &Path) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d == dir))
        .unwrap_or(false)
}
