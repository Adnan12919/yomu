//! yomu: a minimal, GPU-accelerated, Wayland-native manga reader.

use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use gpui_kit::assets::IconName;
use gpui_kit::component::ColorTokens;
use gpui_kit::component::Icon;
use gpui_kit::component::button::{Button, ButtonRounded, ButtonVariants};
use gpui_kit::component::{Disableable, Sizable};
use gpui_kit::component::{Theme as KitTheme, ThemeConfig, ThemeRegistry};
#[allow(unused_imports)]
use gpui_kit::popup::{PopupAnchor, PopupConstraintAdjustment, PopupGravity, PopupOptions};
#[allow(unused_imports)]
use gpui_kit::{
    Anchor, Animation, AnimationExt, AnyElement, AnyWindowHandle, App, Bounds, ClickEvent, Context,
    Entity, ExternalPaths, FocusHandle, FontWeight, KeyBinding, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit, PathPromptOptions, Pixels, Point,
    RenderImage, Rgba, ScrollDelta, ScrollWheelEvent, SharedString, Styled, Subscription,
    TitlebarOptions, Window, WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions,
    actions, anchored, application, canvas, deferred, div, ease_out_quint, img, point, prelude::*,
    px, relative, size,
};
use image::Frame;
use serde::Deserialize;
use yomu::{
    archive::{self, SourceKind},
    imaging,
    layout::{self, Arrive, FitMode, Viewport},
    library::{self, Book, DataDirs, Library, OPACITY_MAX, OPACITY_MIN, Settings},
    menu::{self, BookAction, Entry, Item},
    motion::{self, PageTransition},
    pagecache, search,
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

/// The small set of semantic Kit tokens used by yomu's custom reader/library composition.
#[derive(Clone, Copy)]
struct UiColors {
    background: gpui_kit::Hsla,
    surface: gpui_kit::Hsla,
    surface_hover: gpui_kit::Hsla,
    border: gpui_kit::Hsla,
    foreground: gpui_kit::Hsla,
    muted: gpui_kit::Hsla,
    accent: gpui_kit::Hsla,
    danger: gpui_kit::Hsla,
    danger_hover: gpui_kit::Hsla,
    widget_opacity: f32,
}

impl UiColors {
    fn from_tokens(tokens: ColorTokens) -> Self {
        Self {
            background: tokens.background,
            surface: tokens.surface,
            surface_hover: tokens.secondary,
            border: tokens.border,
            foreground: tokens.foreground,
            muted: tokens.muted_foreground,
            accent: tokens.primary,
            danger: tokens.destructive,
            danger_hover: tokens.destructive,
            widget_opacity: 1.0,
        }
    }

    fn from_app(cx: &App) -> Self {
        cx.try_global::<KitTheme>()
            .map(|theme| {
                let mut colors = Self::from_tokens(theme.semantic_tokens().colors);
                colors.danger = theme.danger;
                colors.danger_hover = theme.danger_hover;
                colors
            })
            .unwrap_or_else(|| Self::from_tokens(ColorTokens::dark()))
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct CustomThemeFile {
    name: String,
    themes: Vec<CustomThemeDefinition>,
}

impl Default for CustomThemeFile {
    fn default() -> Self {
        Self {
            name: "yomu custom themes".to_string(),
            themes: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct CustomThemeDefinition {
    name: String,
    mode: String,
    colors: std::collections::BTreeMap<String, String>,
    font_size: Option<f32>,
    font_family: Option<String>,
    mono_font_size: Option<f32>,
    mono_font_family: Option<String>,
    radius: Option<usize>,
    radius_lg: Option<usize>,
    shadow: Option<bool>,
}

impl Default for CustomThemeDefinition {
    fn default() -> Self {
        Self {
            name: String::new(),
            mode: "dark".to_string(),
            colors: std::collections::BTreeMap::new(),
            font_size: None,
            font_family: None,
            mono_font_size: None,
            mono_font_family: None,
            radius: None,
            radius_lg: None,
            shadow: None,
        }
    }
}

const CUSTOM_THEME_COLOR_KEYS: &[&str] = &[
    "accent.background",
    "accent.foreground",
    "background",
    "border",
    "button.background",
    "button.foreground",
    "button.hover.background",
    "button.primary.background",
    "button.primary.foreground",
    "button.primary.hover.background",
    "danger.background",
    "danger.foreground",
    "foreground",
    "input.border",
    "link",
    "muted.background",
    "muted.foreground",
    "popover.background",
    "popover.foreground",
    "primary.background",
    "primary.foreground",
    "primary.hover.background",
    "ring",
    "scrollbar.background",
    "secondary.background",
    "secondary.foreground",
    "secondary.hover.background",
];

fn parse_custom_themes(
    contents: &str,
    built_in_names: &HashSet<String>,
) -> Result<HashMap<String, Rc<ThemeConfig>>> {
    let file: CustomThemeFile =
        toml::from_str(contents).map_err(|error| anyhow!("parsing themes.toml: {error}"))?;
    let mut names = HashSet::new();
    let mut themes = HashMap::new();

    for definition in file.themes {
        let name = definition.name.trim();
        if name.is_empty() {
            return Err(anyhow!("each custom theme needs a non-empty name"));
        }
        if built_in_names.contains(name) {
            return Err(anyhow!(
                "custom theme name “{name}” conflicts with a built-in theme"
            ));
        }
        if !names.insert(name.to_string()) {
            return Err(anyhow!("duplicate custom theme name “{name}”"));
        }
        if definition.mode != "dark" && definition.mode != "light" {
            return Err(anyhow!("theme “{name}” mode must be `dark` or `light`"));
        }
        if definition.colors.is_empty() {
            return Err(anyhow!("theme “{name}” needs at least one color override"));
        }
        for (key, value) in &definition.colors {
            if !CUSTOM_THEME_COLOR_KEYS.contains(&key.as_str()) {
                return Err(anyhow!("unsupported color token `{key}` in theme “{name}”"));
            }
            let digits = value.strip_prefix('#').unwrap_or("");
            if !matches!(digits.len(), 6 | 8)
                || !digits.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(anyhow!(
                    "color `{key}` in theme “{name}” must use #RRGGBB or #RRGGBBAA"
                ));
            }
        }

        let config_value = serde_json::json!({
            "name": name,
            "mode": definition.mode,
            "colors": definition.colors,
            "font.size": definition.font_size,
            "font.family": definition.font_family,
            "mono_font.size": definition.mono_font_size,
            "mono_font.family": definition.mono_font_family,
            "radius": definition.radius,
            "radius.lg": definition.radius_lg,
            "shadow": definition.shadow,
        });
        let config: ThemeConfig = serde_json::from_value(config_value)
            .map_err(|error| anyhow!("invalid theme “{name}”: {error}"))?;
        themes.insert(name.to_string(), Rc::new(config));
    }

    if themes.is_empty() {
        let set_name = file.name.trim();
        let set_name = if set_name.is_empty() {
            "custom theme set"
        } else {
            set_name
        };
        return Err(anyhow!(
            "{set_name} must define at least one [[themes]] entry"
        ));
    }
    Ok(themes)
}

fn custom_theme_path(_data_dirs: &DataDirs) -> Result<PathBuf> {
    #[cfg(test)]
    {
        Ok(_data_dirs
            .library_file()
            .parent()
            .unwrap_or(Path::new("."))
            .join("themes.toml"))
    }
    #[cfg(not(test))]
    {
        let config_dir = dirs::config_dir()
            .ok_or_else(|| anyhow!("could not determine the user's configuration directory"))?;
        Ok(config_dir.join("yomu").join("themes.toml"))
    }
}

#[allow(dead_code)]
fn custom_theme_fingerprint(path: &Path) -> Option<u64> {
    let contents = fs::read(path).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    contents.hash(&mut hasher);
    Some(hasher.finish())
}

fn ensure_custom_theme_file(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("custom theme path has no parent directory"))?;
    fs::create_dir_all(parent)?;
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => {
            use std::io::Write as _;
            file.write_all(include_bytes!("../themes.example.toml"))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn built_in_theme_names(cx: &App) -> HashSet<String> {
    cx.try_global::<ThemeRegistry>()
        .map(|registry| registry.themes().keys().map(ToString::to_string).collect())
        .unwrap_or_default()
}

fn load_custom_themes(path: &Path, cx: &App) -> Result<HashMap<String, Rc<ThemeConfig>>> {
    let contents =
        fs::read_to_string(path).map_err(|error| anyhow!("reading {}: {error}", path.display()))?;
    let built_in_names = built_in_theme_names(cx);
    parse_custom_themes(&contents, &built_in_names)
}

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
    /// Original dimensions are retained separately from decoded textures so continuous layout
    /// can keep stable geometry while pages are evicted from the image cache.
    orig_sizes: Vec<Option<(u32, u32)>>,
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
    continuous: bool,
    webcomics: bool,
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
            orig_sizes: Vec::new(),
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
            continuous: settings.continuous || settings.webcomics,
            webcomics: settings.webcomics,
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

    fn continuous_layout(&self, vp: &Viewport) -> (Vec<f32>, Vec<f32>) {
        let width = (vp.width * self.zoom).max(1.0);
        let gap = if self.webcomics {
            0.0
        } else {
            12.0 * self.zoom
        };
        let mut tops = Vec::with_capacity(self.total);
        let mut heights = Vec::with_capacity(self.total);
        let mut top = 0.0;
        for ix in 0..self.total {
            tops.push(top);
            // Unknown pages use a typical portrait ratio until their metadata is read.
            let (w, h) = self.orig_sizes.get(ix).and_then(|v| *v).unwrap_or((2, 3));
            let height = width * h as f32 / w.max(1) as f32;
            heights.push(height);
            top += height + gap;
        }
        (tops, heights)
    }

    fn continuous_page_at(&self, y: f32, vp: &Viewport) -> usize {
        let (tops, heights) = self.continuous_layout(vp);
        tops.iter()
            .zip(heights)
            .position(|(top, height)| y < *top + height)
            .unwrap_or_else(|| self.total.saturating_sub(1))
    }

    fn orig_of(page: &Page) -> (f32, f32) {
        (page.orig.0 as f32, page.orig.1 as f32)
    }

    /// Apply a pending page-turn arrival and clamp the scroll offset. Returns the display size
    /// of the current page, or `None` while it is still being decoded.
    fn settle(&mut self, vp: &Viewport) -> Option<(f32, f32)> {
        if self.continuous {
            if self.total == 0 {
                return None;
            }
            let (tops, heights) = self.continuous_layout(vp);
            let doc_height =
                tops.last().copied().unwrap_or(0.0) + heights.last().copied().unwrap_or(0.0);
            let page = self.page.min(self.total - 1);
            if let Some(arrive) = self.arrive.take() {
                self.scroll_target.1 = match arrive {
                    Arrive::Start => tops[page],
                    Arrive::End => (tops[page] + heights[page] - vp.height).max(0.0),
                };
                self.off = self.scroll_target;
            }
            let width = vp.width * self.zoom;
            let max_x = (width - vp.width).max(0.0);
            let max_y = (doc_height - vp.height).max(0.0);
            self.off.0 = self.off.0.clamp(0.0, max_x);
            self.off.1 = self.off.1.clamp(0.0, max_y);
            self.scroll_target.0 = self.scroll_target.0.clamp(0.0, max_x);
            self.scroll_target.1 = self.scroll_target.1.clamp(0.0, max_y);
            self.page = self.continuous_page_at(self.off.1 + vp.height * 0.5, vp);
            return Some((width, heights[self.page]));
        }
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

#[derive(Clone)]
struct BookMenu {
    book_id: String,
    entries: Vec<Entry>,
    position: Point<Pixels>,
    selected: Option<usize>,
}

#[allow(dead_code)]
struct NativeBookMenu {
    parent: Entity<Yomu>,
    parent_window: AnyWindowHandle,
    menu: BookMenu,
    colors: UiColors,
    selected: Option<usize>,
    _release_subscription: Option<Subscription>,
}

#[allow(dead_code)]
struct NativeLibraryMenu {
    parent: Entity<Yomu>,
    parent_window: AnyWindowHandle,
    colors: UiColors,
    selected: Option<usize>,
    grouped: bool,
    _release_subscription: Option<Subscription>,
}

#[allow(dead_code)]
struct NativeCommandPrompt {
    parent: Entity<Yomu>,
    parent_window: AnyWindowHandle,
    colors: UiColors,
    input: String,
    selection: usize,
    _release_subscription: Option<Subscription>,
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

struct SeriesDialog {
    book_ids: Vec<String>,
    value: String,
}

#[derive(Default)]
struct LibraryMenu {
    selected: usize,
}

/// At most one menu or dialog is open at a time; it sits on top of whatever view is showing.
enum Overlay {
    Menu(BookMenu),
    LibraryMenu(LibraryMenu),
    Confirm(ConfirmDialog),
    Properties(PropertiesDialog),
    Series(SeriesDialog),
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum SettingsTab {
    #[default]
    Appearance,
    Library,
    Reader,
    Keyboard,
}

struct Slider {
    /// Where the track was last painted, so mouse positions can be turned into a value.
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    dragging: bool,
}

struct LibraryGroup<'a> {
    key: String,
    name: String,
    first_seen: usize,
    books: Vec<&'a Book>,
}

enum ImportEvent {
    Book(Book),
    Finished(library::ImportReport),
}

fn with_alpha(color: gpui_kit::Hsla, alpha: f32) -> Rgba {
    let mut color: Rgba = color.into();
    color.a = alpha;
    color
}

fn directory_groups(books: &[Book]) -> Vec<LibraryGroup<'_>> {
    let mut buckets: std::collections::BTreeMap<String, (String, usize, Vec<&Book>)> =
        std::collections::BTreeMap::new();
    for (index, book) in books.iter().enumerate() {
        let manual_series = book
            .series
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty());
        let (key, name) = if let Some(series) = manual_series {
            (
                format!("series:{}", series.to_lowercase()),
                series.to_string(),
            )
        } else {
            let parent = book.path.parent().unwrap_or_else(|| Path::new("."));
            let display = parent
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| parent.display().to_string());
            (format!("directory:{}", parent.display()), display)
        };
        buckets
            .entry(key)
            .and_modify(|(_, _, members)| members.push(book))
            .or_insert_with(|| (name, index, vec![book]));
    }

    let mut groups: Vec<_> = buckets
        .into_iter()
        .filter_map(|(key, (name, first_seen, members))| {
            if members.len() < 2 {
                return None;
            }
            Some(LibraryGroup {
                key,
                name,
                first_seen,
                books: members,
            })
        })
        .collect();
    groups.sort_by_key(|group| group.first_seen);
    groups
}

fn overlay_shade(colors: UiColors) -> gpui_kit::Hsla {
    if colors.background.l > 0.5 {
        colors.foreground
    } else {
        colors.background
    }
}

struct Yomu {
    focus_handle: FocusHandle,
    dirs: Arc<DataDirs>,
    library: Library,
    colors: UiColors,
    custom_themes: HashMap<String, Rc<ThemeConfig>>,
    custom_theme_path: PathBuf,
    theme_status: Option<String>,
    reader: Option<Reader>,
    /// Images that are no longer shown. Their GPU textures are freed on the next render,
    /// because that is where we have access to the `Window`.
    garbage: Vec<Arc<RenderImage>>,
    toast: Option<SharedString>,
    toast_seq: u64,
    hud: Option<SharedString>,
    hud_seq: u64,
    importing: bool,
    importing_title: Option<String>,
    imported_count: usize,
    /// The desktop asks for reduced motion (see `motion::detect_reduced_motion`).
    reduce_motion: bool,
    /// When the previous animated frame was drawn; `None` while nothing is animating.
    last_frame: Option<Instant>,
    /// Tests drive the animation clock by hand instead of using real time.
    #[cfg(test)]
    frame_dt: Option<f32>,
    overlay: Option<Overlay>,
    /// True when a menu, dialog or prompt is an independent xdg_popup window.
    native_popup: bool,
    native_popup_window: Option<AnyWindowHandle>,
    settings_tab: SettingsTab,
    command_selection: usize,
    overlay_closed_at: Option<Instant>,
    /// Books picked for bulk actions in the library.
    selection: Selection,
    /// Transient library filter; it is not part of persisted reader settings.
    search_query: String,
    search_active: bool,
    search_caret_visible: bool,
    search_blink_seq: u64,
    open_group: Option<String>,
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
        let custom_theme_path = custom_theme_path(&dirs).unwrap_or_else(|error| {
            eprintln!("yomu: cannot determine custom theme path: {error:#}");
            dirs.library_file()
                .parent()
                .unwrap_or(Path::new("."))
                .join("themes.toml")
        });
        let mut theme_status = None;
        let mut custom_themes = HashMap::new();
        match ensure_custom_theme_file(&custom_theme_path).and_then(|()| {
            let contents = fs::read_to_string(&custom_theme_path)?;
            let built_in_names = built_in_theme_names(cx);
            parse_custom_themes(&contents, &built_in_names)
        }) {
            Ok(themes) => custom_themes = themes,
            Err(error) => theme_status = Some(format!("Custom theme file: {error:#}")),
        }
        let selected_theme = library.settings().theme_name.as_str();
        let selected_config = custom_themes.get(selected_theme).cloned().or_else(|| {
            cx.try_global::<ThemeRegistry>()
                .and_then(|registry| registry.themes().get(selected_theme).cloned())
        });
        if let Some(config) = selected_config {
            KitTheme::update(cx, |theme| theme.apply_config(&config));
        } else if selected_theme != "Default Dark" {
            if let Some(config) = cx
                .try_global::<ThemeRegistry>()
                .and_then(|registry| registry.themes().get("Default Dark").cloned())
            {
                KitTheme::update(cx, |theme| theme.apply_config(&config));
                theme_status.get_or_insert_with(|| {
                    format!("Theme “{selected_theme}” was unavailable; using Default Dark")
                });
            }
        }
        let mut colors = UiColors::from_app(cx);
        colors.widget_opacity = library.settings().opacity;
        let state = Self {
            focus_handle: cx.focus_handle(),
            dirs,
            library,
            colors,
            custom_themes,
            custom_theme_path,
            theme_status,
            reader: None,
            garbage: Vec::new(),
            toast,
            toast_seq: 0,
            hud: None,
            hud_seq: 0,
            importing: false,
            importing_title: None,
            imported_count: 0,
            reduce_motion: false,
            last_frame: None,
            #[cfg(test)]
            frame_dt: None,
            overlay: None,
            native_popup: false,
            native_popup_window: None,
            settings_tab: SettingsTab::Appearance,
            command_selection: 0,
            overlay_closed_at: None,
            selection: Selection::new(),
            search_query: String::new(),
            search_active: false,
            search_caret_visible: false,
            search_blink_seq: 0,
            open_group: None,
            slider: Slider {
                bounds: Rc::new(Cell::new(None)),
                dragging: false,
            },
            applied_opaque: None,
        };

        #[cfg(not(test))]
        state.watch_custom_theme_file(cx);

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

    /// Raised widget surfaces stay more opaque than the window glass behind them, making each
    /// card or popup read as a separate pane while still following the user's opacity setting.
    fn widget_surface(&self, color: gpui_kit::Hsla) -> Rgba {
        let alpha = (self.colors.widget_opacity + 0.12).clamp(0.65, 0.95);
        with_alpha(color, alpha)
    }

    #[allow(dead_code)]
    fn widget_hover(&self, color: gpui_kit::Hsla) -> Rgba {
        let alpha = (self.colors.widget_opacity + 0.15).clamp(0.70, 0.98);
        with_alpha(color, alpha)
    }

    /// Keep menus and native popups translucent at every window opacity so Niri compositor blur shows through.
    fn popup_surface(&self) -> Rgba {
        let alpha = (self.colors.widget_opacity + 0.15).clamp(0.70, 0.92);
        with_alpha(self.colors.surface, alpha)
    }

    fn menu_surface(&self) -> Rgba {
        self.popup_surface()
    }

    /// Watch the generated custom theme file so Noctalia palette changes apply without a manual
    /// reload. Hashing is done on the background executor; the small TOML is parsed on change.
    #[cfg(not(test))]
    fn watch_custom_theme_file(&self, cx: &mut Context<Self>) {
        let path = self.custom_theme_path.clone();
        cx.spawn(async move |this, cx| {
            let mut last_hash = custom_theme_fingerprint(&path);
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(750))
                    .await;
                let check_path = path.clone();
                let next_hash = cx
                    .background_executor()
                    .spawn(async move { custom_theme_fingerprint(&check_path) })
                    .await;
                if next_hash.is_none() || next_hash == last_hash {
                    continue;
                }

                // Noctalia may replace the file atomically; debounce until the new contents
                // remain stable so a partial write is never applied.
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let check_path = path.clone();
                let stable_hash = cx
                    .background_executor()
                    .spawn(async move { custom_theme_fingerprint(&check_path) })
                    .await;
                if stable_hash != next_hash {
                    continue;
                }
                if this
                    .update(cx, |this, cx| this.reload_custom_themes(cx))
                    .is_err()
                {
                    break;
                }
                last_hash = stable_hash;
            }
        })
        .detach();
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
        self.close_native_popup(cx);
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

    fn close_native_popup(&mut self, cx: &mut Context<Self>) {
        let Some(popup) = self.native_popup_window.take() else {
            self.native_popup = false;
            return;
        };
        let _ = cx.update_window(popup, |_, window, _| window.remove_window());
        self.native_popup = false;
        self.dismiss();
        cx.notify();
    }

    fn overlay_recently_closed(&self) -> bool {
        self.overlay_closed_at
            .is_some_and(|closed| closed.elapsed() < CLICK_GUARD)
    }

    #[cfg(test)]
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

    fn open_series_dialog(&mut self, book_ids: Vec<String>, cx: &mut Context<Self>) {
        if book_ids.is_empty() {
            return;
        }
        let books: Vec<&Book> = book_ids
            .iter()
            .filter_map(|id| self.library.get(id))
            .collect();
        let existing = books
            .first()
            .and_then(|first| first.series.clone())
            .filter(|name| {
                books.iter().all(|book| {
                    book.series
                        .as_deref()
                        .is_some_and(|other| other.eq_ignore_ascii_case(name))
                })
            });
        let common_parent = books
            .first()
            .and_then(|book| book.path.parent().map(PathBuf::from))
            .filter(|parent| {
                books
                    .iter()
                    .all(|book| book.path.parent() == Some(parent.as_path()))
            })
            .and_then(|parent| {
                parent
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            });
        let value = existing.or(common_parent).unwrap_or_default();
        self.dismiss();
        self.overlay = Some(Overlay::Series(SeriesDialog { book_ids, value }));
        cx.notify();
    }

    fn save_series_dialog(&mut self, cx: &mut Context<Self>) {
        let Some(Overlay::Series(dialog)) = self.overlay.take() else {
            return;
        };
        self.overlay_closed_at = Some(Instant::now());
        let count = dialog.book_ids.len();
        let series = (!dialog.value.trim().is_empty()).then_some(dialog.value);
        self.library.set_series(&dialog.book_ids, series.clone());
        self.save_library();
        let message = match (series, count) {
            (Some(name), 1) => format!("Added to series “{name}”"),
            (Some(name), _) => format!("Assigned {count} books to “{name}”"),
            (None, 1) => "Removed from series".to_string(),
            (None, _) => format!("Removed {count} books from their series"),
        };
        self.show_toast(message, cx);
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
            BookAction::SetSeries => {
                let ids = if self.selection.contains(id) && self.selection.len() > 1 {
                    self.selected_ids()
                } else {
                    vec![id.to_string()]
                };
                self.open_series_dialog(ids, cx);
                return;
            }
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
            BookAction::SetSeries => {}
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
        if self.native_popup {
            self.close_native_popup(cx);
        }
        match self.overlay {
            None => self.overlay = Some(Overlay::Settings),
            Some(Overlay::Settings) => self.close_overlay(cx),
            Some(_) => {}
        }
        cx.notify();
    }

    fn open_settings_window(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.open_settings(cx);
    }

    fn request_reset_settings(&mut self, cx: &mut Context<Self>) {
        if *self.library.settings() == Settings::default() {
            return;
        }
        self.overlay = Some(Overlay::Confirm(ConfirmDialog {
            title: "Reset settings?".into(),
            message: "All preferences go back to their defaults.".into(),
            confirm_label: "Reset",
            action: ConfirmAction::ResetSettings,
        }));
        cx.notify();
    }

    fn reset_settings(&mut self, cx: &mut Context<Self>) {
        *self.library.settings_mut() = Settings::default();
        let defaults = self.library.settings().clone();
        self.apply_theme(&defaults.theme_name, cx);
        if let Some(reader) = self.reader.as_mut() {
            reader.rtl = defaults.rtl;
            reader.fit = defaults.fit;
            reader.zoom = 1.0;
            reader.continuous = defaults.continuous;
            reader.webcomics = defaults.webcomics;
            reader.continuous |= reader.webcomics;
            reader.arrive = Some(Arrive::Start);
        }
        self.save_library();
        self.slider.dragging = false;
        self.overlay = Some(Overlay::Settings);
        cx.notify();
    }

    fn apply_theme(&mut self, name: &str, cx: &mut Context<Self>) {
        let config = self.custom_themes.get(name).cloned().or_else(|| {
            cx.try_global::<ThemeRegistry>()
                .and_then(|registry| registry.themes().get(name).cloned())
        });
        let Some(config) = config else {
            self.show_toast(format!("Theme “{name}” is unavailable"), cx);
            return;
        };
        KitTheme::update(cx, |theme| theme.apply_config(&config));
        self.library.settings_mut().theme_name = name.to_string();
        self.colors = UiColors::from_app(cx);
        self.colors.widget_opacity = self.library.settings().opacity;
        self.save_library();
        cx.notify();
    }

    fn cycle_theme(&mut self, delta: isize, cx: &mut Context<Self>) {
        let mut names: Vec<String> = cx
            .try_global::<ThemeRegistry>()
            .map(|registry| {
                registry
                    .sorted_themes()
                    .iter()
                    .map(|theme| theme.name.to_string())
                    .collect()
            })
            .unwrap_or_default();
        names.extend(self.custom_themes.keys().cloned());
        names.sort_by_key(|name| name.to_lowercase());
        if names.is_empty() {
            return;
        }
        let current = self.library.settings().theme_name.as_str();
        let index = names.iter().position(|name| name == current).unwrap_or(0) as isize;
        let next = (index + delta).rem_euclid(names.len() as isize) as usize;
        self.apply_theme(&names[next], cx);
    }

    fn reload_custom_themes(&mut self, cx: &mut Context<Self>) {
        let themes = match ensure_custom_theme_file(&self.custom_theme_path)
            .and_then(|()| load_custom_themes(&self.custom_theme_path, cx))
        {
            Ok(themes) => themes,
            Err(error) => {
                self.theme_status = Some(format!("Custom theme file error: {error:#}"));
                cx.notify();
                return;
            }
        };

        let selected = self.library.settings().theme_name.clone();
        let was_custom = self.custom_themes.contains_key(&selected);
        self.custom_themes = themes;
        self.theme_status = Some(format!(
            "Loaded {} custom theme(s) from {}",
            self.custom_themes.len(),
            self.custom_theme_path.display()
        ));

        if let Some(config) = self.custom_themes.get(&selected).cloned() {
            KitTheme::update(cx, |theme| theme.apply_config(&config));
            self.colors = UiColors::from_app(cx);
            self.colors.widget_opacity = self.library.settings().opacity;
        } else if was_custom {
            self.apply_theme("Default Dark", cx);
        }
        cx.notify();
    }

    fn set_opacity(&mut self, value: f32, persist: bool, cx: &mut Context<Self>) {
        self.library.settings_mut().set_opacity(value);
        self.colors.widget_opacity = self.library.settings().opacity;
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
        if matches!(token, "enter" | "space")
            && let Some(Overlay::LibraryMenu(menu)) = self.overlay.as_ref()
        {
            let selected = menu.selected;
            self.run_library_menu_action(selected, window, cx);
            return;
        }
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
            Some(Overlay::LibraryMenu(menu)) => match token {
                "escape" => self.close_overlay(cx),
                "down" | "j" | "tab" | "C-n" => {
                    menu.selected = (menu.selected + 1) % 5;
                    cx.notify();
                }
                "up" | "k" | "S-tab" | "C-p" => {
                    menu.selected = (menu.selected + 4) % 5;
                    cx.notify();
                }
                "home" => {
                    menu.selected = 0;
                    cx.notify();
                }
                "end" => {
                    menu.selected = 4;
                    cx.notify();
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
            Some(Overlay::Series(_)) => {}
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

    fn run_library_menu_action(
        &mut self,
        selected: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.overlay = None;
        match selected {
            0 => self.import_dialog(false, cx),
            1 => self.import_dialog(true, cx),
            2 => self.open_settings_window(_window, cx),
            3 => {
                self.library.settings_mut().group_series = !self.library.settings().group_series;
                self.open_group = None;
                self.save_library();
                cx.notify();
            }
            4 => cx.quit(),
            _ => {}
        }
    }

    fn on_series_key(
        &mut self,
        token: &str,
        key_char: Option<&str>,
        control: bool,
        alt: bool,
        cx: &mut Context<Self>,
    ) {
        match token {
            "escape" => self.close_overlay(cx),
            "enter" => self.save_series_dialog(cx),
            "backspace" => {
                if let Some(Overlay::Series(dialog)) = self.overlay.as_mut() {
                    dialog.value.pop();
                }
                cx.notify();
            }
            "C-a" => {
                if let Some(Overlay::Series(dialog)) = self.overlay.as_mut() {
                    dialog.value.clear();
                }
                cx.notify();
            }
            _ if !control && !alt => {
                if let Some(text) =
                    key_char.filter(|text| !text.is_empty() && !text.chars().any(char::is_control))
                {
                    if let Some(Overlay::Series(dialog)) = self.overlay.as_mut() {
                        dialog.value.push_str(text);
                    }
                    cx.notify();
                }
            }
            _ => {}
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
        self.imported_count = 0;
        self.importing_title = None;
        cx.notify();

        let dirs = self.dirs.clone();
        let existing = self.library.ids();
        let (sender, receiver) = mpsc::channel();
        cx.background_executor()
            .spawn(async move {
                let report =
                    library::import_paths_with_progress(&paths, &dirs, &existing, |book| {
                        let _ = sender.send(ImportEvent::Book(book.clone()));
                    });
                let _ = sender.send(ImportEvent::Finished(report));
            })
            .detach();

        cx.spawn(async move |this, cx| {
            loop {
                match receiver.try_recv() {
                    Ok(ImportEvent::Book(book)) => {
                        this.update(cx, |this, cx| {
                            let title = book.title.clone();
                            if this.library.add(book) {
                                this.save_library();
                                this.imported_count += 1;
                            }
                            this.importing_title = Some(title);
                            cx.notify();
                        })
                        .ok();
                        // Leave a frame for the newly inserted cover/title before showing the
                        // next completed volume, even when several finish almost together.
                        cx.background_executor()
                            .timer(Duration::from_millis(16))
                            .await;
                    }
                    Ok(ImportEvent::Finished(report)) => {
                        this.update(cx, |this, cx| {
                            this.importing = false;
                            this.importing_title = None;
                            this.show_toast(report.summary(), cx);
                        })
                        .ok();
                        break;
                    }
                    Err(mpsc::TryRecvError::Empty) => {
                        cx.background_executor()
                            .timer(Duration::from_millis(40))
                            .await;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        this.update(cx, |this, cx| {
                            this.importing = false;
                            this.importing_title = None;
                            cx.notify();
                        })
                        .ok();
                        break;
                    }
                }
            }
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
    fn open_context_menu(
        &mut self,
        id: &str,
        position: Point<Pixels>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.native_popup {
            self.close_native_popup(cx);
        }
        let entries = if self.selection.contains(id) && self.selection.len() > 1 {
            menu::selection_menu(self.selection.len())
        } else {
            self.selection.clear();
            let Some(book) = self.library.get(id) else {
                return;
            };
            menu::book_menu(book, book.path.exists())
        };
        let menu = BookMenu {
            book_id: id.to_string(),
            entries,
            position,
            selected: None,
        };
        self.overlay = Some(Overlay::Menu(menu.clone()));
        #[cfg(not(test))]
        if self.open_native_context_popup(menu, _window, cx) {
            return;
        }
        self.native_popup = false;
        cx.notify();
    }

    #[cfg(not(test))]
    fn open_native_context_popup(
        &mut self,
        menu: BookMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let parent_window = window.window_handle();
        let parent = cx.entity();
        let height = menu
            .entries
            .iter()
            .map(|entry| match entry {
                Entry::Item(_) => 28.0,
                Entry::Separator => 5.0,
            })
            .sum::<f32>()
            + 12.0;
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: point(px(0.), px(0.)),
                size: size(px(MENU_W), px(height)),
            })),
            titlebar: None,
            focus: true,
            show: true,
            kind: WindowKind::AnchoredPopup(PopupOptions {
                parent: parent_window,
                anchor_rect: Bounds {
                    origin: menu.position,
                    size: size(px(1.), px(1.)),
                },
                anchor: PopupAnchor::TopLeft,
                gravity: PopupGravity::BottomRight,
                constraint_adjustment: PopupConstraintAdjustment::SLIDE_X
                    | PopupConstraintAdjustment::SLIDE_Y
                    | PopupConstraintAdjustment::FLIP_X
                    | PopupConstraintAdjustment::FLIP_Y,
                offset: point(px(2.), px(2.)),
                grab: true,
            }),
            is_resizable: false,
            is_movable: false,
            window_background: WindowBackgroundAppearance::Transparent,
            ..Default::default()
        };

        self.native_popup = true;
        let colors = self.colors;
        let opened = cx.open_window(options, move |popup_window, cx| {
            let this_window = popup_window.window_handle();
            cx.new(|cx| {
                let mut popup = NativeBookMenu {
                    parent: parent.clone(),
                    parent_window,
                    menu,
                    colors,
                    selected: None,
                    _release_subscription: None,
                };
                let parent = parent.clone();
                popup._release_subscription = Some(cx.on_release(move |_, app| {
                    let _ = parent.update(app, |state, cx| {
                        if state.native_popup_window == Some(this_window) {
                            state.native_popup = false;
                            state.native_popup_window = None;
                        }
                        if matches!(state.overlay, Some(Overlay::Menu(_))) {
                            state.dismiss();
                        }
                        cx.notify();
                    });
                }));
                popup
            })
        });
        match opened {
            Ok(handle) => {
                self.native_popup_window = Some(handle.into());
                true
            }
            Err(error) => {
                eprintln!(
                    "yomu: native context popup unavailable; using in-window menu: {error:#}"
                );
                self.native_popup = false;
                false
            }
        }
    }

    fn open_library_menu(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.native_popup {
            self.close_native_popup(cx);
        }
        self.overlay = Some(Overlay::LibraryMenu(LibraryMenu::default()));
        #[cfg(not(test))]
        if self.open_native_library_menu(_window, cx) {
            return;
        }
        self.native_popup = false;
        cx.notify();
    }

    #[cfg(not(test))]
    fn open_native_library_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let parent_window = window.window_handle();
        let parent = cx.entity();
        let window_size = window.viewport_size();
        let width = 300.0;
        let height = 340.0;

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: point(px(0.), px(0.)),
                size: size(px(width), px(height)),
            })),
            titlebar: None,
            focus: true,
            show: true,
            kind: WindowKind::AnchoredPopup(PopupOptions {
                parent: parent_window,
                anchor_rect: Bounds {
                    origin: point(window_size.width - px(24.0), px(56.0)),
                    size: size(px(1.0), px(1.0)),
                },
                anchor: PopupAnchor::TopRight,
                gravity: PopupGravity::BottomLeft,
                constraint_adjustment: PopupConstraintAdjustment::all(),
                offset: point(px(0.), px(4.0)),
                grab: true,
            }),
            is_resizable: false,
            is_movable: false,
            window_background: WindowBackgroundAppearance::Transparent,
            ..Default::default()
        };

        self.native_popup = true;
        let colors = self.colors;
        let grouped = self.library.settings().group_series;
        let opened = cx.open_window(options, move |popup_window, cx| {
            let this_window = popup_window.window_handle();
            cx.new(|cx| {
                let mut popup = NativeLibraryMenu {
                    parent: parent.clone(),
                    parent_window,
                    colors,
                    selected: None,
                    grouped,
                    _release_subscription: None,
                };
                let parent = parent.clone();
                popup._release_subscription = Some(cx.on_release(move |_, app| {
                    let _ = parent.update(app, |state, cx| {
                        if state.native_popup_window == Some(this_window) {
                            state.native_popup = false;
                            state.native_popup_window = None;
                        }
                        if matches!(state.overlay, Some(Overlay::LibraryMenu(_))) {
                            state.dismiss();
                        }
                        cx.notify();
                    });
                }));
                popup
            })
        });

        match opened {
            Ok(handle) => {
                self.native_popup_window = Some(handle.into());
                true
            }
            Err(error) => {
                eprintln!("yomu: native library menu unavailable; using in-window menu: {error:#}");
                self.native_popup = false;
                false
            }
        }
    }

    #[cfg(not(test))]
    fn open_native_command_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let parent_window = window.window_handle();
        let parent = cx.entity();
        let colors = self.colors;
        let input = self
            .reader
            .as_ref()
            .and_then(|r| r.vim.cmdline_text().map(str::to_owned))
            .unwrap_or_default();
        let window_size = window.viewport_size();
        let width = 560.0;
        let height = 260.0;

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: point(px(0.), px(0.)),
                size: size(px(width), px(height)),
            })),
            titlebar: None,
            focus: true,
            show: true,
            kind: WindowKind::AnchoredPopup(PopupOptions {
                parent: parent_window,
                anchor_rect: Bounds {
                    origin: point(window_size.width / 2.0, window_size.height / 2.5),
                    size: size(px(1.0), px(1.0)),
                },
                anchor: PopupAnchor::Center,
                gravity: PopupGravity::Center,
                constraint_adjustment: PopupConstraintAdjustment::all(),
                offset: point(px(0.), px(0.)),
                grab: true,
            }),
            is_resizable: false,
            is_movable: false,
            window_background: WindowBackgroundAppearance::Transparent,
            ..Default::default()
        };

        self.native_popup = true;
        let opened = cx.open_window(options, move |_, cx| {
            cx.new(|cx| {
                let mut popup = NativeCommandPrompt {
                    parent: parent.clone(),
                    parent_window,
                    colors,
                    input,
                    selection: 0,
                    _release_subscription: None,
                };
                let parent = parent.clone();
                popup._release_subscription = Some(cx.on_release(move |_, app| {
                    let _ = parent.update(app, |state, cx| {
                        state.native_popup = false;
                        state.native_popup_window = None;
                        if let Some(reader) = state.reader.as_mut() {
                            reader.vim.cancel_cmdline();
                        }
                        cx.notify();
                    });
                }));
                popup
            })
        });

        match opened {
            Ok(handle) => {
                self.native_popup_window = Some(handle.into());
                true
            }
            Err(error) => {
                eprintln!(
                    "yomu: native command prompt unavailable; using in-window palette: {error:#}"
                );
                self.native_popup = false;
                false
            }
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

    fn on_library_key(
        &mut self,
        token: &str,
        key_char: Option<&str>,
        control: bool,
        alt: bool,
        cx: &mut Context<Self>,
    ) {
        if matches!(token, "C-f" | "/" | "slash") {
            self.activate_search(cx);
            return;
        }
        if self.search_active {
            match token {
                "escape" => {
                    if self.search_query.is_empty() {
                        self.deactivate_search(cx);
                    } else {
                        self.search_query.clear();
                        self.search_caret_visible = true;
                        cx.notify();
                    }
                }
                "enter" => {
                    self.deactivate_search(cx);
                }
                "backspace" => {
                    self.search_query.pop();
                    cx.notify();
                }
                "C-a" => {
                    self.search_query.clear();
                    cx.notify();
                }
                _ if !control && !alt => {
                    if let Some(text) = key_char
                        .filter(|text| !text.is_empty() && !text.chars().any(char::is_control))
                    {
                        self.search_query.push_str(text);
                        cx.notify();
                    }
                }
                _ => {}
            }
            return;
        }
        match token {
            "C-a" => {
                let order = self.library_order();
                self.selection.select_all(&order);
                cx.notify();
            }
            "escape" | "backspace" => {
                if !self.selection.is_empty() {
                    self.selection.clear();
                    cx.notify();
                } else if self.open_group.is_some() {
                    self.open_group = None;
                    cx.notify();
                }
            }
            "delete" => self.request_remove_selected(cx),
            _ => {}
        }
    }

    fn activate_search(&mut self, cx: &mut Context<Self>) {
        self.search_active = true;
        self.search_caret_visible = true;
        self.search_blink_seq = self.search_blink_seq.wrapping_add(1);
        let sequence = self.search_blink_seq;
        cx.notify();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(530))
                    .await;
                let active = this
                    .update(cx, |this, cx| {
                        if this.search_blink_seq != sequence || !this.search_active {
                            return false;
                        }
                        this.search_caret_visible = !this.search_caret_visible;
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !active {
                    break;
                }
            }
        })
        .detach();
    }

    fn deactivate_search(&mut self, cx: &mut Context<Self>) {
        self.search_active = false;
        self.search_caret_visible = false;
        self.search_blink_seq = self.search_blink_seq.wrapping_add(1);
        cx.notify();
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
        self.search_active = false;
        window.set_window_title(&book.title);
        window.focus(&self.focus_handle, cx);

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
                reader.orig_sizes = vec![None; total];
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
        window.focus(&self.focus_handle, cx);
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
        reader.begin_transition(target > reader.page, animate && !reader.continuous);
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
        let max = if reader.continuous {
            let (tops, heights) = reader.continuous_layout(&vp);
            (
                (display.0 - vp.width).max(0.0),
                (tops.last().copied().unwrap_or(0.0) + heights.last().copied().unwrap_or(0.0)
                    - vp.height)
                    .max(0.0),
            )
        } else {
            layout::max_offset(display, &vp)
        };
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

        if axis_max <= EDGE_EPS && !(reader.continuous && vertical) {
            self.turn_by(turn_forward, count, arrive, cx);
            return;
        }
        let at_end = if positive {
            current >= axis_max - EDGE_EPS
        } else {
            current <= EDGE_EPS
        };
        if at_end && !(reader.continuous && vertical) {
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
        if !reader.animate {
            reader.off = reader.scroll_target;
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
        let max = if reader.continuous {
            let (tops, heights) = reader.continuous_layout(&vp);
            (tops.last().copied().unwrap_or(0.0) + heights.last().copied().unwrap_or(0.0)
                - vp.height)
                .max(0.0)
        } else {
            layout::max_offset(display, &vp).1
        };
        reader.scroll_target.1 = max * fraction;
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
            if reader.continuous {
                let (old_tops, old_heights) = reader.continuous_layout(&vp);
                let old_doc = old_tops.last().copied().unwrap_or(0.0)
                    + old_heights.last().copied().unwrap_or(0.0);
                let old_y = reader.scroll_target.1;
                let zoom = layout::clamp_zoom(zoom);
                reader.zoom = zoom;
                let (new_tops, new_heights) = reader.continuous_layout(&vp);
                let new_doc = new_tops.last().copied().unwrap_or(0.0)
                    + new_heights.last().copied().unwrap_or(0.0);
                reader.scroll_target.1 = if old_doc > vp.height {
                    old_y / (old_doc - vp.height) * (new_doc - vp.height).max(0.0)
                } else {
                    0.0
                };
                let ratio = if old_display.0 > 0.0 {
                    (reader.scroll_target.0 + anchor.0) / old_display.0
                } else {
                    0.5
                };
                reader.scroll_target.0 = (ratio * vp.width * zoom - anchor.0).max(0.0);
                reader.off = reader.scroll_target;
                reader.cancel_transition();
                format!("continuous · {}%", (zoom * 100.0).round() as i32)
            } else {
                let Some(orig) = reader.current().map(Reader::orig_of) else {
                    return;
                };
                let zoom = layout::clamp_zoom(zoom);
                let old_origin = layout::origin(reader.off, old_display, &vp);
                let new_display = layout::display_size(reader.fit, zoom, orig, &vp);
                let off = layout::zoom_anchor_offset(old_origin, old_display, new_display, anchor);
                reader.cancel_transition();
                reader.zoom = zoom;
                reader.set_scroll(layout::clamp_offset(off, new_display, &vp));
                format!(
                    "{} · {}%",
                    reader.fit.label(),
                    (zoom * 100.0).round() as i32
                )
            }
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

    fn set_animations(&mut self, value: Option<bool>, cx: &mut Context<Self>) {
        let enabled = value.unwrap_or(!self.library.settings().animate);
        self.library.settings_mut().animate = enabled;
        self.save_library();
        let message = match (enabled, self.reduce_motion) {
            (true, true) => "animations on (the desktop asks for reduced motion, so they stay off)",
            (true, false) => "animations on",
            (false, _) => "animations off",
        };
        self.show_hud(message, cx);
    }

    fn set_continuous(&mut self, value: Option<bool>, cx: &mut Context<Self>) {
        let next = value.unwrap_or(!self.library.settings().continuous);
        self.library.settings_mut().continuous = next;
        if let Some(reader) = self.reader.as_mut() {
            reader.continuous = next || reader.webcomics;
            reader.zoom = 1.0;
            reader.off = (0.0, 0.0);
            reader.scroll_target = (0.0, 0.0);
            reader.arrive = Some(Arrive::Start);
            reader.cancel_transition();
        }
        self.save_library();
        let webcomics = self.library.settings().webcomics;
        self.show_hud(
            if webcomics {
                "webcomics mode"
            } else if next {
                "continuous scrolling"
            } else {
                "single page"
            },
            cx,
        );
    }

    fn set_webcomics(&mut self, value: Option<bool>, cx: &mut Context<Self>) {
        let next = value.unwrap_or(!self.library.settings().webcomics);
        self.library.settings_mut().webcomics = next;
        if let Some(reader) = self.reader.as_mut() {
            reader.webcomics = next;
            reader.continuous = next || self.library.settings().continuous;
            reader.zoom = 1.0;
            reader.off = (0.0, 0.0);
            reader.scroll_target = (0.0, 0.0);
            reader.arrive = Some(Arrive::Start);
            reader.cancel_transition();
        }
        self.save_library();
        self.show_hud(
            if next {
                "webcomics mode"
            } else if self.library.settings().continuous {
                "continuous scrolling"
            } else {
                "single page"
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

        let max = if reader.continuous {
            let (tops, heights) = reader.continuous_layout(&vp);
            (
                (display.0 - vp.width).max(0.0),
                (tops.last().copied().unwrap_or(0.0) + heights.last().copied().unwrap_or(0.0)
                    - vp.height)
                    .max(0.0),
            )
        } else {
            layout::max_offset(display, &vp)
        };
        if reader.continuous {
            let target = (reader.scroll_target.1 - dy).clamp(0.0, max.1);
            reader.scroll_target.1 = target;
            if !discrete || !reader.animate {
                reader.off.1 = target;
            }
        } else if max.1 > EDGE_EPS {
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
        let target = (drag.off0.0 - dx, drag.off0.1 - dy);
        if reader.continuous {
            let (tops, heights) = reader.continuous_layout(&vp);
            let max = (
                (display.0 - vp.width).max(0.0),
                (tops.last().copied().unwrap_or(0.0) + heights.last().copied().unwrap_or(0.0)
                    - vp.height)
                    .max(0.0),
            );
            reader.set_scroll((target.0.clamp(0.0, max.0), target.1.clamp(0.0, max.1)));
        } else {
            reader.set_scroll(layout::clamp_offset(target, display, &vp));
        }
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
            if matches!(self.overlay, Some(Overlay::Series(_))) {
                self.on_series_key(
                    &token,
                    keystroke.key_char.as_deref(),
                    mods.control,
                    mods.alt,
                    cx,
                );
                return;
            }
            self.on_overlay_key(&token, window, cx);
            return;
        }
        let Some(reader) = self.reader.as_mut() else {
            self.on_library_key(
                &token,
                keystroke.key_char.as_deref(),
                mods.control,
                mods.alt,
                cx,
            );
            return;
        };

        if let Some(draft) = reader.vim.cmdline_text().map(str::to_owned)
            && matches!(token.as_str(), "up" | "down" | "tab" | "S-tab")
        {
            let suggestions = command_suggestions(&draft);
            if !suggestions.is_empty() {
                let count = suggestions.len();
                let backwards = token == "up" || token == "S-tab";
                self.command_selection = if backwards {
                    self.command_selection.wrapping_add(count).wrapping_sub(1) % count
                } else {
                    (self.command_selection + 1) % count
                };
                if token == "tab" || token == "S-tab" {
                    reader
                        .vim
                        .replace_cmdline(suggestions[self.command_selection].0.clone());
                    self.command_selection = 0;
                }
            }
            cx.notify();
            return;
        }

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
        self.command_selection = 0;
        cx.notify();
        for action in actions {
            self.apply(action, vp, window, cx);
        }
        if let Some(reader) = self.reader.as_ref()
            && reader.vim.in_cmdline()
        {
            #[cfg(not(test))]
            if self.open_native_command_prompt(window, cx) {
                return;
            }
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
            Action::SetAnimations(value) => self.set_animations(value, cx),
            Action::SetContinuous(value) => self.set_continuous(value, cx),
            Action::SetWebcomics(value) => self.set_webcomics(value, cx),
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
            Action::OpenSettings => self.open_settings_window(window, cx),
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

        let (wanted, animating, progress_changed): (
            Vec<(usize, u32)>,
            bool,
            Option<(String, usize)>,
        ) = {
            let Some(reader) = self.reader.as_mut() else {
                return false;
            };
            reader.viewport = vp;
            reader.animate = animate;
            let Some(pages) = reader.pages.clone() else {
                return false;
            };
            let previous_page = reader.page;
            reader.settle(&vp);
            let animating = reader.advance_animations(dt, &vp);
            if reader.continuous {
                reader.settle(&vp);
            }
            let progress_changed = if reader.continuous && reader.page != previous_page {
                let id = reader.book_id.clone();
                let page = reader.page;
                if let Some(book) = self.library.get_mut(&id) {
                    book.current_page = page;
                    book.opened = true;
                    book.last_read = Some(now_secs());
                }
                Some((id, page))
            } else {
                None
            };

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
            let keep: HashSet<usize> = if reader.continuous {
                let (tops, heights) = reader.continuous_layout(&vp);
                let low = reader.off.1;
                let high = low + vp.height;
                let visible: Vec<usize> = tops
                    .iter()
                    .zip(&heights)
                    .enumerate()
                    .filter_map(|(ix, (top, height))| {
                        (*top < high && *top + *height > low).then_some(ix)
                    })
                    .collect();
                let first = visible.first().copied().unwrap_or(reader.page);
                let last = visible.last().copied().unwrap_or(reader.page);
                (first.saturating_sub(1)..=(last + 2).min(pages.len().saturating_sub(1))).collect()
            } else {
                (reader.page.saturating_sub(pagecache::KEEP_BEHIND)
                    ..=reader.page + pagecache::KEEP_AHEAD)
                    .collect()
            };
            for ix in pagecache::plan_eviction_with_keep(
                &entries,
                reader.page,
                pagecache::BUDGET_BYTES,
                &keep,
            ) {
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

            let wanted_indices: Vec<usize> = if reader.continuous {
                keep.iter()
                    .copied()
                    .filter(|ix| *ix < pages.len())
                    .collect()
            } else {
                pagecache::prefetch_order(reader.page, pages.len())
            };
            let wanted = wanted_indices
                .into_iter()
                .map(|ix| {
                    let tier = if reader.continuous {
                        let width = vp.width * reader.zoom * vp.scale;
                        let (w, h) = reader.orig_sizes.get(ix).and_then(|v| *v).unwrap_or((2, 3));
                        layout::tier_for(width.max(width * h as f32 / w.max(1) as f32))
                    } else if ix == reader.page {
                        current_tier
                    } else {
                        neighbour_tier
                    };
                    (ix, tier)
                })
                .collect();
            (wanted, animating, progress_changed)
        };

        if progress_changed.is_some() {
            self.save_library();
        }

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
                let old_top = if reader.continuous && ix < reader.page {
                    let (tops, _) = reader.continuous_layout(&reader.viewport);
                    tops.get(reader.page).copied()
                } else {
                    None
                };
                if let Some(size) = reader.orig_sizes.get_mut(ix) {
                    *size = Some(orig);
                }
                if let Some(old_top) = old_top {
                    let (tops, _) = reader.continuous_layout(&reader.viewport);
                    let shift = tops.get(reader.page).copied().unwrap_or(old_top) - old_top;
                    reader.off.1 = (reader.off.1 + shift).max(0.0);
                    reader.scroll_target.1 = (reader.scroll_target.1 + shift).max(0.0);
                }
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
        if self.native_popup {
            return Some(
                deferred(self.scrim(false, cx))
                    .with_priority(1)
                    .into_any_element(),
            );
        }
        Some(match self.overlay.as_ref()? {
            Overlay::Menu(menu) => self.render_menu(menu, cx),
            Overlay::LibraryMenu(menu) => self.render_library_menu(menu, cx),
            Overlay::Confirm(dialog) => self.render_confirm(dialog, cx),
            Overlay::Properties(dialog) => self.render_properties(dialog, cx),
            Overlay::Series(dialog) => self.render_series_dialog(dialog, cx),
            Overlay::Settings => self.render_settings(cx),
        })
    }

    /// Full-window layer behind a dialog: swallows clicks meant for the view underneath and
    /// closes the dialog when clicked. `dim` darkens the view behind it.
    fn scrim(&self, dim: bool, cx: &mut Context<Self>) -> gpui_kit::Div {
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
            .bg(with_alpha(
                overlay_shade(self.colors),
                if dim {
                    0.25 + self.colors.widget_opacity * 0.30
                } else {
                    0.0
                },
            ))
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
            .rounded_lg()
            .bg(self.menu_surface())
            .border_1()
            .border_color(self.colors.border)
            .shadow_lg()
            .text_sm()
            .text_color(self.colors.foreground)
            .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| {
                cx.stop_propagation()
            })
            .on_mouse_down(MouseButton::Right, |_: &MouseDownEvent, _, cx| {
                cx.stop_propagation()
            });

        for (ix, entry) in menu.entries.iter().enumerate() {
            list = match entry {
                Entry::Separator => {
                    list.child(div().h(px(1.)).mx_2().my_1().bg(self.colors.border))
                }
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
                        .anchor(Anchor::TopLeft)
                        .snap_to_window_with_margin(px(8.))
                        .child(list),
                ),
        )
        .with_priority(1)
        .into_any_element()
    }

    fn render_library_menu(&self, menu: &LibraryMenu, cx: &mut Context<Self>) -> AnyElement {
        let entries = [
            (IconName::FilePlus, "Add files", "Import CBZ and ZIP comics"),
            (
                IconName::FolderPlus,
                "Add folder",
                "Import a folder of images",
            ),
            (
                IconName::Settings,
                "Settings",
                "Themes and reader preferences",
            ),
            (
                IconName::Folder,
                if self.library.settings().group_series {
                    "Show individual volumes"
                } else {
                    "Group into folders"
                },
                "Group volumes by source folder or assigned series",
            ),
            (IconName::LogOut, "Quit", "Close yomu"),
        ];
        let mut list = div()
            .w(px(260.))
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .rounded_lg()
            .bg(self.menu_surface())
            .border_1()
            .border_color(self.colors.border)
            .shadow_lg()
            .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| {
                cx.stop_propagation()
            })
            .on_mouse_down(MouseButton::Right, |_: &MouseDownEvent, _, cx| {
                cx.stop_propagation()
            });
        for (index, (icon, label, description)) in entries.into_iter().enumerate() {
            let selected = menu.selected == index;
            list = list.child(
                div()
                    .id(SharedString::from(format!("library-menu-{index}")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .cursor_pointer()
                    .when(selected, |row| {
                        row.bg(with_alpha(self.colors.accent, 0.20))
                            .border_1()
                            .border_color(with_alpha(self.colors.accent, 0.70))
                    })
                    .hover(|style| style.bg(with_alpha(self.colors.accent, 0.15)))
                    .child(Icon::new(icon).size_4().text_color(self.colors.foreground))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(self.colors.foreground)
                                    .child(label),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(self.colors.muted)
                                    .child(description),
                            ),
                    )
                    .on_mouse_move(cx.listener(move |this, _: &MouseMoveEvent, _, cx| {
                        if let Some(Overlay::LibraryMenu(menu)) = this.overlay.as_mut() {
                            menu.selected = index;
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.run_library_menu_action(index, window, cx);
                    })),
            );
        }

        deferred(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _, cx| this.close_overlay(cx)),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _: &MouseDownEvent, _, cx| this.close_overlay(cx)),
                )
                .child(
                    div()
                        .absolute()
                        .top(px(52.))
                        .right(px(18.))
                        .child(self.animate_popup("library-menu-popup", list)),
                ),
        )
        .with_priority(2)
        .into_any_element()
    }

    fn menu_item(
        &self,
        menu: &BookMenu,
        ix: usize,
        item: &Item,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let icon = match item.action {
            BookAction::Open => IconName::BookOpen,
            BookAction::StartOver | BookAction::ResetProgress => IconName::RotateCcw,
            BookAction::MarkRead => IconName::CircleCheck,
            BookAction::MarkUnread => IconName::CircleX,
            BookAction::RevealFile => IconName::FolderOpen,
            BookAction::Properties => IconName::Info,
            BookAction::SetSeries => IconName::Library,
            BookAction::Remove | BookAction::RemoveSelected => IconName::Trash,
            BookAction::ClearSelection => IconName::X,
        };
        let item_color = if !item.enabled {
            self.colors.muted
        } else if item.danger {
            self.colors.danger
        } else {
            self.colors.foreground
        };
        let base = div()
            .id(SharedString::from(format!("menu-item-{ix}")))
            .h(px(28.))
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .rounded_lg()
            .child(
                div()
                    .w(px(20.))
                    .h(px(20.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Icon::new(icon).size_4().text_color(item_color)),
            )
            .child(item.label.clone());

        if !item.enabled {
            return base
                .text_color(self.colors.muted)
                .opacity(0.6)
                .into_any_element();
        }

        let action = item.action;
        let book_id = menu.book_id.clone();
        base.text_color(if item.danger {
            self.colors.danger
        } else {
            self.colors.foreground
        })
        .cursor_pointer()
        .when(menu.selected == Some(ix), |row| {
            row.bg(with_alpha(self.colors.accent, 0.20))
                .border_1()
                .border_color(with_alpha(self.colors.accent, 0.70))
        })
        .hover(|style| style.bg(with_alpha(self.colors.accent, 0.15)))
        .on_mouse_move(cx.listener(move |this, _: &MouseMoveEvent, _, cx| this.menu_hover(ix, cx)))
        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.run_book_action(action, &book_id, window, cx);
        }))
        .into_any_element()
    }

    fn panel(&self, width: f32) -> gpui_kit::Div {
        div()
            .w(px(width))
            .max_w_full()
            .flex()
            .flex_col()
            .gap_4()
            .p_6()
            .rounded_lg()
            .bg(self.widget_surface(self.colors.surface))
            .border_1()
            .border_color(self.colors.border)
            .shadow_lg()
            .text_sm()
            .text_color(self.colors.foreground)
            .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| {
                cx.stop_propagation()
            })
            .on_mouse_down(MouseButton::Right, |_: &MouseDownEvent, _, cx| {
                cx.stop_propagation()
            })
    }

    fn animate_popup<E>(&self, id: &'static str, surface: E) -> gpui_kit::AnimationElement<E>
    where
        E: IntoElement + Styled + 'static,
    {
        surface.with_animation(
            id,
            Animation::new(Duration::from_millis(if self.reduce_motion {
                1
            } else {
                180
            }))
            .with_easing(ease_out_quint()),
            |surface, progress| {
                surface
                    .opacity(progress)
                    .relative()
                    .top(px((1.0 - progress) * 9.0))
            },
        )
    }

    fn render_confirm(&self, dialog: &ConfirmDialog, cx: &mut Context<Self>) -> AnyElement {
        self.scrim(true, cx)
            .child(
                self.animate_popup("confirm-dialog", self.panel(380.))
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(dialog.title.clone()),
                    )
                    .child(
                        div()
                            .text_color(self.colors.muted)
                            .child(dialog.message.clone()),
                    )
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
                                self.colors,
                                true,
                                cx.listener(|this, _: &ClickEvent, _, cx| this.close_overlay(cx)),
                            ))
                            .child(panel_button(
                                "confirm-accept",
                                dialog.confirm_label,
                                ButtonKind::Primary,
                                self.colors,
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
                        .text_color(self.colors.muted)
                        .child(label.clone()),
                )
                .child(div().flex_1().min_w_0().truncate().child(value.clone()))
        });

        self.scrim(true, cx)
            .child(
                self.animate_popup("properties-dialog", self.panel(480.))
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
                        self.colors,
                        true,
                        cx.listener(|this, _: &ClickEvent, _, cx| this.close_overlay(cx)),
                    ))),
            )
            .into_any_element()
    }

    fn render_series_dialog(&self, dialog: &SeriesDialog, cx: &mut Context<Self>) -> AnyElement {
        self.scrim(true, cx)
            .child(
                self.animate_popup("series-dialog", self.panel(460.))
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(if dialog.book_ids.len() == 1 {
                                "Set series"
                            } else {
                                "Set series for selected books"
                            }),
                    )
                    .child(div().text_color(self.colors.muted).child(
                        "A folder name is suggested when the books share a parent folder. Edit it, or clear the field to leave them ungrouped.",
                    ))
                    .child(
                        div()
                            .id("series-name-input")
                            .w_full()
                            .min_h(px(36.))
                            .px_3()
                            .py_2()
                            .rounded_lg()
                            .border_1()
                            .border_color(self.colors.accent)
                            .bg(self.widget_surface(self.colors.background))
                            .text_color(self.colors.foreground)
                            .child(if dialog.value.is_empty() {
                                "Type a series name…".to_string()
                            } else {
                                dialog.value.clone()
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_between()
                            .items_center()
                            .child(div().text_xs().text_color(self.colors.muted).child(
                                "Type · Enter to save · Esc to cancel · Ctrl+A clears",
                            ))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap_2()
                                    .child(panel_button(
                                        "series-cancel",
                                        "Cancel",
                                        ButtonKind::Normal,
                                        self.colors,
                                        true,
                                        cx.listener(|this, _: &ClickEvent, _, cx| {
                                            this.close_overlay(cx)
                                        }),
                                    ))
                                    .child(panel_button(
                                        "series-save",
                                        "Save",
                                        ButtonKind::Primary,
                                        self.colors,
                                        true,
                                        cx.listener(|this, _: &ClickEvent, _, cx| {
                                            this.save_series_dialog(cx)
                                        }),
                                    )),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let settings = self.library.settings();
        let opacity = settings.opacity;
        let theme_name = settings.theme_name.clone();
        let theme_path = self.custom_theme_path.display().to_string();
        let theme_status = self.theme_status.clone();
        let continuous = settings.continuous;
        let webcomics = settings.webcomics;
        let rtl = settings.rtl;
        let fit = settings.fit;
        let animate = settings.animate;
        let grouped = settings.group_series;
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
                    .bg(self.colors.border)
                    .child(
                        div()
                            .h_full()
                            .w(relative(fraction))
                            .rounded_full()
                            .bg(self.colors.accent),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .top(px(2.))
                    .left(px(fraction * (SLIDER_W - KNOB)))
                    .size(px(KNOB))
                    .rounded_full()
                    .bg(self.colors.foreground),
            );

        // No dimming scrim here: you want to see the window behind the panel while dragging.
        let tab_button = |tab, label| {
            let selected = self.settings_tab == tab;
            let id = match tab {
                SettingsTab::Appearance => "settings-tab-appearance",
                SettingsTab::Library => "settings-tab-library",
                SettingsTab::Reader => "settings-tab-reader",
                SettingsTab::Keyboard => "settings-tab-keyboard",
            };
            panel_button(
                id,
                label,
                if selected {
                    ButtonKind::Primary
                } else {
                    ButtonKind::Normal
                },
                self.colors,
                true,
                cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.settings_tab = tab;
                    cx.notify();
                }),
            )
        };
        let appearance_tab = tab_button(SettingsTab::Appearance, "Appearance");
        let library_tab = tab_button(SettingsTab::Library, "Library");
        let reader_tab = tab_button(SettingsTab::Reader, "Reader");
        let keyboard_tab = tab_button(SettingsTab::Keyboard, "Keyboard & Commands");

        let body = match self.settings_tab {
            SettingsTab::Appearance => div()
                .flex().flex_col().gap_3()
                .child(div().font_weight(FontWeight::SEMIBOLD).child("Theme"))
                .child(div().flex().flex_row().items_center().gap_2()
                    .child(panel_button("settings-theme-previous", "‹", ButtonKind::Normal, self.colors, true,
                        cx.listener(|this, _: &ClickEvent, _, cx| this.cycle_theme(-1, cx))))
                    .child(div().flex_1().text_center().text_color(self.colors.muted).child(theme_name))
                    .child(panel_button("settings-theme-next", "›", ButtonKind::Normal, self.colors, true,
                        cx.listener(|this, _: &ClickEvent, _, cx| this.cycle_theme(1, cx))))
                )
                .child(div().flex().flex_row().items_center().justify_between().gap_2()
                    .child(div().min_w_0().text_xs().text_color(self.colors.muted).truncate().child(theme_path))
                    .child(panel_button("settings-theme-reload", "Reload custom themes", ButtonKind::Normal,
                        self.colors, true, cx.listener(|this, _: &ClickEvent, _, cx| this.reload_custom_themes(cx)))))
                .children(theme_status.map(|status| div().text_xs().text_color(self.colors.muted).child(status)))
                .child(div().flex().justify_between().child("Window background opacity")
                    .child(div().text_color(self.colors.muted).child(format!("{}%", (opacity * 100.0).round() as u32))))
                .child(track)
                .into_any_element(),
            SettingsTab::Library => div().flex().flex_col().gap_3()
                .child(div().font_weight(FontWeight::SEMIBOLD).child("Library"))
                .child(panel_button("settings-group-series", if grouped { "Folder grouping: On" } else { "Folder grouping: Off" },
                    if grouped { ButtonKind::Primary } else { ButtonKind::Normal }, self.colors, true, cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.library.settings_mut().group_series = !this.library.settings().group_series;
                        this.open_group = None;
                        this.save_library();
                        cx.notify();
                    })))
                .child(div().text_xs().text_color(self.colors.muted).child("Group volumes from the same directory into folder cards. A manually assigned series takes precedence; clearing it restores the directory group."))
                .into_any_element(),
            SettingsTab::Reader => div().flex().flex_col().gap_3()
                .child(div().font_weight(FontWeight::SEMIBOLD).child("Reader"))
                .child(panel_button("settings-direction", if rtl { "Reading direction: Right to left" } else { "Reading direction: Left to right" },
                    if rtl { ButtonKind::Primary } else { ButtonKind::Normal }, self.colors, true, cx.listener(move |this, _: &ClickEvent, _, cx| this.set_direction(!rtl, cx))))
                .child(panel_button("settings-fit", format!("Page fit: {} (click to cycle)", fit.label()),
                    ButtonKind::Normal, self.colors, true, cx.listener(move |this, _: &ClickEvent, _, cx| this.set_fit(fit.next(), cx))))
                    .child(panel_button("settings-animations", if animate { "Page animations: On" } else { "Page animations: Off" },
                    if animate { ButtonKind::Primary } else { ButtonKind::Normal }, self.colors, true, cx.listener(|this, _: &ClickEvent, _, cx| this.set_animations(None, cx))))
                .child(panel_button("settings-continuous", if continuous { "Reading mode: Continuous" } else { "Reading mode: Single page" },
                    if continuous { ButtonKind::Primary } else { ButtonKind::Normal }, self.colors, true, cx.listener(|this, _: &ClickEvent, _, cx| this.set_continuous(None, cx))))
                .child(panel_button("settings-webcomics", if webcomics { "Webcomics mode: On" } else { "Webcomics mode: Off" },
                    if webcomics { ButtonKind::Primary } else { ButtonKind::Normal }, self.colors, true, cx.listener(|this, _: &ClickEvent, _, cx| this.set_webcomics(None, cx))))
                .child(div().text_xs().text_color(self.colors.muted).child("Webcomics mode forces a gapless vertical strip. Turning it off restores the selected single-page or continuous mode."))
                .into_any_element(),
            SettingsTab::Keyboard => {
                let mut content = div().flex().flex_col().gap_3().child(div().font_weight(FontWeight::SEMIBOLD).child("Keyboard & Commands"));
                for (section, rows) in HELP {
                    let mut group = div().flex().flex_col().gap_1()
                        .child(div().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(self.colors.muted).child(*section));
                    for (keys, description) in *rows {
                        group = group.child(div().flex().justify_between().gap_4()
                            .child(div().min_w(px(150.)).text_color(self.colors.accent).child(*keys))
                            .child(div().flex_1().text_color(self.colors.foreground).child(*description)));
                    }
                    content = content.child(group);
                }
                content.into_any_element()
            }
        };

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
                self.animate_popup("settings-dialog", self.panel(640.))
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Settings"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(appearance_tab)
                            .child(library_tab)
                            .child(reader_tab)
                            .child(keyboard_tab),
                    )
                    .child(div().min_h(px(250.)).child(body))
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
                                self.colors,
                                !is_default,
                                cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.request_reset_settings(cx)
                                }),
                            ))
                            .child(panel_button(
                                "settings-done",
                                "Done",
                                ButtonKind::Primary,
                                self.colors,
                                true,
                                cx.listener(|this, _: &ClickEvent, _, cx| this.close_overlay(cx)),
                            )),
                    ),
            )
            .into_any_element()
    }

    fn render_library(&self, cx: &mut Context<Self>) -> AnyElement {
        let all_books = self.library.books();
        let count = all_books.len();
        let books: Vec<&Book> = all_books
            .iter()
            .filter(|book| {
                search::matches(
                    &book.title,
                    &book.path,
                    book.series.as_deref(),
                    &self.search_query,
                )
            })
            .collect();
        let status: Option<SharedString> = if self.importing {
            Some(
                self.importing_title
                    .as_ref()
                    .map(|title| format!("{} imported · {title}", self.imported_count))
                    .unwrap_or_else(|| "Scanning folder…".to_string())
                    .into(),
            )
        } else {
            self.toast.clone()
        };

        let selected = self.selection.len();

        let search_value = div()
            .flex_1()
            .flex()
            .items_center()
            .child(self.search_query.clone())
            .children((self.search_active && self.search_caret_visible).then(|| {
                div()
                    .ml(px(1.))
                    .w(px(1.5))
                    .h(px(18.))
                    .rounded_full()
                    .bg(self.colors.accent)
            }));
        let search_field = div()
            .id("library-search")
            .w(px(360.))
            .max_w(px(520.))
            .h(px(38.))
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .rounded_lg()
            .border_1()
            .border_color(if self.search_active {
                self.colors.accent
            } else {
                self.colors.border
            })
            .bg(self.widget_surface(self.colors.surface))
            .text_sm()
            .text_color(if self.search_query.is_empty() {
                self.colors.muted
            } else {
                self.colors.foreground
            })
            .child(div().text_lg().child("⌕"))
            .child(search_value)
            .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| {
                cx.stop_propagation()
            })
            .on_mouse_down(MouseButton::Right, |_: &MouseDownEvent, _, cx| {
                cx.stop_propagation()
            })
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.activate_search(cx);
            }));
        let search_field: AnyElement = if self.search_active {
            self.animate_popup("library-search-focus", search_field)
                .into_any_element()
        } else {
            search_field.into_any_element()
        };

        let selection_controls = if selected > 0 {
            let controls = div().flex().flex_row().items_center().gap_2();
            let controls = if selected < count {
                controls.child(button(
                    "select-all",
                    "Select all",
                    self.colors,
                    cx.listener(|this, _: &ClickEvent, _, cx| {
                        let order = this.library_order();
                        this.selection.select_all(&order);
                        cx.notify();
                    }),
                ))
            } else {
                controls
            };
            Some(
                controls
                    .child(button_colored(
                        "select-remove",
                        "Remove",
                        self.colors,
                        cx.listener(|this, _: &ClickEvent, _, cx| this.request_remove_selected(cx)),
                    ))
                    .child(button(
                        "select-cancel",
                        "Cancel",
                        self.colors,
                        cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.selection.clear();
                            cx.notify();
                        }),
                    )),
            )
        } else {
            None
        };

        let header = div()
            .relative()
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .h(px(72.))
            .px_6()
            .children(selection_controls.map(|controls| div().absolute().left_6().child(controls)))
            .when(selected == 0, |h| {
                #[cfg(test)]
                {
                    h.child(div().debug_selector(|| "add-files".into()).size_0())
                }
                #[cfg(not(test))]
                {
                    h
                }
            })
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .child(search_field),
            )
            .child(div().flex_1().flex().justify_end().child(icon_button(
                "library-menu-toggle",
                "Open menu",
                IconName::Menu,
                self.colors,
                cx.listener(|this, _: &ClickEvent, window, cx| {
                    if this.native_popup || matches!(this.overlay, Some(Overlay::LibraryMenu(_))) {
                        this.close_overlay(cx);
                    } else {
                        this.open_library_menu(window, cx);
                    }
                }),
            )));

        let body = if count == 0 {
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
                        .text_color(self.colors.foreground)
                        .child("Your library is empty"),
                )
                .child(div().text_sm().text_color(self.colors.muted).child(
                    "Drop CBZ files or folders onto this window, or use Add files (Ctrl+O).",
                ))
                .into_any_element()
        } else if books.is_empty() {
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
                        .text_color(self.colors.foreground)
                        .child("No matching books"),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(self.colors.muted)
                        .child("Try another title or source path."),
                )
                .into_any_element()
        } else if self.library.settings().group_series {
            let groups = directory_groups(all_books);
            let visible_groups: Vec<_> = groups
                .iter()
                .filter_map(|group| {
                    let members: Vec<_> = group
                        .books
                        .iter()
                        .copied()
                        .filter(|book| books.iter().any(|visible| visible.id == book.id))
                        .collect();
                    (!members.is_empty()).then_some((group, members))
                })
                .collect();
            if let Some(open_key) = self.open_group.as_deref()
                && let Some((group, members)) = visible_groups
                    .iter()
                    .find(|(group, _)| group.key == open_key)
            {
                let mut content = div().flex().flex_col().gap_4().child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(icon_button(
                            "group-back",
                            "Back to library",
                            IconName::ArrowLeft,
                            self.colors,
                            cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.open_group = None;
                                cx.notify();
                            }),
                        ))
                        .child(
                            div()
                                .text_lg()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(self.colors.foreground)
                                .child(format!("{} · {}", group.name, members.len())),
                        ),
                );
                content = content.child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap_6()
                        .children(members.iter().map(|book| self.book_card(book, cx))),
                );
                div()
                    .id("grid")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_6()
                    .pb_6()
                    .child(content)
                    .into_any_element()
            } else {
                let grouped_ids: HashSet<_> = visible_groups
                    .iter()
                    .flat_map(|(_, members)| members.iter().map(|book| book.id.as_str()))
                    .collect();
                let loose_books: Vec<_> = books
                    .iter()
                    .copied()
                    .filter(|book| !grouped_ids.contains(book.id.as_str()))
                    .collect();
                let mut cards: Vec<AnyElement> = visible_groups
                    .iter()
                    .map(|(group, members)| self.group_card(group, members, cx))
                    .collect();
                cards.extend(loose_books.iter().map(|book| self.book_card(book, cx)));
                div()
                    .id("grid")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_6()
                    .pb_6()
                    .child(div().flex().flex_row().flex_wrap().gap_6().children(cards))
                    .into_any_element()
            }
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
            .relative()
            .flex()
            .flex_col()
            .child(header)
            .child(body)
            .children(status.map(|text| {
                div()
                    .absolute()
                    .bottom_5()
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .child(pill(div(), text, self.colors))
            }))
            .into_any_element()
    }

    fn group_card(
        &self,
        group: &LibraryGroup<'_>,
        books: &[&Book],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let first = books[0];
        let group_key = group.key.clone();
        div()
            .id(SharedString::from(format!("folder-{}", group.key)))
            .w(px(CARD_W))
            .flex_none()
            .flex()
            .flex_col()
            .rounded_lg()
            .border_1()
            .border_color(self.colors.border)
            .bg(self.widget_surface(self.colors.surface))
            .overflow_hidden()
            .cursor_pointer()
            .hover(|style| style.border_color(self.colors.accent).shadow_md())
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.open_group = Some(group_key.clone());
                cx.notify();
            }))
            .child(
                div()
                    .relative()
                    .w(px(CARD_W))
                    .h(px(CARD_H))
                    .overflow_hidden()
                    .rounded_lg()
                    .bg(self.widget_surface(self.colors.surface))
                    .child(
                        img(self.dirs.cover(&first.id))
                            .w_full()
                            .h_full()
                            .object_fit(ObjectFit::Cover),
                    )
                    .child(
                        div()
                            .absolute()
                            .top_2()
                            .right_2()
                            .px_2()
                            .py_1()
                            .rounded_lg()
                            .bg(self.widget_surface(self.colors.background))
                            .text_xs()
                            .text_color(self.colors.foreground)
                            .child(format!("{} volumes", books.len())),
                    ),
            )
            .child(
                div()
                    .p_3()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(self.colors.foreground)
                    .truncate()
                    .child(group.name.clone()),
            )
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
            .rounded_lg()
            .overflow_hidden()
            .bg(self.widget_surface(self.colors.surface))
            .border_2()
            .border_color(if selected {
                self.colors.accent
            } else {
                self.colors.surface
            })
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
                    .bg(with_alpha(self.colors.accent, 0.22))
            }));

        let progress_bar = div()
            .h(px(3.))
            .w_full()
            .rounded_full()
            .bg(self.colors.surface_hover)
            .child(
                div()
                    .h_full()
                    .w(relative(progress))
                    .rounded_full()
                    .bg(self.colors.accent),
            );

        div()
            .id(SharedString::from(format!("book-{}", book.id)))
            .w(px(CARD_W))
            .flex_none()
            .flex()
            .flex_col()
            .rounded_lg()
            .border_1()
            .border_color(if selected {
                self.colors.accent
            } else {
                self.colors.border
            })
            .bg(self.widget_surface(self.colors.surface))
            .overflow_hidden()
            .cursor_pointer()
            .hover(|style| style.border_color(self.colors.accent).shadow_md())
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
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.open_context_menu(&menu_id, event.position, window, cx);
                    // The root window also listens for right-clicks to dismiss an
                    // already-open popup. Do not let this opening click bubble up
                    // and immediately close the popup we just created.
                    cx.stop_propagation();
                }),
            )
            .child(cover)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(self.colors.foreground)
                            .truncate()
                            .child(SharedString::from(book.title.clone())),
                    )
                    .child(progress_bar)
                    .child(div().text_xs().text_color(self.colors.muted).child(status)),
            )
            .into_any_element()
    }

    fn render_reader(&self, reader: &Reader, cx: &mut Context<Self>) -> AnyElement {
        let vp = reader.viewport;

        // Pages are drawn at a size and position we compute ourselves, so what you see is exactly
        // what the fit mode and zoom say, independent of the texture's pixel size. During a page
        // turn the outgoing and incoming pages are both drawn, shifted along the same strip.
        let mut content: Vec<AnyElement> = Vec::new();
        if let Some(error) = &reader.error {
            content.push(centered(
                format!("Couldn't open this book: {error}"),
                self.colors,
            ));
        } else if reader.continuous {
            let (tops, heights) = reader.continuous_layout(&vp);
            let width = vp.width * reader.zoom;
            let left = (vp.width - width) / 2.0 - reader.off.0;
            let visible_top = reader.off.1;
            let visible_bottom = visible_top + vp.height;
            for (ix, (top, height)) in tops.iter().zip(&heights).enumerate() {
                if *top + *height < visible_top || *top > visible_bottom {
                    continue;
                }
                if let Some(page) = reader.images.get(&ix) {
                    content.push(page_layer(
                        &page.image,
                        left,
                        *top - reader.off.1,
                        (width, *height),
                    ));
                } else if let Some(error) = reader.failed.get(&ix) {
                    content.push(centered(
                        format!("Couldn't decode page {}: {error}", ix + 1),
                        self.colors,
                    ));
                }
            }
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
                    content.push(centered(
                        format!("Couldn't decode page {}: {error}", reader.page + 1),
                        self.colors,
                    ));
                } else if reader.pages.is_none() {
                    content.push(centered("Opening…", self.colors));
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
            .bg(self.colors.accent);
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
                    "{} / {}  ·  {}  ·  {}%  ·  {}  ·  {}",
                    reader.page + 1,
                    reader.total,
                    reader.fit.label(),
                    (reader.zoom * 100.0).round() as i32,
                    if reader.rtl {
                        "right to left"
                    } else {
                        "left to right"
                    },
                    if reader.webcomics {
                        "webcomics"
                    } else if reader.continuous {
                        "continuous"
                    } else {
                        "single page"
                    }
                ),
                self.colors,
            )
        });
        let hud = self.hud.clone().map(|text| {
            pill(
                div().absolute().top_4().left_0().right_0(),
                text,
                self.colors,
            )
        });

        let vim_status = if let Some(input) = reader.vim.cmdline_text() {
            let suggestions = command_suggestions(input);
            let mut prompt = div()
                .w(px(560.))
                .max_w_full()
                .flex()
                .flex_col()
                .rounded_lg()
                .border_1()
                .border_color(self.colors.border)
                .bg(self.widget_surface(self.colors.surface))
                .text_sm();
            if !suggestions.is_empty() {
                let mut rows = div().flex().flex_col().p_1();
                for (index, (completion, label, description)) in suggestions.iter().enumerate() {
                    let selected = index == self.command_selection;
                    let completion = completion.clone();
                    rows = rows.child(
                        div()
                            .id(SharedString::from(format!("command-suggestion-{index}")))
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_4()
                            .px_2()
                            .py_1()
                            .rounded_lg()
                            .cursor_pointer()
                            .when(selected, |row| {
                                row.bg(with_alpha(
                                    self.colors.accent,
                                    (self.library.settings().opacity + 0.10).min(1.0),
                                ))
                            })
                            .child(div().text_color(self.colors.accent).child(label.clone()))
                            .child(
                                div()
                                    .flex_1()
                                    .text_right()
                                    .text_color(self.colors.muted)
                                    .child(*description),
                            )
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                if let Some(reader) = this.reader.as_mut() {
                                    reader.vim.replace_cmdline(completion.clone());
                                }
                                this.command_selection = 0;
                                cx.notify();
                            })),
                    );
                }
                prompt = prompt.child(rows);
            }
            prompt = prompt.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_t_1()
                    .border_color(self.colors.border)
                    .px_3()
                    .py_2()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(self.colors.accent)
                            .child(":"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_color(self.colors.foreground)
                            .child(format!("{input}▏")),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(self.colors.muted)
                            .child("↑↓ select · Tab complete · Enter run · Esc cancel"),
                    ),
            );
            Some(
                div()
                    .absolute()
                    .bottom_6()
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .child(self.animate_popup("reader-command-prompt", prompt)),
            )
        } else {
            reader.vim.status().map(|text| {
                pill(
                    div().absolute().bottom_4().left_0().right_0(),
                    text,
                    self.colors,
                )
            })
        };

        div()
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(with_alpha(
                self.colors.background,
                self.library.settings().opacity,
            ))
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
            .children(if self.native_popup { None } else { vim_status })
            .children(reader.show_help.then(|| help_overlay(self.colors)))
            .into_any_element()
    }
}

