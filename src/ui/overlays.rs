//! Overlay definitions: menus, dialogs, settings tabs, and modal states.

use gpui_kit::component::input::InputState;
use gpui_kit::{Entity, Pixels, Point, SharedString};
use yomu::menu::{BookAction, Entry};

#[derive(Clone)]
pub struct BookMenu {
    pub book_id: String,
    pub entries: Vec<Entry>,
    pub position: Point<Pixels>,
    pub selected: Option<usize>,
}

pub enum ConfirmAction {
    Book(BookAction, String),
    RemoveSelected,
    ResetSettings,
    ClearThumbnailCache,
    ClearAllData,
}

pub struct ConfirmDialog {
    pub title: SharedString,
    pub message: SharedString,
    pub confirm_label: &'static str,
    pub action: ConfirmAction,
}

pub struct PropertiesDialog {
    pub title: SharedString,
    pub rows: Vec<(String, String)>,
}

pub struct SeriesDialog {
    pub book_ids: Vec<String>,
    pub input: Entity<InputState>,
}

#[derive(Default)]
pub struct LibraryMenu {
    pub selected: usize,
}

/// At most one menu or dialog is open at a time; it sits on top of whatever view is showing.
pub enum Overlay {
    Menu(BookMenu),
    LibraryMenu(LibraryMenu),
    Confirm(ConfirmDialog),
    Properties(PropertiesDialog),
    Series(SeriesDialog),
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsTab {
    #[default]
    Appearance,
    Library,
    Reader,
    Storage,
    Keyboard,
}

impl SettingsTab {
    pub fn from_index(idx: usize) -> Self {
        match idx {
            0 => Self::Appearance,
            1 => Self::Library,
            2 => Self::Reader,
            3 => Self::Storage,
            4 => Self::Keyboard,
            _ => Self::Appearance,
        }
    }

    pub fn to_index(self) -> usize {
        match self {
            Self::Appearance => 0,
            Self::Library => 1,
            Self::Reader => 2,
            Self::Storage => 3,
            Self::Keyboard => 4,
        }
    }
}
