//! yomu: a minimal, GPU-accelerated, Wayland-native manga reader.

use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use gpui::{
    AnyElement, App, Application, Bounds, ClickEvent, Context, Corner, ExternalPaths,
    FocusHandle, FontWeight, KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ObjectFit, PathPromptOptions, Pixels, Point, RenderImage, Rgba,
    ScrollDelta, ScrollWheelEvent, SharedString, TitlebarOptions, Window, WindowBackgroundAppearance,
    WindowBounds, WindowOptions, actions, anchored, canvas, deferred, div, img, prelude::*, px,
    relative, rgb, rgba, size,
};
use image::Frame;
use yomu::{
    archive::{self, SourceKind},
    imaging,
    layout::{self, Arrive, FitMode, Viewport},
    library::{self, Book, DataDirs, Library, OPACITY_MAX, OPACITY_MIN, Settings},
    menu::{self, BookAction, Entry, Item},
    util::now_secs,
    vim::{self, Action, Amount, Dir, Vim},
};

actions!(yomu, [ToggleFullscreen, ImportFiles, ImportFolder, OpenSettings, Quit]);

// Palette
const BG: u32 = 0x0f0f11;
const READER_BG: u32 = 0x060607;
const SURFACE: u32 = 0x1a1a1e;
const SURFACE_HI: u32 = 0x26262c;
const TRACK: u32 = 0x2c2c33;
const TEXT: u32 = 0xe9e7e4;
const MUTED: u32 = 0x8b8b93;
const ACCENT: u32 = 0xe2725b;

const CARD_W: f32 = 180.0;
const CARD_H: f32 = 270.0;

/// How far `j`/`k`/`h`/`l` scroll, in logical pixels.
const LINE_STEP: f32 = 64.0;
/// A wheel notch is reported as this many pixels.
const WHEEL_LINE: f32 = 48.0;
/// Slack when deciding whether the page is already scrolled to its edge.
const EDGE_EPS: f32 = 1.0;
/// Mouse travel before a press counts as a drag instead of a click.
const DRAG_THRESHOLD: f32 = 4.0;
const HUD_SECS: u64 = 2;

const MENU_W: f32 = 224.0;
const SLIDER_W: f32 = 280.0;
const KNOB: f32 = 16.0;
/// One arrow-key press moves the opacity slider this much.
const OPACITY_STEP: f32 = 0.05;
/// Clicks this soon after a menu or dialog closes are ignored, so double-clicking a menu item
/// cannot also click the book underneath it.
const CLICK_GUARD: Duration = Duration::from_millis(300);

#[derive(Clone, Copy)]
enum Side {
    Left,
    Right,
}

fn viewport_of(window: &Window) -> Viewport {
    let size = window.viewport_size();
    Viewport {
        width: f32::from(size.width),
        height: f32::from(size.height),
        scale: window.scale_factor(),
    }
}

/// A decoded page, drawn at a size we compute ourselves (see `layout`).
struct Page {
    image: Arc<RenderImage>,
    /// Dimensions of the original image, which is what fit modes are computed from.
    orig: (u32, u32),
    /// Decode tier (longest side in pixels) this image was made at.
    tier: u32,
}

#[derive(Clone, Copy)]
struct Drag {
    start: (f32, f32),
    off0: (f32, f32),
    moved: bool,
}

/// State of the book that is currently open.
struct Reader {
    book_id: String,
    path: PathBuf,
    kind: SourceKind,
    /// `None` while the page list is still being read from disk.
    pages: Option<Arc<Vec<String>>>,
    page: usize,
    total: usize,

    /// Right-to-left reading order (the manga default).
    rtl: bool,
    fit: FitMode,
    /// Multiplier on top of the fit size; `1.0` is exactly the fit.
    zoom: f32,
    /// How far the page is scrolled from its top-left corner.
    off: (f32, f32),
    /// Scroll position to apply once the new page's size is known.
    arrive: Option<Arrive>,
    viewport: Viewport,

    images: HashMap<usize, Page>,
    pending: HashSet<usize>,
    failed: HashMap<usize, String>,
    error: Option<String>,

    vim: Vim,
    drag: Option<Drag>,
    /// Where `''` jumps back to.
    back_page: Option<usize>,
    show_info: bool,
    show_help: bool,
}

impl Reader {
    fn new(book: &Book, settings: &Settings) -> Self {
        Self {
            book_id: book.id.clone(),
            path: book.path.clone(),
            kind: book.kind,
            pages: None,
            page: book.current_page.min(book.page_count.saturating_sub(1)),
            total: book.page_count,
            rtl: settings.rtl,
            fit: settings.fit,
            zoom: 1.0,
            off: (0.0, 0.0),
            arrive: Some(Arrive::Start),
            viewport: Viewport::EMPTY,
            images: HashMap::new(),
            pending: HashSet::new(),
            failed: HashMap::new(),
            error: None,
            vim: Vim::new(),
            drag: None,
            back_page: None,
            show_info: false,
            show_help: false,
        }
    }

    fn current(&self) -> Option<&Page> {
        self.images.get(&self.page)
    }

    fn orig_of(page: &Page) -> (f32, f32) {
        (page.orig.0 as f32, page.orig.1 as f32)
    }

    /// Apply a pending page-turn arrival and clamp the scroll offset. Returns the display size
    /// of the current page, or `None` while it is still being decoded.
    fn settle(&mut self, vp: &Viewport) -> Option<(f32, f32)> {
        let orig = Self::orig_of(self.current()?);
        let display = layout::display_size(self.fit, self.zoom, orig, vp);
        let max = layout::max_offset(display, vp);
        if let Some(arrive) = self.arrive.take() {
            self.off = layout::arrival_offset(arrive, self.rtl, max);
        }
        self.off = layout::clamp_offset(self.off, display, vp);
        Some(display)
    }
}

struct BookMenu {
    book_id: String,
    entries: Vec<Entry>,
    position: Point<Pixels>,
    selected: Option<usize>,
}

enum ConfirmAction {
    Book(BookAction, String),
    ResetSettings,
}

struct ConfirmDialog {
    title: SharedString,
    message: SharedString,
    confirm_label: &'static str,
    action: ConfirmAction,
}

struct PropertiesDialog {
    title: SharedString,
    rows: Vec<(String, String)>,
}

/// At most one menu or dialog is open at a time; it sits on top of whatever view is showing.
enum Overlay {
    Menu(BookMenu),
    Confirm(ConfirmDialog),
    Properties(PropertiesDialog),
    Settings,
}

struct Slider {
    /// Where the track was last painted, so mouse positions can be turned into a value.
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    dragging: bool,
}

fn with_alpha(color: u32, alpha: f32) -> Rgba {
    let mut color = rgb(color);
    color.a = alpha;
    color
}

struct Yomu {
    focus_handle: FocusHandle,
    dirs: Arc<DataDirs>,
    library: Library,
    reader: Option<Reader>,
    /// Images that are no longer shown. Their GPU textures are freed on the next render,
    /// because that is where we have access to the `Window`.
    garbage: Vec<Arc<RenderImage>>,
    toast: Option<SharedString>,
    toast_seq: u64,
    hud: Option<SharedString>,
    hud_seq: u64,
    importing: bool,
    overlay: Option<Overlay>,
    overlay_closed_at: Option<Instant>,
    slider: Slider,
    /// Which background appearance the window currently has (`None` before the first frame).
    applied_opaque: Option<bool>,
}

impl Yomu {
    fn new(cx: &mut Context<Self>) -> Self {
        let dirs = DataDirs::new().expect("cannot create the yomu data directory");
        Self::with_dirs(dirs, cx)
    }

    fn with_dirs(dirs: DataDirs, cx: &mut Context<Self>) -> Self {
        let dirs = Arc::new(dirs);
        let (library, toast) = match Library::load(&dirs) {
            Ok(library) => (library, None),
            Err(err) => (Library::default(), Some(SharedString::from(format!("{err:#}")))),
        };
        Self {
            focus_handle: cx.focus_handle(),
            dirs,
            library,
            reader: None,
            garbage: Vec::new(),
            toast,
            toast_seq: 0,
            hud: None,
            hud_seq: 0,
            importing: false,
            overlay: None,
            overlay_closed_at: None,
            slider: Slider { bounds: Rc::new(Cell::new(None)), dragging: false },
            applied_opaque: None,
        }
    }

    // ------------------------------------------------------------------------------------
    // Housekeeping
    // ------------------------------------------------------------------------------------

    fn save_library(&mut self) {
        if let Err(err) = self.library.save(&self.dirs) {
            eprintln!("yomu: failed to save library: {err:#}");
        }
    }

