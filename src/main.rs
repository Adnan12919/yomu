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
    FontWeight, KeyBinding, MouseButton, MouseDownEvent, ObjectFit, PathPromptOptions,
    RenderImage, ScrollDelta, ScrollWheelEvent, SharedString, TitlebarOptions, Window,
    WindowBounds, WindowOptions, actions, div, img, prelude::*, px, relative, rgb, rgba, size,
};
use image::Frame;
use yomu::{
    archive::{self, SourceKind},
    imaging,
    library::{self, Book, DataDirs, Library},
    util::now_secs,
};

actions!(
    yomu,
    [
        StepLeft,
        StepRight,
        PageForward,
        PageBack,
        FirstPage,
        LastPage,
        CloseBook,
        ToggleDirection,
        ToggleInfo,
        ToggleFullscreen,
        ImportFiles,
        ImportFolder,
        Quit,
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

#[derive(Clone, Copy)]
enum Side {
    Left,
    Right,
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
    show_info: bool,
    /// Decoded pages, each tagged with the viewport bucket it was sized for.
    images: HashMap<usize, (Arc<RenderImage>, (u32, u32))>,
    pending: HashSet<usize>,
    failed: HashMap<usize, String>,
    bucket: (u32, u32),
    error: Option<String>,
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
    importing: bool,
    rtl_default: bool,
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
            importing: false,
            rtl_default: true,
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

        self.reader = Some(Reader {
            book_id: book.id.clone(),
            path: book.path.clone(),
            kind: book.kind,
            pages: None,
            page: book.current_page.min(book.page_count.saturating_sub(1)),
            total: book.page_count,
            rtl: self.rtl_default,
            show_info: false,
            images: HashMap::new(),
            pending: HashSet::new(),
            failed: HashMap::new(),
            bucket: (0, 0),
            error: None,
        });

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
            self.rtl_default = reader.rtl;
            self.garbage
                .extend(reader.images.into_values().map(|(image, _)| image));
        }
        window.set_window_title("yomu");
        window.focus(&self.focus_handle);
        self.save_library();
        cx.notify();
    }

    // ------------------------------------------------------------------------------------
    // Page navigation
    // ------------------------------------------------------------------------------------

    fn goto_page(&mut self, target: usize, cx: &mut Context<Self>) {
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
        let book_id = reader.book_id.clone();
        if let Some(book) = self.library.get_mut(&book_id) {
            book.current_page = target;
            book.opened = true;
            book.last_read = Some(now_secs());
        }
        self.save_library();
        cx.notify();
    }

    fn turn(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(page) = self.reader.as_ref().map(|r| r.page) else {
            return;
        };
        let target = if forward {
            page.saturating_add(1)
        } else {
            page.saturating_sub(1)
        };
        self.goto_page(target, cx);
    }

    /// Arrow keys and screen-edge clicks are about screen direction, so which one advances
    /// depends on the reading direction.
    fn step(&mut self, side: Side, cx: &mut Context<Self>) {
        let Some(rtl) = self.reader.as_ref().map(|r| r.rtl) else {
            return;
        };
        let forward = matches!((side, rtl), (Side::Left, true) | (Side::Right, false));
        self.turn(forward, cx);
    }

    // ------------------------------------------------------------------------------------
    // Page decoding
    // ------------------------------------------------------------------------------------

    /// Called every render: sizes decodes to the window, frees far-away pages and prefetches
    /// the next ones, so turning a page is normally instant.
    fn sync_reader(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let scale = window.scale_factor();
        let viewport = window.viewport_size();
        let bucket = (
            fit_bucket(f32::from(viewport.width) * scale),
            fit_bucket(f32::from(viewport.height) * scale),
        );

        let wanted: Vec<usize> = {
            let Some(reader) = self.reader.as_mut() else {
                return;
            };
            let Some(pages) = reader.pages.clone() else {
                return;
            };
            reader.bucket = bucket;

            let low = reader.page.saturating_sub(2);
            let high = reader.page + 4;
            let stale: Vec<usize> = reader
                .images
                .keys()
                .copied()
                .filter(|ix| *ix < low || *ix > high)
                .collect();
            for ix in stale {
                if let Some((image, _)) = reader.images.remove(&ix) {
                    let _ = window.drop_image(image);
                }
            }

            let current = reader.page as isize;
            [0isize, 1, 2, -1, 3, 4]
                .iter()
                .map(|delta| current + delta)
                .filter(|ix| *ix >= 0 && (*ix as usize) < pages.len())
                .map(|ix| ix as usize)
                .collect()
        };

        for ix in wanted {
            self.request_page(ix, cx);
        }
    }

    fn request_page(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(reader) = self.reader.as_mut() else {
            return;
        };
        let Some(pages) = reader.pages.clone() else {
            return;
        };
        if ix >= pages.len() || reader.pending.contains(&ix) || reader.failed.contains_key(&ix) {
            return;
        }
        if matches!(reader.images.get(&ix), Some((_, bucket)) if *bucket == reader.bucket) {
            return;
        }
        reader.pending.insert(ix);

        let book_id = reader.book_id.clone();
        let path = reader.path.clone();
        let kind = reader.kind;
        let bucket = reader.bucket;
        let name = pages[ix].clone();

        let task = cx.background_executor().spawn(async move {
            let bytes = archive::read_page(&path, kind, &name)?;
            let bgra = imaging::decode_fit_bgra(&bytes, bucket.0, bucket.1)?;
            anyhow::Ok(Arc::new(RenderImage::new(vec![Frame::new(bgra)])))
        });

        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| this.page_decoded(book_id, ix, bucket, result, cx))
                .ok();
        })
        .detach();
    }

    fn page_decoded(
        &mut self,
        book_id: String,
        ix: usize,
        bucket: (u32, u32),
        result: Result<Arc<RenderImage>>,
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
            Ok(image) => {
                if let Some((old, _)) = reader.images.insert(ix, (image, bucket)) {
                    self.garbage.push(old);
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
        let content = if let Some(error) = &reader.error {
            centered(format!("Couldn't open this book: {error}"))
        } else if let Some((image, _)) = reader.images.get(&reader.page) {
            img(image.clone())
                .size_full()
                .object_fit(ObjectFit::Contain)
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
            div()
                .absolute()
                .bottom_4()
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .px_3()
                        .py_1()
                        .rounded_full()
                        .bg(rgba(0x000000b3))
                        .text_sm()
                        .text_color(rgb(TEXT))
                        .child(format!(
                            "{} / {}  ·  {}",
                            reader.page + 1,
                            reader.total,
                            if reader.rtl { "right to left" } else { "left to right" }
                        )),
                )
        });

        div()
            .size_full()
            .relative()
            .bg(rgb(READER_BG))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    let width = f32::from(window.viewport_size().width);
                    let x = f32::from(event.position.x);
                    if x < width / 3.0 {
                        this.step(Side::Left, cx);
                    } else if x > width * 2.0 / 3.0 {
                        this.step(Side::Right, cx);
                    }
                }),
            )
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                // Only discrete wheel notches turn pages; smooth touchpad deltas would
                // flip through dozens of pages per swipe.
                if let ScrollDelta::Lines(lines) = &event.delta {
                    if lines.y < 0.0 {
                        this.turn(true, cx);
                    } else if lines.y > 0.0 {
                        this.turn(false, cx);
                    }
                }
            }))
            .child(content)
            .child(progress_line)
            .children(info)
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
            .on_action(cx.listener(|this, _: &StepLeft, _, cx| this.step(Side::Left, cx)))
            .on_action(cx.listener(|this, _: &StepRight, _, cx| this.step(Side::Right, cx)))
            .on_action(cx.listener(|this, _: &PageForward, _, cx| this.turn(true, cx)))
            .on_action(cx.listener(|this, _: &PageBack, _, cx| this.turn(false, cx)))
            .on_action(cx.listener(|this, _: &FirstPage, _, cx| this.goto_page(0, cx)))
            .on_action(cx.listener(|this, _: &LastPage, _, cx| this.goto_page(usize::MAX, cx)))
            .on_action(cx.listener(|this, _: &CloseBook, window, cx| this.close_book(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleDirection, _, cx| {
                if let Some(reader) = this.reader.as_mut() {
                    reader.rtl = !reader.rtl;
                    this.rtl_default = reader.rtl;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleInfo, _, cx| {
                if let Some(reader) = this.reader.as_mut() {
                    reader.show_info = !reader.show_info;
                    cx.notify();
                }
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

/// Round a size in device pixels up to a multiple of 256, so that resizing the window by a few
/// pixels does not trigger a re-decode of every page.
fn fit_bucket(device_px: f32) -> u32 {
    (((device_px / 256.0).ceil() as u32).max(1) * 256).min(8192)
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
        cx.bind_keys([
            // Reader
            KeyBinding::new("left", StepLeft, Some("Reader")),
            KeyBinding::new("right", StepRight, Some("Reader")),
            KeyBinding::new("space", PageForward, Some("Reader")),
            KeyBinding::new("down", PageForward, Some("Reader")),
            KeyBinding::new("pagedown", PageForward, Some("Reader")),
            KeyBinding::new("shift-space", PageBack, Some("Reader")),
            KeyBinding::new("up", PageBack, Some("Reader")),
            KeyBinding::new("pageup", PageBack, Some("Reader")),
            KeyBinding::new("home", FirstPage, Some("Reader")),
            KeyBinding::new("end", LastPage, Some("Reader")),
            KeyBinding::new("escape", CloseBook, Some("Reader")),
            KeyBinding::new("q", CloseBook, Some("Reader")),
            KeyBinding::new("r", ToggleDirection, Some("Reader")),
            KeyBinding::new("i", ToggleInfo, Some("Reader")),
            KeyBinding::new("f", ToggleFullscreen, Some("Reader")),
            // Library
            KeyBinding::new("ctrl-o", ImportFiles, Some("Library")),
            KeyBinding::new("ctrl-shift-o", ImportFolder, Some("Library")),
            // Everywhere
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