impl Render for NativeBookMenu {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let opacity = (self.colors.widget_opacity + 0.15).clamp(0.70, 0.92);
        let mut list = div()
            .size_full()
            .flex()
            .flex_col()
            .p_1()
            .rounded_xl()
            .overflow_hidden()
            .bg(with_alpha(self.colors.surface, opacity))
            .border_1()
            .border_color(self.colors.border)
            .shadow_lg()
            .text_sm()
            .text_color(self.colors.foreground)
            .on_key_down(cx.listener(|_, event: &KeyDownEvent, popup, _| {
                if event.keystroke.key == "escape" {
                    popup.remove_window();
                }
            }));

        for (ix, entry) in self.menu.entries.iter().enumerate() {
            match entry {
                Entry::Separator => {
                    list = list.child(div().h(px(1.)).mx_2().my_1().bg(self.colors.border));
                }
                Entry::Item(item) => {
                    let icon = match item.action {
                        BookAction::Open => IconName::BookOpen,
                        BookAction::StartOver | BookAction::ResetProgress => IconName::RotateCcw,
                        BookAction::MarkRead => IconName::CircleCheck,
                        BookAction::MarkUnread => IconName::CircleX,
                        BookAction::RevealFile => IconName::FolderOpen,
                        BookAction::Properties => IconName::Info,
                        BookAction::SetSeries => IconName::Library,
                        BookAction::Remove | BookAction::RemoveSelected => IconName::Trash,
                        BookAction::ClearSelection => IconName::X,
                    };
                    let item_color = if !item.enabled {
                        self.colors.muted
                    } else if item.danger {
                        self.colors.danger
                    } else {
                        self.colors.foreground
                    };
                    let icon_color = item_color;
                    let selected = self.selected == Some(ix);
                    let enabled = item.enabled;
                    let action = item.action;
                    let id = self.menu.book_id.clone();
                    let parent = self.parent.clone();
                    let parent_window = self.parent_window;
                    let row = div()
                        .id(SharedString::from(format!("native-menu-item-{ix}")))
                        .h(px(28.))
                        .px_2()
                        .flex()
                        .items_center()
                        .gap_2()
                        .rounded_md()
                        .border_1()
                        .border_color(if selected && enabled {
                            with_alpha(self.colors.accent, 0.70)
                        } else {
                            with_alpha(self.colors.accent, 0.0)
                        })
                        .when(selected && enabled, |row| {
                            row.bg(with_alpha(self.colors.accent, 0.20))
                        })
                        .child(Icon::new(icon).size_4().text_color(icon_color))
                        .child(item.label.clone())
                        .text_color(item_color)
                        .when(!enabled, |row| row.opacity(0.6));
                    let row = if enabled {
                        row.cursor_pointer()
                            .on_mouse_move(cx.listener(move |this, _: &MouseMoveEvent, _, cx| {
                                if this.selected != Some(ix) {
                                    this.selected = Some(ix);
                                    cx.notify();
                                }
                            }))
                            .on_click(cx.listener(move |_, _: &ClickEvent, popup, cx| {
                                let _ = cx.update_window(parent_window, |_, main_window, app| {
                                    let _ = parent.update(app, |state, state_cx| {
                                        state.run_book_action(action, &id, main_window, state_cx);
                                    });
                                });
                                popup.remove_window();
                            }))
                    } else {
                        row
                    };
                    list = list.child(row);
                }
            }
        }

