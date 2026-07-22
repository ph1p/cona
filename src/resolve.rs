//! Optional semantic name resolution via the out-of-process stack-graphs
//! helper (`cona-resolve-helper`). This is the sharpest tier of the
//! disambiguation policy: when the cheap heuristics in `graph::narrow_by_scope`
//! (scope → file → dir → arity) leave a name ambiguous AND the language has
//! published TSG rules, cona asks the helper to resolve the exact reference
//! to its definition(s).
//!
//! Everything here is FAIL-OPEN. The helper is a separate binary (it carries an
//! incompatible tree-sitter runtime — see docs/spike-semantic-resolution.md) so
//! it may simply be absent. A missing binary, a spawn error, a non-zero exit,
//! or unparseable output all return `None` — the caller then keeps its
//! name-based + arity result, exactly as if this tier didn't exist. Semantic
//! resolution only ever NARROWS; it never invents or drops a result on error.

use crate::install::{fetch_release_archive, release_target, HELPER_EXE};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// A reference to resolve: 1-based line + symbol name. No column — matching is
/// by (line, name), so cona and the helper never have to agree on a column
/// encoding. A (line, name) that isn't unique yields no semantic answer.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Ref {
    pub line: usize,
    pub name: String,
}

#[derive(Serialize)]
struct Request<'a> {
    lang: &'a str,
    path: &'a str,
    source: &'a str,
    refs: &'a [Ref],
    #[serde(skip_serializing_if = "Vec::is_empty")]
    deps: Vec<DepFile>,
}

/// A dependency file fed to the helper for cross-file resolution: a reference
/// in the primary file can resolve to a definition living in one of these.
#[derive(Serialize, Clone)]
pub struct DepFile {
    pub path: String,
    pub source: String,
}

#[derive(Deserialize)]
struct Response {
    #[serde(default)]
    resolved: Vec<Resolved>,
    #[serde(default)]
    #[allow(dead_code)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct Resolved {
    #[serde(rename = "ref")]
    reference: Ref,
    defs: Vec<Def>,
}

#[derive(Deserialize, Clone)]
pub struct Def {
    /// File the definition resolved to. With cross-file resolution this may be
    /// a dep file's path, not the primary file. Older helpers omit it → "".
    #[serde(default)]
    pub file: String,
    pub line: usize,
    #[serde(default)]
    #[allow(dead_code)]
    pub symbol: Option<String>,
}

/// Languages the helper ships TSG rules for. cona's own `detect_lang`
/// labels map straight through. Anything else → no semantic tier (the caller
/// never even spawns the helper).
pub fn lang_supported(lang: &str) -> bool {
    matches!(
        lang,
        "typescript" | "tsx" | "javascript" | "python" | "rust"
    )
}

/// Located once per process: `Some(path)` if the helper binary is findable,
/// `None` if not (cached so we don't re-probe PATH on every ambiguous name).
fn helper_path() -> Option<&'static PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(locate_helper).as_ref()
}

/// The four non-fetching discovery steps, in order, each tagged with how the
/// helper was found. Shared by `locate_helper` (which falls through to fetch)
/// and `helper_status` (which stops here) so the probe order can't drift.
fn probe_existing() -> Option<(PathBuf, &'static str)> {
    // 1) explicit override
    if let Ok(p) = std::env::var("CONA_RESOLVE_HELPER") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some((pb, "CONA_RESOLVE_HELPER"));
        }
    }
    // 2) sibling of the running cona binary (release tarball / install.sh)
    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            let sib = dir.join(HELPER_EXE);
            if sib.is_file() {
                return Some((sib, "sibling of cona"));
            }
        }
    }
    // 3) anywhere on PATH
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let cand = dir.join(HELPER_EXE);
            if cand.is_file() {
                return Some((cand, "PATH"));
            }
        }
    }
    // 4) previously auto-fetched into ~/.cona/bin
    if let Some(cached) = fetched_helper_path() {
        if cached.is_file() {
            return Some((cached, "auto-fetched"));
        }
    }
    None
}

fn locate_helper() -> Option<PathBuf> {
    if let Some((p, _)) = probe_existing() {
        return Some(p);
    }
    // `cargo install` users have no sibling helper — fetch the matching binary
    // from the GitHub release for THIS cona version (once). All fail-open:
    // offline / no prebuilt / extract error → None, and cona keeps its
    // name-based + arity result.
    fetch_helper().ok().filter(|p| p.is_file())
}

/// Where an auto-fetched helper is cached: `~/.cona/bin/<exe>`.
fn fetched_helper_path() -> Option<PathBuf> {
    Some(crate::db::data_dir().ok()?.join("bin").join(HELPER_EXE))
}