    fn show_toast(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.toast_seq += 1;
        let seq = self.toast_seq;
        self.toast = Some(message.into());
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(5)).await;
            this.update(cx, |this, cx| {
                if this.toast_seq == seq {
                    this.toast = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// A short message at the top of the reader (zoom level, fit mode, errors).
    fn show_hud(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.hud_seq += 1;
        let seq = self.hud_seq;
        self.hud = Some(message.into());
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(HUD_SECS)).await;
            this.update(cx, |this, cx| {
                if this.hud_seq == seq {
                    this.hud = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    // ------------------------------------------------------------------------------------
    // Menus and dialogs
    // ------------------------------------------------------------------------------------

    /// Close the current overlay without touching anything else.
    fn dismiss(&mut self) {
        if self.overlay.take().is_some() {
            self.overlay_closed_at = Some(Instant::now());
        }
        self.slider.dragging = false;
    }

    /// Escape / click outside. Cancelling the "reset settings?" question goes back to Settings.
    fn close_overlay(&mut self, cx: &mut Context<Self>) {
        let back_to_settings = matches!(
            &self.overlay,
            Some(Overlay::Confirm(dialog)) if matches!(dialog.action, ConfirmAction::ResetSettings)
        );
        let was_dragging = self.slider.dragging;
        self.dismiss();
        if was_dragging {
            self.save_library();
        }
        if back_to_settings {
            self.overlay = Some(Overlay::Settings);
        }
        cx.notify();
    }

    fn overlay_recently_closed(&self) -> bool {
        self.overlay_closed_at
            .is_some_and(|closed| closed.elapsed() < CLICK_GUARD)
    }

    fn open_book_menu(&mut self, id: &str, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(book) = self.library.get(id) else {
            return;
        };
        let entries = menu::book_menu(book, book.path.exists());
        self.overlay = Some(Overlay::Menu(BookMenu {
            book_id: id.to_string(),
            entries,
            position,
            selected: None,
        }));
        cx.notify();
    }

    fn menu_hover(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(Overlay::Menu(menu)) = self.overlay.as_mut() {
            if menu.selected != Some(ix) {
                menu.selected = Some(ix);
                cx.notify();
            }
        }
    }

    /// Run a menu action, asking first if it would throw away progress.
    fn run_book_action(&mut self, action: BookAction, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(book) = self.library.get(id).cloned() else {
            self.dismiss();
            cx.notify();
            return;
        };
        if let Some(confirmation) = menu::confirmation(action, &book) {
            self.overlay = Some(Overlay::Confirm(ConfirmDialog {
                title: confirmation.title.into(),
                message: confirmation.message.into(),
                confirm_label: confirmation.confirm_label,
                action: ConfirmAction::Book(action, id.to_string()),
            }));
            cx.notify();
            return;
        }
        self.perform_book_action(action, id, window, cx);
    }

    fn perform_book_action(&mut self, action: BookAction, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.dismiss();
        let Some(title) = self.library.get(id).map(|book| book.title.clone()) else {
            cx.notify();
            return;
        };
        match action {
            BookAction::Open => self.open_book(id, window, cx),
            BookAction::StartOver => {
                if let Some(book) = self.library.get_mut(id) {
                    book.start_over();
                }
                self.save_library();
                self.open_book(id, window, cx);
            }
            BookAction::MarkRead => {
                if let Some(book) = self.library.get_mut(id) {
                    book.mark_read();
                }
                self.save_library();
                self.show_toast(format!("Marked “{title}” as read"), cx);
            }
            BookAction::MarkUnread | BookAction::ResetProgress => {
                if let Some(book) = self.library.get_mut(id) {
                    book.reset_progress();
                }
                self.save_library();
                let message = if action == BookAction::ResetProgress {
                    format!("Progress reset for “{title}”")
                } else {
                    format!("Marked “{title}” as unread")
                };
                self.show_toast(message, cx);
            }
            BookAction::RevealFile => match self.library.get(id).map(|book| book.path.clone()) {
                Some(path) if path.exists() => cx.reveal_path(&path),
                _ => self.show_toast(format!("“{title}” is missing from disk"), cx),
            },
            BookAction::Properties => {
                if let Some(book) = self.library.get(id) {
                    self.overlay = Some(Overlay::Properties(PropertiesDialog {
                        title: book.title.clone().into(),
                        rows: menu::properties(book),
                    }));
                }
            }
            BookAction::Remove => self.remove_book(id, cx),
        }
        cx.notify();
    }

    fn confirm_accept(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Overlay::Confirm(dialog)) = self.overlay.take() else {
            return;
        };
        self.overlay_closed_at = Some(Instant::now());
        match dialog.action {
            ConfirmAction::Book(action, id) => self.perform_book_action(action, &id, window, cx),
            ConfirmAction::ResetSettings => self.reset_settings(cx),
        }
    }

    // ------------------------------------------------------------------------------------
    // Settings
    // ------------------------------------------------------------------------------------

    fn open_settings(&mut self, cx: &mut Context<Self>) {
        match self.overlay {
            None => self.overlay = Some(Overlay::Settings),
            Some(Overlay::Settings) => self.close_overlay(cx),
            Some(_) => {}
        }
        cx.notify();
    }

    fn request_reset_settings(&mut self, cx: &mut Context<Self>) {
        if *self.library.settings() == Settings::default() {
            return;
        }
        self.overlay = Some(Overlay::Confirm(ConfirmDialog {
            title: "Reset settings?".into(),
            message: "Background opacity, reading direction and page fit go back to their defaults."
                .into(),
            confirm_label: "Reset",
            action: ConfirmAction::ResetSettings,
        }));
        cx.notify();
    }

    fn reset_settings(&mut self, cx: &mut Context<Self>) {
        *self.library.settings_mut() = Settings::default();
        let defaults = self.library.settings().clone();
        if let Some(reader) = self.reader.as_mut() {
            reader.rtl = defaults.rtl;
            reader.fit = defaults.fit;
            reader.zoom = 1.0;
            reader.arrive = Some(Arrive::Start);
        }
        self.save_library();
        self.slider.dragging = false;
        self.overlay = Some(Overlay::Settings);
        cx.notify();
    }

    fn set_opacity(&mut self, value: f32, persist: bool, cx: &mut Context<Self>) {
        self.library.settings_mut().set_opacity(value);
        if persist {
            self.save_library();
        }
        cx.notify();
    }

    /// Turn a mouse x position into an opacity, accounting for the knob's width.
    fn slider_set_from_x(&mut self, x: f32, cx: &mut Context<Self>) {
        let Some(bounds) = self.slider.bounds.get() else {
            return;
        };
        let left = f32::from(bounds.origin.x) + KNOB / 2.0;
        let span = (f32::from(bounds.size.width) - KNOB).max(1.0);
        let fraction = ((x - left) / span).clamp(0.0, 1.0);
        self.set_opacity(OPACITY_MIN + fraction * (OPACITY_MAX - OPACITY_MIN), false, cx);
    }

    /// Save once when a drag ends instead of on every mouse move.
    fn end_slider_drag(&mut self, cx: &mut Context<Self>) {
        if self.slider.dragging {
            self.slider.dragging = false;
            self.save_library();
            cx.notify();
        }
    }

    /// Fully opaque windows use the opaque appearance, which lets the compositor skip blending;
    /// anything lower needs real alpha transparency.
    fn sync_appearance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let opaque = self.library.settings().opacity >= OPACITY_MAX - 0.001;
        if self.applied_opaque == Some(opaque) {
            return;
        }
        self.applied_opaque = Some(opaque);
        let appearance = if opaque {
            WindowBackgroundAppearance::Opaque
        } else {
            WindowBackgroundAppearance::Transparent
        };
        // Reconfiguring the surface mid-draw is not safe, so do it right after this frame.
        window.defer(cx, move |window, _| window.set_background_appearance(appearance));
    }

    fn on_overlay_key(&mut self, token: &str, window: &mut Window, cx: &mut Context<Self>) {
        match self.overlay.as_mut() {
            Some(Overlay::Menu(menu)) => match token {
                "escape" => self.close_overlay(cx),
                "down" | "j" | "tab" | "C-n" => {
                    menu.selected = menu::step(&menu.entries, menu.selected, true);
                    cx.notify();
                }
                "up" | "k" | "S-tab" | "C-p" => {
                    menu.selected = menu::step(&menu.entries, menu.selected, false);
                    cx.notify();
                }
                "home" => {
                    menu.selected = menu::step(&menu.entries, None, true);
                    cx.notify();
                }
                "end" => {
                    menu.selected = menu::step(&menu.entries, None, false);
                    cx.notify();
                }
                "enter" | "space" => {
                    let picked = menu.selected.and_then(|ix| match &menu.entries[ix] {
                        Entry::Item(item) if item.enabled => Some(item.action),
                        _ => None,
                    });
                    let id = menu.book_id.clone();
                    if let Some(action) = picked {
                        self.run_book_action(action, &id, window, cx);
                    }
                }
                _ => {}
            },
            Some(Overlay::Confirm(_)) => match token {
                "enter" | "y" => self.confirm_accept(window, cx),
                "escape" | "n" | "q" => self.close_overlay(cx),
                _ => {}
            },
            Some(Overlay::Properties(_)) => {
                if matches!(token, "escape" | "enter" | "space" | "q") {
                    self.close_overlay(cx);
                }
            }
            Some(Overlay::Settings) => {
                let current = self.library.settings().opacity;
                match token {
                    "escape" | "enter" | "q" => self.close_overlay(cx),
                    "left" | "h" | "down" | "j" => self.set_opacity(current - OPACITY_STEP, true, cx),
                    "right" | "l" | "up" | "k" => self.set_opacity(current + OPACITY_STEP, true, cx),
                    "home" => self.set_opacity(OPACITY_MIN, true, cx),
                    "end" => self.set_opacity(OPACITY_MAX, true, cx),
                    _ => {}
                }
            }
            None => {}
        }
    }

    // ------------------------------------------------------------------------------------
    // Import
    // ------------------------------------------------------------------------------------

    fn import_dialog(&mut self, directories: bool, cx: &mut Context<Self>) {
        if self.importing || self.overlay.is_some() {
            return;
        }
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: !directories,
            directories,
            multiple: true,
            prompt: Some("Import".into()),
        });
        cx.spawn(async move |this, cx| match rx.await {
            Ok(Ok(Some(paths))) => {
                this.update(cx, |this, cx| this.import_paths(paths, cx)).ok();
            }
            Ok(Err(err)) => {
                let message = format!("{err:#}");
                this.update(cx, |this, cx| this.show_toast(message, cx)).ok();
            }
            _ => {}
        })
        .detach();
    }

    fn import_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if self.importing || paths.is_empty() {
            return;
        }
        self.importing = true;
        cx.notify();

        let dirs = self.dirs.clone();
        let existing = self.library.ids();
        let task = cx
            .background_executor()
            .spawn(async move { library::import_paths(&paths, &dirs, &existing) });

        cx.spawn(async move |this, cx| {
            let report = task.await;
            this.update(cx, |this, cx| {
                let summary = report.summary();
                for book in report.added {
                    this.library.add(book);
                }
                this.save_library();
                this.importing = false;
                this.show_toast(summary, cx);
            })
            .ok();
        })
        .detach();
    }

    fn remove_book(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(title) = self.library.get(id).map(|b| b.title.clone()) else {
            return;
        };
        self.library.remove(id, &self.dirs);
        self.save_library();
        self.show_toast(format!("Removed “{title}” from the library"), cx);
    }

    // ------------------------------------------------------------------------------------
    // Opening and closing books
    // ------------------------------------------------------------------------------------

    fn open_book(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(book) = self.library.get(id).cloned() else {
            return;
        };
        if !book.path.exists() {
            self.show_toast(format!("“{}” is missing from disk", book.title), cx);
            return;
        }

        window.set_window_title(&book.title);
        window.focus(&self.focus_handle);

        let settings = self.library.settings().clone();
        self.reader = Some(Reader::new(&book, &settings));

        if let Some(entry) = self.library.get_mut(id) {
            entry.opened = true;
            entry.last_read = Some(now_secs());
        }
        self.save_library();

        // Listing a big archive's pages touches the disk, so keep it off the UI thread.
        let (path, kind, book_id) = (book.path, book.kind, book.id);
        let task = cx
            .background_executor()
            .spawn(async move { archive::list_pages(&path, kind) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| this.pages_listed(book_id, result, cx))
                .ok();
        })
        .detach();

        cx.notify();
    }

    fn pages_listed(&mut self, book_id: String, result: Result<Vec<String>>, cx: &mut Context<Self>) {
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        if reader.book_id != book_id {
            return;
        }
        match result {
            Ok(pages) if pages.is_empty() => reader.error = Some("no pages found".into()),
            Ok(pages) => {
                reader.total = pages.len();
                reader.page = reader.page.min(pages.len() - 1);
                let (page, total) = (reader.page, pages.len());
                reader.pages = Some(Arc::new(pages));
                if let Some(book) = self.library.get_mut(&book_id) {
                    book.page_count = total;
                    book.current_page = page;
                }
                self.save_library();
            }
            Err(err) => reader.error = Some(format!("{err:#}")),
        }
        cx.notify();
    }