        list
    }
}

impl Render for NativeLibraryMenu {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let opacity = (self.colors.widget_opacity + 0.15).clamp(0.70, 0.92);
        let entries = [
            (IconName::FilePlus, "Add files", "Import CBZ and ZIP comics"),
            (
                IconName::FolderPlus,
                "Add folder",
                "Import a folder of images",
            ),
            (
                IconName::Settings,
                "Settings",
                "Themes and reader preferences",
            ),
            (
                IconName::Folder,
                if self.grouped {
                    "Show individual volumes"
                } else {
                    "Group into folders"
                },
                "Group volumes by source folder or assigned series",
            ),
            (IconName::LogOut, "Quit", "Close yomu"),
        ];

        let parent = self.parent.clone();
        let parent_window = self.parent_window;
        let colors = self.colors;

        let mut list = div()
            .size_full()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .rounded_xl()
            .overflow_hidden()
            .bg(with_alpha(colors.surface, opacity))
            .border_1()
            .border_color(colors.border)
            .shadow_lg()
            .on_key_down(cx.listener(|_, event: &KeyDownEvent, popup, _| {
                if event.keystroke.key == "escape" {
                    popup.remove_window();
                }
            }));

        for (index, (icon, label, description)) in entries.into_iter().enumerate() {
            let selected = self.selected == Some(index);
            let parent = parent.clone();
            let row = div()
                .id(SharedString::from(format!("native-library-menu-{index}")))
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .px_3()
                .py_2()
                .rounded_lg()
                .cursor_pointer()
                .border_1()
                .border_color(if selected {
                    with_alpha(colors.accent, 0.70)
                } else {
                    with_alpha(colors.accent, 0.0)
                })
                .when(selected, |row| row.bg(with_alpha(colors.accent, 0.20)))
                .child(Icon::new(icon).size_4().text_color(colors.foreground))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(div().text_sm().text_color(colors.foreground).child(label))
                        .child(div().text_xs().text_color(colors.muted).child(description)),
                )
                .on_mouse_move(cx.listener(move |this, _: &MouseMoveEvent, _, cx| {
                    if this.selected != Some(index) {
                        this.selected = Some(index);
                        cx.notify();
                    }
                }))
                .on_click(cx.listener(move |_, _: &ClickEvent, popup, cx| {
                    popup.remove_window();
                    let _ = cx.update_window(parent_window, |_, main_window, app| {
                        let _ = parent.update(app, |state, state_cx| {
                            state.run_library_menu_action(index, main_window, state_cx);
                        });
                    });
                }));
            list = list.child(row);
        }

        list
    }
}