/// Download the release archive for the current cona version, extract just
/// the helper binary into `~/.cona/bin`, and return its path. Best-effort:
/// any failure (no prebuilt for this platform, network down, archive without a
/// helper) returns `Err` and the caller degrades gracefully.
fn fetch_helper() -> anyhow::Result<PathBuf> {
    use anyhow::{anyhow, bail};

    // opt-out escape hatch for locked-down / offline environments
    if std::env::var("CONA_NO_FETCH_HELPER").is_ok() {
        bail!("helper fetch disabled via CONA_NO_FETCH_HELPER");
    }
    // (a cached binary is already returned by `probe_existing` step 4 before we
    // ever get here, so no need to re-check `dst.is_file()`)
    let dst = fetched_helper_path().ok_or_else(|| anyhow!("no home dir"))?;
    // back off after a failed attempt so an offline machine doesn't fire a
    // (slow) curl on every ambiguous query — retry at most once per 24h.
    let stamp = dst.with_file_name(".helper-fetch-attempt");
    if let Ok(meta) = std::fs::metadata(&stamp) {
        if let Ok(modified) = meta.modified() {
            if let Ok(elapsed) = modified.elapsed() {
                if elapsed.as_secs() < 24 * 3600 {
                    bail!("helper fetch backed off (recent failed attempt)");
                }
            }
        }
    }
    if let Some(dir) = stamp.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&stamp, b""); // touch BEFORE trying (so a hang backs off too)

    let target = release_target().ok_or_else(|| anyhow!("no prebuilt for this platform"))?;
    let ver = env!("CARGO_PKG_VERSION");
    let tmp = std::env::temp_dir().join(format!("cona-helper-{ver}-{target}"));
    let _ = std::fs::remove_dir_all(&tmp);
    fetch_release_archive(ver, target, &tmp)?;
    let extracted = tmp.join(HELPER_EXE);
    if !extracted.is_file() {
        // release for this platform shipped without a helper — expected on
        // targets where the helper build was skipped; don't retry churn.
        bail!("archive has no helper for {target}");
    }
    if let Some(dir) = dst.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::copy(&extracted, &dst)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755));
    }
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = std::fs::remove_file(&stamp); // success → clear the back-off marker
    Ok(dst)
}

/// Whether a resolve helper is available at all — lets callers skip the whole
/// ambiguity-detection dance when there's no semantic tier to consult.
pub fn available() -> bool {
    helper_path().is_some()
}

/// Status for `doctor`: the already-present helper path WITHOUT triggering an
/// auto-fetch. Returns the path and how it was found, or `None` if it would
/// have to be fetched.
pub fn helper_status() -> Option<(PathBuf, &'static str)> {
    probe_existing()
}

/// The languages the semantic tier covers, for display.
pub const SUPPORTED_LANGS: &str = "typescript, tsx, javascript, python, rust";

/// Resolve reference positions to their definition sites. Returns, per input
/// ref (same order), the list of definition positions the helper found; an
/// empty inner list means "no semantic answer for that ref". Returns `None`
/// (fail-open) if the helper is unavailable or anything goes wrong.
pub fn resolve_refs(lang: &str, path: &str, source: &str, refs: &[Ref]) -> Option<Vec<Vec<Def>>> {
    resolve_refs_in(lang, path, source, refs, &[])
}

/// Process-lifetime cache of helper responses, keyed by a hash of
/// (lang, primary path+mtime, dep paths+mtimes, refs). The helper is one-shot
/// (a fresh process rebuilds the whole stack graph each spawn), so caching the
/// RESPONSE here is what makes a repeated ambiguous query within one cona
/// invocation free — the "mtime-keyed cache" roadmap item, kept fail-open and
/// entirely cona-side so the helper stays a stateless subprocess.
type CacheMap = std::collections::HashMap<u64, Option<Vec<Vec<Def>>>>;
fn response_cache() -> &'static std::sync::Mutex<CacheMap> {
    static CACHE: OnceLock<std::sync::Mutex<CacheMap>> = OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(CacheMap::new()))
}

/// mtime of a file as nanos since epoch, or 0 if unknown (unknown → the key
/// still differs from a known-mtime key, so we never serve a stale hit for a
/// file we couldn't stat; worst case we just recompute).
fn mtime_key(path: &str) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn cache_key(lang: &str, path: &str, refs: &[Ref], deps: &[DepFile]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    lang.hash(&mut h);
    path.hash(&mut h);
    mtime_key(path).hash(&mut h);
    for r in refs {
        r.line.hash(&mut h);
        r.name.hash(&mut h);
    }
    for d in deps {
        d.path.hash(&mut h);
        mtime_key(&d.path).hash(&mut h);
    }
    h.finish()
}

/// A candidate definition an ambiguous reference might bind to: its bare name
/// plus the (file, line) where the definition lives. Callers build these from
/// their own index rows; `disambiguate` matches semantic resolutions against
/// them so stack-graphs' intermediate binding nodes (imports, re-exports) never
/// masquerade as the answer.
#[derive(Clone)]
pub struct Candidate {
    pub name: String,
    pub file: String,
    pub line: i64,
}

