/// Fuzzy matching fallback for `find`: exact/substring queries stay in SQL;
/// this ranks candidates only when SQL found nothing. Higher score = better;
/// None = no match. Case-insensitive. Substring beats subsequence, early and
/// tight matches beat late and scattered ones.
pub fn fuzzy_score(query: &str, candidate: &str) -> Option<i64> {
    if query.is_empty() || candidate.is_empty() {
        return None;
    }
    let q = query.to_ascii_lowercase();
    let c = candidate.to_ascii_lowercase();
    if let Some(pos) = c.find(&q) {
        return Some(1000 - (pos as i64) * 4 - (c.len() as i64 - q.len() as i64));
    }
    // in-order subsequence: every query char must appear, gaps cost
    let mut score: i64 = 500;
    let mut rest = c.as_str();
    let mut last_end: usize = 0;
    let mut consumed: usize = 0;
    for qc in q.chars() {
        let pos = rest.find(qc)?;
        score -= (pos as i64) * 2; // gap penalty
        let advance = pos + qc.len_utf8();
        consumed += advance;
        rest = &rest[advance..];
        last_end = consumed;
    }
    score -= (c.len() - last_end) as i64 / 2; // trailing-noise penalty
    if score <= 0 {
        None
    } else {
        Some(score)
    }
}

/// Rank candidates for the `find` fallback: each item is scored as the max of
/// its bare name and qualified name (so `login` finds `UserService.login`),
/// ties break toward the shorter qualified name, and only the top `limit`
/// survive. Returns (score, index-into-input) pairs, best first.
pub fn rank<'a>(
    query: &str,
    candidates: impl Iterator<Item = (usize, &'a str, &'a str)>,
    limit: usize,
) -> Vec<(i64, usize)> {
    let mut scored: Vec<(i64, usize, usize)> = candidates
        .filter_map(|(idx, bare, qualified)| {
            let s = fuzzy_score(query, bare).max(fuzzy_score(query, qualified))?;
            Some((s, qualified.len(), idx))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.truncate(limit);
    scored.into_iter().map(|(s, _, idx)| (s, idx)).collect()
}

#[cfg(test)]
mod tests {
    use super::fuzzy_score;
    use super::rank;

    #[test]
    fn rank_prefers_score_then_shorter_qualified() {
        let cands = [
            ("login", "UserService.login"),
            ("login", "Very.Long.Path.login"),
            ("logout", "UserService.logout"),
            ("unrelated", "zzz"),
        ];
        let ranked = rank(
            "login",
            cands.iter().enumerate().map(|(i, (b, q))| (i, *b, *q)),
            10,
        );
        assert_eq!(ranked[0].1, 0, "shorter qualified name wins the tie");
        assert_eq!(ranked[1].1, 1);
        assert!(!ranked.iter().any(|(_, i)| *i == 3), "no-match is dropped");
    }

    #[test]
    fn substring_beats_subsequence() {
        let sub = fuzzy_score("index", "reindex_file").unwrap();
        let seq = fuzzy_score("idxf", "reindex_file").unwrap();
        assert!(sub > seq, "{sub} vs {seq}");
    }

    #[test]
    fn case_insensitive_and_ordering() {
        assert!(fuzzy_score("HttpLog", "http_logger").is_some());
        // earlier substring match ranks higher
        let early = fuzzy_score("log", "logger_init").unwrap();
        let late = fuzzy_score("log", "init_logger").unwrap();
        assert!(early > late);
    }

    #[test]
    fn no_match_is_none() {
        assert!(fuzzy_score("zzz", "open_project_db").is_none());
        assert!(fuzzy_score("", "x").is_none());
        assert!(fuzzy_score("x", "").is_none());
    }
}
