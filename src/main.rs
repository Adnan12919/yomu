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
    AnyElement, App, Application, Bounds, ClickEvent, Context, Corner, ExternalPaths, FocusHandle,
    FontWeight, KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ObjectFit, PathPromptOptions, Pixels, Point, RenderImage, Rgba, ScrollDelta,
    ScrollWheelEvent, SharedString, TitlebarOptions, Window, WindowBackgroundAppearance,
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
    motion::{self, PageTransition},
    pagecache,
    selection::Selection,
    util::now_secs,
    vim::{self, Action, Amount, Dir, Vim},
};

actions!(
    yomu,
    [
        ToggleFullscreen,
        ImportFiles,
        ImportFolder,
        OpenSettings,
        Quit
    ]
);

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
/// A wheel notch scrolls this many pixels (and is one zoom step with Ctrl).
const WHEEL_LINE: f32 = 96.0;
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
    /// Memory the decoded pixels take (BGRA), for the cache budget.
    bytes: u64,
}

/// The page that is sliding out during a page turn, frozen as it was when the turn began.
struct Outgoing {
    image: Arc<RenderImage>,
    display: (f32, f32),
    origin: (f32, f32),
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

    /// Right-to-left reading order (manga). Off by default: pages run left to right.
    rtl: bool,
    fit: FitMode,
    /// Multiplier on top of the fit size; `1.0` is exactly the fit.
    zoom: f32,
    /// How far the page is scrolled from its top-left corner (what is drawn this frame).
    off: (f32, f32),
    /// Where `off` is gliding to. Equal to `off` when nothing is moving. Key and wheel-notch
    /// scrolling move this; edge checks and repeated presses accumulate on it, not on `off`.
    scroll_target: (f32, f32),
    /// Scroll position to apply once the new page's size is known.
    arrive: Option<Arrive>,
    viewport: Viewport,