#[allow(dead_code)]
impl NativeCommandPrompt {
    fn on_key(&mut self, event: &KeyDownEvent, popup: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        match key {
            "escape" => {
                popup.remove_window();
            }
            "enter" => {
                let parent = self.parent.clone();
                let parent_window = self.parent_window;
                let _ = cx.update_window(parent_window, |_, main_window, app| {
                    let _ = parent.update(app, |state, state_cx| {
                        if let Some(reader) = state.reader.as_mut() {
                            let vp = viewport_of(main_window);
                            let actions = reader.vim.feed("enter");
                            for action in actions {
                                state.apply(action, vp, main_window, state_cx);
                            }
                        }
                    });
                });
                popup.remove_window();
            }
            "up" => {
                let suggestions = command_suggestions(&self.input);
                if !suggestions.is_empty() {
                    let count = suggestions.len();
                    self.selection = self.selection.wrapping_add(count).wrapping_sub(1) % count;
                    cx.notify();
                }
            }
            "down" => {
                let suggestions = command_suggestions(&self.input);
                if !suggestions.is_empty() {
                    let count = suggestions.len();
                    self.selection = (self.selection + 1) % count;
                    cx.notify();
                }
            }
            "tab" => {
                let suggestions = command_suggestions(&self.input);
                if !suggestions.is_empty() {
                    let chosen = &suggestions[self.selection % suggestions.len()].0;
                    self.input = chosen.clone();
                    self.selection = 0;
                    let parent = self.parent.clone();
                    let input_clone = self.input.clone();
                    let _ = parent.update(cx, |state, _| {
                        if let Some(reader) = state.reader.as_mut() {
                            reader.vim.replace_cmdline(input_clone);
                        }
                    });
                    cx.notify();
                }
            }
            "backspace" => {
                if self.input.is_empty() {
                    popup.remove_window();
                } else {
                    self.input.pop();
                    self.selection = 0;
                    let parent = self.parent.clone();
                    let input_clone = self.input.clone();
                    let _ = parent.update(cx, |state, _| {
                        if let Some(reader) = state.reader.as_mut() {
                            reader.vim.replace_cmdline(input_clone);
                        }
                    });
                    cx.notify();
                }
            }
            _ => {
                if let Some(ch) = &event.keystroke.key_char {
                    if !event.keystroke.modifiers.control && !event.keystroke.modifiers.alt {
                        self.input.push_str(ch);
                        self.selection = 0;
                        let parent = self.parent.clone();
                        let input_clone = self.input.clone();
                        let _ = parent.update(cx, |state, _| {
                            if let Some(reader) = state.reader.as_mut() {
                                reader.vim.replace_cmdline(input_clone);
                            }
                        });
                        cx.notify();
                    }
                }
            }
        }
    }
}

