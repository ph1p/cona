//! Pure lexical behavior ranking. No stemming, synonyms, type or flow inference.
use std::collections::BTreeSet;

/// Unicode-aware words, including snake_case, camelCase and acronym boundaries.
pub fn tokens(text: &str) -> BTreeSet<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut words = BTreeSet::new();
    let mut word = String::new();
    for (i, &c) in chars.iter().enumerate() {
        let boundary = i > 0
            && c.is_uppercase()
            && (chars[i - 1].is_lowercase()
                || (chars[i - 1].is_uppercase()
                    && chars.get(i + 1).is_some_and(|n| n.is_lowercase())));
        if (boundary || !c.is_alphanumeric()) && !word.is_empty() {
            words.insert(std::mem::take(&mut word));
        }
        if c.is_alphanumeric() {
            word.extend(c.to_lowercase());
        }
    }
    if !word.is_empty() {
        words.insert(word);
    }
    words
}

pub fn query_tokens(query: &str) -> BTreeSet<String> {
    tokens(query)
        .into_iter()
        .filter(|t| {
            !matches!(
                t.as_str(),
                "a" | "an"
                    | "and"
                    | "are"
                    | "as"
                    | "at"
                    | "be"
                    | "by"
                    | "for"
                    | "from"
                    | "how"
                    | "in"
                    | "is"
                    | "it"
                    | "of"
                    | "on"
                    | "or"
                    | "that"
                    | "the"
                    | "this"
                    | "to"
                    | "with"
                    | "where"
                    | "which"
            )
        })
        .take(32)
        .collect()
}

pub struct Fields<'a> {
    pub name: &'a str,
    pub qualified: &'a str,
    pub signature: &'a str,
    pub docs: &'a str,
    pub body: &'a str,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Match {
    pub score: usize,
    pub reasons: Vec<String>,
}

/// Per-field presence, not frequency: repeated boilerplate cannot inflate rank.
pub fn score(query: &str, terms: &BTreeSet<String>, fields: &Fields<'_>) -> Option<Match> {
    let exact = !query.trim().is_empty()
        && (query.trim().to_lowercase() == fields.name.to_lowercase()
            || query.trim().to_lowercase() == fields.qualified.to_lowercase());
    if terms.is_empty() && !exact {
        return None;
    }
    let mut score = if exact { 100_000 } else { 0 };
    let mut reasons = if exact {
        vec!["exact name".into()]
    } else {
        vec![]
    };
    let mut covered = BTreeSet::new();
    for (label, weight, text) in [
        ("name", 80, fields.name),
        ("signature", 30, fields.signature),
        ("docs", 20, fields.docs),
        ("body", 10, fields.body),
    ] {
        let words = tokens(text);
        let matched: Vec<_> = terms.intersection(&words).cloned().collect();
        if !matched.is_empty() {
            score += weight * matched.len();
            covered.extend(matched.iter().cloned());
            reasons.push(format!("{label}: {}", matched.join(", ")));
        }
    }
    if score == 0 {
        return None;
    }
    score += covered.len() * 200;
    if !terms.is_empty() && covered.len() == terms.len() {
        score += 500;
    }
    Some(Match { score, reasons })
}

/// Bound display text by Unicode scalar count without slicing UTF-8 bytes.
pub fn excerpt(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut out: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rank(query: &str, name: &str, docs: &str, body: &str) -> usize {
        score(
            query,
            &query_tokens(query),
            &Fields {
                name,
                qualified: name,
                signature: "",
                docs,
                body,
            },
        )
        .map_or(0, |m| m.score)
    }

    #[test]
    fn split_unicode_and_acronyms() {
        assert_eq!(
            tokens("HTTPServer retryDelay café_Δοκιμή"),
            tokens("http server retry delay café δοκιμή")
        );
        assert_eq!(excerpt("é中abc", 2), "é中…");
    }

    #[test]
    fn behavior_and_exact_names() {
        assert!(
            rank("retry timeout", "attempt", "", "retry timeout")
                > rank("retry timeout", "retry", "", "")
        );
        assert!(rank("retry", "retry", "", "") > rank("retry", "worker", "retry", "retry"));
        assert!(
            rank("cache", "cacheResult", "", "") > rank("cache", "worker", "", "cache cache cache")
        );
        assert!(rank("retry", "worker", "retry", "") > rank("retry", "worker", "", "retry"));
    }

    #[test]
    fn stopwords_empty_and_presence_only() {
        assert_eq!(rank("the and", "worker", "", ""), 0);
        assert_eq!(rank("", "worker", "", ""), 0);
        assert_eq!(
            rank("retry", "worker", "", "retry"),
            rank("retry", "worker", "", "retry retry")
        );
        assert_eq!(rank("the", "the", "", ""), 100_000);
        assert_eq!(rank("CAFÉ", "café", "", ""), rank("café", "café", "", ""));
    }
}
