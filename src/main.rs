//! yomu: a minimal, GPU-accelerated, Wayland-native manga reader.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::Result;
use gpui::{
    AnyElement, App, Application, Bounds, ClickEvent, Context, ExternalPaths, FocusHandle,
    FontWeight, KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ObjectFit, PathPromptOptions, RenderImage, ScrollDelta, ScrollWheelEvent,
    SharedString, TitlebarOptions, Window, WindowBounds, WindowOptions, actions, div, img,
    prelude::*, px, relative, rgb, rgba, size,
};
use image::Frame;
use yomu::{
    archive::{self, SourceKind},
    imaging,
    layout::{self, Arrive, FitMode, Viewport},
    library::{self, Book, DataDirs, Library, Settings},
    util::now_secs,
    vim::{self, Action, Amount, Dir, Vim},
};

actions!(yomu, [ToggleFullscreen, ImportFiles, ImportFolder, Quit]);

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
}

impl Yomu {
    fn new(cx: &mut Context<Self>) -> Self {
        let dirs = Arc::new(DataDirs::new().expect("cannot create the yomu data directory"));
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
    // Import
    // ------------------------------------------------------------------------------------

    fn import_dialog(&mut self, directories: bool, cx: &mut Context<Self>) {
        if self.importing {
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
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
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
        let remove_id = book.id.clone();
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
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.open_book(&open_id, window, cx);
            }))
            // Shift + right-click removes a book from the library (the file is never touched).
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    if event.modifiers.shift {
                        this.remove_book(&remove_id, cx);
                    }
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
            .bg(rgb(READER_BG))
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

        let reading = self.reader.is_some();
        let content = match self.reader.as_ref() {
            Some(reader) => self.render_reader(reader, cx),
            None => self.render_library(cx),
        };

        div()
            .key_context(if reading { "Reader" } else { "Library" })
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key(event, window, cx);
            }))
            .on_action(cx.listener(|_, _: &ToggleFullscreen, window, _| window.toggle_fullscreen()))
            .on_action(cx.listener(|this, _: &ImportFiles, _, cx| this.import_dialog(false, cx)))
            .on_action(cx.listener(|this, _: &ImportFolder, _, cx| this.import_dialog(true, cx)))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.import_paths(paths.paths().to_vec(), cx);
            }))
            .child(content)
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
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(rgba(0x000000a6))
        .child(panel)
        .into_any_element()
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