impl Render for NativeCommandPrompt {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let opacity = (self.colors.widget_opacity + 0.15).clamp(0.70, 0.92);
        let suggestions = command_suggestions(&self.input);

        let mut prompt = div()
            .size_full()
            .flex()
            .flex_col()
            .justify_end()
            .rounded_xl()
            .overflow_hidden()
            .bg(with_alpha(self.colors.surface, opacity))
            .border_1()
            .border_color(self.colors.border)
            .shadow_lg()
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, popup, cx| {
                this.on_key(event, popup, cx);
            }));

        if !suggestions.is_empty() {
            let mut rows = div()
                .id("native-command-suggestions")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_1()
                .p_2();

            for (index, (completion, label, description)) in suggestions.iter().enumerate() {
                let selected = index == self.selection;
                let completion = completion.clone();
                let parent = self.parent.clone();
                rows = rows.child(
                    div()
                        .id(SharedString::from(format!("native-cmd-suggestion-{index}")))
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_4()
                        .px_2()
                        .py_1()
                        .rounded_lg()
                        .cursor_pointer()
                        .border_1()
                        .border_color(if selected {
                            with_alpha(self.colors.accent, 0.70)
                        } else {
                            with_alpha(self.colors.accent, 0.0)
                        })
                        .when(selected, |row| row.bg(with_alpha(self.colors.accent, 0.20)))
                        .child(div().text_color(self.colors.accent).child(label.clone()))
                        .child(
                            div()
                                .flex_1()
                                .text_right()
                                .text_color(self.colors.muted)
                                .child(*description),
                        )
                        .on_mouse_move(cx.listener(move |this, _: &MouseMoveEvent, _, cx| {
                            if this.selection != index {
                                this.selection = index;
                                cx.notify();
                            }
                        }))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.input = completion.clone();
                            this.selection = 0;
                            let parent = parent.clone();
                            let input_clone = this.input.clone();
                            let _ = parent.update(cx, |state, _| {
                                if let Some(reader) = state.reader.as_mut() {
                                    reader.vim.replace_cmdline(input_clone);
                                }
                            });
                            cx.notify();
                        })),
                );
            }
            prompt = prompt.child(rows);
        }

        prompt = prompt.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .border_t_1()
                .border_color(self.colors.border)
                .px_3()
                .py_2()
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(self.colors.accent)
                        .child(":"),
                )
                .child(
                    div()
                        .flex_1()
                        .text_color(self.colors.foreground)
                        .child(format!("{}▏", self.input)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(self.colors.muted)
                        .child("↑↓ select · Tab complete · Enter run · Esc cancel"),
                ),
        );

        prompt
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
            with_alpha(self.colors.background, 0.0)
        } else {
            with_alpha(self.colors.background, self.library.settings().opacity)
        };
        let overlay = self.render_overlay(cx);

        div()
            .key_context(if reading { "Reader" } else { "Library" })
            .track_focus(&self.focus_handle)
            .size_full()
            .rounded_lg()
            .overflow_hidden()
            .bg(root_bg)
            .text_color(self.colors.foreground)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key(event, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    if this.native_popup {
                        this.close_native_popup(cx);
                    }
                    if this.search_active {
                        this.deactivate_search(cx);
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    if this.native_popup {
                        this.close_native_popup(cx);
                    }
                }),
            )
            .on_action(cx.listener(|_, _: &ToggleFullscreen, window, _| window.toggle_fullscreen()))
            .on_action(cx.listener(|this, _: &ImportFiles, _, cx| this.import_dialog(false, cx)))
            .on_action(cx.listener(|this, _: &ImportFolder, _, cx| this.import_dialog(true, cx)))
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
                this.open_settings_window(window, cx)
            }))
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