    fn close_book(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(reader) = self.reader.take() {
            self.garbage
                .extend(reader.images.into_values().map(|page| page.image));
        }
        self.hud = None;
        window.set_window_title("yomu");
        window.focus(&self.focus_handle);
        self.save_library();
        cx.notify();
    }

    // ------------------------------------------------------------------------------------
    // Page turning
    // ------------------------------------------------------------------------------------

    fn goto_page(&mut self, target: usize, arrive: Arrive, cx: &mut Context<Self>) {
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        if reader.pages.is_none() || reader.total == 0 {
            return;
        }
        let target = target.min(reader.total - 1);
        if target == reader.page {
            return;
        }
        reader.page = target;
        reader.arrive = Some(arrive);
        reader.drag = None;
        let book_id = reader.book_id.clone();
        if let Some(book) = self.library.get_mut(&book_id) {
            book.current_page = target;
            book.opened = true;
            book.last_read = Some(now_secs());
        }
        self.save_library();
        cx.notify();
    }

    /// A jump that `''` can undo (`gg`, `G`, `12G`, `:12`, marks).
    fn jump_to(&mut self, target: usize, cx: &mut Context<Self>) {
        if let Some(reader) = self.reader.as_mut() {
            let clamped = target.min(reader.total.saturating_sub(1));
            if clamped != reader.page {
                reader.back_page = Some(reader.page);
            }
        }
        self.goto_page(target, Arrive::Start, cx);
    }

    fn turn_by(&mut self, forward: bool, count: usize, arrive: Arrive, cx: &mut Context<Self>) {
        let Some(page) = self.reader.as_ref().map(|r| r.page) else {
            return;
        };
        let target = if forward {
            page.saturating_add(count)
        } else {
            page.saturating_sub(count)
        };
        self.goto_page(target, arrive, cx);
    }

    /// Screen-edge clicks are about screen direction, so which one advances depends on the
    /// reading direction.
    fn step(&mut self, side: Side, cx: &mut Context<Self>) {
        let Some(rtl) = self.reader.as_ref().map(|r| r.rtl) else {
            return;
        };
        let forward = matches!((side, rtl), (Side::Left, true) | (Side::Right, false));
        let arrive = if forward { Arrive::Start } else { Arrive::End };
        self.turn_by(forward, 1, arrive, cx);
    }

    // ------------------------------------------------------------------------------------
    // Scrolling, zooming, fit modes
    // ------------------------------------------------------------------------------------

    /// `j`/`k`/`h`/`l`/space/etc. Scrolls the page; when the page cannot scroll any further in
    /// that direction (or does not overflow at all) it turns the page instead.
    fn scroll(&mut self, dir: Dir, amount: Amount, count: usize, vp: Viewport, cx: &mut Context<Self>) {
        let count = count.max(1);
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        let Some(display) = reader.settle(&vp) else {
            return;
        };
        let max = layout::max_offset(display, &vp);
        let vertical = matches!(dir, Dir::Up | Dir::Down);
        let positive = matches!(dir, Dir::Down | Dir::Right);
        let (axis_max, view, current) = if vertical {
            (max.1, vp.height, reader.off.1)
        } else {
            (max.0, vp.width, reader.off.0)
        };

        // Which way a page turn goes when the page cannot scroll: down is forward, and
        // left/right depend on the reading direction.
        let turn_forward = if vertical { positive } else { positive != reader.rtl };
        let arrive = if turn_forward { Arrive::Start } else { Arrive::End };

        if axis_max <= EDGE_EPS {
            self.turn_by(turn_forward, count, arrive, cx);
            return;
        }
        let at_end = if positive {
            current >= axis_max - EDGE_EPS
        } else {
            current <= EDGE_EPS
        };
        if at_end {
            self.turn_by(turn_forward, 1, arrive, cx);
            return;
        }

        let step = match amount {
            Amount::Line => LINE_STEP,
            Amount::HalfPage => view * 0.5,
            Amount::Page => view * 0.9,
        };
        let delta = step * count as f32 * if positive { 1.0 } else { -1.0 };
        let target = (current + delta).clamp(0.0, axis_max);
        if vertical {
            reader.off.1 = target;
        } else {
            reader.off.0 = target;
        }
        cx.notify();
    }

    fn scroll_vertical_to(&mut self, fraction: f32, vp: Viewport, cx: &mut Context<Self>) {
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        let Some(display) = reader.settle(&vp) else {
            return;
        };
        let max = layout::max_offset(display, &vp);
        reader.off.1 = max.1 * fraction;
        cx.notify();
    }

    fn zoom_to(&mut self, zoom: f32, anchor: (f32, f32), vp: Viewport, cx: &mut Context<Self>) {
        let message = {
            let Some(reader) = self.reader.as_mut() else {
                return;
            };
            let Some(old_display) = reader.settle(&vp) else {
                return;
            };
            let Some(orig) = reader.current().map(Reader::orig_of) else {
                return;
            };
            let zoom = layout::clamp_zoom(zoom);
            let old_origin = layout::origin(reader.off, old_display, &vp);
            let new_display = layout::display_size(reader.fit, zoom, orig, &vp);
            let off = layout::zoom_anchor_offset(old_origin, old_display, new_display, anchor);
            reader.zoom = zoom;
            reader.off = layout::clamp_offset(off, new_display, &vp);
            format!("{} · {}%", reader.fit.label(), (zoom * 100.0).round() as i32)
        };
        self.show_hud(message, cx);
    }

    fn zoom_by(&mut self, factor: f32, anchor: (f32, f32), vp: Viewport, cx: &mut Context<Self>) {
        let Some(zoom) = self.reader.as_ref().map(|r| r.zoom) else {
            return;
        };
        self.zoom_to(zoom * factor, anchor, vp, cx);
    }

    fn set_fit(&mut self, mode: FitMode, cx: &mut Context<Self>) {
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        reader.fit = mode;
        reader.zoom = 1.0;
        reader.arrive = Some(Arrive::Start);
        self.library.settings_mut().fit = mode;
        self.save_library();
        self.show_hud(mode.label(), cx);
    }

    fn set_direction(&mut self, rtl: bool, cx: &mut Context<Self>) {
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        reader.rtl = rtl;
        self.library.settings_mut().rtl = rtl;
        self.save_library();
        self.show_hud(if rtl { "right to left" } else { "left to right" }, cx);
    }

    fn on_wheel(
        &mut self,
        delta: (f32, f32),
        zoom: bool,
        at: (f32, f32),
        discrete: bool,
        vp: Viewport,
        cx: &mut Context<Self>,
    ) {
        let (dx, dy) = delta;
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        let Some(display) = reader.settle(&vp) else {
            return;
        };

        if zoom {
            // Wheel up (positive) zooms in, anchored at the pointer.
            self.zoom_by(1.2f32.powf(dy / WHEEL_LINE), at, vp, cx);
            return;
        }

        let max = layout::max_offset(display, &vp);
        if max.1 > EDGE_EPS {
            // Wheel down gives a negative dy and should move further down the page.
            let at_bottom = reader.off.1 >= max.1 - EDGE_EPS;
            let at_top = reader.off.1 <= EDGE_EPS;
            if discrete && ((dy < 0.0 && at_bottom) || (dy > 0.0 && at_top)) {
                let forward = dy < 0.0;
                let arrive = if forward { Arrive::Start } else { Arrive::End };
                self.turn_by(forward, 1, arrive, cx);
                return;
            }
            reader.off.1 = (reader.off.1 - dy).clamp(0.0, max.1);
        } else if discrete && dy != 0.0 {
            let forward = dy < 0.0;
            let arrive = if forward { Arrive::Start } else { Arrive::End };
            self.turn_by(forward, 1, arrive, cx);
            return;
        }
        if max.0 > EDGE_EPS {
            reader.off.0 = (reader.off.0 - dx).clamp(0.0, max.0);
        }
        cx.notify();
    }

    fn drag_start(&mut self, at: (f32, f32)) {
        if let Some(reader) = self.reader.as_mut() {
            let off0 = reader.off;
            reader.drag = Some(Drag { start: at, off0, moved: false });
        }
    }

    fn drag_move(&mut self, at: (f32, f32), pressed: bool, vp: Viewport, cx: &mut Context<Self>) {
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        let Some(drag) = reader.drag else {
            return;
        };
        if !pressed {
            reader.drag = None;
            return;
        }
        let (dx, dy) = (at.0 - drag.start.0, at.1 - drag.start.1);
        if !drag.moved && dx.abs().max(dy.abs()) < DRAG_THRESHOLD {
            return;
        }
        let Some(display) = reader.settle(&vp) else {
            return;
        };
        reader.drag = Some(Drag { moved: true, ..drag });
        reader.off = layout::clamp_offset((drag.off0.0 - dx, drag.off0.1 - dy), display, &vp);
        cx.notify();
    }

    /// Mouse released: if it was not a drag, treat it as a click on the left/right third.
    fn drag_end(&mut self, x: f32, vp: Viewport, cx: &mut Context<Self>) {
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        let Some(drag) = reader.drag.take() else {
            return;
        };
        if drag.moved {
            return;
        }
        if x < vp.width / 3.0 {
            self.step(Side::Left, cx);
        } else if x > vp.width * 2.0 / 3.0 {
            self.step(Side::Right, cx);
        }
    }

    // ------------------------------------------------------------------------------------
    // Keyboard
    // ------------------------------------------------------------------------------------

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let mods = &keystroke.modifiers;
        if mods.platform {
            return;
        }
        let token = vim::key_token(
            &keystroke.key,
            keystroke.key_char.as_deref(),
            mods.control,
            mods.alt,
            mods.shift,
        );

        // An open menu or dialog gets every key; nothing leaks through to the reader.
        if self.overlay.is_some() {
            self.on_overlay_key(&token, window, cx);
            return;
        }
        let Some(reader) = self.reader.as_mut() else {
            return;
        };

        if reader.show_help
            && !reader.vim.in_cmdline()
            && matches!(token.as_str(), "escape" | "?" | "q")
        {
            reader.show_help = false;
            cx.notify();
            return;
        }

