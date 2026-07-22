/// Replace lines [start, end] (1-based, inclusive) of `src` with `replacement`.
/// Pure function so it can be unit-tested independently of the CLI.
/// A CRLF source stays CRLF throughout (replacement is normalized to match).
pub fn splice_lines(src: &str, start: usize, end: usize, replacement: &str) -> String {
    let crlf = src.contains("\r\n");
    let replacement = replacement.replace("\r\n", "\n");
    let lines: Vec<&str> = src.lines().collect();
    let start_idx = start.saturating_sub(1);
    let end_idx = end.min(lines.len());
    let mut out = String::with_capacity(src.len() + replacement.len());
    for l in &lines[..start_idx.min(lines.len())] {
        out.push_str(l);
        out.push('\n');
    }
    out.push_str(replacement.trim_end_matches('\n'));
    out.push('\n');
    for l in &lines[end_idx..] {
        out.push_str(l);
        out.push('\n');
    }
    if crlf {
        out.replace('\n', "\r\n")
    } else {
        out
    }
}

/// Insert `code` after the first `at` lines of `src` (0 = prepend). `at` is
/// clamped to the line count, so an out-of-range value appends. Works on an
/// empty source (produces just the inserted code). CRLF is preserved.
pub fn splice_insert(src: &str, at: usize, code: &str) -> String {
    let crlf = src.contains("\r\n");
    let code = code.replace("\r\n", "\n");
    let lines: Vec<&str> = src.lines().collect();
    let at = at.min(lines.len());
    let mut out = String::with_capacity(src.len() + code.len() + 1);
    for l in &lines[..at] {
        out.push_str(l);
        out.push('\n');
    }
    out.push_str(code.trim_end_matches('\n'));
    out.push('\n');
    for l in &lines[at..] {
        out.push_str(l);
        out.push('\n');
    }
    if crlf {
        out.replace('\n', "\r\n")
    } else {
        out
    }
}

/// Replace `old` (of known byte length) with `new` at the given identifier
/// positions ((1-based line, byte col within the line), any order). Pure —
/// the splice logic for `rename`. CRLF sources stay CRLF; a trailing newline
/// is preserved exactly as in the input.
pub fn apply_renames(src: &str, positions: &[(usize, usize)], old_len: usize, new: &str) -> String {
    let crlf = src.contains("\r\n");
    let had_trailing_nl = src.ends_with('\n');
    let mut lines: Vec<String> = src.lines().map(str::to_string).collect();
    // one global sort, right-to-left within each line keeps cols valid
    let mut positions: Vec<(usize, usize)> = positions.to_vec();
    positions.sort_unstable_by(|a, b| b.cmp(a));
    for (ln, col) in positions {
        let Some(line) = lines.get_mut(ln - 1) else {
            continue;
        };
        if col + old_len <= line.len() {
            line.replace_range(col..col + old_len, new);
        }
    }
    let sep = if crlf { "\r\n" } else { "\n" };
    let mut out = lines.join(sep);
    if had_trailing_nl {
        out.push_str(sep);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_middle() {
        let src = "a\nb\nc\nd\n";
        assert_eq!(splice_lines(src, 2, 3, "X\nY"), "a\nX\nY\nd\n");
    }

    #[test]
    fn replaces_first_and_last() {
        assert_eq!(splice_lines("a\nb\n", 1, 1, "Z"), "Z\nb\n");
        assert_eq!(splice_lines("a\nb\n", 2, 2, "Z"), "a\nZ\n");
    }

    #[test]
    fn whole_file() {
        assert_eq!(splice_lines("a\nb\n", 1, 2, "only"), "only\n");
    }

    #[test]
    fn out_of_range_end_is_clamped() {
        assert_eq!(splice_lines("a\nb\n", 2, 99, "Z"), "a\nZ\n");
    }

    #[test]
    fn crlf_source_stays_crlf() {
        let src = "a\r\nb\r\nc\r\n";
        assert_eq!(splice_lines(src, 2, 2, "X\nY"), "a\r\nX\r\nY\r\nc\r\n");
        // CRLF replacement into CRLF source: no double-\r
        assert_eq!(
            splice_lines(src, 2, 2, "X\r\nY\r\n"),
            "a\r\nX\r\nY\r\nc\r\n"
        );
    }

    #[test]
    fn lf_source_stays_lf_even_with_crlf_replacement() {
        assert_eq!(splice_lines("a\nb\n", 2, 2, "X\r\nY"), "a\nX\nY\n");
    }

    #[test]
    fn insert_prepend_middle_append() {
        assert_eq!(splice_insert("a\nb\n", 0, "Z"), "Z\na\nb\n"); // prepend
        assert_eq!(splice_insert("a\nb\n", 1, "Z"), "a\nZ\nb\n"); // after line 1
        assert_eq!(splice_insert("a\nb\n", 99, "Z"), "a\nb\nZ\n"); // clamp → append
    }

    #[test]
    fn insert_into_empty_source() {
        assert_eq!(splice_insert("", 0, "fn main() {}"), "fn main() {}\n");
    }

    #[test]
    fn insert_preserves_crlf() {
        assert_eq!(
            splice_insert("a\r\nb\r\n", 1, "X\nY"),
            "a\r\nX\r\nY\r\nb\r\n"
        );
    }

    #[test]
    fn rename_multiple_hits_same_line_right_to_left() {
        let src = "foo(foo, foo)\nbar()\n";
        let pos = vec![(1, 0), (1, 4), (1, 9)];
        assert_eq!(
            apply_renames(src, &pos, 3, "longer"),
            "longer(longer, longer)\nbar()\n"
        );
    }

    #[test]
    fn rename_preserves_crlf_and_trailing_newline() {
        let src = "foo()\r\nfoo()\r\n";
        let out = apply_renames(src, &[(1, 0), (2, 0)], 3, "x");
        assert_eq!(out, "x()\r\nx()\r\n");
        let no_nl = apply_renames("foo", &[(1, 0)], 3, "yy");
        assert_eq!(no_nl, "yy");
    }
}
