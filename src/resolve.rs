//! Optional semantic name resolution via the out-of-process stack-graphs
//! helper (`cona-resolve-helper`) — the sharpest disambiguation tier: when
//! `graph::narrow_by_scope` (scope → file → dir → arity) leaves a name
//! ambiguous AND the language has TSG rules, the helper resolves the exact
//! reference to its definition(s).
//!
//! Everything here is FAIL-OPEN. The helper is a separate binary (incompatible
//! tree-sitter runtime — see docs/architecture.md) and may be absent. Missing
//! binary, spawn error, non-zero exit or bad output all return `None`, and the
//! caller keeps its name-based + arity result. Semantic resolution only ever
//! NARROWS; it never invents or drops a result on error.

use crate::install::{fetch_release_archive, release_target, HELPER_EXE};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// A reference to resolve: 1-based line + symbol name. No column, so cona and
/// the helper never have to agree on a column encoding. A non-unique
/// (line, name) yields no semantic answer.
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

/// A dependency file for cross-file resolution: a primary-file reference can
/// resolve to a definition in one of these.
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

#[derive(Serialize, Deserialize, Clone)]
pub struct Def {
    /// File the definition resolved to (may be a dep file). Older helpers omit
    /// it → "".
    #[serde(default)]
    pub file: String,
    pub line: usize,
    #[serde(default)]
    #[allow(dead_code)]
    pub symbol: Option<String>,
}

/// Languages the helper ships TSG rules for (`detect_lang` labels). Anything
/// else → no semantic tier; the helper is never spawned.
pub fn lang_supported(lang: &str) -> bool {
    matches!(
        lang,
        "typescript" | "tsx" | "javascript" | "python" | "rust"
    )
}

/// Located once per process, so PATH isn't re-probed per ambiguous name.
fn helper_path() -> Option<&'static PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(locate_helper).as_ref()
}

/// The four non-fetching discovery steps, each tagged with how the helper was
/// found. Shared by `locate_helper` (then fetches) and `helper_status` (stops
/// here) so the probe order can't drift.
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
    // `cargo install` users have no sibling helper — fetch it from the GitHub
    // release for THIS cona version (once). Fail-open: offline / no prebuilt /
    // extract error → None.
    fetch_helper().ok().filter(|p| p.is_file())
}

/// Where an auto-fetched helper is cached: `~/.cona/bin/<exe>`.
fn fetched_helper_path() -> Option<PathBuf> {
    Some(crate::db::data_dir().ok()?.join("bin").join(HELPER_EXE))
}

/// Download this version's release archive and extract just the helper into
/// `~/.cona/bin`. Best-effort: any failure (no prebuilt, network down, archive
/// without a helper) returns `Err` and the caller degrades gracefully.
fn fetch_helper() -> anyhow::Result<PathBuf> {
    use anyhow::{anyhow, bail};

    // opt-out escape hatch for locked-down / offline environments
    if std::env::var("CONA_NO_FETCH_HELPER").is_ok() {
        bail!("helper fetch disabled via CONA_NO_FETCH_HELPER");
    }
    // (a cached binary was already returned by `probe_existing` step 4)
    let dst = fetched_helper_path().ok_or_else(|| anyhow!("no home dir"))?;
    // back off after a failure so an offline machine doesn't curl on every
    // ambiguous query — retry at most once per 24h.
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
        // expected on targets where the helper build was skipped
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

/// Whether a resolve helper is available — lets callers skip ambiguity
/// detection entirely when there's no semantic tier.
pub fn available() -> bool {
    helper_path().is_some()
}

/// Status for `doctor`: the present helper and how it was found, WITHOUT
/// triggering an auto-fetch (`None` = would have to be fetched).
pub fn helper_status() -> Option<(PathBuf, &'static str)> {
    probe_existing()
}

/// The languages the semantic tier covers, for display.
pub const SUPPORTED_LANGS: &str = "typescript, tsx, javascript, python, rust";

/// Resolve reference positions to definition sites: per input ref (same
/// order), the defs found; empty = no semantic answer. `None` (fail-open) if
/// the helper is unavailable or anything goes wrong.
pub fn resolve_refs(lang: &str, path: &str, source: &str, refs: &[Ref]) -> Option<Vec<Vec<Def>>> {
    resolve_refs_in(lang, path, source, refs, &[])
}

/// Cache of helper responses, keyed by a hash of everything the answer
/// depends on (lang, primary path+source, refs, deps, helper binary). Two
/// tiers: in memory, making repeats within one MCP session or `rename` free;
/// on disk under `<data_dir>/resolve-cache/`, because the TS/TSX helper spends
/// ~0.7s compiling TSG rules on EVERY spawn (1–2s per CLI `context`).
/// Content-keyed (not mtime): a stale hit is impossible and the key is
/// cwd-independent.
type CacheMap = std::collections::HashMap<u64, Option<Vec<Vec<Def>>>>;
fn response_cache() -> &'static std::sync::Mutex<CacheMap> {
    static CACHE: OnceLock<std::sync::Mutex<CacheMap>> = OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(CacheMap::new()))
}

/// Disk entries kept before the cache directory is cleared wholesale — a
/// cheap bound; a cleared entry costs one helper spawn to rebuild.
const DISK_CACHE_MAX: usize = 2000;

