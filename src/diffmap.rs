//! Pure helpers for `cona diff`: parse `git diff --unified=0` output into
//! per-file changed line ranges (new side), so changed lines can be mapped to
//! the symbols that contain them. No git or IO in here — fully unit-tested.

/// One changed file from a unified diff, with new-side line ranges.
#[derive(Debug, PartialEq)]
pub struct FileChange {
    pub path: String,
    /// Inclusive (start, end) line ranges on the NEW side of the diff.
    /// A pure deletion (zero new lines) is represented as the single line
    /// the deletion happened at, so it still maps to an enclosing symbol.
    pub ranges: Vec<(i64, i64)>,
    pub deleted: bool,
}

/// Parse `git diff --unified=0` text. Only headers and hunk markers are
/// inspected; content lines are ignored.
pub fn parse_unified(diff: &str) -> Vec<FileChange> {
    let mut out: Vec<FileChange> = Vec::new();
    let mut old_path: Option<String> = None;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("--- ") {
            old_path = rest.strip_prefix("a/").map(str::to_string);
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            if rest == "/dev/null" {
                // deleted file — keep the old path so it can be reported
                if let Some(p) = old_path.take() {
                    out.push(FileChange {
                        path: p,
                        ranges: Vec::new(),
                        deleted: true,
                    });
                }
            } else if let Some(p) = rest.strip_prefix("b/") {
                out.push(FileChange {
                    path: p.to_string(),
                    ranges: Vec::new(),
                    deleted: false,
                });
                old_path = None;
            }
        } else if let Some(rest) = line.strip_prefix("@@ ") {
            // "@@ -a[,b] +c[,d] @@ …" → new-side range
            let Some(plus) = rest.split(' ').find_map(|t| t.strip_prefix('+')) else {
                continue;
            };
            let mut it = plus.splitn(2, ',');
            let start: i64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let count: i64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(1);
            if let Some(fc) = out.last_mut() {
                if fc.deleted {
                    continue;
                }
                if count == 0 {
                    // pure deletion: anchor at the surrounding line
                    fc.ranges.push((start.max(1), start.max(1)));
                } else {
                    fc.ranges.push((start, start + count - 1));
                }
            }
        }
    }
    out
}

/// True if the symbol span [s, e] overlaps any changed range.
pub fn overlaps(s: i64, e: i64, ranges: &[(i64, i64)]) -> bool {
    ranges.iter().any(|(rs, re)| s <= *re && e >= *rs)
}

/// True if [s, e] contains at least one changed line (from `ranges`) that no
/// interval in `covers` covers. Used to keep a container symbol whose own
/// lines changed, while dropping one whose changes all lie in nested symbols.
pub fn has_uncovered(s: i64, e: i64, ranges: &[(i64, i64)], covers: &[(i64, i64)]) -> bool {
    let mut sorted: Vec<(i64, i64)> = covers.to_vec();
    sorted.sort_unstable();
    for (rs, re) in ranges {
        let (cs, ce) = ((*rs).max(s), (*re).min(e));
        if cs > ce {
            continue;
        }
        let mut cur = cs;
        for (vs, ve) in &sorted {
            if *ve < cur {
                continue;
            }
            if *vs > cur {
                return true; // gap before this cover
            }
            cur = (*ve + 1).max(cur);
            if cur > ce {
                break;
            }
        }
        if cur <= ce {
            return true; // tail not covered
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modified_and_pure_deletion_hunks() {
        let diff = "\
diff --git a/src/x.rs b/src/x.rs
index 111..222 100644
--- a/src/x.rs
+++ b/src/x.rs
@@ -10,2 +12,3 @@ fn foo() {
+a
@@ -30 +33,0 @@ fn bar() {
-gone
";
        let fcs = parse_unified(diff);
        assert_eq!(fcs.len(), 1);
        assert_eq!(fcs[0].path, "src/x.rs");
        assert!(!fcs[0].deleted);
        // +12,3 → 12..14; +33,0 (pure deletion) → anchored at 33
        assert_eq!(fcs[0].ranges, vec![(12, 14), (33, 33)]);
    }

    #[test]
    fn parses_new_and_deleted_files() {
        let diff = "\
diff --git a/new.py b/new.py
--- /dev/null
+++ b/new.py
@@ -0,0 +1,5 @@
diff --git a/old.py b/old.py
--- a/old.py
+++ /dev/null
@@ -1,7 +0,0 @@
";
        let fcs = parse_unified(diff);
        assert_eq!(fcs.len(), 2);
        assert_eq!(fcs[0].path, "new.py");
        assert_eq!(fcs[0].ranges, vec![(1, 5)]);
        assert!(!fcs[0].deleted);
        assert_eq!(fcs[1].path, "old.py");
        assert!(fcs[1].deleted);
    }

    #[test]
    fn uncovered_lines_keep_container() {
        // change spans 10..40, nested symbol covers only 20..25 → uncovered
        assert!(has_uncovered(1, 100, &[(10, 40)], &[(20, 25)]));
        // all changed lines inside the nested cover → fully covered
        assert!(!has_uncovered(1, 100, &[(20, 25)], &[(18, 30)]));
        // two covers with a gap on a changed line
        assert!(has_uncovered(1, 100, &[(10, 30)], &[(10, 19), (21, 30)]));
        // no covers at all → any overlapping change is uncovered
        assert!(has_uncovered(1, 100, &[(50, 50)], &[]));
        // change outside the span is ignored
        assert!(!has_uncovered(1, 10, &[(50, 60)], &[]));
    }

    #[test]
    fn overlap_is_inclusive() {
        assert!(overlaps(5, 10, &[(10, 20)]));
        assert!(overlaps(5, 10, &[(1, 5)]));
        assert!(!overlaps(5, 10, &[(11, 20)]));
        assert!(!overlaps(5, 10, &[]));
    }
}