fn centered(text: impl Into<SharedString>, colors: UiColors) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(colors.muted)
        .child(text.into())
        .into_any_element()
}

/// A small rounded label centred horizontally inside `row` (an absolutely positioned div).
fn pill(row: gpui_kit::Div, text: impl Into<SharedString>, colors: UiColors) -> gpui_kit::Div {
    row.flex().justify_center().child(
        div()
            .px_3()
            .py_1()
            .rounded_full()
            .bg(with_alpha(
                colors.background,
                (colors.widget_opacity + 0.10).min(1.0),
            ))
            .text_sm()
            .text_color(colors.foreground)
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
            (
                "r  v  w  i  f",
                "direction, continuous mode, webcomics, page info, fullscreen",
            ),
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
            (":continuous [on|off]", "toggle stacked continuous reading"),
            (":webcomics [on|off]", "force gapless webcomics strip mode"),
        ],
    ),
];

/// (completed command text, display text, short explanation)
fn command_suggestions(input: &str) -> Vec<(String, String, &'static str)> {
    const COMMANDS: &[(&str, &str)] = &[
        ("fit", "Choose page fit mode"),
        ("zoom", "Set zoom percentage or reset"),
        ("rtl", "Read right to left"),
        ("ltr", "Read left to right"),
        ("animate", "Toggle page animations"),
        ("continuous", "Toggle continuous scrolling"),
        ("webcomics", "Toggle gapless webcomics mode"),
        ("goto", "Jump to a page number"),
        ("mark", "Set a letter mark"),
        ("settings", "Open Settings"),
        ("info", "Toggle page information"),
        ("help", "Show keyboard help"),
        ("fullscreen", "Toggle fullscreen"),
        ("q", "Return to the library"),
        ("qa", "Quit yomu"),
    ];
    const FITS: &[(&str, &str)] = &[
        ("page", "Fit the whole page"),
        ("width", "Fit the page width"),
        ("height", "Fit the page height"),
        ("original", "Show at original size"),
    ];
    const TOGGLES: &[(&str, &str)] = &[("on", "Enable"), ("off", "Disable")];
    const ZOOM: &[(&str, &str)] = &[
        ("reset", "Reset zoom"),
        ("150", "Set zoom to 150% (edit the number as needed)"),
    ];

    let trimmed = input.trim_start();
    let mut words = trimmed.split_whitespace();
    let command = words.next().unwrap_or_default();
    let has_separator = trimmed.ends_with(char::is_whitespace);
    if let Some((_, _)) = COMMANDS.iter().find(|(name, _)| *name == command)
        && (has_separator || input.contains(' '))
    {
        let args = if command == "fit" {
            FITS
        } else if matches!(command, "animate" | "continuous" | "webcomics") {
            TOGGLES
        } else if command == "zoom" {
            ZOOM
        } else {
            &[]
        };
        let partial = words.next().unwrap_or_default();
        return args
            .iter()
            .filter(|(value, _)| value.starts_with(partial))
            .map(|(value, description)| {
                (
                    format!("{command} {value}"),
                    (*value).to_string(),
                    *description,
                )
            })
            .collect();
    }

    let prefix = if has_separator { "" } else { command };
    COMMANDS
        .iter()
        .filter(|(name, _)| name.starts_with(prefix))
        .take(7)
        .map(|(name, description)| (format!("{name} "), (*name).to_string(), *description))
        .collect()
}

