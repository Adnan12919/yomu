//! Case-insensitive library search helpers, independent of gpui.

use std::path::Path;

/// Match a query against a book's display title, source path, or assigned series.
///
/// An empty or whitespace-only query matches every book. Matching is substring based and
/// case-insensitive; the stored title and path are never modified.
pub fn matches(title: &str, path: &Path, series: Option<&str>, query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }
    let query = query.to_lowercase();
    title.to_lowercase().contains(&query)
        || path.to_string_lossy().to_lowercase().contains(&query)
        || series.is_some_and(|name| name.to_lowercase().contains(&query))
}