        let vp = viewport_of(window);
        let actions = reader.vim.feed(&token);
        cx.notify();
        for action in actions {
            self.apply(action, vp, window, cx);
        }
    }

    fn apply(&mut self, action: Action, vp: Viewport, window: &mut Window, cx: &mut Context<Self>) {
        let center = (vp.width / 2.0, vp.height / 2.0);
        match action {
            Action::Scroll { dir, amount, count } => self.scroll(dir, amount, count, vp, cx),
            Action::NextPage(n) => self.turn_by(true, n, Arrive::Start, cx),
            Action::PrevPage(n) => self.turn_by(false, n, Arrive::Start, cx),
            Action::GotoPage(n) => self.jump_to(n.saturating_sub(1), cx),
            Action::FirstPage => self.jump_to(0, cx),
            Action::LastPage => self.jump_to(usize::MAX, cx),
            Action::ZoomIn(n) => self.zoom_by(layout::ZOOM_STEP.powi(n.min(64) as i32), center, vp, cx),
            Action::ZoomOut(n) => self.zoom_by(layout::ZOOM_STEP.powi(-(n.min(64) as i32)), center, vp, cx),
            Action::ZoomReset => self.zoom_to(1.0, center, vp, cx),
            Action::ZoomSet(zoom) => self.zoom_to(zoom, center, vp, cx),
            Action::Fit(mode) => self.set_fit(mode, cx),
            Action::CycleFit => {
                if let Some(next) = self.reader.as_ref().map(|r| r.fit.next()) {
                    self.set_fit(next, cx);
                }
            }
            Action::ScrollTop => self.scroll_vertical_to(0.0, vp, cx),
            Action::ScrollCenter => self.scroll_vertical_to(0.5, vp, cx),
            Action::ScrollBottom => self.scroll_vertical_to(1.0, vp, cx),
            Action::SetMark(key) => {
                let Some((id, page)) = self.reader.as_ref().map(|r| (r.book_id.clone(), r.page)) else {
                    return;
                };
                if let Some(book) = self.library.get_mut(&id) {
                    book.marks.insert(key.to_string(), page);
                }
                self.save_library();
                self.show_hud(format!("Mark {key} set on page {}", page + 1), cx);
            }
            Action::JumpMark(key) => {
                let Some(id) = self.reader.as_ref().map(|r| r.book_id.clone()) else {
                    return;
                };
                let target = self
                    .library
                    .get(&id)
                    .and_then(|book| book.marks.get(&key.to_string()).copied());
                match target {
                    Some(page) => self.jump_to(page, cx),
                    None => self.show_hud(format!("No mark {key}"), cx),
                }
            }
            Action::JumpBack => {
                let Some(reader) = self.reader.as_mut() else {
                    return;
                };
                match reader.back_page {
                    Some(previous) => {
                        reader.back_page = Some(reader.page);
                        self.goto_page(previous, Arrive::Start, cx);
                    }
                    None => self.show_hud("No previous position", cx),
                }
            }
            Action::ToggleDirection => {
                if let Some(rtl) = self.reader.as_ref().map(|r| r.rtl) {
                    self.set_direction(!rtl, cx);
                }
            }
            Action::SetDirection { rtl } => self.set_direction(rtl, cx),
            Action::ToggleInfo => {
                if let Some(reader) = self.reader.as_mut() {
                    reader.show_info = !reader.show_info;
                    cx.notify();
                }
            }
            Action::ToggleHelp => {
                if let Some(reader) = self.reader.as_mut() {
                    reader.show_help = !reader.show_help;
                    cx.notify();
                }
            }
            Action::ToggleFullscreen => window.toggle_fullscreen(),
            Action::OpenSettings => self.open_settings(cx),
            Action::CloseBook => self.close_book(window, cx),
            Action::QuitApp => cx.quit(),
            Action::Message(text) => self.show_hud(text, cx),
        }
    }

    // ------------------------------------------------------------------------------------
    // Page decoding
    // ------------------------------------------------------------------------------------

    /// Called every render: settles the scroll position, frees far-away pages and decodes the
    /// current and next pages at the resolution the current zoom needs.
    fn sync_reader(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let vp = viewport_of(window);

        let wanted: Vec<(usize, u32)> = {
            let Some(reader) = self.reader.as_mut() else {
                return;
            };
            reader.viewport = vp;
            let Some(pages) = reader.pages.clone() else {
                return;
            };
            reader.settle(&vp);

            let low = reader.page.saturating_sub(2);
            let high = reader.page + 4;
            let stale: Vec<usize> = reader
                .images
                .keys()
                .copied()
                .filter(|ix| *ix < low || *ix > high)
                .collect();
            for ix in stale {
                if let Some(page) = reader.images.remove(&ix) {
                    let _ = window.drop_image(page.image);
                }
            }

            // The current page needs enough pixels for the current zoom; its neighbours only
            // for zoom 1, since a page turn resets the view anyway.
            let long_side = |display: (f32, f32)| display.0.max(display.1) * vp.scale;
            let (current_tier, neighbour_tier) = match reader.current().map(Reader::orig_of) {
                Some(orig) => (
                    layout::tier_for(long_side(layout::display_size(reader.fit, reader.zoom, orig, &vp))),
                    layout::tier_for(long_side(layout::display_size(reader.fit, 1.0, orig, &vp))),
                ),
                None => {
                    let tier = layout::tier_for(vp.width.max(vp.height) * vp.scale);
                    (tier, tier)
                }
            };

            let current = reader.page as isize;
            [0isize, 1, 2, -1, 3, 4]
                .iter()
                .filter_map(|delta| {
                    let ix = current + delta;
                    if ix < 0 || ix as usize >= pages.len() {
                        return None;
                    }
                    let tier = if *delta == 0 { current_tier } else { neighbour_tier };
                    Some((ix as usize, tier))
                })
                .collect()
        };

        for (ix, tier) in wanted {
            self.request_page(ix, tier, cx);
        }
    }

    fn request_page(&mut self, ix: usize, tier: u32, cx: &mut Context<Self>) {
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        let Some(pages) = reader.pages.clone() else {
            return;
        };
        if ix >= pages.len() || reader.pending.contains(&ix) || reader.failed.contains_key(&ix) {
            return;
        }
        if matches!(reader.images.get(&ix), Some(page) if page.tier >= tier) {
            return;
        }
        reader.pending.insert(ix);

        let book_id = reader.book_id.clone();
        let path = reader.path.clone();
        let kind = reader.kind;
        let name = pages[ix].clone();

        let task = cx.background_executor().spawn(async move {
            let bytes = archive::read_page(&path, kind, &name)?;
            let decoded = imaging::decode_fit_bgra(&bytes, tier, tier)?;
            anyhow::Ok((
                Arc::new(RenderImage::new(vec![Frame::new(decoded.image)])),
                decoded.orig,
            ))
        });

        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| this.page_decoded(book_id, ix, tier, result, cx))
                .ok();
        })
        .detach();
    }

    fn page_decoded(
        &mut self,
        book_id: String,
        ix: usize,
        tier: u32,
        result: Result<(Arc<RenderImage>, (u32, u32))>,
        cx: &mut Context<Self>,
    ) {
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        if reader.book_id != book_id {
            return;
        }
        reader.pending.remove(&ix);
        match result {
            Ok((image, orig)) => {
                if let Some(old) = reader.images.insert(ix, Page { image, orig, tier }) {
                    self.garbage.push(old.image);
                }
            }
            Err(err) => {
                reader.failed.insert(ix, format!("{err:#}"));
            }
        }
        cx.notify();
    }

    // ------------------------------------------------------------------------------------
    // Rendering
    // ------------------------------------------------------------------------------------

    fn render_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        Some(match self.overlay.as_ref()? {
            Overlay::Menu(menu) => self.render_menu(menu, cx),
            Overlay::Confirm(dialog) => self.render_confirm(dialog, cx),
            Overlay::Properties(dialog) => self.render_properties(dialog, cx),
            Overlay::Settings => self.render_settings(cx),
        })
    }

    /// Full-window layer behind a dialog: swallows clicks meant for the view underneath and
    /// closes the dialog when clicked. `dim` darkens the view behind it.
    fn scrim(&self, dim: bool, cx: &mut Context<Self>) -> gpui::Div {
        // Explicit insets matter: an absolutely positioned child without them is placed after the
        // in-flow content (below the window) instead of covering it.
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(if dim { 0x0000_008c } else { 0x0000_0000 }))
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| this.close_overlay(cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _: &MouseDownEvent, _, cx| this.close_overlay(cx)),
            )
    }

    fn render_menu(&self, menu: &BookMenu, cx: &mut Context<Self>) -> AnyElement {
        let mut list = div()
            .w(px(MENU_W))
            .flex()
            .flex_col()
            .p_1()
            .rounded_md()
            .bg(rgb(SURFACE))
            .border_1()
            .border_color(rgb(TRACK))
            .shadow_lg()
            .text_sm()
            .text_color(rgb(TEXT))
            .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_: &MouseDownEvent, _, cx| cx.stop_propagation());

        for (ix, entry) in menu.entries.iter().enumerate() {
            list = match entry {
                Entry::Separator => list.child(div().h(px(1.)).mx_2().my_1().bg(rgb(TRACK))),
                Entry::Item(item) => list.child(self.menu_item(menu, ix, item, cx)),
            };
        }

        #[cfg(test)]
        let list = list.debug_selector(|| "book-menu".into());

        // `anchored` keeps the menu inside the window (it slides back in near the edges) and
        // `deferred` paints it above everything else.
        deferred(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _, cx| this.close_overlay(cx)),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _: &MouseDownEvent, _, cx| this.close_overlay(cx)),
                )
                .child(
                    anchored()
                        .position(menu.position)
                        .anchor(Corner::TopLeft)
                        .snap_to_window_with_margin(px(8.))
                        .child(list),
                ),
        )
        .with_priority(1)
        .into_any_element()
    }

    fn menu_item(&self, menu: &BookMenu, ix: usize, item: &Item, cx: &mut Context<Self>) -> AnyElement {
        let base = div()
            .id(SharedString::from(format!("menu-item-{ix}")))
            .h(px(28.))
            .px_3()
            .flex()
            .items_center()
            .rounded_sm()
            .child(item.label.clone());

        if !item.enabled {
            return base.text_color(rgb(MUTED)).opacity(0.6).into_any_element();
        }

        let action = item.action;
        let book_id = menu.book_id.clone();
        base.text_color(rgb(if item.danger { ACCENT } else { TEXT }))
            .cursor_pointer()
            .when(menu.selected == Some(ix), |row| row.bg(rgb(SURFACE_HI)))
            .on_mouse_move(cx.listener(move |this, _: &MouseMoveEvent, _, cx| this.menu_hover(ix, cx)))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.run_book_action(action, &book_id, window, cx);
            }))
            .into_any_element()
    }

    fn panel(&self, width: f32) -> gpui::Div {
        div()
            .w(px(width))
            .max_w_full()
            .flex()
            .flex_col()
            .gap_4()
            .p_6()
            .rounded_lg()
            .bg(rgb(SURFACE))
            .border_1()
            .border_color(rgb(TRACK))
            .shadow_lg()
            .text_sm()
            .text_color(rgb(TEXT))
            .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_: &MouseDownEvent, _, cx| cx.stop_propagation())
    }

    fn render_confirm(&self, dialog: &ConfirmDialog, cx: &mut Context<Self>) -> AnyElement {
        self.scrim(true, cx)
            .child(
                self.panel(380.)
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(dialog.title.clone()),
                    )
                    .child(div().text_color(rgb(MUTED)).child(dialog.message.clone()))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(panel_button(
                                "confirm-cancel",
                                "Cancel",
                                ButtonKind::Normal,
                                true,
                                cx.listener(|this, _: &ClickEvent, _, cx| this.close_overlay(cx)),
                            ))
                            .child(panel_button(
                                "confirm-accept",
                                dialog.confirm_label,
                                ButtonKind::Primary,
                                true,
                                cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.confirm_accept(window, cx);
                                }),
                            )),
                    ),
            )
            .into_any_element()
    }

    fn render_properties(&self, dialog: &PropertiesDialog, cx: &mut Context<Self>) -> AnyElement {
        let rows = dialog.rows.iter().map(|(label, value)| {
            div()
                .flex()
                .flex_row()
                .gap_4()
                .child(
                    div()
                        .w(px(84.))
                        .flex_none()
                        .text_color(rgb(MUTED))
                        .child(label.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(value.clone()),
                )
        });

        self.scrim(true, cx)
            .child(
                self.panel(480.)
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(dialog.title.clone()),
                    )
                    .child(div().flex().flex_col().gap_2().children(rows))
                    .child(
                        div().flex().flex_row().justify_end().child(panel_button(
                            "properties-close",
                            "Close",
                            ButtonKind::Normal,
                            true,
                            cx.listener(|this, _: &ClickEvent, _, cx| this.close_overlay(cx)),
                        )),
                    ),
            )
            .into_any_element()
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let settings = self.library.settings();
        let opacity = settings.opacity;
        let fraction = (opacity - OPACITY_MIN) / (OPACITY_MAX - OPACITY_MIN);
        let is_default = *settings == Settings::default();
        let bounds = self.slider.bounds.clone();

        // The rail is inset by half a knob on each side so the knob's centre sits exactly at
        // the end of the filled part.
        let track = div()
            .relative()
            .w(px(SLIDER_W))
            .h(px(20.))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    this.slider.dragging = true;
                    this.slider_set_from_x(f32::from(event.position.x), cx);
                }),
            )
            .child(
                canvas(move |area, _, _| bounds.set(Some(area)), |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            )
            .child(
                div()
                    .absolute()
                    .left(px(KNOB / 2.0))
                    .top(px(8.))
                    .w(px(SLIDER_W - KNOB))
                    .h(px(4.))
                    .rounded_full()
                    .bg(rgb(TRACK))
                    .child(
                        div()
                            .h_full()
                            .w(relative(fraction))
                            .rounded_full()
                            .bg(rgb(ACCENT)),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .top(px(2.))
                    .left(px(fraction * (SLIDER_W - KNOB)))
                    .size(px(KNOB))
                    .rounded_full()
                    .bg(rgb(TEXT)),
            );

        // No dimming scrim here: you want to see the window behind the panel while dragging.
        self.scrim(false, cx)
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if !this.slider.dragging {
                    return;
                }
                if event.pressed_button == Some(MouseButton::Left) {
                    this.slider_set_from_x(f32::from(event.position.x), cx);
                } else {
                    this.end_slider_drag(cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.end_slider_drag(cx)),
            )
            .child(
                self.panel(SLIDER_W + 48.)
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Settings"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .justify_between()
                                    .child("Window background opacity")
                                    .child(
                                        div()
                                            .text_color(rgb(MUTED))
                                            .child(format!("{}%", (opacity * 100.0).round() as u32)),
                                    ),
                            )
                            .child(track)
                            .child(
                                div().text_xs().text_color(rgb(MUTED)).child(
                                    "Lower values let the desktop show through the window \
                                     background. Pages, covers and text stay fully opaque.",
                                ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(panel_button(
                                "settings-reset",
                                "Reset to Defaults",
                                ButtonKind::Normal,
                                !is_default,
                                cx.listener(|this, _: &ClickEvent, _, cx| this.request_reset_settings(cx)),
                            ))
                            .child(panel_button(
                                "settings-done",
                                "Done",
                                ButtonKind::Primary,
                                true,
                                cx.listener(|this, _: &ClickEvent, _, cx| this.close_overlay(cx)),
                            )),
                    ),
            )
            .into_any_element()
    }

    fn render_library(&self, cx: &mut Context<Self>) -> AnyElement {
        let books = self.library.books();
        let count = books.len();
        let status: Option<SharedString> = if self.importing {
            Some("Importing…".into())
        } else {
            self.toast.clone()
        };

        let header = div()
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .justify_between()
            .h(px(56.))
            .px_6()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap_3()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT))
                            .child("yomu"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(MUTED))
                            .child(format!("{count} {}", if count == 1 { "book" } else { "books" })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .children(status.map(|text| {
                        div()
                            .max_w(px(520.))
                            .text_sm()
                            .text_color(rgb(MUTED))
                            .truncate()
                            .child(text)
                    }))
                    .child(button(
                        "add-files",
                        "Add files",
                        cx.listener(|this, _: &ClickEvent, _, cx| this.import_dialog(false, cx)),
                    ))
                    .child(button(
                        "add-folder",
                        "Add folder",
                        cx.listener(|this, _: &ClickEvent, _, cx| this.import_dialog(true, cx)),
                    ))
                    .child(button(
                        "settings",
                        "Settings",
                        cx.listener(|this, _: &ClickEvent, _, cx| this.open_settings(cx)),
                    )),
            );

        let body = if books.is_empty() {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    div()
                        .text_xl()
                        .text_color(rgb(TEXT))
                        .child("Your library is empty"),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(MUTED))
                        .child("Drop CBZ files or folders onto this window, or use Add files (Ctrl+O)."),
                )
                .into_any_element()
        } else {
            div()
                .id("grid")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .px_6()
                .pb_6()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap_6()
                        .children(books.iter().map(|book| self.book_card(book, cx))),
                )
                .into_any_element()
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn book_card(&self, book: &Book, cx: &mut Context<Self>) -> AnyElement {
        let open_id = book.id.clone();
        let menu_id = book.id.clone();
        let progress = book.progress();
        let status = if book.is_finished() {
            "Finished".to_string()
        } else if book.opened {
            format!(
                "Page {} of {} · {}%",
                book.current_page + 1,
                book.page_count,
                (progress * 100.0).round() as u32
            )
        } else {
            format!("{} pages", book.page_count)
        };

        let cover = div()
            .w(px(CARD_W))
            .h(px(CARD_H))
            .flex_none()
            .rounded_md()
            .overflow_hidden()
            .bg(rgb(SURFACE))
            .child(
                img(self.dirs.cover(&book.id))
                    .w_full()
                    .h_full()
                    .object_fit(ObjectFit::Cover),
            );

        let progress_bar = div()
            .h(px(3.))
            .w_full()
            .rounded_full()
            .bg(rgb(TRACK))
            .child(
                div()
                    .h_full()
                    .w(relative(progress))
                    .rounded_full()
                    .bg(rgb(ACCENT)),
            );

        div()
            .id(SharedString::from(format!("book-{}", book.id)))
            .w(px(CARD_W))
            .flex_none()
            .flex()
            .flex_col()
            .gap_2()
            .cursor_pointer()
            .hover(|style| style.opacity(0.88))
            // `on_click` only ever fires for the left button, so right-clicking never opens a book.
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                if this.overlay_recently_closed() {
                    return;
                }
                this.open_book(&open_id, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.open_book_menu(&menu_id, event.position, cx);
                }),
            )
            .child(cover)
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(TEXT))
                    .truncate()
                    .child(SharedString::from(book.title.clone())),
            )
            .child(progress_bar)
            .child(div().text_xs().text_color(rgb(MUTED)).child(status))
            .into_any_element()
    }

    fn render_reader(&self, reader: &Reader, cx: &mut Context<Self>) -> AnyElement {
        let vp = reader.viewport;

        // The page is drawn at a size and position we compute ourselves, so what you see is
        // exactly what the fit mode and zoom say, independent of the texture's pixel size.
        let content = if let Some(error) = &reader.error {
            centered(format!("Couldn't open this book: {error}"))
        } else if let Some(page) = reader.current() {
            let display = layout::display_size(reader.fit, reader.zoom, Reader::orig_of(page), &vp);
            let (left, top) = layout::origin(reader.off, display, &vp);
            img(page.image.clone())
                .absolute()
                .left(px(left))
                .top(px(top))
                .w(px(display.0))
                .h(px(display.1))
                .object_fit(ObjectFit::Fill)
                .into_any_element()
        } else if let Some(error) = reader.failed.get(&reader.page) {
            centered(format!("Couldn't decode page {}: {error}", reader.page + 1))
        } else if reader.pages.is_none() {
            centered("Opening…")
        } else {
            div().size_full().into_any_element()
        };

        let fraction = (reader.page + 1) as f32 / reader.total.max(1) as f32;
        let progress_line = div()
            .absolute()
            .bottom_0()
            .h(px(2.))
            .w(relative(fraction))
            .bg(rgb(ACCENT));
        // Manga fills right to left; western comics left to right.
        let progress_line = if reader.rtl {
            progress_line.right_0()
        } else {
            progress_line.left_0()
        };

        let info = reader.show_info.then(|| {
            pill(
                div().absolute().bottom_4().left_0().right_0(),
                format!(
                    "{} / {}  ·  {}  ·  {}%  ·  {}",
                    reader.page + 1,
                    reader.total,
                    reader.fit.label(),
                    (reader.zoom * 100.0).round() as i32,
                    if reader.rtl { "right to left" } else { "left to right" }
                ),
            )
        });
        let hud = self
            .hud
            .clone()
            .map(|text| pill(div().absolute().top_4().left_0().right_0(), text));

        let vim_status = reader.vim.status().map(|text| {
            let text = if reader.vim.in_cmdline() { format!("{text}▏") } else { text };
            div()
                .absolute()
                .bottom_0()
                .left_0()
                .px_3()
                .py_1()
                .bg(rgba(0x000000d9))
                .text_sm()
                .text_color(rgb(TEXT))
                .child(text)
        });

        div()
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(with_alpha(READER_BG, self.library.settings().opacity))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, _| {
                    this.drag_start((f32::from(event.position.x), f32::from(event.position.y)));
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                let at = (f32::from(event.position.x), f32::from(event.position.y));
                let pressed = event.pressed_button == Some(MouseButton::Left);
                this.drag_move(at, pressed, viewport_of(window), cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, window, cx| {
                    this.drag_end(f32::from(event.position.x), viewport_of(window), cx);
                }),
            )
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
                let delta = event.delta.pixel_delta(px(WHEEL_LINE));
                let discrete = matches!(event.delta, ScrollDelta::Lines(_));
                this.on_wheel(
                    (f32::from(delta.x), f32::from(delta.y)),
                    event.modifiers.control,
                    (f32::from(event.position.x), f32::from(event.position.y)),
                    discrete,
                    viewport_of(window),
                    cx,
                );
            }))
            .child(content)
            .child(progress_line)
            .children(info)
            .children(hud)
            .children(vim_status)
            .children(reader.show_help.then(help_overlay))
            .into_any_element()
    }
}