fn help_overlay(colors: UiColors) -> AnyElement {
    let mut panel = div()
        .w(px(600.))
        .max_w_full()
        .flex()
        .flex_col()
        .gap_3()
        .p_6()
        .rounded_lg()
        .bg(with_alpha(
            colors.surface,
            (colors.widget_opacity + 0.10).min(1.0),
        ))
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
                        .text_color(colors.muted)
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
                                .text_color(colors.accent)
                                .child(*keys),
                        )
                        .child(div().text_color(colors.foreground).child(*what))
                })),
        );
    }
    panel = panel.child(
        div()
            .text_xs()
            .text_color(colors.muted)
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
        .bg(with_alpha(
            overlay_shade(colors),
            (0.5 + colors.widget_opacity * 0.15).min(0.65),
        ))
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
    colors: UiColors,
    enabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let alpha = (colors.widget_opacity + 0.10).min(1.0);
    let button = Button::new(id)
        .label(label)
        .rounded(ButtonRounded::Large)
        .small()
        .disabled(!enabled)
        .on_click(on_click);
    match kind {
        ButtonKind::Normal => button
            .secondary()
            .outline()
            .bg(with_alpha(colors.surface, alpha))
            .into_any_element(),
        ButtonKind::Primary => button
            .primary()
            .bg(with_alpha(colors.accent, alpha))
            .into_any_element(),
    }
}