fn disk_cache_dir() -> Option<PathBuf> {
    crate::db::data_dir().ok().map(|d| d.join("resolve-cache"))
}

fn disk_get(key: u64) -> Option<Vec<Vec<Def>>> {
    let f = disk_cache_dir()?.join(format!("{key:016x}.json"));
    serde_json::from_slice(&std::fs::read(f).ok()?).ok()
}

/// Best effort: a failed write only means the next call spawns again.
fn disk_put(key: u64, defs: &[Vec<Def>]) {
    if crate::db::is_read_only() {
        return;
    }
    let Some(dir) = disk_cache_dir() else {
        return;
    };
    // every 64th write checks the bound, so the common path is one write
    if key.is_multiple_of(64) && std::fs::read_dir(&dir).is_ok_and(|rd| rd.count() > DISK_CACHE_MAX)
    {
        let _ = std::fs::remove_dir_all(&dir);
    }
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(bytes) = serde_json::to_vec(defs) {
        let _ = std::fs::write(dir.join(format!("{key:016x}.json")), bytes);
    }
}

/// Identity of the helper binary (path, size, mtime): an upgraded helper may
/// resolve differently, so its answers must not be served from the old one's.
fn helper_identity() -> (String, u64, u64) {
    let Some(p) = helper_path() else {
        return Default::default();
    };
    let m = std::fs::metadata(p).ok();
    let mtime = m
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    (
        p.to_string_lossy().into_owned(),
        m.map_or(0, |m| m.len()),
        mtime,
    )
}

fn cache_key(lang: &str, path: &str, source: &str, refs: &[Ref], deps: &[DepFile]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    lang.hash(&mut h);
    path.hash(&mut h);
    source.hash(&mut h);
    for r in refs {
        r.line.hash(&mut h);
        r.name.hash(&mut h);
    }
    for d in deps {
        d.path.hash(&mut h);
        d.source.hash(&mut h);
    }
    helper_identity().hash(&mut h);
    h.finish()
}

/// A candidate definition (bare name + file, line) built from the caller's
/// index rows. `disambiguate` matches resolutions against these so
/// stack-graphs' intermediate nodes (imports, re-exports) never masquerade as
/// the answer.
#[derive(Clone)]
pub struct Candidate {
    pub name: String,
    pub file: String,
    pub line: i64,
}

/// THE semantic-disambiguation policy for `context`, `callers`/`callees` and
/// `rename`. Asks the helper (with `deps` for cross-file resolution) and keeps
/// only defs that coincide with one of that name's `candidates`. Per input ref
/// (same order): the uniquely-resolved `(file, line)`, or `None` when there
/// was no answer, a non-candidate, or ambiguity among candidates. Fail-open: a
/// missing/broken helper yields all `None`.
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
        // defs matching a candidate row (empty `file` = the primary file)
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

/// Like [`resolve_refs`] but stitches `deps` into the same stack graph for
/// cross-file resolution (empty = same-file only).
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
    let key = cache_key(lang, path, source, refs, deps);
    if let Ok(cache) = response_cache().lock() {
        if let Some(hit) = cache.get(&key) {
            return hit.clone();
        }
    }
    // Only answers are persisted: a `None` is a missing or failing helper,
    // which may be fixed by the next call.
    let result = match disk_get(key) {
        Some(hit) => Some(hit),
        None => {
            let r = resolve_refs_uncached(lang, path, source, refs, deps);
            if let Some(defs) = &r {
                disk_put(key, defs);
            }
            r
        }
    };
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
        let key = |lang, path, src, refs: &[Ref], deps: &[DepFile]| {
            cache_key(lang, path, src, refs, deps)
        };
        let base = key("typescript", "a.ts", "s", &r, &[]);
        // same inputs → same key (deterministic)
        assert_eq!(base, key("typescript", "a.ts", "s", &r, &[]));
        // language, path, source, ref, and dep-set each shift the key
        assert_ne!(base, key("javascript", "a.ts", "s", &r, &[]));
        assert_ne!(base, key("typescript", "b.ts", "s", &r, &[]));
        assert_ne!(base, key("typescript", "a.ts", "t", &r, &[]));
        let r2 = [Ref {
            line: 2,
            name: "foo".into(),
        }];
        assert_ne!(base, key("typescript", "a.ts", "s", &r2, &[]));
        let deps = [DepFile {
            path: "b.ts".into(),
            source: "x".into(),
        }];
        assert_ne!(base, key("typescript", "a.ts", "s", &r, &deps));
        // an edited dep is a different answer, not a stale hit
        let edited = [DepFile {
            path: "b.ts".into(),
            source: "y".into(),
        }];
        assert_ne!(
            key("typescript", "a.ts", "s", &r, &deps),
            key("typescript", "a.ts", "s", &r, &edited)
        );
    }

    #[test]
    fn def_tolerates_missing_file_field() {
        // older helper output without `file` still deserializes (file → "")
        let d: Def = serde_json::from_str(r#"{"line":3,"symbol":"foo"}"#).unwrap();
        assert_eq!(d.file, "");
        assert_eq!(d.line, 3);
    }
}