impl Render for Yomu {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for image in self.garbage.drain(..) {
            let _ = window.drop_image(image);
        }
        self.sync_reader(window, cx);
        self.sync_appearance(window, cx);

        let reading = self.reader.is_some();
        let content = match self.reader.as_ref() {
            Some(reader) => self.render_reader(reader, cx),
            None => self.render_library(cx),
        };

        // The reader paints its own background, so the root only has one in the library;
        // two translucent layers would add up to a different opacity than the setting says.
        let root_bg = if reading {
            rgba(0x0000_0000)
        } else {
            with_alpha(BG, self.library.settings().opacity)
        };
        let overlay = self.render_overlay(cx);

        div()
            .key_context(if reading { "Reader" } else { "Library" })
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(root_bg)
            .text_color(rgb(TEXT))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key(event, window, cx);
            }))
            .on_action(cx.listener(|_, _: &ToggleFullscreen, window, _| window.toggle_fullscreen()))
            .on_action(cx.listener(|this, _: &ImportFiles, _, cx| this.import_dialog(false, cx)))
            .on_action(cx.listener(|this, _: &ImportFolder, _, cx| this.import_dialog(true, cx)))
            .on_action(cx.listener(|this, _: &OpenSettings, _, cx| this.open_settings(cx)))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                if this.overlay.is_none() {
                    this.import_paths(paths.paths().to_vec(), cx);
                }
            }))
            .child(content)
            .children(overlay)
    }
}