fn button(
    id: &'static str,
    label: &'static str,
    colors: UiColors,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    button_colored(id, label, colors, on_click)
}

fn icon_button(
    id: &'static str,
    label: &'static str,
    icon: IconName,
    colors: UiColors,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let button = Button::new(id)
        .accessibility_label(label)
        .rounded(ButtonRounded::Large)
        .small()
        .ghost()
        .text_color(colors.foreground)
        .bg(with_alpha(
            colors.surface,
            (colors.widget_opacity + 0.10).min(1.0),
        ))
        // Render the bundled SVG directly so its theme color is explicit. The
        // Button icon convenience path inherits a color from Kit's component
        // theme, which can be transparent against custom/translucent surfaces.
        .child(Icon::new(icon).size_4().text_color(colors.foreground))
        .on_click(on_click);
    #[cfg(test)]
    {
        div()
            .debug_selector(|| id.into())
            .child(button)
            .into_any_element()
    }
    #[cfg(not(test))]
    {
        button.into_any_element()
    }
}

/// A header button with a theme-aware destructive variant when requested.
fn button_colored(
    id: &'static str,
    label: &'static str,
    colors: UiColors,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let button = Button::new(id)
        .label(label)
        .rounded(ButtonRounded::Large)
        .small()
        .on_click(on_click);
    let button = if label == "Remove" {
        button.danger()
    } else {
        button.secondary()
    };
    let alpha = (colors.widget_opacity + 0.10).min(1.0);
    let button = button.bg(with_alpha(
        if label == "Remove" {
            colors.danger
        } else {
            colors.surface
        },
        alpha,
    ));
    #[cfg(test)]
    {
        div().debug_selector(|| id.into()).child(button)
    }
    #[cfg(not(test))]
    {
        button
    }
}

fn main() {
    application()
        // GPUI Kit icons are SVG asset paths. Without registering this source,
        // custom-rendered toolbar and context-menu icons resolve to empty images.
        .with_assets(gpui_kit::assets::AllAssets)
        .run(|cx: &mut App| {
            gpui_kit::init(cx);
            // The reader handles its own keys (vim-style, see `vim.rs`); only these are bindings.
            cx.bind_keys([
                KeyBinding::new("ctrl-o", ImportFiles, Some("Library")),
                KeyBinding::new("ctrl-shift-o", ImportFolder, Some("Library")),
                KeyBinding::new("f11", ToggleFullscreen, None),
                KeyBinding::new("ctrl-,", OpenSettings, None),
                KeyBinding::new("ctrl-q", Quit, None),
            ]);

            cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
            cx.on_window_closed(|cx, _window_id| {
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
                    window.focus(&focus, cx);
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
    use gpui_kit::{Entity, Modifiers, TestAppContext, VisualTestContext, point};
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
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.bind_keys([KeyBinding::new("ctrl-,", OpenSettings, None)]);
        });
        cx.add_window_view(move |window, cx| {
            let yomu = Yomu::with_dirs(dirs, cx);
            window.focus(&yomu.focus_handle, cx);
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
            Some(Overlay::LibraryMenu(_)) => "library-menu",
            Some(Overlay::Confirm(_)) => "confirm",
            Some(Overlay::Properties(_)) => "properties",
            Some(Overlay::Series(_)) => "series",
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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
        assert_eq!(selected(&view, cx), Some(8), "Remove from Library is last");
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
    fn shift_click_selects_a_range(cx: &mut TestAppContext) {
        let fx = fixture("shiftclick");
        let (view, cx) = launch(&fx, cx);

        ctrl_click(cx, 0);
        click_with(cx, card(2), Modifiers::shift());
        assert_eq!(selected(&view, cx), fx.ids);
        assert_eq!(reading(&view, cx), None);
    }

    #[gpui_kit::test]
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

    #[gpui_kit::test]
    fn escape_and_backspace_exit_open_group(cx: &mut TestAppContext) {
        let fx = fixture("opengroup");
        let (view, cx) = launch(&fx, cx);

        view.update(cx, |this, _| {
            this.open_group = Some("series:test".into());
        });
        assert_eq!(
            view.read_with(cx, |this, _| this.open_group.clone()),
            Some("series:test".into())
        );

        cx.simulate_keystrokes("escape");
        assert_eq!(
            view.read_with(cx, |this, _| this.open_group.clone()),
            None,
            "escape exits open group"
        );

        view.update(cx, |this, _| {
            this.open_group = Some("series:test".into());
        });
        cx.simulate_keystrokes("backspace");
        assert_eq!(
            view.read_with(cx, |this, _| this.open_group.clone()),
            None,
            "backspace exits open group"
        );
    }

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
    fn delete_with_nothing_selected_does_nothing(cx: &mut TestAppContext) {
        let fx = fixture("deletenothing");
        let (view, cx) = launch(&fx, cx);
        cx.simulate_keystrokes("delete");
        assert_eq!(book_count(&view, cx), 3);
        assert_eq!(overlay_kind(&view, cx), "none");
    }

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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
                "Set Series for Selected Books…".to_string(),
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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
            series: None,
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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

    #[gpui_kit::test]
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