    /// The slide between pages, if one is running. At most one exists (no queue); a page turn
    /// that arrives mid-slide replaces it. Purely visual: `page` and the saved progress already
    /// point at the new page the moment you turn.
    transition: Option<PageTransition>,
    outgoing: Option<Outgoing>,
    /// The viewport the slide started in; resizing the window cancels it.
    transition_viewport: Viewport,
    /// Whether page turns and scrolling animate (the setting, unless the desktop asks for
    /// reduced motion).
    animate: bool,
    /// Textures no longer shown, to be freed by the window on its next frame.
    released: Vec<Arc<RenderImage>>,

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
            scroll_target: (0.0, 0.0),
            arrive: Some(Arrive::Start),
            viewport: Viewport::EMPTY,
            transition: None,
            outgoing: None,
            transition_viewport: Viewport::EMPTY,
            animate: settings.animate,
            released: Vec::new(),
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
            self.set_scroll(layout::arrival_offset(arrive, self.rtl, max));
        }
        self.off = layout::clamp_offset(self.off, display, vp);
        self.scroll_target = layout::clamp_offset(self.scroll_target, display, vp);
        Some(display)
    }

    /// Jump the scroll position (no glide).
    fn set_scroll(&mut self, off: (f32, f32)) {
        self.off = off;
        self.scroll_target = off;
    }

    /// Hand a texture that is no longer drawn to the window to free, unless the page cache
    /// still holds it.
    fn release(&mut self, image: Arc<RenderImage>) {
        if !self
            .images
            .values()
            .any(|page| Arc::ptr_eq(&page.image, &image))
        {
            self.released.push(image);
        }
    }

    fn cancel_transition(&mut self) {
        self.transition = None;
        if let Some(outgoing) = self.outgoing.take() {
            self.release(outgoing.image);
        }
    }

    /// Start the slide for a page turn. Call *before* `page` changes: the page on screen right
    /// now is what slides out.
    ///
    /// No slide is started (and any running one is dropped) when animations are off, when the
    /// page on screen is not decoded yet, or when the window has no size. If a slide is already
    /// running it is replaced, never queued: the page that was sliding in takes over as the
    /// outgoing page from exactly where it is, so rapid turns stay smooth and cannot fall behind.
    fn begin_transition(&mut self, forward: bool, enabled: bool) {
        let vp = self.viewport;
        let on_screen = self
            .current()
            .map(|page| (page.image.clone(), Self::orig_of(page)));
        let Some((image, orig)) =
            on_screen.filter(|_| enabled && vp.width >= 1.0 && vp.height >= 1.0)
        else {
            self.cancel_transition();
            return;
        };

        let travel = motion::travel(forward, self.rtl);
        let display = layout::display_size(self.fit, self.zoom, orig, &vp);
        let origin = layout::origin(self.off, display, &vp);
        let next = match self.transition {
            Some(running) => running.interrupted_by(travel),
            None => PageTransition::starting(travel),
        };
        if let Some(previous) = self.outgoing.take() {
            self.release(previous.image);
        }
        self.outgoing = Some(Outgoing {
            image,
            display,
            origin,
        });
        self.transition = Some(next);
        self.transition_viewport = vp;
    }

    /// Advance the slide and the scroll glide by `dt` seconds of real time. Returns whether
    /// another frame is needed; when it returns `false` nothing is moving and the window can
    /// stay idle.
    fn advance_animations(&mut self, dt: f32, vp: &Viewport) -> bool {
        let dt = motion::clamp_frame_dt(dt);

        let resized = vp.width != self.transition_viewport.width
            || vp.height != self.transition_viewport.height;
        if self.transition.is_some() && (resized || self.failed.contains_key(&self.page)) {
            self.cancel_transition();
        }
        if !self.animate {
            self.cancel_transition();
            self.off = self.scroll_target;
            return false;
        }

        let mut animating = false;
        if let Some(mut transition) = self.transition {
            // The clock only runs once the new page can be shown, so a slow decode keeps the old
            // page on screen instead of showing a blank.
            if self.current().is_some() {
                transition.advance(dt);
                if transition.is_done() {
                    self.transition = None;
                    if let Some(outgoing) = self.outgoing.take() {
                        self.release(outgoing.image);
                    }
                } else {
                    self.transition = Some(transition);
                    animating = true;
                }
            }
        }

        let (off, moving) = motion::step_scroll(self.off, self.scroll_target, dt);
        self.off = off;
        animating || moving
    }

    /// Every texture this reader still owns, for freeing when the book closes.
    fn into_images(self) -> Vec<Arc<RenderImage>> {
        let mut images: Vec<_> = self.images.into_values().map(|page| page.image).collect();
        images.extend(self.outgoing.map(|outgoing| outgoing.image));
        images.extend(self.released);
        images
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
    RemoveSelected,
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
    /// The desktop asks for reduced motion (see `motion::detect_reduced_motion`).
    reduce_motion: bool,
    /// When the previous animated frame was drawn; `None` while nothing is animating.
    last_frame: Option<Instant>,
    /// Tests drive the animation clock by hand instead of using real time.
    #[cfg(test)]
    frame_dt: Option<f32>,
    overlay: Option<Overlay>,
    overlay_closed_at: Option<Instant>,
    /// Books picked for bulk actions in the library.
    selection: Selection,
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
            Err(err) => (
                Library::default(),
                Some(SharedString::from(format!("{err:#}"))),
            ),
        };
        let state = Self {
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
            reduce_motion: false,
            last_frame: None,
            #[cfg(test)]
            frame_dt: None,
            overlay: None,
            overlay_closed_at: None,
            selection: Selection::new(),
            slider: Slider {
                bounds: Rc::new(Cell::new(None)),
                dragging: false,
            },
            applied_opaque: None,
        };

        // Asking the desktop about reduced motion can take a moment (it may spawn `gsettings`),
        // so do it off the UI thread. Tests skip it to stay independent of the machine.
        #[cfg(not(test))]
        cx.spawn(async move |this, cx| {
            let reduce = cx
                .background_executor()
                .spawn(async { motion::detect_reduced_motion() })
                .await;
            this.update(cx, |this, cx| {
                this.reduce_motion = reduce;
                cx.notify();
            })
            .ok();
        })
        .detach();

        state
    }

    /// Whether page turns and scrolling should animate right now.
    fn animations_enabled(&self) -> bool {
        self.library.settings().animate && !self.reduce_motion
    }

    /// An input just started an animation: start the frame clock now so the first frame already
    /// accounts for the input-to-frame latency.
    fn wake(&mut self) {
        if self.last_frame.is_none() {
            self.last_frame = Some(Instant::now());
        }
    }

    /// Seconds since the previous animated frame.
    fn frame_delta(&self, now: Instant) -> f32 {
        #[cfg(test)]
        {
            if let Some(dt) = self.frame_dt {
                return dt;
            }
        }
        self.last_frame
            .map_or(0.0, |previous| now.duration_since(previous).as_secs_f32())
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
            cx.background_executor()
                .timer(Duration::from_secs(HUD_SECS))
                .await;
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
        if let Some(Overlay::Menu(menu)) = self.overlay.as_mut()
            && menu.selected != Some(ix)
        {
            menu.selected = Some(ix);
            cx.notify();
        }
    }

    /// Run a menu action, asking first if it would throw away progress.
    fn run_book_action(
        &mut self,
        action: BookAction,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Bulk actions work on the selection, not on the book that was right-clicked.
        match action {
            BookAction::RemoveSelected => {
                self.request_remove_selected(cx);
                return;
            }
            BookAction::ClearSelection => {
                self.dismiss();
                self.selection.clear();
                cx.notify();
                return;
            }
            _ => {}
        }
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

    fn perform_book_action(
        &mut self,
        action: BookAction,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
            // Handled in `run_book_action`; they never reach here.
            BookAction::RemoveSelected | BookAction::ClearSelection => {}
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
            ConfirmAction::RemoveSelected => self.remove_selected(cx),
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
            message:
                "Background opacity, reading direction and page fit go back to their defaults."
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
        self.set_opacity(
            OPACITY_MIN + fraction * (OPACITY_MAX - OPACITY_MIN),
            false,
            cx,
        );
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
        window.defer(cx, move |window, _| {
            window.set_background_appearance(appearance)
        });
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
                    "left" | "h" | "down" | "j" => {
                        self.set_opacity(current - OPACITY_STEP, true, cx)
                    }
                    "right" | "l" | "up" | "k" => {
                        self.set_opacity(current + OPACITY_STEP, true, cx)
                    }
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
                this.update(cx, |this, cx| this.import_paths(paths, cx))
                    .ok();
            }
            Ok(Err(err)) => {
                let message = format!("{err:#}");
                this.update(cx, |this, cx| this.show_toast(message, cx))
                    .ok();
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
        self.prune_selection();
        self.save_library();
        self.show_toast(format!("Removed “{title}” from the library"), cx);
    }

    // ------------------------------------------------------------------------------------
    // Selection and bulk removal
    // ------------------------------------------------------------------------------------

    /// Book ids in library order.
    fn library_order(&self) -> Vec<String> {
        self.library
            .books()
            .iter()
            .map(|book| book.id.clone())
            .collect()
    }

    fn selected_ids(&self) -> Vec<String> {
        self.selection.in_order(&self.library_order())
    }

    fn prune_selection(&mut self) {
        let existing = self.library.ids();
        self.selection.retain_existing(&existing);
    }

    /// A click on a book card with the given modifiers. Returns `false` if it is a plain click
    /// that should open the book instead.
    fn select_click(&mut self, id: &str, ctrl: bool, shift: bool, cx: &mut Context<Self>) -> bool {
        let order = self.library_order();
        let handled = self.selection.click(&order, id, ctrl, shift);
        if handled {
            cx.notify();
        }
        handled
    }

    /// Right-click on a book: a book inside a multi-book selection gets the bulk menu; anywhere
    /// else the selection is dropped (like a file manager) and the book gets its own menu.
    fn open_context_menu(&mut self, id: &str, position: Point<Pixels>, cx: &mut Context<Self>) {
        if self.selection.contains(id) && self.selection.len() > 1 {
            self.overlay = Some(Overlay::Menu(BookMenu {
                book_id: id.to_string(),
                entries: menu::selection_menu(self.selection.len()),
                position,
                selected: None,
            }));
            cx.notify();
        } else {
            self.selection.clear();
            self.open_book_menu(id, position, cx);
        }
    }

    /// Remove every selected book from the library, asking first only if progress or marks
    /// would be lost. The comic files are never touched.
    fn request_remove_selected(&mut self, cx: &mut Context<Self>) {
        let ids = self.selected_ids();
        if ids.is_empty() {
            return;
        }
        let confirmation = {
            let books: Vec<&Book> = ids.iter().filter_map(|id| self.library.get(id)).collect();
            menu::bulk_remove_confirmation(&books)
        };
        match confirmation {
            Some(confirmation) => {
                self.overlay = Some(Overlay::Confirm(ConfirmDialog {
                    title: confirmation.title.into(),
                    message: confirmation.message.into(),
                    confirm_label: confirmation.confirm_label,
                    action: ConfirmAction::RemoveSelected,
                }));
                cx.notify();
            }
            None => self.remove_selected(cx),
        }
    }

    fn remove_selected(&mut self, cx: &mut Context<Self>) {
        self.dismiss();
        let ids = self.selected_ids();
        for id in &ids {
            self.library.remove(id, &self.dirs);
        }
        self.selection.clear();
        self.save_library();
        let message = if ids.len() == 1 {
            "Removed 1 book from the library".to_string()
        } else {
            format!("Removed {} books from the library", ids.len())
        };
        self.show_toast(message, cx);
    }

    fn on_library_key(&mut self, token: &str, cx: &mut Context<Self>) {
        match token {
            "C-a" => {
                let order = self.library_order();
                self.selection.select_all(&order);
                cx.notify();
            }
            "escape" => {
                if !self.selection.is_empty() {
                    self.selection.clear();
                    cx.notify();
                }
            }
            "delete" => self.request_remove_selected(cx),
            _ => {}
        }
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

        self.selection.clear();
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

    fn pages_listed(
        &mut self,
        book_id: String,
        result: Result<Vec<String>>,
        cx: &mut Context<Self>,
    ) {
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
            self.garbage.extend(reader.into_images());
        }
        self.last_frame = None;
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
        let animate = self.animations_enabled();
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
        // The reading position changes immediately; the slide is only a visual that catches up.
        reader.begin_transition(target > reader.page, animate);
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
        self.wake();
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
    fn scroll(
        &mut self,
        dir: Dir,
        amount: Amount,
        count: usize,
        vp: Viewport,
        cx: &mut Context<Self>,
    ) {
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
        // Edge checks and repeated presses work on where the scroll is *heading*, so pressing
        // `j` several times quickly accumulates instead of restarting from the glide's position.
        let (axis_max, view, current) = if vertical {
            (max.1, vp.height, reader.scroll_target.1)
        } else {
            (max.0, vp.width, reader.scroll_target.0)
        };

        // Which way a page turn goes when the page cannot scroll: down is forward, and
        // left/right depend on the reading direction.
        let turn_forward = if vertical {
            positive
        } else {
            positive != reader.rtl
        };
        let arrive = if turn_forward {
            Arrive::Start
        } else {
            Arrive::End
        };

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
            reader.scroll_target.1 = target;
        } else {
            reader.scroll_target.0 = target;
        }
        self.wake();
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
        reader.scroll_target.1 = max.1 * fraction;
        self.wake();
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
            // Zooming is immediate (and ends any slide, whose frozen page would no longer fit).
            reader.cancel_transition();
            reader.zoom = zoom;
            reader.set_scroll(layout::clamp_offset(off, new_display, &vp));
            format!(
                "{} · {}%",
                reader.fit.label(),
                (zoom * 100.0).round() as i32
            )
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
        reader.cancel_transition();
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
        self.show_hud(
            if rtl {
                "right to left"
            } else {
                "left to right"
            },
            cx,
        );
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
            let at_bottom = reader.scroll_target.1 >= max.1 - EDGE_EPS;
            let at_top = reader.scroll_target.1 <= EDGE_EPS;
            if discrete && ((dy < 0.0 && at_bottom) || (dy > 0.0 && at_top)) {
                let forward = dy < 0.0;
                let arrive = if forward { Arrive::Start } else { Arrive::End };
                self.turn_by(forward, 1, arrive, cx);
                return;
            }
            let target = (reader.scroll_target.1 - dy).clamp(0.0, max.1);
            reader.scroll_target.1 = target;
            if !discrete {
                // Touchpads already send a smooth stream of small deltas: follow them 1:1.
                reader.off.1 = target;
            }
        } else if discrete && dy != 0.0 {
            let forward = dy < 0.0;
            let arrive = if forward { Arrive::Start } else { Arrive::End };
            self.turn_by(forward, 1, arrive, cx);
            return;
        }
        if max.0 > EDGE_EPS {
            let target = (reader.scroll_target.0 - dx).clamp(0.0, max.0);
            reader.scroll_target.0 = target;
            if !discrete {
                reader.off.0 = target;
            }
        }
        self.wake();
        cx.notify();
    }

    fn drag_start(&mut self, at: (f32, f32)) {
        if let Some(reader) = self.reader.as_mut() {
            let off0 = reader.off;
            reader.drag = Some(Drag {
                start: at,
                off0,
                moved: false,
            });
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
        reader.drag = Some(Drag {
            moved: true,
            ..drag
        });
        reader.set_scroll(layout::clamp_offset(
            (drag.off0.0 - dx, drag.off0.1 - dy),
            display,
            &vp,
        ));
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
            self.on_library_key(&token, cx);
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
            Action::ZoomIn(n) => {
                self.zoom_by(layout::ZOOM_STEP.powi(n.min(64) as i32), center, vp, cx)
            }
            Action::ZoomOut(n) => {
                self.zoom_by(layout::ZOOM_STEP.powi(-(n.min(64) as i32)), center, vp, cx)
            }
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
                let Some((id, page)) = self.reader.as_ref().map(|r| (r.book_id.clone(), r.page))
                else {
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
            Action::SetAnimations(value) => {
                let enabled = value.unwrap_or(!self.library.settings().animate);
                self.library.settings_mut().animate = enabled;
                self.save_library();
                let message = match (enabled, self.reduce_motion) {
                    (true, true) => {
                        "animations on (the desktop asks for reduced motion, so they stay off)"
                    }
                    (true, false) => "animations on",
                    (false, _) => "animations off",
                };
                self.show_hud(message, cx);
            }
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
    fn sync_reader(&mut self, window: &mut Window, cx: &mut Context<Self>, dt: f32) -> bool {
        let vp = viewport_of(window);
        let animate = self.animations_enabled();

        let (wanted, animating): (Vec<(usize, u32)>, bool) = {
            let Some(reader) = self.reader.as_mut() else {
                return false;
            };
            reader.viewport = vp;
            reader.animate = animate;
            let Some(pages) = reader.pages.clone() else {
                return false;
            };
            reader.settle(&vp);
            let animating = reader.advance_animations(dt, &vp);

            // Keep a bounded window of decoded pages around the current one, and stay under the
            // memory budget even with very large pages.
            let entries: Vec<pagecache::Entry> = reader
                .images
                .iter()
                .map(|(index, page)| pagecache::Entry {
                    index: *index,
                    bytes: page.bytes,
                })
                .collect();
            for ix in pagecache::plan_eviction(&entries, reader.page, pagecache::BUDGET_BYTES) {
                if let Some(page) = reader.images.remove(&ix) {
                    // A page that is still sliding out is freed when its slide ends instead.
                    let leaving = reader
                        .outgoing
                        .as_ref()
                        .is_some_and(|outgoing| Arc::ptr_eq(&outgoing.image, &page.image));
                    if !leaving {
                        let _ = window.drop_image(page.image);
                    }
                }
            }

            // The current page needs enough pixels for the current zoom; its neighbours only
            // for zoom 1, since a page turn keeps the fit mode but you start unzoomed there.
            let long_side = |display: (f32, f32)| display.0.max(display.1) * vp.scale;
            let (current_tier, neighbour_tier) = match reader.current().map(Reader::orig_of) {
                Some(orig) => (
                    layout::tier_for(long_side(layout::display_size(
                        reader.fit,
                        reader.zoom,
                        orig,
                        &vp,
                    ))),
                    layout::tier_for(long_side(layout::display_size(reader.fit, 1.0, orig, &vp))),
                ),
                None => {
                    let tier = layout::tier_for(vp.width.max(vp.height) * vp.scale);
                    (tier, tier)
                }
            };

            let wanted = pagecache::prefetch_order(reader.page, pages.len())
                .into_iter()
                .map(|ix| {
                    (
                        ix,
                        if ix == reader.page {
                            current_tier
                        } else {
                            neighbour_tier
                        },
                    )
                })
                .collect();
            (wanted, animating)
        };

        // Textures released by finished slides are freed on the next frame.
        if let Some(reader) = self.reader.as_mut() {
            self.garbage.append(&mut reader.released);
        }
        for (ix, tier) in wanted {
            self.request_page(ix, tier, cx);
        }
        animating
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
            let (width, height) = decoded.image.dimensions();
            anyhow::Ok((
                Arc::new(RenderImage::new(vec![Frame::new(decoded.image)])),
                decoded.orig,
                u64::from(width) * u64::from(height) * 4,
            ))
        });

        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.page_decoded(book_id, ix, tier, result, cx)
            })
            .ok();
        })
        .detach();
    }

    fn page_decoded(
        &mut self,
        book_id: String,
        ix: usize,
        tier: u32,
        result: Result<(Arc<RenderImage>, (u32, u32), u64)>,
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
            Ok((image, orig, bytes)) => {
                if let Some(old) = reader.images.insert(
                    ix,
                    Page {
                        image,
                        orig,
                        tier,
                        bytes,
                    },
                ) {
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
            .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| {
                cx.stop_propagation()
            })
            .on_mouse_down(MouseButton::Right, |_: &MouseDownEvent, _, cx| {
                cx.stop_propagation()
            });

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

    fn menu_item(
        &self,
        menu: &BookMenu,
        ix: usize,
        item: &Item,
        cx: &mut Context<Self>,
    ) -> AnyElement {
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
            .on_mouse_move(
                cx.listener(move |this, _: &MouseMoveEvent, _, cx| this.menu_hover(ix, cx)),
            )
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
            .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| {
                cx.stop_propagation()
            })
            .on_mouse_down(MouseButton::Right, |_: &MouseDownEvent, _, cx| {
                cx.stop_propagation()
            })
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
                .child(div().flex_1().min_w_0().truncate().child(value.clone()))
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
                    .child(div().flex().flex_row().justify_end().child(panel_button(
                        "properties-close",
                        "Close",
                        ButtonKind::Normal,
                        true,
                        cx.listener(|this, _: &ClickEvent, _, cx| this.close_overlay(cx)),
                    ))),
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
                                        div().text_color(rgb(MUTED)).child(format!(
                                            "{}%",
                                            (opacity * 100.0).round() as u32
                                        )),
                                    ),
                            )
                            .child(track)
                            .child(div().text_xs().text_color(rgb(MUTED)).child(
                                "Lower values let the desktop show through the window \
                                     background. Pages, covers and text stay fully opaque.",
                            )),
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
                                cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.request_reset_settings(cx)
                                }),
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

        let selected = self.selection.len();
        let subtitle = if selected > 0 {
            format!("{selected} of {count} selected")
        } else {
            format!("{count} {}", if count == 1 { "book" } else { "books" })
        };

        let right = div()
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
            }));
        // While books are selected the header offers what you can do with the selection instead
        // of importing and settings; Esc or Cancel goes back.
        let right = if selected > 0 {
            let right = if selected < count {
                right.child(button(
                    "select-all",
                    "Select all",
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        let order = this.library_order();
                        this.selection.select_all(&order);
                        cx.notify();
                    }),
                ))
            } else {
                right
            };
            right
                .child(button_colored(
                    "select-remove",
                    "Remove",
                    ACCENT,
                    cx.listener(|this, _: &ClickEvent, _, cx| this.request_remove_selected(cx)),
                ))
                .child(button(
                    "select-cancel",
                    "Cancel",
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.selection.clear();
                        cx.notify();
                    }),
                ))
        } else {
            right
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
                ))
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
                    .child(div().text_sm().text_color(rgb(MUTED)).child(subtitle)),
            )
            .child(right);

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
                .child(div().text_sm().text_color(rgb(MUTED)).child(
                    "Drop CBZ files or folders onto this window, or use Add files (Ctrl+O).",
                ))
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

        let selected = self.selection.contains(&book.id);
        let cover = div()
            .relative()
            .w(px(CARD_W))
            .h(px(CARD_H))
            .flex_none()
            .rounded_md()
            .overflow_hidden()
            .bg(rgb(SURFACE))
            .border_2()
            .border_color(if selected { rgb(ACCENT) } else { rgb(SURFACE) })
            .child(
                img(self.dirs.cover(&book.id))
                    .w_full()
                    .h_full()
                    .object_fit(ObjectFit::Cover),
            )
            .children(selected.then(|| {
                // A tint over the cover so a selected book reads as selected at a glance.
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .bg(rgba(0xe2725b38))
            }));

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
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if this.overlay_recently_closed() {
                    return;
                }
                // Ctrl/Shift-click, or any click while books are selected, edits the selection;
                // only a plain click on an unselected library opens the book.
                let modifiers = event.modifiers();
                if this.select_click(&open_id, modifiers.control, modifiers.shift, cx) {
                    return;
                }
                this.open_book(&open_id, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.open_context_menu(&menu_id, event.position, cx);
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

        // Pages are drawn at a size and position we compute ourselves, so what you see is exactly
        // what the fit mode and zoom say, independent of the texture's pixel size. During a page
        // turn the outgoing and incoming pages are both drawn, shifted along the same strip.
        let mut content: Vec<AnyElement> = Vec::new();
        if let Some(error) = &reader.error {
            content.push(centered(format!("Couldn't open this book: {error}")));
        } else {
            let (shift_out, shift_in) = match &reader.transition {
                Some(transition) => (
                    transition.outgoing_shift() * vp.width,
                    transition.incoming_shift() * vp.width,
                ),
                None => (0.0, 0.0),
            };
            if let Some(outgoing) = &reader.outgoing {
                content.push(page_layer(
                    &outgoing.image,
                    outgoing.origin.0 + shift_out,
                    outgoing.origin.1,
                    outgoing.display,
                ));
            }
            if let Some(page) = reader.current() {
                let display =
                    layout::display_size(reader.fit, reader.zoom, Reader::orig_of(page), &vp);
                let (left, top) = layout::origin(reader.off, display, &vp);
                content.push(page_layer(&page.image, left + shift_in, top, display));
            } else if reader.outgoing.is_none() {
                if let Some(error) = reader.failed.get(&reader.page) {
                    content.push(centered(format!(
                        "Couldn't decode page {}: {error}",
                        reader.page + 1
                    )));
                } else if reader.pages.is_none() {
                    content.push(centered("Opening…"));
                }
                // otherwise: blank for the moment it takes the first page to decode
            }
        }

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
                    if reader.rtl {
                        "right to left"
                    } else {
                        "left to right"
                    }
                ),
            )
        });
        let hud = self
            .hud
            .clone()
            .map(|text| pill(div().absolute().top_4().left_0().right_0(), text));

        let vim_status = reader.vim.status().map(|text| {
            let text = if reader.vim.in_cmdline() {
                format!("{text}▏")
            } else {
                text
            };
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
            .children(content)
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
        // Animations are driven by the time between frames, never by a frame count, and only
        // ask for another frame while something is actually moving.
        let now = Instant::now();
        let dt = self.frame_delta(now);
        if self.sync_reader(window, cx, dt) {
            self.last_frame = Some(now);
            window.request_animation_frame();
        } else {
            self.last_frame = None;
        }
        // Textures that a finished slide just let go of are not drawn this frame, so free them
        // now rather than waiting for a frame that an idle window might never draw.
        for image in self.garbage.drain(..) {
            let _ = window.drop_image(image);
        }
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

/// One page image at an exact position and size (see `render_reader`).
fn page_layer(image: &Arc<RenderImage>, left: f32, top: f32, display: (f32, f32)) -> AnyElement {
    img(image.clone())
        .absolute()
        .left(px(left))
        .top(px(top))
        .w(px(display.0))
        .h(px(display.1))
        .object_fit(ObjectFit::Fill)
        .into_any_element()
}

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
            (":animate [on|off]", "page-turn slide and scroll glide"),
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
                        .child(
                            div()
                                .w(px(190.))
                                .flex_none()
                                .text_color(rgb(ACCENT))
                                .child(*keys),
                        )
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
    button_colored(id, label, TEXT, on_click)
}

/// A header button with a custom text colour (the accent for destructive actions).
fn button_colored(
    id: &'static str,
    label: &'static str,
    color: u32,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let base = div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .bg(rgb(SURFACE))
        .text_sm()
        .text_color(rgb(color))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(SURFACE_HI)))
        .on_click(on_click)
        .child(label);
    #[cfg(test)]
    let base = base.debug_selector(|| id.into());
    base
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
    use std::{
        fs,
        io::{Cursor, Write},
    };

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

    fn launch<'a>(
        fx: &Fixture,
        cx: &'a mut TestAppContext,
    ) -> (Entity<Yomu>, &'a mut VisualTestContext) {
        let dirs = fx.dirs.clone();
        cx.update(|cx| cx.bind_keys([KeyBinding::new("ctrl-,", OpenSettings, None)]));
        cx.add_window_view(move |window, cx| {
            let yomu = Yomu::with_dirs(dirs, cx);
            window.focus(&yomu.focus_handle);
            yomu
        })
    }

    fn book(view: &Entity<Yomu>, cx: &mut VisualTestContext, id: &str) -> Book {
        view.read_with(cx, |this, _| {
            this.library.get(id).cloned().expect("book exists")
        })
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
        view.update_in(cx, move |this, window, cx| {
            this.run_book_action(action, &id, window, cx)
        });
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
        view.update(cx, move |this, cx| {
            this.open_book_menu(&id, point(px(100.), px(100.)), cx)
        });
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
            this.library
                .get_mut(&fx.ids[1])
                .unwrap()
                .marks
                .insert("a".into(), 2);
        });

        open_menu(&view, cx, &id);
        assert!(menu_labels(&view, cx).contains(&"Resume Reading".to_string()));
        act(&view, cx, &id, BookAction::ResetProgress);
        assert_eq!(overlay_kind(&view, cx), "confirm");
        assert_eq!(
            book(&view, cx, &id).current_page,
            3,
            "nothing changes before confirming"
        );

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
    fn remove_never_deletes_the_file_and_confirms_only_when_progress_would_be_lost(
        cx: &mut TestAppContext,
    ) {
        let fx = fixture("remove");
        let (view, cx) = launch(&fx, cx);

        // unread: removed straight away
        let (a, b) = (fx.ids[0].clone(), fx.ids[1].clone());
        act(&view, cx, &a, BookAction::Remove);
        assert_eq!(overlay_kind(&view, cx), "none");
        assert!(saved(&fx, &a).is_none());
        assert!(fx.files[0].exists(), "the comic file must survive");
        assert!(
            !fx.dirs.cover(&a).exists(),
            "the cached cover is cleaned up"
        );

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
        assert_eq!(
            reading(&view, cx),
            Some(3),
            "resume lands on the saved page"
        );
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
        assert_eq!(
            reading(&view, cx),
            None,
            "right-click must not open the book"
        );
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
        assert_eq!(
            reading(&view, cx),
            None,
            "the click only dismisses the menu"
        );

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

        for (x, y) in [
            (2.0, 2.0),
            (width - 2.0, 2.0),
            (2.0, height - 2.0),
            (width - 2.0, height - 2.0),
        ] {
            let id = id.clone();
            view.update(cx, move |this, cx| {
                this.open_book_menu(&id, point(px(x), px(y)), cx)
            });
            cx.run_until_parked();
            let bounds = cx.debug_bounds("book-menu").expect("menu was painted");
            let (left, top) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
            let right = left + f32::from(bounds.size.width);
            let bottom = top + f32::from(bounds.size.height);
            assert!(left >= 0.0 && top >= 0.0, "({x},{y}) -> {bounds:?}");
            assert!(
                right <= width && bottom <= height,
                "({x},{y}) -> {bounds:?}"
            );
            close(&view, cx);
        }

        // away from the edges it opens exactly where you clicked
        view.update(cx, |this, cx| {
            this.open_book_menu(&fx.ids[0], point(px(300.), px(200.)), cx)
        });
        cx.run_until_parked();
        let bounds = cx.debug_bounds("book-menu").unwrap();
        assert_eq!(
            (f32::from(bounds.origin.x), f32::from(bounds.origin.y)),
            (300.0, 200.0)
        );
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
        assert_eq!(
            selected(&view, cx),
            Some(4),
            "skips separators: Start, Mark as Read, Open Location"
        );
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
        assert_eq!(
            reading(&view, cx),
            Some(0),
            "reader keys are ignored behind a dialog"
        );
        cx.simulate_keystrokes("escape");
        assert_eq!(overlay_kind(&view, cx), "none");
        assert_eq!(
            reading(&view, cx),
            Some(0),
            "escape closed the dialog, not the book"
        );
        cx.simulate_keystrokes("J");
        assert_eq!(
            reading(&view, cx),
            Some(1),
            "the reader works again afterwards"
        );
    }

    // ---------------------------------------------------------------------------------------
    // Settings
    // ---------------------------------------------------------------------------------------

    #[gpui::test]
    fn settings_open_with_shortcut_and_header_button_and_close_with_escape(
        cx: &mut TestAppContext,
    ) {
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
        assert!(
            (saved_settings(&fx).opacity - 0.90).abs() < 1e-4,
            "saved on every change"
        );
        cx.simulate_keystrokes("right");
        assert!((opacity(&view, cx) - 0.95).abs() < 1e-4);
        cx.simulate_keystrokes("home");
        assert!((opacity(&view, cx) - OPACITY_MIN).abs() < 1e-4);
        cx.simulate_keystrokes("left");
        assert!(
            (opacity(&view, cx) - OPACITY_MIN).abs() < 1e-4,
            "cannot go below the minimum"
        );
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

        cx.simulate_mouse_down(
            point(left + px(1.), y),
            MouseButton::Left,
            Modifiers::default(),
        );
        assert!(
            (opacity(&view, cx) - OPACITY_MIN).abs() < 1e-4,
            "pressing at the left end"
        );
        assert!(
            (saved_settings(&fx).opacity - 1.0).abs() < 1e-4,
            "not saved while dragging"
        );

        let middle = point((left + right) / 2.0, y);
        cx.simulate_mouse_move(middle, MouseButton::Left, Modifiers::default());
        let expected = (OPACITY_MIN + OPACITY_MAX) / 2.0;
        assert!(
            (opacity(&view, cx) - expected).abs() < 0.02,
            "dragging to the middle"
        );

        // dragging past the end (even off the track) clamps
        cx.simulate_mouse_move(
            point(right + px(200.), y),
            MouseButton::Left,
            Modifiers::default(),
        );
        assert_eq!(opacity(&view, cx), OPACITY_MAX);

        cx.simulate_mouse_move(middle, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(middle, MouseButton::Left, Modifiers::default());
        let value = opacity(&view, cx);
        assert!(
            (saved_settings(&fx).opacity - value).abs() < 1e-4,
            "saved on release"
        );
        assert_eq!(
            overlay_kind(&view, cx),
            "settings",
            "dragging does not dismiss the panel"
        );
    }

    #[gpui::test]
    fn window_appearance_switches_between_opaque_and_transparent(cx: &mut TestAppContext) {
        let fx = fixture("appearance");
        let (view, cx) = launch(&fx, cx);
        let applied = |view: &Entity<Yomu>, cx: &mut VisualTestContext| {
            view.read_with(cx, |this, _| this.applied_opaque)
        };
        assert_eq!(
            applied(&view, cx),
            Some(true),
            "100% uses the opaque appearance"
        );

        view.update(cx, |this, cx| this.set_opacity(0.6, true, cx));
        cx.run_until_parked();
        assert_eq!(
            applied(&view, cx),
            Some(false),
            "below 100% needs real transparency"
        );

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
            settings.rtl = true;
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
        assert_eq!(
            overlay_kind(&view, cx),
            "settings",
            "back on the panel to see the result"
        );
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
            this.set_direction(true, cx);
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
        assert!(!rtl, "left to right is the default");
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
        assert_eq!(
            view.read_with(cx, |this, _| this.library.settings().clone()),
            Settings::default()
        );
    }

    // ---------------------------------------------------------------------------------------
    // Multiple selection and bulk removal
    // ---------------------------------------------------------------------------------------

    fn click_with(cx: &mut VisualTestContext, at: Point<Pixels>, modifiers: Modifiers) {
        cx.simulate_mouse_down(at, MouseButton::Left, modifiers);
        cx.simulate_mouse_up(at, MouseButton::Left, modifiers);
    }

    fn ctrl_click(cx: &mut VisualTestContext, index: usize) {
        click_with(cx, card(index), Modifiers::control());
    }

    fn selected(view: &Entity<Yomu>, cx: &mut VisualTestContext) -> Vec<String> {
        view.read_with(cx, |this, _| this.selected_ids())
    }

    fn book_count(view: &Entity<Yomu>, cx: &mut VisualTestContext) -> usize {
        view.read_with(cx, |this, _| this.library.books().len())
    }

    #[gpui::test]
    fn ctrl_click_selects_without_opening_and_plain_clicks_then_toggle(cx: &mut TestAppContext) {
        let fx = fixture("selectclick");
        let (view, cx) = launch(&fx, cx);

        ctrl_click(cx, 0);
        assert_eq!(selected(&view, cx), vec![fx.ids[0].clone()]);
        assert_eq!(reading(&view, cx), None, "ctrl+click never opens a book");

        // selection mode: a plain click adds, and clicking a selected book removes it
        click(cx, card(2), MouseButton::Left);
        assert_eq!(
            selected(&view, cx),
            vec![fx.ids[0].clone(), fx.ids[2].clone()]
        );
        assert_eq!(reading(&view, cx), None);
        click(cx, card(0), MouseButton::Left);
        assert_eq!(selected(&view, cx), vec![fx.ids[2].clone()]);

        // deselecting the last one leaves selection mode: a plain click opens again
        click(cx, card(2), MouseButton::Left);
        assert!(selected(&view, cx).is_empty());
        click(cx, card(1), MouseButton::Left);
        assert_eq!(reading(&view, cx), Some(0));
        assert!(selected(&view, cx).is_empty());
    }

    #[gpui::test]
    fn shift_click_selects_a_range(cx: &mut TestAppContext) {
        let fx = fixture("shiftclick");
        let (view, cx) = launch(&fx, cx);

        ctrl_click(cx, 0);
        click_with(cx, card(2), Modifiers::shift());
        assert_eq!(selected(&view, cx), fx.ids);
        assert_eq!(reading(&view, cx), None);
    }

    #[gpui::test]
    fn select_all_and_escape(cx: &mut TestAppContext) {
        let fx = fixture("selectall");
        let (view, cx) = launch(&fx, cx);

        cx.simulate_keystrokes("ctrl-a");
        assert_eq!(selected(&view, cx), fx.ids);
        cx.simulate_keystrokes("escape");
        assert!(selected(&view, cx).is_empty());
        assert_eq!(
            reading(&view, cx),
            None,
            "escape in the library does nothing else"
        );
    }

    #[gpui::test]
    fn delete_removes_the_selection_at_once_when_nothing_is_lost(cx: &mut TestAppContext) {
        let fx = fixture("bulkdelete");
        let (view, cx) = launch(&fx, cx);

        ctrl_click(cx, 0);
        ctrl_click(cx, 1);
        cx.simulate_keystrokes("delete");
        assert_eq!(
            overlay_kind(&view, cx),
            "none",
            "no confirmation: nothing would be lost"
        );
        assert_eq!(book_count(&view, cx), 1);
        assert!(selected(&view, cx).is_empty());
        let on_disk = Library::load(&fx.dirs).unwrap();
        assert_eq!(on_disk.books().len(), 1, "saved once, for the whole batch");
        assert_eq!(on_disk.books()[0].id, fx.ids[2]);
        assert!(
            fx.files.iter().all(|file| file.exists()),
            "comic files are never deleted"
        );
        assert!(!fx.dirs.cover(&fx.ids[0]).exists() && !fx.dirs.cover(&fx.ids[1]).exists());
        let toast = view.read_with(cx, |this, _| this.toast.clone().map(|t| t.to_string()));
        assert_eq!(toast.as_deref(), Some("Removed 2 books from the library"));
    }

    #[gpui::test]
    fn bulk_removal_asks_first_when_progress_would_be_lost(cx: &mut TestAppContext) {
        let fx = fixture("bulkconfirm");
        let (view, cx) = launch(&fx, cx);
        set_progress(&view, cx, &fx.ids[1], 3);

        cx.simulate_keystrokes("ctrl-a");
        cx.simulate_keystrokes("delete");
        assert_eq!(overlay_kind(&view, cx), "confirm");
        assert_eq!(
            book_count(&view, cx),
            3,
            "nothing is removed before confirming"
        );

        // cancelling keeps both the books and the selection
        cx.simulate_keystrokes("escape");
        assert_eq!(overlay_kind(&view, cx), "none");
        assert_eq!(book_count(&view, cx), 3);
        assert_eq!(selected(&view, cx).len(), 3);

        cx.simulate_keystrokes("delete");
        cx.simulate_keystrokes("enter");
        assert_eq!(book_count(&view, cx), 0);
        assert!(selected(&view, cx).is_empty());
        assert!(fx.files.iter().all(|file| file.exists()));
        assert_eq!(Library::load(&fx.dirs).unwrap().books().len(), 0);
    }

    #[gpui::test]
    fn delete_with_nothing_selected_does_nothing(cx: &mut TestAppContext) {
        let fx = fixture("deletenothing");
        let (view, cx) = launch(&fx, cx);
        cx.simulate_keystrokes("delete");
        assert_eq!(book_count(&view, cx), 3);
        assert_eq!(overlay_kind(&view, cx), "none");
    }

    #[gpui::test]
    fn header_buttons_manage_the_selection(cx: &mut TestAppContext) {
        let fx = fixture("headerbuttons");
        let (view, cx) = launch(&fx, cx);
        // (gpui keeps the last painted bounds of every selector, so only "has it ever appeared"
        // can be checked for these, not "has it disappeared")
        assert!(
            cx.debug_bounds("select-remove").is_none(),
            "no selection controls until needed"
        );
        assert!(cx.debug_bounds("add-files").is_some());

        ctrl_click(cx, 0);
        let select_all = cx
            .debug_bounds("select-all")
            .expect("the header offers Select all");
        click(cx, select_all.center(), MouseButton::Left);
        assert_eq!(selected(&view, cx), fx.ids);

        let cancel = cx.debug_bounds("select-cancel").unwrap();
        click(cx, cancel.center(), MouseButton::Left);
        assert!(selected(&view, cx).is_empty());

        ctrl_click(cx, 0);
        ctrl_click(cx, 1);
        let remove = cx.debug_bounds("select-remove").unwrap();
        click(cx, remove.center(), MouseButton::Left);
        assert_eq!(book_count(&view, cx), 1);
        assert!(selected(&view, cx).is_empty());
    }

    #[gpui::test]
    fn right_click_on_a_selected_book_offers_bulk_actions(cx: &mut TestAppContext) {
        let fx = fixture("bulkmenu");
        let (view, cx) = launch(&fx, cx);

        ctrl_click(cx, 0);
        ctrl_click(cx, 1);
        click(cx, card(1), MouseButton::Right);
        assert_eq!(overlay_kind(&view, cx), "menu");
        assert_eq!(
            menu_labels(&view, cx),
            vec![
                "Clear Selection".to_string(),
                "Remove 2 Books from Library".to_string()
            ]
        );
        assert_eq!(
            selected(&view, cx).len(),
            2,
            "opening the menu keeps the selection"
        );

        cx.simulate_keystrokes("down enter");
        assert!(selected(&view, cx).is_empty());
        assert_eq!(book_count(&view, cx), 3);

        // (clicks right after a menu closes are ignored on purpose)
        std::thread::sleep(Duration::from_millis(350));
        ctrl_click(cx, 0);
        ctrl_click(cx, 2);
        click(cx, card(2), MouseButton::Right);
        cx.simulate_keystrokes("end enter");
        assert_eq!(book_count(&view, cx), 1);
        assert_eq!(Library::load(&fx.dirs).unwrap().books()[0].id, fx.ids[1]);
    }

    #[gpui::test]
    fn right_click_on_an_unselected_book_drops_the_selection(cx: &mut TestAppContext) {
        let fx = fixture("rightunselected");
        let (view, cx) = launch(&fx, cx);

        ctrl_click(cx, 0);
        ctrl_click(cx, 1);
        click(cx, card(2), MouseButton::Right);
        assert!(selected(&view, cx).is_empty());
        assert_eq!(
            menu_labels(&view, cx)[0],
            "Start Reading",
            "the normal single-book menu"
        );

        // a single selected book also gets the normal menu, not a "1 book" bulk menu
        close(&view, cx);
        std::thread::sleep(Duration::from_millis(350));
        ctrl_click(cx, 0);
        click(cx, card(0), MouseButton::Right);
        assert_eq!(menu_labels(&view, cx)[0], "Start Reading");
    }

    #[gpui::test]
    fn removing_one_book_updates_the_selection_and_opening_clears_it(cx: &mut TestAppContext) {
        let fx = fixture("pruneselection");
        let (view, cx) = launch(&fx, cx);

        ctrl_click(cx, 0);
        ctrl_click(cx, 1);
        act(&view, cx, &fx.ids[0], BookAction::Remove);
        assert_eq!(
            selected(&view, cx),
            vec![fx.ids[1].clone()],
            "the removed book left the selection"
        );

        act(&view, cx, &fx.ids[1], BookAction::Open);
        assert_eq!(reading(&view, cx), Some(0));
        assert!(
            selected(&view, cx).is_empty(),
            "opening a book clears the selection"
        );
    }

    #[gpui::test]
    fn selection_keys_do_not_leak_into_the_reader(cx: &mut TestAppContext) {
        let fx = fixture("readerkeys");
        let (view, cx) = launch(&fx, cx);
        act(&view, cx, &fx.ids[0], BookAction::Open);
        cx.simulate_keystrokes("ctrl-a");
        assert!(
            selected(&view, cx).is_empty(),
            "Ctrl+A only selects in the library"
        );
        cx.simulate_keystrokes("delete");
        assert_eq!(book_count(&view, cx), 3);
    }

    // ---------------------------------------------------------------------------------------
    // Reading direction, page-turn slide, smooth scrolling
    // ---------------------------------------------------------------------------------------

    const TEST_VIEWPORT: Viewport = Viewport {
        width: 800.0,
        height: 600.0,
        scale: 1.0,
    };

    fn fake_page(shade: u8) -> Page {
        let image = image::RgbaImage::from_pixel(60, 90, image::Rgba([shade, 0, 0, 255]));
        Page {
            image: Arc::new(RenderImage::new(vec![Frame::new(image)])),
            orig: (60, 90),
            tier: 1024,
            bytes: 60 * 90 * 4,
        }
    }

    /// A reader with `pages` decoded pages and an 800x600 viewport, no window needed.
    fn test_reader(pages: usize, settings: &Settings) -> Reader {
        let book = Book {
            id: "b".into(),
            title: "Test".into(),
            path: PathBuf::from("/tmp/test.cbz"),
            kind: SourceKind::Archive,
            page_count: pages,
            current_page: 0,
            opened: true,
            added_at: 0,
            last_read: None,
            marks: Default::default(),
        };
        let mut reader = Reader::new(&book, settings);
        reader.pages = Some(Arc::new((0..pages).map(|i| format!("{i}.png")).collect()));
        reader.viewport = TEST_VIEWPORT;
        for index in 0..pages {
            reader.images.insert(index, fake_page(index as u8));
        }
        reader
    }

    /// Turn the page the way `Yomu::goto_page` does: start the slide, then move.
    fn turn(reader: &mut Reader, forward: bool) {
        let target = if forward {
            reader.page + 1
        } else {
            reader.page - 1
        };
        reader.begin_transition(forward, true);
        reader.page = target;
    }

    fn run_frames(reader: &mut Reader, hz: f32) -> usize {
        let mut frames = 0;
        while reader.advance_animations(1.0 / hz, &TEST_VIEWPORT) {
            frames += 1;
            assert!(frames < 5_000, "animation never stopped");
        }
        frames
    }

    #[test]
    fn default_reading_direction_is_left_to_right() {
        assert!(!Settings::default().rtl);
        assert!(!test_reader(3, &Settings::default()).rtl);
    }

    #[test]
    fn page_turn_slides_in_the_direction_of_reading() {
        for (rtl, forward, expected) in [
            (false, true, -1.0), // left to right, next page: content moves left
            (false, false, 1.0), // left to right, previous page: content moves right
            (true, true, 1.0),   // right to left (manga) mirrors it
            (true, false, -1.0),
        ] {
            let settings = Settings {
                rtl,
                ..Settings::default()
            };
            let mut reader = test_reader(4, &settings);
            reader.page = 2;
            let leaving = reader.images[&2].image.clone();
            turn(&mut reader, forward);
            let transition = reader.transition.expect("a slide started");
            assert_eq!(transition.travel(), expected, "rtl={rtl} forward={forward}");
            assert!(Arc::ptr_eq(
                &reader.outgoing.as_ref().unwrap().image,
                &leaving
            ));
        }
    }

    #[test]
    fn slide_finishes_and_never_frees_a_texture_the_cache_still_holds() {
        let mut reader = test_reader(3, &Settings::default());
        turn(&mut reader, true);
        let frames = run_frames(&mut reader, 144.0);
        assert!(
            frames > 5,
            "the slide spans several frames, not a jump ({frames})"
        );
        assert!(reader.transition.is_none() && reader.outgoing.is_none());
        assert!(reader.released.is_empty());
    }

    #[test]
    fn an_evicted_outgoing_page_is_released_exactly_once_when_its_slide_ends() {
        let mut reader = test_reader(3, &Settings::default());
        let leaving = reader.images[&0].image.clone();
        turn(&mut reader, true);
        reader.images.remove(&0); // evicted while still sliding out
        run_frames(&mut reader, 60.0);
        assert_eq!(reader.released.len(), 1);
        assert!(Arc::ptr_eq(&reader.released[0], &leaving));
    }

    #[test]
    fn rapid_turns_replace_the_slide_instead_of_queueing_and_continue_from_where_the_page_is() {
        let mut reader = test_reader(5, &Settings::default());
        turn(&mut reader, true);
        for _ in 0..4 {
            reader.advance_animations(1.0 / 120.0, &TEST_VIEWPORT);
        }
        let on_screen = reader.transition.unwrap().incoming_shift();
        assert!(on_screen > 0.0 && on_screen < 1.0, "mid-slide");

        let first_out = reader.images[&0].image.clone();
        reader.images.remove(&0);
        let second_out = reader.images[&1].image.clone();
        turn(&mut reader, true); // page 1 -> 2 while 0 -> 1 is still sliding
        assert!(Arc::ptr_eq(
            &reader.outgoing.as_ref().unwrap().image,
            &second_out
        ));
        assert_eq!(
            reader.transition.unwrap().outgoing_shift(),
            on_screen,
            "no jump when interrupted"
        );
        assert!(
            reader
                .released
                .iter()
                .any(|image| Arc::ptr_eq(image, &first_out)),
            "the page that was leaving is dropped, not kept around"
        );
        assert_eq!(reader.released.len(), 1);

        // keep mashing: still exactly one slide, and it settles
        for _ in 0..2 {
            turn(&mut reader, true);
        }
        assert_eq!(reader.page, 4);
        run_frames(&mut reader, 144.0);
        assert!(reader.transition.is_none() && reader.outgoing.is_none());
    }

    #[test]
    fn pressing_back_mid_slide_reverses_smoothly() {
        let mut reader = test_reader(4, &Settings::default());
        reader.page = 1;
        turn(&mut reader, true);
        for _ in 0..3 {
            reader.advance_animations(1.0 / 120.0, &TEST_VIEWPORT);
        }
        let on_screen = reader.transition.unwrap().incoming_shift();
        turn(&mut reader, false);
        let transition = reader.transition.unwrap();
        assert_eq!(transition.travel(), 1.0, "now moving the other way");
        assert_eq!(transition.outgoing_shift(), on_screen);
    }

    #[test]
    fn no_slide_when_animations_are_off_the_page_is_not_ready_or_there_is_no_window() {
        // animations off
        let mut reader = test_reader(3, &Settings::default());
        reader.begin_transition(true, false);
        assert!(reader.transition.is_none() && reader.outgoing.is_none());

        // the page on screen is not decoded yet (initial loading)
        let mut reader = test_reader(3, &Settings::default());
        reader.images.remove(&0);
        reader.begin_transition(true, true);
        assert!(reader.transition.is_none() && reader.outgoing.is_none());

        // no window size yet
        let mut reader = test_reader(3, &Settings::default());
        reader.viewport = Viewport::EMPTY;
        reader.begin_transition(true, true);
        assert!(reader.transition.is_none());

        // a freshly opened book never starts with a slide
        assert!(test_reader(3, &Settings::default()).transition.is_none());
    }

    #[test]
    fn resizing_the_window_cancels_the_slide() {
        let mut reader = test_reader(3, &Settings::default());
        turn(&mut reader, true);
        assert!(reader.transition.is_some());
        let resized = Viewport {
            width: 900.0,
            ..TEST_VIEWPORT
        };
        reader.advance_animations(0.016, &resized);
        assert!(reader.transition.is_none() && reader.outgoing.is_none());
    }

    #[test]
    fn the_slide_waits_for_the_new_page_instead_of_showing_a_blank() {
        let mut reader = test_reader(4, &Settings::default());
        reader.begin_transition(true, true);
        reader.page = 3;
        reader.images.remove(&3); // still decoding

        assert!(
            !reader.advance_animations(0.1, &TEST_VIEWPORT),
            "no frames requested while waiting"
        );
        let waiting = reader.transition.expect("still pending");
        assert_eq!(
            waiting.outgoing_shift(),
            0.0,
            "the old page stays where it was"
        );

        reader.images.insert(3, fake_page(3));
        assert!(
            reader.advance_animations(0.016, &TEST_VIEWPORT),
            "starts moving once the page is ready"
        );
        assert!(reader.transition.unwrap().outgoing_shift() != 0.0);
    }

    #[test]
    fn a_page_that_fails_to_decode_ends_the_slide() {
        let mut reader = test_reader(3, &Settings::default());
        reader.begin_transition(true, true);
        reader.page = 1;
        reader.failed.insert(1, "broken".into());
        reader.advance_animations(0.016, &TEST_VIEWPORT);
        assert!(reader.transition.is_none() && reader.outgoing.is_none());
    }

    #[test]
    fn scroll_glides_to_its_target_and_then_stops_asking_for_frames() {
        let mut reader = test_reader(2, &Settings::default());
        reader.scroll_target = (0.0, 300.0);
        let mut last = 0.0;
        let mut frames = 0;
        while reader.advance_animations(1.0 / 144.0, &TEST_VIEWPORT) {
            assert!(
                reader.off.1 >= last && reader.off.1 <= 300.0,
                "monotonic, no overshoot"
            );
            assert!(reader.off.1 < 300.0 || frames > 0);
            last = reader.off.1;
            frames += 1;
            assert!(frames < 1_000);
        }
        assert_eq!(reader.off, (0.0, 300.0));
        assert!(frames > 10, "a glide, not a jump ({frames} frames)");
        assert!(
            !reader.advance_animations(1.0 / 144.0, &TEST_VIEWPORT),
            "idle afterwards"
        );
    }

    #[test]
    fn scroll_glide_takes_the_same_time_at_every_refresh_rate() {
        let mut durations = Vec::new();
        for hz in [60.0, 120.0, 144.0, 240.0] {
            let mut reader = test_reader(2, &Settings::default());
            reader.scroll_target = (0.0, 400.0);
            let frames = run_frames(&mut reader, hz);
            durations.push(frames as f32 / hz);
        }
        let (min, max) = durations
            .iter()
            .fold((f32::MAX, 0.0f32), |(lo, hi), d| (lo.min(*d), hi.max(*d)));
        assert!(max - min < 0.03, "{durations:?}");
    }

    #[test]
    fn with_animations_off_scrolling_snaps_and_nothing_slides() {
        let settings = Settings {
            animate: false,
            ..Settings::default()
        };
        let mut reader = test_reader(3, &settings);
        reader.scroll_target = (0.0, 250.0);
        assert!(!reader.advance_animations(0.016, &TEST_VIEWPORT));
        assert_eq!(reader.off, (0.0, 250.0));
        reader.begin_transition(true, reader.animate);
        assert!(reader.transition.is_none());
    }

    #[test]
    fn closing_a_book_hands_back_every_texture() {
        let mut reader = test_reader(4, &Settings::default());
        turn(&mut reader, true);
        reader.images.remove(&0);
        reader.released.push(fake_page(9).image);
        // 3 cached + 1 outgoing (no longer cached) + 1 already released
        assert_eq!(reader.into_images().len(), 5);
    }

    // ---- through the real views ----

    fn with_reader<T>(
        view: &Entity<Yomu>,
        cx: &mut VisualTestContext,
        f: impl FnOnce(&Reader) -> T,
    ) -> T {
        view.read_with(cx, |this, _| {
            f(this.reader.as_ref().expect("a book is open"))
        })
    }

    fn open_book_by_index(
        fx: &Fixture,
        view: &Entity<Yomu>,
        cx: &mut VisualTestContext,
        index: usize,
    ) {
        act(view, cx, &fx.ids[index], BookAction::Open);
        cx.run_until_parked();
        assert!(with_reader(view, cx, |r| r.pages.is_some() && r.current().is_some()));
    }

    /// Run animation frames with a fixed 120 Hz clock until nothing is moving.
    fn settle_animations(view: &Entity<Yomu>, cx: &mut VisualTestContext) {
        view.update(cx, |this, _| this.frame_dt = Some(1.0 / 120.0));
        for _ in 0..600 {
            cx.run_until_parked();
            if view.read_with(cx, |this, _| this.last_frame.is_none()) {
                return;
            }
            view.update(cx, |_, cx| cx.notify());
        }
        panic!("animation never settled");
    }

    fn page(view: &Entity<Yomu>, cx: &mut VisualTestContext) -> usize {
        with_reader(view, cx, |r| r.page)
    }

    #[gpui::test]
    fn left_to_right_navigation_in_both_directions_at_the_start_middle_and_end(
        cx: &mut TestAppContext,
    ) {
        let fx = fixture("ltr");
        let (view, cx) = launch(&fx, cx);
        open_book_by_index(&fx, &view, cx, 0);
        assert!(
            !with_reader(&view, cx, |r| r.rtl),
            "left to right by default"
        );

        cx.simulate_keystrokes("left");
        assert_eq!(page(&view, cx), 0, "cannot go before the first page");
        cx.simulate_keystrokes("right");
        assert_eq!(page(&view, cx), 1);
        cx.simulate_keystrokes("right right");
        assert_eq!(page(&view, cx), 3);
        cx.simulate_keystrokes("left");
        assert_eq!(page(&view, cx), 2, "backward from the middle");

        cx.simulate_keystrokes("shift-g");
        assert_eq!(page(&view, cx), 4);
        cx.simulate_keystrokes("right");
        assert_eq!(page(&view, cx), 4, "cannot go past the last page");
        cx.simulate_keystrokes("left");
        assert_eq!(page(&view, cx), 3, "backward from the end");
        cx.simulate_keystrokes("g g");
        assert_eq!(page(&view, cx), 0);

        // vim motions and the other next/previous keys agree
        cx.simulate_keystrokes("l");
        assert_eq!(page(&view, cx), 1);
        cx.simulate_keystrokes("h");
        assert_eq!(page(&view, cx), 0);
        cx.simulate_keystrokes("space");
        assert_eq!(page(&view, cx), 1);
        cx.simulate_keystrokes("shift-space");
        assert_eq!(page(&view, cx), 0);

        // every step was saved
        cx.simulate_keystrokes("right right");
        assert_eq!(saved(&fx, &fx.ids[0]).unwrap().current_page, 2);
    }

    #[gpui::test]
    fn right_to_left_is_an_option_and_mirrors_left_right(cx: &mut TestAppContext) {
        let fx = fixture("rtl");
        let (view, cx) = launch(&fx, cx);
        open_book_by_index(&fx, &view, cx, 0);
        view.update(cx, |this, cx| this.set_direction(true, cx));
        assert!(saved_settings(&fx).rtl, "the choice is remembered");

        cx.simulate_keystrokes("right");
        assert_eq!(
            page(&view, cx),
            0,
            "right goes back in a right-to-left book"
        );
        cx.simulate_keystrokes("left");
        assert_eq!(page(&view, cx), 1);
        cx.simulate_keystrokes("h h");
        assert_eq!(page(&view, cx), 3, "h is left, which is forward");
        cx.simulate_keystrokes("shift-g");
        cx.simulate_keystrokes("left");
        assert_eq!(page(&view, cx), 4, "no further than the last page");
        cx.simulate_keystrokes("right");
        assert_eq!(page(&view, cx), 3);
        // pages keep their order: j/k, J/K and gg/G do not depend on the direction
        cx.simulate_keystrokes("g g");
        cx.simulate_keystrokes("shift-j");
        assert_eq!(page(&view, cx), 1);

        // the next book starts with the remembered preference
        view.update_in(cx, |this, window, cx| this.close_book(window, cx));
        open_book_by_index(&fx, &view, cx, 1);
        assert!(with_reader(&view, cx, |r| r.rtl));
    }

    #[gpui::test]
    fn clicking_the_screen_edges_and_the_wheel_follow_the_reading_direction(
        cx: &mut TestAppContext,
    ) {
        let fx = fixture("clicks");
        let (view, cx) = launch(&fx, cx);
        open_book_by_index(&fx, &view, cx, 0);
        let (left, right) = (point(px(100.), px(500.)), point(px(1800.), px(500.)));

        click(cx, right, MouseButton::Left);
        assert_eq!(page(&view, cx), 1, "right edge advances left to right");
        click(cx, left, MouseButton::Left);
        assert_eq!(page(&view, cx), 0);

        view.update(cx, |this, cx| this.set_direction(true, cx));
        click(cx, left, MouseButton::Left);
        assert_eq!(page(&view, cx), 1, "the left edge advances right to left");
        click(cx, right, MouseButton::Left);
        assert_eq!(page(&view, cx), 0);

        // a wheel notch on a page that fits turns the page, whichever way you read
        for rtl in [false, true] {
            view.update(cx, |this, cx| this.set_direction(rtl, cx));
            view.update_in(cx, |this, window, cx| {
                this.on_wheel(
                    (0.0, -96.0),
                    false,
                    (0.0, 0.0),
                    true,
                    viewport_of(window),
                    cx,
                )
            });
            assert_eq!(page(&view, cx), 1);
            view.update_in(cx, |this, window, cx| {
                this.on_wheel(
                    (0.0, 96.0),
                    false,
                    (0.0, 0.0),
                    true,
                    viewport_of(window),
                    cx,
                )
            });
            assert_eq!(page(&view, cx), 0);
        }
    }

    #[gpui::test]
    fn decoded_pages_stay_in_a_small_window_whatever_the_direction(cx: &mut TestAppContext) {
        for rtl in [false, true] {
            let fx = fixture(if rtl { "cache-rtl" } else { "cache-ltr" });
            let (view, cx) = launch(&fx, cx);
            open_book_by_index(&fx, &view, cx, 0);
            view.update(cx, |this, cx| this.set_direction(rtl, cx));
            cx.simulate_keystrokes("shift-g");
            cx.run_until_parked();
            settle_animations(&view, cx);

            let (cached, current) = with_reader(&view, cx, |r| {
                let mut keys: Vec<usize> = r.images.keys().copied().collect();
                keys.sort();
                (keys, r.page)
            });
            assert_eq!(current, 4);
            assert!(
                cached.contains(&4) && cached.contains(&3),
                "the page behind is ready: {cached:?}"
            );
            assert!(
                cached.len() <= 1 + pagecache::KEEP_BEHIND + pagecache::KEEP_AHEAD,
                "{cached:?}"
            );
            let garbage = view.read_with(cx, |this, _| this.garbage.len());
            assert_eq!(garbage, 0, "no textures waiting to be freed once idle");
        }
    }

    // ---- page-turn slide through the app ----

    #[gpui::test]
    fn a_page_turn_slides_then_settles_and_position_is_never_behind(cx: &mut TestAppContext) {
        let fx = fixture("slide");
        let (view, cx) = launch(&fx, cx);
        open_book_by_index(&fx, &view, cx, 0);
        assert!(
            with_reader(&view, cx, |r| r.transition.is_none()
                && r.outgoing.is_none()),
            "no slide on opening"
        );

        view.update(cx, |this, cx| {
            this.goto_page(1, Arrive::Start, cx);
            let reader = this.reader.as_ref().unwrap();
            // the position is already on the new page; only the visual is catching up
            assert_eq!(reader.page, 1);
            assert_eq!(this.library.get(&reader.book_id).unwrap().current_page, 1);
            assert_eq!(
                reader.transition.unwrap().travel(),
                -1.0,
                "forward, left to right: content moves left"
            );
            assert!(reader.outgoing.is_some());
        });
        settle_animations(&view, cx);
        assert!(with_reader(&view, cx, |r| r.transition.is_none()
            && r.outgoing.is_none()));
        assert_eq!(page(&view, cx), 1);
        assert_eq!(view.read_with(cx, |this, _| this.garbage.len()), 0);

        // backward slides the other way, and right to left mirrors both
        view.update(cx, |this, cx| {
            this.goto_page(0, Arrive::End, cx);
            assert_eq!(
                this.reader.as_ref().unwrap().transition.unwrap().travel(),
                1.0
            );
        });
        settle_animations(&view, cx);
        view.update(cx, |this, cx| {
            this.set_direction(true, cx);
            this.goto_page(1, Arrive::Start, cx);
            assert_eq!(
                this.reader.as_ref().unwrap().transition.unwrap().travel(),
                1.0
            );
        });
        settle_animations(&view, cx);
    }

    #[gpui::test]
    fn rapid_page_turns_never_desync_the_position_from_the_picture(cx: &mut TestAppContext) {
        let fx = fixture("rapid");
        let (view, cx) = launch(&fx, cx);
        open_book_by_index(&fx, &view, cx, 0);
        let id = fx.ids[0].clone();

        view.update(cx, |this, cx| {
            for step in [
                true, true, true, true, false, false, true, false, false, false,
            ] {
                this.turn_by(step, 1, Arrive::Start, cx);
                let reader = this.reader.as_ref().unwrap();
                assert!(
                    reader.transition.is_some() || reader.outgoing.is_none(),
                    "at most one slide"
                );
                assert_eq!(
                    this.library.get(&id).unwrap().current_page,
                    reader.page,
                    "saved position tracks the page instantly"
                );
            }
        });
        settle_animations(&view, cx);
        assert_eq!(page(&view, cx), 0);
        assert!(with_reader(&view, cx, |r| r.transition.is_none()
            && r.outgoing.is_none()
            && r.released.is_empty()));
        assert_eq!(saved(&fx, &id).unwrap().current_page, 0);

        // key repeat arrives as separate key presses
        cx.simulate_keystrokes("right right right right");
        settle_animations(&view, cx);
        assert_eq!(page(&view, cx), 4);
        assert_eq!(saved(&fx, &id).unwrap().current_page, 4);
    }

    #[gpui::test]
    fn zooming_fitting_and_resizing_do_not_slide(cx: &mut TestAppContext) {
        let fx = fixture("noslide");
        let (view, cx) = launch(&fx, cx);
        open_book_by_index(&fx, &view, cx, 0);

        for action in 0..3 {
            view.update_in(cx, |this, window, cx| {
                this.goto_page(this.reader.as_ref().unwrap().page + 1, Arrive::Start, cx);
                assert!(this.reader.as_ref().unwrap().transition.is_some());
                match action {
                    0 => this.set_fit(FitMode::Width, cx),
                    1 => {
                        let vp = viewport_of(window);
                        this.zoom_by(1.25, (vp.width / 2.0, vp.height / 2.0), vp, cx);
                    }
                    _ => {
                        let reader = this.reader.as_mut().unwrap();
                        let vp = Viewport {
                            width: reader.viewport.width + 100.0,
                            ..reader.viewport
                        };
                        reader.advance_animations(0.016, &vp);
                    }
                }
                let reader = this.reader.as_ref().unwrap();
                assert!(
                    reader.transition.is_none() && reader.outgoing.is_none(),
                    "action {action}"
                );
            });
            settle_animations(&view, cx);
        }
    }

    #[gpui::test]
    fn switching_comics_starts_clean(cx: &mut TestAppContext) {
        let fx = fixture("switch");
        let (view, cx) = launch(&fx, cx);
        open_book_by_index(&fx, &view, cx, 0);
        view.update(cx, |this, cx| this.goto_page(2, Arrive::Start, cx));
        view.update_in(cx, |this, window, cx| this.close_book(window, cx));
        cx.run_until_parked();
        open_book_by_index(&fx, &view, cx, 1);
        assert!(with_reader(&view, cx, |r| r.transition.is_none()
            && r.outgoing.is_none()
            && r.page == 0));
        assert!(
            view.read_with(cx, |this, _| this.last_frame.is_none()),
            "no animation clock left running"
        );
    }

    #[gpui::test]
    fn reduced_motion_and_the_animate_command_turn_animation_off(cx: &mut TestAppContext) {
        let fx = fixture("reduced");
        let (view, cx) = launch(&fx, cx);
        open_book_by_index(&fx, &view, cx, 0);

        // the desktop asks for reduced motion
        view.update(cx, |this, _| this.reduce_motion = true);
        view.update(cx, |this, cx| {
            this.goto_page(1, Arrive::Start, cx);
            assert!(
                this.reader.as_ref().unwrap().transition.is_none(),
                "no slide"
            );
        });
        cx.run_until_parked();
        assert_eq!(page(&view, cx), 1, "navigation itself is unaffected");
        view.update(cx, |this, _| this.reduce_motion = false);

        // `:animate off` is a saved preference
        view.update_in(cx, |this, window, cx| {
            this.apply(
                Action::SetAnimations(Some(false)),
                viewport_of(window),
                window,
                cx,
            );
            this.goto_page(2, Arrive::Start, cx);
            assert!(this.reader.as_ref().unwrap().transition.is_none());
        });
        assert!(!saved_settings(&fx).animate);

        // the toggle form flips it back
        view.update_in(cx, |this, window, cx| {
            this.apply(Action::SetAnimations(None), viewport_of(window), window, cx);
        });
        assert!(saved_settings(&fx).animate);
        view.update(cx, |this, cx| {
            this.goto_page(3, Arrive::Start, cx);
            assert!(this.reader.as_ref().unwrap().transition.is_some());
        });
        settle_animations(&view, cx);
    }

    // ---- smooth scrolling through the app ----

    #[gpui::test]
    fn keyboard_scrolling_glides_accumulates_and_leaves_the_app_idle(cx: &mut TestAppContext) {
        let fx = fixture("glide");
        let (view, cx) = launch(&fx, cx);
        open_book_by_index(&fx, &view, cx, 0);
        // zoom in far enough that the page is taller than the window
        view.update_in(cx, |this, window, cx| {
            let vp = viewport_of(window);
            this.zoom_to(2.0, (vp.width / 2.0, vp.height / 2.0), vp, cx);
        });
        cx.run_until_parked();
        settle_animations(&view, cx);
        assert!(with_reader(&view, cx, |r| layout::max_offset(
            layout::display_size(r.fit, r.zoom, (60.0, 90.0), &r.viewport),
            &r.viewport
        )
        .1 > 100.0));

        // zooming at the centre leaves the page scrolled part way down
        let base = with_reader(&view, cx, |r| r.off.1);
        view.update_in(cx, |this, window, cx| {
            let vp = viewport_of(window);
            this.scroll(Dir::Down, Amount::Line, 1, vp, cx);
            this.scroll(Dir::Down, Amount::Line, 1, vp, cx);
            let reader = this.reader.as_ref().unwrap();
            assert_eq!(
                reader.scroll_target.1,
                base + 2.0 * LINE_STEP,
                "presses accumulate on the target"
            );
            assert_eq!(reader.off.1, base, "the picture has not jumped; it glides");
        });
        settle_animations(&view, cx);
        assert_eq!(
            with_reader(&view, cx, |r| r.off.1),
            base + 2.0 * LINE_STEP,
            "arrives exactly"
        );
        assert!(
            view.read_with(cx, |this, _| this.last_frame.is_none()),
            "no frames requested when idle"
        );

        // pressing up goes back along the same path
        cx.simulate_keystrokes("k");
        settle_animations(&view, cx);
        assert_eq!(with_reader(&view, cx, |r| r.off.1), base + LINE_STEP);
    }

    #[gpui::test]
    fn touchpad_deltas_are_followed_one_to_one_and_wheel_notches_glide(cx: &mut TestAppContext) {
        let fx = fixture("wheel");
        let (view, cx) = launch(&fx, cx);
        open_book_by_index(&fx, &view, cx, 0);
        view.update_in(cx, |this, window, cx| {
            let vp = viewport_of(window);
            this.zoom_to(2.0, (vp.width / 2.0, vp.height / 2.0), vp, cx);
        });
        settle_animations(&view, cx);

        let base = with_reader(&view, cx, |r| r.off.1);
        view.update_in(cx, |this, window, cx| {
            let vp = viewport_of(window);
            // a touchpad: small pixel deltas, not discrete
            this.on_wheel((0.0, -30.0), false, (0.0, 0.0), false, vp, cx);
            let reader = this.reader.as_ref().unwrap();
            assert_eq!(
                (reader.off.1, reader.scroll_target.1),
                (base + 30.0, base + 30.0),
                "no lag"
            );
            // a wheel notch: glides
            this.on_wheel((0.0, -WHEEL_LINE), false, (0.0, 0.0), true, vp, cx);
            let reader = this.reader.as_ref().unwrap();
            assert_eq!(reader.scroll_target.1, base + 30.0 + WHEEL_LINE);
            assert_eq!(reader.off.1, base + 30.0);
        });
        settle_animations(&view, cx);
        assert_eq!(
            with_reader(&view, cx, |r| r.off.1),
            base + 30.0 + WHEEL_LINE
        );
    }

    #[gpui::test]
    fn scrolling_with_reduced_motion_is_immediate(cx: &mut TestAppContext) {
        let fx = fixture("glide-off");
        let (view, cx) = launch(&fx, cx);
        open_book_by_index(&fx, &view, cx, 0);
        view.update_in(cx, |this, window, cx| {
            let vp = viewport_of(window);
            this.zoom_to(2.0, (vp.width / 2.0, vp.height / 2.0), vp, cx);
            this.reduce_motion = true;
        });
        settle_animations(&view, cx);
        let base = with_reader(&view, cx, |r| r.off.1);
        view.update_in(cx, |this, window, cx| {
            this.scroll(Dir::Down, Amount::Line, 3, viewport_of(window), cx);
        });
        settle_animations(&view, cx);
        assert_eq!(with_reader(&view, cx, |r| r.off.1), base + 3.0 * LINE_STEP);
    }

    #[gpui::test]
    fn the_whole_session_leaves_no_leaked_state(cx: &mut TestAppContext) {
        let fx = fixture("leaks");
        let (view, cx) = launch(&fx, cx);
        open_book_by_index(&fx, &view, cx, 0);
        for _ in 0..3 {
            cx.simulate_keystrokes("right right right right left left left left");
        }
        settle_animations(&view, cx);
        assert_eq!(view.read_with(cx, |this, _| this.garbage.len()), 0);
        let (cached, pending, released) = with_reader(&view, cx, |r| {
            (r.images.len(), r.pending.len(), r.released.len())
        });
        assert!(cached <= 1 + pagecache::KEEP_BEHIND + pagecache::KEEP_AHEAD);
        assert_eq!((pending, released), (0, 0));
        view.update_in(cx, |this, window, cx| this.close_book(window, cx));
        settle_animations(&view, cx);
        assert!(view.read_with(cx, |this, _| this.reader.is_none()
            && this.last_frame.is_none()));
    }

    // ---------------------------------------------------------------------------------------
    // Measurements. `cargo test perf_ -- --ignored --nocapture`
    // ---------------------------------------------------------------------------------------

    fn median(values: &mut [f64]) -> f64 {
        values.sort_by(|a, b| a.total_cmp(b));
        values[values.len() / 2]
    }

    #[test]
    #[ignore = "prints timings"]
    fn perf_decode_cost_per_tier() {
        // a large, noisy 3000x4500 page, like a high resolution scan
        let (w, h) = (3000u32, 4500u32);
        let image = image::RgbImage::from_fn(w, h, |x, y| {
            let n = (x.wrapping_mul(2654435761) ^ y.wrapping_mul(40503)) >> 24;
            image::Rgb([
                ((x / 12 + n) & 255) as u8,
                ((y / 17 + n) & 255) as u8,
                (((x + y) / 23) & 255) as u8,
            ])
        });
        let mut jpeg = Vec::new();
        image::DynamicImage::ImageRgb8(image)
            .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
                &mut jpeg, 85,
            ))
            .unwrap();
        println!(
            "source page: {w}x{h} JPEG, {:.1} MB",
            jpeg.len() as f64 / 1e6
        );

        let mut times = Vec::new();
        for _ in 0..5 {
            let start = Instant::now();
            let decoded = image::load_from_memory(&jpeg).unwrap();
            times.push(start.elapsed().as_secs_f64() * 1000.0);
            assert_eq!(decoded.width(), w);
        }
        println!(
            "JPEG decode only (no scaling): {:.0} ms",
            median(&mut times)
        );

        let full = image::load_from_memory(&jpeg).unwrap();
        for filter in [
            image::imageops::FilterType::Lanczos3,
            image::imageops::FilterType::CatmullRom,
            image::imageops::FilterType::Triangle,
        ] {
            let mut times = Vec::new();
            for _ in 0..3 {
                let start = Instant::now();
                let _ = full.resize_exact(1365, 2048, filter);
                times.push(start.elapsed().as_secs_f64() * 1000.0);
            }
            println!(
                "resize 3000x4500 -> 1365x2048 with {filter:?}: {:.0} ms",
                median(&mut times)
            );
        }

        for tier in layout::TIERS {
            let mut times = Vec::new();
            let mut bytes = 0u64;
            for _ in 0..3 {
                let start = Instant::now();
                let decoded = imaging::decode_fit_bgra(&jpeg, tier, tier).unwrap();
                times.push(start.elapsed().as_secs_f64() * 1000.0);
                let (dw, dh) = decoded.image.dimensions();
                bytes = u64::from(dw) * u64::from(dh) * 4;
            }
            println!(
                "tier {tier:>4}: decode+scale {:>6.0} ms (median of 3, background thread), result {:>5.1} MB",
                median(&mut times),
                bytes as f64 / 1e6
            );
        }
    }

    #[test]
    #[ignore = "prints timings"]
    fn perf_per_frame_animation_math() {
        const N: u32 = 2_000_000;
        let start = Instant::now();
        let mut off = (0.0f32, 0.0f32);
        let mut sink = 0.0f32;
        for i in 0..N {
            let target = (0.0, 400.0 + (i % 7) as f32);
            off = motion::step_scroll(off, target, 1.0 / 144.0).0;
            let mut transition = PageTransition::starting(-1.0);
            transition.advance(0.01);
            sink += transition.outgoing_shift() + transition.incoming_shift() + off.1;
        }
        let per_frame = start.elapsed().as_secs_f64() * 1e9 / f64::from(N);
        println!("slide + glide state update: {per_frame:.0} ns per frame (sink {sink:.1})");
    }

    #[gpui::test]
    #[ignore = "prints timings"]
    fn perf_frames_during_a_slide(cx: &mut TestAppContext) {
        let fx = fixture("perf");
        let (view, cx) = launch(&fx, cx);
        open_book_by_index(&fx, &view, cx, 0);
        view.update(cx, |this, _| this.frame_dt = Some(1.0 / 144.0));

        let mut times = Vec::new();
        for turn in 0..20 {
            view.update(cx, |this, cx| {
                this.turn_by(turn % 2 == 0, 1, Arrive::Start, cx)
            });
            for _ in 0..40 {
                // one frame = state update + element tree + layout + paint
                let start = Instant::now();
                view.update(cx, |_, cx| cx.notify());
                cx.run_until_parked();
                times.push(start.elapsed().as_secs_f64() * 1000.0);
                if view.read_with(cx, |this, _| this.last_frame.is_none()) {
                    break;
                }
            }
        }
        times.sort_by(|a, b| a.total_cmp(b));
        let mean = times.iter().sum::<f64>() / times.len() as f64;
        println!(
            "{} animated frames: mean {mean:.2} ms, median {:.2} ms, p95 {:.2} ms, max {:.2} ms \
             (view update + layout + paint into the test platform; unoptimised build, no GPU)",
            times.len(),
            times[times.len() / 2],
            times[(times.len() * 95 / 100).min(times.len() - 1)],
            times[times.len() - 1],
        );
    }
}
