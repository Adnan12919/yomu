//! What the library's right-click menu offers for a book. Pure data and logic (no gpui), so the
//! rules about which actions make sense are unit tested.

use std::fs;

use crate::{
    archive::SourceKind,
    library::{Book, ReadStatus},
    util::{format_date, format_size},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BookAction {
    /// Start reading, or resume where the reader left off.
    Open,
    StartOver,
    MarkRead,
    MarkUnread,
    ResetProgress,
    RevealFile,
    Properties,
    /// Remove from the library. The comic file itself is never touched.
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub action: BookAction,
    pub label: String,
    pub enabled: bool,
    /// Drawn in the accent colour: the action throws something away.
    pub danger: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Item(Item),
    Separator,
}

fn item(action: BookAction, label: &str, enabled: bool) -> Entry {
    Entry::Item(Item { action, label: label.to_string(), enabled, danger: false })
}

/// The entries for `book`. Only actions that make sense for its reading status are included, so
/// nothing is redundant: an unread book has no "Reset progress", a finished one no "Mark as read".
/// Actions that need the file on disk are disabled when it is missing.
pub fn book_menu(book: &Book, file_exists: bool) -> Vec<Entry> {
    let status = book.status();
    let mut entries = Vec::new();

    match status {
        ReadStatus::Unread => entries.push(item(BookAction::Open, "Start Reading", file_exists)),
        ReadStatus::InProgress => {
            entries.push(item(BookAction::Open, "Resume Reading", file_exists));
            entries.push(item(BookAction::StartOver, "Start from Beginning", file_exists));
        }
        ReadStatus::Finished => {
            entries.push(item(BookAction::StartOver, "Start from Beginning", file_exists));
        }
    }
    entries.push(Entry::Separator);

    match status {
        ReadStatus::Unread => entries.push(item(BookAction::MarkRead, "Mark as Read", true)),
        ReadStatus::InProgress => {
            entries.push(item(BookAction::MarkRead, "Mark as Read", true));
            entries.push(item(BookAction::ResetProgress, "Reset Progress…", true));
        }
        ReadStatus::Finished => {
            entries.push(item(BookAction::MarkUnread, "Mark as Unread", true));
        }
    }
    entries.push(Entry::Separator);

    entries.push(item(BookAction::RevealFile, "Open File Location", file_exists));
    entries.push(item(BookAction::Properties, "Properties", true));
    entries.push(Entry::Separator);

    entries.push(Entry::Item(Item {
        action: BookAction::Remove,
        label: "Remove from Library".to_string(),
        enabled: true,
        danger: true,
    }));
    entries
}

/// Move the highlight to the next (or previous) enabled item, wrapping around. With no current
/// selection, going forward picks the first item and going back picks the last.
pub fn step(entries: &[Entry], current: Option<usize>, forward: bool) -> Option<usize> {
    let count = entries.len() as isize;
    if count == 0 {
        return None;
    }
    let start = match (current, forward) {
        (Some(index), _) => index as isize,
        (None, true) => -1,
        (None, false) => count,
    };
    (1..=count)
        .map(|offset| {
            let delta = if forward { offset } else { -offset };
            (start + delta).rem_euclid(count) as usize
        })
        .find(|index| matches!(&entries[*index], Entry::Item(item) if item.enabled))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirmation {
    pub title: String,
    pub message: String,
    pub confirm_label: &'static str,
}

/// Whether `action` needs an "are you sure?" first: only when it would throw away progress or
/// marks that are hard to get back.
pub fn confirmation(action: BookAction, book: &Book) -> Option<Confirmation> {
    match action {
        BookAction::ResetProgress => Some(Confirmation {
            title: "Reset progress?".to_string(),
            message: format!(
                "“{}” goes back to unread and you lose your place. Marks are kept.",
                book.title
            ),
            confirm_label: "Reset",
        }),
        BookAction::Remove if book.has_progress() || !book.marks.is_empty() => Some(Confirmation {
            title: "Remove from library?".to_string(),
            message: format!(
                "Your progress and marks for “{}” will be lost. The file itself is not deleted.",
                book.title
            ),
            confirm_label: "Remove",
        }),
        _ => None,
    }
}

/// Label and value rows for the Properties dialog. Only information that is actually known.
pub fn properties(book: &Book) -> Vec<(String, String)> {
    let exists = book.path.exists();
    let mut rows = Vec::new();

    let location = book.path.display().to_string();
    rows.push((
        "Location".to_string(),
        if exists { location } else { format!("{location} (missing)") },
    ));

    let format = match book.kind {
        SourceKind::Folder => "Image folder",
        SourceKind::Archive => {
            let lower = book.path.to_string_lossy().to_ascii_lowercase();
            if lower.ends_with(".zip") { "ZIP archive" } else { "CBZ archive" }
        }
    };
    rows.push(("Format".to_string(), format.to_string()));
    rows.push(("Pages".to_string(), book.page_count.to_string()));

    let progress = match book.status() {
        ReadStatus::Unread => "Unread".to_string(),
        ReadStatus::Finished => "Finished".to_string(),
        ReadStatus::InProgress => format!(
            "Page {} of {} ({}%)",
            book.current_page + 1,
            book.page_count,
            (book.progress() * 100.0).round() as u32
        ),
    };
    rows.push(("Progress".to_string(), progress));

    if book.kind == SourceKind::Archive && exists {
        if let Ok(meta) = fs::metadata(&book.path) {
            rows.push(("Size".to_string(), format_size(meta.len())));
        }
    }

    if !book.marks.is_empty() {
        let marks = book
            .marks
            .iter()
            .map(|(key, page)| format!("{key}: p. {}", page + 1))
            .collect::<Vec<_>>()
            .join("   ");
        rows.push(("Marks".to_string(), marks));
    }

    rows.push(("Added".to_string(), format_date(book.added_at)));
    if let Some(last_read) = book.last_read {
        rows.push(("Last read".to_string(), format_date(last_read)));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeMap, path::PathBuf};

    fn book(status: ReadStatus) -> Book {
        let mut book = Book {
            id: "id".into(),
            title: "Blue Period".into(),
            path: PathBuf::from("/definitely/not/here.cbz"),
            kind: SourceKind::Archive,
            page_count: 20,
            current_page: 0,
            opened: false,
            added_at: 1_700_000_000,
            last_read: None,
            marks: BTreeMap::new(),
        };
        match status {
            ReadStatus::Unread => {}
            ReadStatus::InProgress => {
                book.opened = true;
                book.current_page = 7;
                book.last_read = Some(1_700_100_000);
            }
            ReadStatus::Finished => {
                book.opened = true;
                book.current_page = 19;
            }
        }
        book
    }

    fn actions(entries: &[Entry]) -> Vec<BookAction> {
        entries
            .iter()
            .filter_map(|e| match e {
                Entry::Item(item) => Some(item.action),
                Entry::Separator => None,
            })
            .collect()
    }

    #[test]
    fn menu_matches_reading_status() {
        use BookAction::*;
        assert_eq!(
            actions(&book_menu(&book(ReadStatus::Unread), true)),
            vec![Open, MarkRead, RevealFile, Properties, Remove]
        );
        assert_eq!(
            actions(&book_menu(&book(ReadStatus::InProgress), true)),
            vec![Open, StartOver, MarkRead, ResetProgress, RevealFile, Properties, Remove]
        );
        assert_eq!(
            actions(&book_menu(&book(ReadStatus::Finished), true)),
            vec![StartOver, MarkUnread, RevealFile, Properties, Remove]
        );
    }

    #[test]
    fn labels_follow_status() {
        let label = |status| match &book_menu(&book(status), true)[0] {
            Entry::Item(item) => item.label.clone(),
            Entry::Separator => unreachable!(),
        };
        assert_eq!(label(ReadStatus::Unread), "Start Reading");
        assert_eq!(label(ReadStatus::InProgress), "Resume Reading");
        assert_eq!(label(ReadStatus::Finished), "Start from Beginning");
    }

    #[test]
    fn separators_never_lead_trail_or_double_up() {
        for status in [ReadStatus::Unread, ReadStatus::InProgress, ReadStatus::Finished] {
            let entries = book_menu(&book(status), true);
            assert!(matches!(entries.first(), Some(Entry::Item(_))));
            assert!(matches!(entries.last(), Some(Entry::Item(_))));
            assert!(entries
                .windows(2)
                .all(|pair| !(pair[0] == Entry::Separator && pair[1] == Entry::Separator)));
        }
    }

    #[test]
    fn missing_file_disables_only_what_needs_it() {
        let entries = book_menu(&book(ReadStatus::InProgress), false);
        for entry in &entries {
            if let Entry::Item(item) = entry {
                let needs_file = matches!(
                    item.action,
                    BookAction::Open | BookAction::StartOver | BookAction::RevealFile
                );
                assert_eq!(item.enabled, !needs_file, "{:?}", item.action);
            }
        }
    }

    #[test]
    fn only_remove_is_marked_dangerous() {
        for entry in book_menu(&book(ReadStatus::InProgress), true) {
            if let Entry::Item(item) = entry {
                assert_eq!(item.danger, item.action == BookAction::Remove);
            }
        }
    }

    #[test]
    fn keyboard_navigation_skips_separators_and_disabled_items_and_wraps() {
        let entries = book_menu(&book(ReadStatus::Unread), false);
        // Start Reading (0, disabled), sep, Mark as Read (2), sep, Open File Location (4,
        // disabled), Properties (5), sep, Remove (7)
        assert_eq!(step(&entries, None, true), Some(2));
        assert_eq!(step(&entries, Some(2), true), Some(5));
        assert_eq!(step(&entries, Some(5), true), Some(7));
        assert_eq!(step(&entries, Some(7), true), Some(2), "wraps to the first enabled item");
        assert_eq!(step(&entries, None, false), Some(7));
        assert_eq!(step(&entries, Some(2), false), Some(7));
        assert_eq!(step(&entries, Some(5), false), Some(2));
        assert_eq!(step(&[], None, true), None);
    }

    #[test]
    fn confirmations_only_when_something_is_lost() {
        let unread = book(ReadStatus::Unread);
        let reading = book(ReadStatus::InProgress);

        assert!(confirmation(BookAction::ResetProgress, &reading).is_some());
        assert!(confirmation(BookAction::Remove, &unread).is_none());
        assert!(confirmation(BookAction::Remove, &reading).is_some());

        let mut marked = unread.clone();
        marked.marks.insert("a".into(), 2);
        assert!(confirmation(BookAction::Remove, &marked).is_some());

        for action in [
            BookAction::Open,
            BookAction::StartOver,
            BookAction::MarkRead,
            BookAction::MarkUnread,
            BookAction::RevealFile,
            BookAction::Properties,
        ] {
            assert!(confirmation(action, &reading).is_none(), "{action:?}");
        }
        let message = confirmation(BookAction::Remove, &reading).unwrap().message;
        assert!(message.contains("not deleted"));
    }

    #[test]
    fn properties_show_only_known_information() {
        let rows = properties(&book(ReadStatus::Unread));
        let get = |name: &str| rows.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
        assert!(get("Location").unwrap().ends_with("(missing)"));
        assert_eq!(get("Format").as_deref(), Some("CBZ archive"));
        assert_eq!(get("Pages").as_deref(), Some("20"));
        assert_eq!(get("Progress").as_deref(), Some("Unread"));
        assert_eq!(get("Added").as_deref(), Some("2023-11-14"));
        assert!(get("Size").is_none(), "no size for a missing file");
        assert!(get("Last read").is_none());
        assert!(get("Marks").is_none());

        let mut reading = book(ReadStatus::InProgress);
        reading.marks.insert("a".into(), 2);
        let rows = properties(&reading);
        let get = |name: &str| rows.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
        assert_eq!(get("Progress").as_deref(), Some("Page 8 of 20 (40%)"));
        assert_eq!(get("Marks").as_deref(), Some("a: p. 3"));
        assert!(get("Last read").is_some());
    }
}
