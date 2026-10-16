//! Case-insensitive library search helpers, independent of gpui.

use std::path::Path;

/// Case-insensitive substring match with zero heap allocations on the common paths.
pub fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if haystack.len() < needle.len() {
        return false;
    }

    // Fast-path: ASCII matching does zero heap allocations.
    if haystack.is_ascii() && needle.is_ascii() {
        let n = needle.as_bytes();
        let h = haystack.as_bytes();
        return h
            .windows(n.len())
            .any(|w| w.iter().zip(n).all(|(a, b)| a.eq_ignore_ascii_case(b)));
    }

    // General Unicode case-folded comparison.
    let needle_chars: Vec<char> = needle.chars().flat_map(|c| c.to_lowercase()).collect();
    if needle_chars.is_empty() {
        return true;
    }
    let haystack_chars: Vec<char> = haystack.chars().flat_map(|c| c.to_lowercase()).collect();
    if haystack_chars.len() < needle_chars.len() {
        return false;
    }
    haystack_chars
        .windows(needle_chars.len())
        .any(|w| w == needle_chars.as_slice())
}

/// Match a query against a book's display title, source path, or assigned series.
///
/// An empty or whitespace-only query matches every book. Matching is substring based and
/// case-insensitive; the stored title and path are never modified.
pub fn matches(title: &str, path: &Path, series: Option<&str>, query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }

    if contains_ignore_case(title, query) {
        return true;
    }

    if let Some(s) = series
        && contains_ignore_case(s, query)
    {
        return true;
    }

    if let Some(p) = path.to_str() {
        contains_ignore_case(p, query)
    } else {
        contains_ignore_case(&path.to_string_lossy(), query)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_empty_or_whitespace_query() {
        let path = Path::new("/comics/naruto.cbz");
        assert!(matches("Naruto Vol. 1", path, None, ""));
        assert!(matches("Naruto Vol. 1", path, None, "   "));
    }

    #[test]
    fn matches_case_insensitively_on_title() {
        let path = Path::new("/comics/naruto.cbz");
        assert!(matches("Naruto Vol. 1", path, None, "naruto"));
        assert!(matches("Naruto Vol. 1", path, None, "VOL"));
        assert!(matches("Naruto Vol. 1", path, None, "1"));
        assert!(!matches("Naruto Vol. 1", path, None, "One Piece"));
    }

    #[test]
    fn matches_on_path_and_series() {
        let path = Path::new("/media/Manga/Bleach/ch01.cbz");
        assert!(matches("Chapter 1", path, Some("Bleach"), "bleach"));
        assert!(matches("Chapter 1", path, None, "manga"));
        assert!(matches("Chapter 1", path, None, "ch01"));
    }
}
