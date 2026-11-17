//! The search every list shares: a case-insensitive substring over a row's fields.

/// Whether a row matches `query`: an empty query matches everything, otherwise any field must
/// contain the query, ignoring case.
pub fn matches(query: &str, fields: &[&str]) -> bool {
    let q = query.to_lowercase();
    q.is_empty() || fields.iter().any(|f| f.to_lowercase().contains(&q))
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn an_empty_query_matches_everything() {
        assert!(matches("", &["a"]));
        assert!(matches("", &[]));
    }

    #[test]
    fn any_field_may_match_ignoring_case() {
        assert!(matches("SLE", &["w1", "sleep"]));
        assert!(matches("w1", &["W1", "sleep"]));
        assert!(!matches("zzz", &["w1", "sleep"]));
    }
}