// ----------------------------------------------------------------------------------------
// Small helpers
// ----------------------------------------------------------------------------------------

fn centered(text: impl Into<SharedString>) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(rgb(MUTED))
        .child(text.into())
        .into_any_element()
}

/// A small rounded label centred horizontally inside `row` (an absolutely positioned div).
fn pill(row: gpui::Div, text: impl Into<SharedString>) -> gpui::Div {
    row.flex().justify_center().child(
        div()
            .px_3()
            .py_1()
            .rounded_full()
            .bg(rgba(0x000000b3))
            .text_sm()
            .text_color(rgb(TEXT))
            .child(text.into()),
    )
}

const HELP: &[(&str, &[(&str, &str)])] = &[
    (
        "Move",
        &[
            ("j k h l  / arrows", "scroll; turns the page at the edge"),
            ("space  S-space", "scroll a screen down / up"),
            ("C-d  C-u", "scroll half a screen"),
            ("J  K  / PgDn  PgUp", "next / previous page"),
            ("gg  G  12G  :12", "first page, last page, page 12"),
            ("ma  'a  ''", "set a mark, jump to it, jump back"),
        ],
    ),
    (
        "View",
        &[
            ("a  s  zh  z1", "fit page / width / height / original size"),
            ("tab", "cycle the fit mode"),
            ("+  -  0  / zi  zo", "zoom in / out / reset"),
            ("ctrl+wheel   drag", "zoom at the pointer   pan the page"),
            ("zt  zz  zb", "scroll to top / middle / bottom"),
            ("r  i  f", "reading direction, page info, fullscreen"),
        ],
    ),
    (
        "Commands (:)",
        &[
            (":fit width", "page, width, height or original"),
            (":zoom 150", "150% of the fit size"),
            (":rtl  :ltr  :q  :qa", "direction, back to library, quit"),
            (":settings  Ctrl+,", "open the settings"),
        ],
    ),
];

fn help_overlay() -> AnyElement {
    let mut panel = div()
        .w(px(600.))
        .max_w_full()
        .flex()
        .flex_col()
        .gap_3()
        .p_6()
        .rounded_lg()
        .bg(rgba(0x0f0f11f2))
        .text_sm();

    for (title, rows) in HELP {
        panel = panel.child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(MUTED))
                        .child(*title),
                )
                .children(rows.iter().map(|(keys, what)| {
                    div()
                        .flex()
                        .flex_row()
                        .gap_4()
                        .child(div().w(px(190.)).flex_none().text_color(rgb(ACCENT)).child(*keys))
                        .child(div().text_color(rgb(TEXT)).child(*what))
                })),
        );
    }
    panel = panel.child(
        div()
            .text_xs()
            .text_color(rgb(MUTED))
            .child("Press ? or Esc to close"),
    );

    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(rgba(0x000000a6))
        .child(panel)
        .into_any_element()
}

#[derive(Clone, Copy)]
enum ButtonKind {
    Normal,
    Primary,
}

/// A button for dialogs: same shape as the header buttons, on the dialog's raised surface.
fn panel_button(
    id: &'static str,
    label: impl Into<SharedString>,
    kind: ButtonKind,
    enabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let (background, hover, foreground) = match kind {
        ButtonKind::Normal => (SURFACE_HI, TRACK, TEXT),
        ButtonKind::Primary => (ACCENT, 0xea8a74, 0x1a1a1e),
    };
    let base = div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .bg(rgb(background))
        .text_sm()
        .text_color(rgb(foreground))
        .child(label.into());
    if enabled {
        base.cursor_pointer()
            .hover(move |style| style.bg(rgb(hover)))
            .on_click(on_click)
            .into_any_element()
    } else {
        base.opacity(0.4).into_any_element()
    }
}

fn button(
    id: &'static str,
    label: &'static str,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .bg(rgb(SURFACE))
        .text_sm()
        .text_color(rgb(TEXT))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(SURFACE_HI)))
        .on_click(on_click)
        .child(label)
}