/// The shared semantic-disambiguation policy used by `context`, `callers`/
/// `callees`, and `rename`. For each ambiguous ref, ask the helper (with `deps`
/// for cross-file resolution), then keep only resolved defs that coincide with
/// one of that name's `candidates`. Returns, per input ref (same order), the
/// uniquely-resolved `(file, line)` — or `None` for that ref when the helper
/// gave no answer, resolved to something that isn't a candidate, or stayed
/// ambiguous among candidates. Fail-open: a missing/broken helper yields all
/// `None`, so every caller simply keeps its name-based result.
pub fn disambiguate(
    lang: &str,
    path: &str,
    source: &str,
    refs: &[Ref],
    candidates: &[Candidate],
    deps: &[DepFile],
) -> Vec<Option<(String, i64)>> {
    let mut out = vec![None; refs.len()];
    let Some(results) = resolve_refs_in(lang, path, source, refs, deps) else {
        return out;
    };
    for (i, (r, defs)) in refs.iter().zip(results).enumerate() {
        // resolved defs that actually coincide with one of this name's
        // candidate rows (a def with empty `file` means the primary file).
        let matched: Vec<(String, i64)> = defs
            .iter()
            .map(|d| {
                let f = if d.file.is_empty() { path } else { &d.file };
                (f.to_string(), d.line as i64)
            })
            .filter(|(f, l)| {
                candidates
                    .iter()
                    .any(|c| c.name == r.name && c.file == *f && c.line == *l)
            })
            .collect();
        if let [only] = matched.as_slice() {
            out[i] = Some(only.clone());
        }
    }
    out
}

/// Like [`resolve_refs`] but also stitches `deps` (extra files) into the same
/// stack graph so a reference can resolve to a definition in another file
/// (cross-file resolution). `deps` may be empty for same-file-only resolution.
pub fn resolve_refs_in(
    lang: &str,
    path: &str,
    source: &str,
    refs: &[Ref],
    deps: &[DepFile],
) -> Option<Vec<Vec<Def>>> {
    if refs.is_empty() || !lang_supported(lang) {
        return None;
    }
    let key = cache_key(lang, path, refs, deps);
    if let Ok(cache) = response_cache().lock() {
        if let Some(hit) = cache.get(&key) {
            return hit.clone();
        }
    }
    let result = resolve_refs_uncached(lang, path, source, refs, deps);
    if let Ok(mut cache) = response_cache().lock() {
        cache.insert(key, result.clone());
    }
    result
}

fn resolve_refs_uncached(
    lang: &str,
    path: &str,
    source: &str,
    refs: &[Ref],
    deps: &[DepFile],
) -> Option<Vec<Vec<Def>>> {
    let bin = helper_path()?;
    let req = Request {
        lang,
        path,
        source,
        refs,
        deps: deps.to_vec(),
    };
    let payload = serde_json::to_vec(&req).ok()?;

    let mut child = Command::new(bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(&payload).ok()?;
    let out = child.wait_with_output().ok()?;
    // non-zero exit → helper reported {"error":…}; degrade silently
    if !out.status.success() {
        return None;
    }
    let resp: Response = serde_json::from_slice(&out.stdout).ok()?;

    // map back to input order; a ref the helper didn't return → empty
    let mut by_ref: Vec<Vec<Def>> = vec![Vec::new(); refs.len()];
    for r in resp.resolved {
        if let Some(i) = refs.iter().position(|p| *p == r.reference) {
            by_ref[i] = r.defs;
        }
    }
    Some(by_ref)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_tsg_languages_supported() {
        for l in ["typescript", "tsx", "javascript", "python", "rust"] {
            assert!(lang_supported(l), "{l} should be supported");
        }
        for l in ["go", "c", "ruby", "", "java"] {
            assert!(!lang_supported(l), "{l} must NOT claim support");
        }
    }

    #[test]
    fn empty_or_unsupported_never_spawns() {
        // no refs → None without touching the helper
        assert!(resolve_refs("typescript", "a.ts", "x;", &[]).is_none());
        assert!(resolve_refs_in("typescript", "a.ts", "x;", &[], &[]).is_none());
        // unsupported language → None regardless of helper presence
        let r = [Ref {
            line: 1,
            name: "foo".into(),
        }];
        assert!(resolve_refs("go", "a.go", "func foo(){}", &r).is_none());
    }

    #[test]
    fn cache_key_reacts_to_inputs() {
        let r = [Ref {
            line: 1,
            name: "foo".into(),
        }];
        let base = cache_key("typescript", "a.ts", &r, &[]);
        // same inputs → same key (deterministic)
        assert_eq!(base, cache_key("typescript", "a.ts", &r, &[]));
        // language, path, ref, and dep-set each shift the key
        assert_ne!(base, cache_key("javascript", "a.ts", &r, &[]));
        assert_ne!(base, cache_key("typescript", "b.ts", &r, &[]));
        let r2 = [Ref {
            line: 2,
            name: "foo".into(),
        }];
        assert_ne!(base, cache_key("typescript", "a.ts", &r2, &[]));
        let deps = [DepFile {
            path: "b.ts".into(),
            source: "x".into(),
        }];
        assert_ne!(base, cache_key("typescript", "a.ts", &r, &deps));
    }

    #[test]
    fn def_tolerates_missing_file_field() {
        // older helper output without `file` still deserializes (file → "")
        let d: Def = serde_json::from_str(r#"{"line":3,"symbol":"foo"}"#).unwrap();
        assert_eq!(d.file, "");
        assert_eq!(d.line, 3);
    }
}