fn main() {
    Application::new().run(|cx: &mut App| {
        // The reader handles its own keys (vim-style, see `vim.rs`); only these are bindings.
        cx.bind_keys([
            KeyBinding::new("ctrl-o", ImportFiles, Some("Library")),
            KeyBinding::new("ctrl-shift-o", ImportFolder, Some("Library")),
            KeyBinding::new("f11", ToggleFullscreen, None),
            KeyBinding::new("ctrl-,", OpenSettings, None),
            KeyBinding::new("ctrl-q", Quit, None),
        ]);

        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(1180.), px(780.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                // Created transparent so the surface has an alpha channel; the app switches to
                // an opaque appearance whenever the background opacity setting is 100%.
                window_background: WindowBackgroundAppearance::Transparent,
                titlebar: Some(TitlebarOptions {
                    title: Some("yomu".into()),
                    ..Default::default()
                }),
                // Compositors match window rules (Niri, Hyprland, Sway…) on the app id.
                app_id: Some("yomu".into()),
                window_min_size: Some(size(px(480.), px(360.))),
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(Yomu::new);
                let focus = view.read(cx).focus_handle.clone();
                window.focus(&focus);
                view
            },
        )
        .expect("failed to open the window");

        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    //! These drive the real views through gpui's test platform: simulated mouse clicks and key
    //! presses go through hit-testing and key dispatch exactly as they do in the running app.

    use super::*;
    use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext, point};
    use std::{fs, io::{Cursor, Write}};

    fn png_bytes() -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(60, 90, image::Rgba([90, 40, 160, 255]));
        let mut out = Cursor::new(Vec::new());
        image.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    struct Fixture {
        files: Vec<PathBuf>,
        dirs: DataDirs,
        /// Book ids in library order: A, B, C.
        ids: Vec<String>,
    }

    fn fixture(tag: &str) -> Fixture {
        let root = std::env::temp_dir().join(format!("yomu-ui-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let dirs = DataDirs::at(root.join("data")).unwrap();

        let mut files = Vec::new();
        for name in ["A", "B", "C"] {
            let path = root.join(format!("{name}.cbz"));
            let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
            let options = zip::write::SimpleFileOptions::default();
            for page in 1..=5 {
                zip.start_file(format!("{page}.png"), options).unwrap();
                zip.write_all(&png_bytes()).unwrap();
            }
            zip.finish().unwrap();
            files.push(path);
        }

        let report = library::import_paths(&files, &dirs, &HashSet::new());
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let mut library = Library::default();
        for book in report.added {
            library.add(book);
        }
        library.save(&dirs).unwrap();
        let ids = library.books().iter().map(|b| b.id.clone()).collect();
        Fixture { files, dirs, ids }
    }

    fn launch<'a>(fx: &Fixture, cx: &'a mut TestAppContext) -> (Entity<Yomu>, &'a mut VisualTestContext) {
        let dirs = fx.dirs.clone();
        cx.update(|cx| cx.bind_keys([KeyBinding::new("ctrl-,", OpenSettings, None)]));
        cx.add_window_view(move |window, cx| {
            let yomu = Yomu::with_dirs(dirs, cx);
            window.focus(&yomu.focus_handle);
            yomu
        })
    }

    fn book(view: &Entity<Yomu>, cx: &mut VisualTestContext, id: &str) -> Book {
        view.read_with(cx, |this, _| this.library.get(id).cloned().expect("book exists"))
    }

    fn saved(fx: &Fixture, id: &str) -> Option<Book> {
        Library::load(&fx.dirs).unwrap().get(id).cloned()
    }

    fn saved_settings(fx: &Fixture) -> Settings {
        Library::load(&fx.dirs).unwrap().settings().clone()
    }

    fn overlay_kind(view: &Entity<Yomu>, cx: &mut VisualTestContext) -> &'static str {
        view.read_with(cx, |this, _| match this.overlay {
            None => "none",
            Some(Overlay::Menu(_)) => "menu",
            Some(Overlay::Confirm(_)) => "confirm",
            Some(Overlay::Properties(_)) => "properties",
            Some(Overlay::Settings) => "settings",
        })
    }

    fn reading(view: &Entity<Yomu>, cx: &mut VisualTestContext) -> Option<usize> {
        view.read_with(cx, |this, _| this.reader.as_ref().map(|r| r.page))
    }

    fn opacity(view: &Entity<Yomu>, cx: &mut VisualTestContext) -> f32 {
        view.read_with(cx, |this, _| this.library.settings().opacity)
    }

    fn act(view: &Entity<Yomu>, cx: &mut VisualTestContext, id: &str, action: BookAction) {
        let id = id.to_string();
        view.update_in(cx, move |this, window, cx| this.run_book_action(action, &id, window, cx));
        cx.run_until_parked();
    }

    fn accept(view: &Entity<Yomu>, cx: &mut VisualTestContext) {
        view.update_in(cx, |this, window, cx| this.confirm_accept(window, cx));
        cx.run_until_parked();
    }

    fn close(view: &Entity<Yomu>, cx: &mut VisualTestContext) {
        view.update(cx, |this, cx| this.close_overlay(cx));
        cx.run_until_parked();
    }

    fn set_progress(view: &Entity<Yomu>, cx: &mut VisualTestContext, id: &str, page: usize) {
        let id = id.to_string();
        view.update(cx, move |this, _| {
            let book = this.library.get_mut(&id).unwrap();
            book.opened = true;
            book.current_page = page;
        });
    }

    fn open_menu(view: &Entity<Yomu>, cx: &mut VisualTestContext, id: &str) {
        let id = id.to_string();
        view.update(cx, move |this, cx| this.open_book_menu(&id, point(px(100.), px(100.)), cx));
        cx.run_until_parked();
    }

    fn menu_labels(view: &Entity<Yomu>, cx: &mut VisualTestContext) -> Vec<String> {
        view.read_with(cx, |this, _| match &this.overlay {
            Some(Overlay::Menu(menu)) => menu
                .entries
                .iter()
                .filter_map(|e| match e {
                    Entry::Item(item) => Some(item.label.clone()),
                    Entry::Separator => None,
                })
                .collect(),
            _ => Vec::new(),
        })
    }

    /// Centre of the nth book card in the library grid (window is 1920 wide in the test platform).
    fn card(index: usize) -> Point<Pixels> {
        point(px(24.0 + 204.0 * index as f32 + 90.0), px(56.0 + 100.0))
    }

    fn click(cx: &mut VisualTestContext, at: Point<Pixels>, button: MouseButton) {
        cx.simulate_mouse_down(at, button, Modifiers::default());
        cx.simulate_mouse_up(at, button, Modifiers::default());
    }

    // ---------------------------------------------------------------------------------------
    // Context menu: actions
    // ---------------------------------------------------------------------------------------

    #[gpui::test]
    fn mark_as_read_then_unread_updates_and_persists(cx: &mut TestAppContext) {
        let fx = fixture("markread");
        let (view, cx) = launch(&fx, cx);
        let id = fx.ids[0].clone();

        open_menu(&view, cx, &id);
        assert_eq!(menu_labels(&view, cx)[0], "Start Reading");
        act(&view, cx, &id, BookAction::MarkRead);
        assert_eq!(overlay_kind(&view, cx), "none");
        let b = book(&view, cx, &id);
        assert_eq!((b.current_page, b.opened), (4, true));
        assert_eq!(saved(&fx, &id).unwrap().current_page, 4);

        open_menu(&view, cx, &id);
        let labels = menu_labels(&view, cx);
        assert_eq!(labels[0], "Start from Beginning");
        assert!(labels.contains(&"Mark as Unread".to_string()));
        assert!(!labels.contains(&"Mark as Read".to_string()));
        act(&view, cx, &id, BookAction::MarkUnread);
        let b = book(&view, cx, &id);
        assert_eq!((b.current_page, b.opened), (0, false));
        let on_disk = saved(&fx, &id).unwrap();
        assert!(!on_disk.opened);

        // repeating the action is harmless
        act(&view, cx, &id, BookAction::MarkUnread);
        assert!(!book(&view, cx, &id).opened);
    }

    #[gpui::test]
    fn reset_progress_asks_first_and_keeps_marks(cx: &mut TestAppContext) {
        let fx = fixture("reset");
        let (view, cx) = launch(&fx, cx);
        let id = fx.ids[1].clone();
        set_progress(&view, cx, &id, 3);
        view.update(cx, |this, _| {
            this.library.get_mut(&fx.ids[1]).unwrap().marks.insert("a".into(), 2);
        });

        open_menu(&view, cx, &id);
        assert!(menu_labels(&view, cx).contains(&"Resume Reading".to_string()));
        act(&view, cx, &id, BookAction::ResetProgress);
        assert_eq!(overlay_kind(&view, cx), "confirm");
        assert_eq!(book(&view, cx, &id).current_page, 3, "nothing changes before confirming");

        // cancelling leaves everything alone
        close(&view, cx);
        assert_eq!(overlay_kind(&view, cx), "none");
        assert_eq!(book(&view, cx, &id).current_page, 3);

        act(&view, cx, &id, BookAction::ResetProgress);
        accept(&view, cx);
        let b = book(&view, cx, &id);
        assert_eq!((b.current_page, b.opened), (0, false));
        assert_eq!(b.marks.get("a"), Some(&2));
        let on_disk = saved(&fx, &id).unwrap();
        assert!(!on_disk.opened);
        assert_eq!(on_disk.marks.get("a"), Some(&2));
    }

    #[gpui::test]
    fn remove_never_deletes_the_file_and_confirms_only_when_progress_would_be_lost(cx: &mut TestAppContext) {
        let fx = fixture("remove");
        let (view, cx) = launch(&fx, cx);

        // unread: removed straight away
        let (a, b) = (fx.ids[0].clone(), fx.ids[1].clone());
        act(&view, cx, &a, BookAction::Remove);
        assert_eq!(overlay_kind(&view, cx), "none");
        assert!(saved(&fx, &a).is_none());
        assert!(fx.files[0].exists(), "the comic file must survive");
        assert!(!fx.dirs.cover(&a).exists(), "the cached cover is cleaned up");

        // with progress: asks first, and cancelling keeps the book
        set_progress(&view, cx, &b, 2);
        act(&view, cx, &b, BookAction::Remove);
        assert_eq!(overlay_kind(&view, cx), "confirm");
        close(&view, cx);
        assert!(saved(&fx, &b).is_some());
        act(&view, cx, &b, BookAction::Remove);
        accept(&view, cx);
        assert!(saved(&fx, &b).is_none());
        assert!(fx.files[1].exists());
        assert_eq!(view.read_with(cx, |this, _| this.library.books().len()), 1);
    }

    #[gpui::test]
    fn resume_and_start_from_beginning_open_the_reader(cx: &mut TestAppContext) {
        let fx = fixture("resume");
        let (view, cx) = launch(&fx, cx);
        let id = fx.ids[2].clone();
        set_progress(&view, cx, &id, 3);

        act(&view, cx, &id, BookAction::Open);
        assert_eq!(reading(&view, cx), Some(3), "resume lands on the saved page");
        view.update_in(cx, |this, window, cx| this.close_book(window, cx));
        cx.run_until_parked();
        assert_eq!(reading(&view, cx), None);

        act(&view, cx, &id, BookAction::StartOver);
        assert_eq!(reading(&view, cx), Some(0));
        assert_eq!(book(&view, cx, &id).current_page, 0);
        assert_eq!(saved(&fx, &id).unwrap().current_page, 0);
    }

    #[gpui::test]
    fn missing_file_disables_file_actions_and_reports_it(cx: &mut TestAppContext) {
        let fx = fixture("missing");
        let (view, cx) = launch(&fx, cx);
        let id = fx.ids[0].clone();
        fs::remove_file(&fx.files[0]).unwrap();

        open_menu(&view, cx, &id);
        let disabled = view.read_with(cx, |this, _| match &this.overlay {
            Some(Overlay::Menu(menu)) => menu
                .entries
                .iter()
                .filter_map(|e| match e {
                    Entry::Item(item) if !item.enabled => Some(item.action),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        });
        assert_eq!(disabled, vec![BookAction::Open, BookAction::RevealFile]);

        // even if triggered anyway (stale menu), nothing breaks
        act(&view, cx, &id, BookAction::Open);
        assert_eq!(reading(&view, cx), None);
        let toast = view.read_with(cx, |this, _| this.toast.clone().map(|t| t.to_string()));
        assert!(toast.unwrap().contains("missing"));

        // bookkeeping actions still work without the file
        act(&view, cx, &id, BookAction::MarkRead);
        assert!(book(&view, cx, &id).is_finished());
        act(&view, cx, &id, BookAction::Remove);
        assert_eq!(overlay_kind(&view, cx), "confirm");
    }

    #[gpui::test]
    fn properties_dialog_lists_book_information(cx: &mut TestAppContext) {
        let fx = fixture("props");
        let (view, cx) = launch(&fx, cx);
        let id = fx.ids[0].clone();
        act(&view, cx, &id, BookAction::Properties);
        assert_eq!(overlay_kind(&view, cx), "properties");
        let rows = view.read_with(cx, |this, _| match &this.overlay {
            Some(Overlay::Properties(dialog)) => dialog.rows.clone(),
            _ => Vec::new(),
        });
        let get = |name: &str| rows.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
        assert_eq!(get("Pages").as_deref(), Some("5"));
        assert_eq!(get("Format").as_deref(), Some("CBZ archive"));
        assert!(get("Location").unwrap().ends_with("A.cbz"));
        assert!(get("Size").is_some());
        close(&view, cx);
        assert_eq!(overlay_kind(&view, cx), "none");
    }

    // ---------------------------------------------------------------------------------------
    // Context menu: mouse and keyboard
    // ---------------------------------------------------------------------------------------

    #[gpui::test]
    fn right_click_opens_the_menu_and_never_opens_the_book(cx: &mut TestAppContext) {
        let fx = fixture("rightclick");
        let (view, cx) = launch(&fx, cx);

        click(cx, card(0), MouseButton::Right);
        assert_eq!(overlay_kind(&view, cx), "menu");
        assert_eq!(reading(&view, cx), None, "right-click must not open the book");
        view.read_with(cx, |this, _| match &this.overlay {
            Some(Overlay::Menu(menu)) => assert_eq!(menu.book_id, fx.ids[0]),
            _ => unreachable!(),
        });

        // a left click on a book (no menu) still opens it
        close(&view, cx);
        std::thread::sleep(Duration::from_millis(350));
        click(cx, card(1), MouseButton::Left);
        assert_eq!(reading(&view, cx), Some(0));
    }

    #[gpui::test]
    fn clicking_outside_closes_the_menu_without_clicking_through(cx: &mut TestAppContext) {
        let fx = fixture("outside");
        let (view, cx) = launch(&fx, cx);

        click(cx, card(0), MouseButton::Right);
        assert_eq!(overlay_kind(&view, cx), "menu");
        // far from the menu, but directly over another book
        click(cx, card(3), MouseButton::Left);
        assert_eq!(overlay_kind(&view, cx), "none");
        assert_eq!(reading(&view, cx), None, "the click only dismisses the menu");

        // and an immediate second click (double-click on a menu item) is swallowed too
        click(cx, card(3), MouseButton::Left);
        assert_eq!(reading(&view, cx), None);
    }

    #[gpui::test]
    fn menu_items_can_be_clicked(cx: &mut TestAppContext) {
        let fx = fixture("itemclick");
        let (view, cx) = launch(&fx, cx);
        let id = fx.ids[0].clone();

        click(cx, card(0), MouseButton::Right);
        // second item of an unread book's menu is "Mark as Read": 4px padding, one 28px row,
        // a 9px separator, then the row itself
        let origin = card(0);
        let at = point(origin.x + px(20.), origin.y + px(4. + 28. + 9. + 14.));
        click(cx, at, MouseButton::Left);
        assert_eq!(overlay_kind(&view, cx), "none");
        assert!(book(&view, cx, &id).is_finished());
    }

    #[gpui::test]
    fn menu_stays_inside_the_window_near_every_edge(cx: &mut TestAppContext) {
        let fx = fixture("edges");
        let (view, cx) = launch(&fx, cx);
        let id = fx.ids[0].clone();
        let (width, height) = (1920.0, 1080.0);

        for (x, y) in [(2.0, 2.0), (width - 2.0, 2.0), (2.0, height - 2.0), (width - 2.0, height - 2.0)] {
            let id = id.clone();
            view.update(cx, move |this, cx| this.open_book_menu(&id, point(px(x), px(y)), cx));
            cx.run_until_parked();
            let bounds = cx.debug_bounds("book-menu").expect("menu was painted");
            let (left, top) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
            let right = left + f32::from(bounds.size.width);
            let bottom = top + f32::from(bounds.size.height);
            assert!(left >= 0.0 && top >= 0.0, "({x},{y}) -> {bounds:?}");
            assert!(right <= width && bottom <= height, "({x},{y}) -> {bounds:?}");
            close(&view, cx);
        }

        // away from the edges it opens exactly where you clicked
        view.update(cx, |this, cx| this.open_book_menu(&fx.ids[0], point(px(300.), px(200.)), cx));
        cx.run_until_parked();
        let bounds = cx.debug_bounds("book-menu").unwrap();
        assert_eq!((f32::from(bounds.origin.x), f32::from(bounds.origin.y)), (300.0, 200.0));
    }

    #[gpui::test]
    fn menu_keyboard_navigation(cx: &mut TestAppContext) {
        let fx = fixture("menukeys");
        let (view, cx) = launch(&fx, cx);
        let id = fx.ids[0].clone();
        open_menu(&view, cx, &id);

        let selected = |view: &Entity<Yomu>, cx: &mut VisualTestContext| {
            view.read_with(cx, |this, _| match &this.overlay {
                Some(Overlay::Menu(menu)) => menu.selected,
                _ => None,
            })
        };
        assert_eq!(selected(&view, cx), None);
        cx.simulate_keystrokes("down");
        assert_eq!(selected(&view, cx), Some(0));
        cx.simulate_keystrokes("down down");
        assert_eq!(selected(&view, cx), Some(4), "skips separators: Start, Mark as Read, Open Location");
        cx.simulate_keystrokes("up");
        assert_eq!(selected(&view, cx), Some(2));
        cx.simulate_keystrokes("end");
        assert_eq!(selected(&view, cx), Some(7), "Remove from Library is last");
        cx.simulate_keystrokes("down");
        assert_eq!(selected(&view, cx), Some(0), "wraps around");

        cx.simulate_keystrokes("escape");
        assert_eq!(overlay_kind(&view, cx), "none");

        // enter activates the highlighted item
        open_menu(&view, cx, &id);
        cx.simulate_keystrokes("down down enter");
        assert!(book(&view, cx, &id).is_finished());
        assert_eq!(overlay_kind(&view, cx), "none");
    }

    #[gpui::test]
    fn keys_do_not_leak_to_the_reader_while_a_dialog_is_open(cx: &mut TestAppContext) {
        let fx = fixture("leak");
        let (view, cx) = launch(&fx, cx);
        let id = fx.ids[0].clone();
        act(&view, cx, &id, BookAction::Open);
        assert_eq!(reading(&view, cx), Some(0));

        cx.simulate_keystrokes("ctrl-,");
        assert_eq!(overlay_kind(&view, cx), "settings");
        cx.simulate_keystrokes("J G space");
        assert_eq!(reading(&view, cx), Some(0), "reader keys are ignored behind a dialog");
        cx.simulate_keystrokes("escape");
        assert_eq!(overlay_kind(&view, cx), "none");
        assert_eq!(reading(&view, cx), Some(0), "escape closed the dialog, not the book");
        cx.simulate_keystrokes("J");
        assert_eq!(reading(&view, cx), Some(1), "the reader works again afterwards");
    }

    // ---------------------------------------------------------------------------------------
    // Settings
    // ---------------------------------------------------------------------------------------

    #[gpui::test]
    fn settings_open_with_shortcut_and_header_button_and_close_with_escape(cx: &mut TestAppContext) {
        let fx = fixture("settingsopen");
        let (view, cx) = launch(&fx, cx);

        cx.simulate_keystrokes("ctrl-,");
        assert_eq!(overlay_kind(&view, cx), "settings");
        cx.simulate_keystrokes("ctrl-,");
        assert_eq!(overlay_kind(&view, cx), "none", "the shortcut toggles");

        view.update(cx, |this, cx| this.open_settings(cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        assert_eq!(overlay_kind(&view, cx), "none");
    }

    #[gpui::test]
    fn opacity_keys_apply_immediately_and_persist(cx: &mut TestAppContext) {
        let fx = fixture("opacitykeys");
        let (view, cx) = launch(&fx, cx);
        assert_eq!(opacity(&view, cx), 1.0, "default is fully opaque");

        view.update(cx, |this, cx| this.open_settings(cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("left left");
        assert!((opacity(&view, cx) - 0.90).abs() < 1e-4);
        assert!((saved_settings(&fx).opacity - 0.90).abs() < 1e-4, "saved on every change");
        cx.simulate_keystrokes("right");
        assert!((opacity(&view, cx) - 0.95).abs() < 1e-4);
        cx.simulate_keystrokes("home");
        assert!((opacity(&view, cx) - OPACITY_MIN).abs() < 1e-4);
        cx.simulate_keystrokes("left");
        assert!((opacity(&view, cx) - OPACITY_MIN).abs() < 1e-4, "cannot go below the minimum");
        cx.simulate_keystrokes("end right");
        assert_eq!(opacity(&view, cx), OPACITY_MAX, "cannot go above 100%");
    }

    #[gpui::test]
    fn slider_follows_the_mouse_and_saves_when_released(cx: &mut TestAppContext) {
        let fx = fixture("slider");
        let (view, cx) = launch(&fx, cx);
        view.update(cx, |this, cx| this.open_settings(cx));
        cx.run_until_parked();

        let track = view
            .read_with(cx, |this, _| this.slider.bounds.get())
            .expect("the slider track has been painted");
        let y = track.center().y;
        let left = track.origin.x;
        let right = track.origin.x + track.size.width;

        cx.simulate_mouse_down(point(left + px(1.), y), MouseButton::Left, Modifiers::default());
        assert!((opacity(&view, cx) - OPACITY_MIN).abs() < 1e-4, "pressing at the left end");
        assert!((saved_settings(&fx).opacity - 1.0).abs() < 1e-4, "not saved while dragging");

        let middle = point((left + right) / 2.0, y);
        cx.simulate_mouse_move(middle, MouseButton::Left, Modifiers::default());
        let expected = (OPACITY_MIN + OPACITY_MAX) / 2.0;
        assert!((opacity(&view, cx) - expected).abs() < 0.02, "dragging to the middle");

        // dragging past the end (even off the track) clamps
        cx.simulate_mouse_move(point(right + px(200.), y), MouseButton::Left, Modifiers::default());
        assert_eq!(opacity(&view, cx), OPACITY_MAX);

        cx.simulate_mouse_move(middle, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(middle, MouseButton::Left, Modifiers::default());
        let value = opacity(&view, cx);
        assert!((saved_settings(&fx).opacity - value).abs() < 1e-4, "saved on release");
        assert_eq!(overlay_kind(&view, cx), "settings", "dragging does not dismiss the panel");
    }

    #[gpui::test]
    fn window_appearance_switches_between_opaque_and_transparent(cx: &mut TestAppContext) {
        let fx = fixture("appearance");
        let (view, cx) = launch(&fx, cx);
        let applied = |view: &Entity<Yomu>, cx: &mut VisualTestContext| {
            view.read_with(cx, |this, _| this.applied_opaque)
        };
        assert_eq!(applied(&view, cx), Some(true), "100% uses the opaque appearance");

        view.update(cx, |this, cx| this.set_opacity(0.6, true, cx));
        cx.run_until_parked();
        assert_eq!(applied(&view, cx), Some(false), "below 100% needs real transparency");

        view.update(cx, |this, cx| this.set_opacity(1.0, true, cx));
        cx.run_until_parked();
        assert_eq!(applied(&view, cx), Some(true));
    }

    #[gpui::test]
    fn reset_to_defaults_confirms_and_restores_everything(cx: &mut TestAppContext) {
        let fx = fixture("resetsettings");
        let (view, cx) = launch(&fx, cx);

        // at the defaults there is nothing to reset
        view.update(cx, |this, cx| {
            this.open_settings(cx);
            this.request_reset_settings(cx);
        });
        cx.run_until_parked();
        assert_eq!(overlay_kind(&view, cx), "settings");

        view.update(cx, |this, cx| {
            this.set_opacity(0.55, true, cx);
            let settings = this.library.settings_mut();
            settings.rtl = false;
            settings.fit = FitMode::Width;
        });
        view.update(cx, |this, cx| this.request_reset_settings(cx));
        cx.run_until_parked();
        assert_eq!(overlay_kind(&view, cx), "confirm");

        // cancelling goes back to the panel and changes nothing
        cx.simulate_keystrokes("escape");
        assert_eq!(overlay_kind(&view, cx), "settings");
        assert!((opacity(&view, cx) - 0.55).abs() < 1e-4);

        view.update(cx, |this, cx| this.request_reset_settings(cx));
        cx.simulate_keystrokes("enter");
        assert_eq!(overlay_kind(&view, cx), "settings", "back on the panel to see the result");
        assert_eq!(opacity(&view, cx), OPACITY_MAX);
        assert_eq!(saved_settings(&fx), Settings::default());
    }

    #[gpui::test]
    fn reset_also_updates_an_open_reader(cx: &mut TestAppContext) {
        let fx = fixture("resetreader");
        let (view, cx) = launch(&fx, cx);
        let id = fx.ids[0].clone();
        act(&view, cx, &id, BookAction::Open);
        view.update(cx, |this, cx| {
            this.set_direction(false, cx);
            this.set_fit(FitMode::Width, cx);
        });
        view.update(cx, |this, cx| {
            this.open_settings(cx);
            this.request_reset_settings(cx);
        });
        cx.simulate_keystrokes("enter");
        let (rtl, fit) = view.read_with(cx, |this, _| {
            let reader = this.reader.as_ref().unwrap();
            (reader.rtl, reader.fit)
        });
        assert!(rtl);
        assert_eq!(fit, FitMode::Page);
    }

    #[gpui::test]
    fn settings_survive_a_restart(cx: &mut TestAppContext) {
        let fx = fixture("restart");
        {
            let (view, cx) = launch(&fx, cx);
            let id = fx.ids[2].clone();
            view.update(cx, |this, cx| this.set_opacity(0.72, true, cx));
            set_progress(&view, cx, &id, 2);
            act(&view, cx, &id, BookAction::MarkRead);
        }
        // a brand new app instance reads the same files
        let (view, cx) = launch(&fx, cx);
        assert!((opacity(&view, cx) - 0.72).abs() < 1e-4);
        assert!(book(&view, cx, &fx.ids[2]).is_finished());
    }

    #[gpui::test]
    fn corrupt_preferences_do_not_lose_the_library(cx: &mut TestAppContext) {
        let fx = fixture("corrupt");
        let text = fs::read_to_string(fx.dirs.library_file()).unwrap();
        let mut json: serde_json::Value = serde_json::from_str(&text).unwrap();
        json["settings"] = serde_json::json!({"opacity": "bogus", "fit": 17, "rtl": null});
        fs::write(fx.dirs.library_file(), json.to_string()).unwrap();

        let (view, cx) = launch(&fx, cx);
        assert_eq!(view.read_with(cx, |this, _| this.library.books().len()), 3);
        assert_eq!(opacity(&view, cx), OPACITY_MAX);
        assert_eq!(view.read_with(cx, |this, _| this.library.settings().clone()), Settings::default());
    }
}
