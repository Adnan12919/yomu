//! The library: which books exist, where reading progress is, and how new ones get imported.

use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    archive::{self, SourceKind},
    imaging,
    layout::FitMode,
    util::{fnv1a, natural_cmp, now_secs},
};

const MAX_SCAN_DEPTH: usize = 6;

/// Where yomu keeps its data (`$XDG_DATA_HOME/yomu`, or `$YOMU_DATA_DIR` if set).
#[derive(Debug, Clone)]
pub struct DataDirs {
    root: PathBuf,
}

impl DataDirs {
    pub fn new() -> Result<Self> {
        let root = match std::env::var_os("YOMU_DATA_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => dirs::data_dir()
                .context("could not determine the user data directory")?
                .join("yomu"),
        };
        Self::at(root)
    }

    pub fn at(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(root.join("covers"))
            .with_context(|| format!("creating {}", root.display()))?;
        Ok(Self { root })
    }

    pub fn library_file(&self) -> PathBuf {
        self.root.join("library.json")
    }

    pub fn cover(&self, id: &str) -> PathBuf {
        self.root.join("covers").join(format!("{id}.jpg"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Book {
    pub id: String,
    pub title: String,
    /// Optional user-assigned series. Missing values in existing libraries remain ungrouped.
    #[serde(default)]
    pub series: Option<String>,
    pub path: PathBuf,
    pub kind: SourceKind,
    pub page_count: usize,
    /// Zero-based index of the page the reader was last on.
    #[serde(default)]
    pub current_page: usize,
    /// Whether the book has ever been opened. Unopened books show 0% instead of 1 page.
    #[serde(default)]
    pub opened: bool,
    pub added_at: u64,
    #[serde(default)]
    pub last_read: Option<u64>,
    /// Vim-style marks: letter to zero-based page index.
    #[serde(default)]
    pub marks: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadStatus {
    Unread,
    InProgress,
    Finished,
}

impl Book {
    pub fn status(&self) -> ReadStatus {
        if self.is_finished() {
            ReadStatus::Finished
        } else if self.opened && self.current_page > 0 {
            ReadStatus::InProgress
        } else {
            ReadStatus::Unread
        }
    }

    /// True if there is reading progress that resetting or removing would throw away.
    pub fn has_progress(&self) -> bool {
        self.status() != ReadStatus::Unread
    }

    pub fn reset_progress(&mut self) {
        self.current_page = 0;
        self.opened = false;
        self.last_read = None;
    }

    pub fn mark_read(&mut self) {
        self.opened = true;
        self.current_page = self.page_count.saturating_sub(1);
        self.last_read = Some(now_secs());
    }

    /// Put the reading position back on the first page, ready to be opened.
    pub fn start_over(&mut self) {
        self.opened = true;
        self.current_page = 0;
        self.last_read = Some(now_secs());
    }

    /// Reading progress in `0.0..=1.0`.
    pub fn progress(&self) -> f32 {
        if !self.opened || self.page_count == 0 {
            return 0.0;
        }
        ((self.current_page + 1) as f32 / self.page_count as f32).clamp(0.0, 1.0)
    }

    pub fn is_finished(&self) -> bool {
        self.opened && self.page_count > 0 && self.current_page + 1 >= self.page_count
    }
}

/// Lowest background opacity the settings allow. Below this the window becomes hard to see.
pub const OPACITY_MIN: f32 = 0.3;
pub const OPACITY_MAX: f32 = 1.0;

/// Clamp to the allowed range and round to a whole percent. Non-finite values fall back to opaque.
pub fn clamp_opacity(value: f32) -> f32 {
    if !value.is_finite() {
        return OPACITY_MAX;
    }
    (value.clamp(OPACITY_MIN, OPACITY_MAX) * 100.0).round() / 100.0
}

/// Version of the library file format. Bumped when a stored default changes meaning.
///
/// * 1 (and files without a version): reading direction defaulted to right-to-left.
/// * 2: reading direction defaults to left-to-right (Western comics); right-to-left is a preference.
pub const LIBRARY_VERSION: u32 = 2;

/// Preferences that carry over from book to book and between launches.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Settings {
    /// Name of the GPUI Kit theme selected in Appearance settings.
    pub theme_name: String,
    /// Right-to-left reading order (manga). Off by default: pages run left to right.
    pub rtl: bool,
    pub fit: FitMode,
    /// Slide between pages and glide when scrolling. Reduced-motion preferences of the desktop
    /// switch this off regardless of the stored value.
    pub animate: bool,
    /// Stack pages vertically and scroll through the whole book instead of turning pages.
    pub continuous: bool,
    /// Force a connected, gapless vertical strip of pages, regardless of the normal reading mode.
    pub webcomics: bool,
    /// Show books under their assigned series headings in the library.
    pub group_series: bool,
    /// Opacity of the window background, `OPACITY_MIN..=OPACITY_MAX`. Page images, covers and
    /// text always stay fully opaque; only the background behind them becomes see-through.
    pub opacity: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme_name: "Default Dark".to_string(),
            rtl: false,
            fit: FitMode::Page,
            animate: true,
            continuous: false,
            webcomics: false,
            group_series: false,
            opacity: OPACITY_MAX,
        }
    }
}

impl Settings {
    pub fn set_opacity(&mut self, value: f32) {
        self.opacity = clamp_opacity(value);
    }

    /// Read settings leniently: any missing, mistyped or out-of-range field falls back to its
    /// default (or is clamped) instead of making the whole library file unreadable.
    pub fn from_value(value: &serde_json::Value) -> Self {
        let mut settings = Self::default();
        let Some(object) = value.as_object() else {
            return settings;
        };
        if let Some(rtl) = object.get("rtl").and_then(|v| v.as_bool()) {
            settings.rtl = rtl;
        }
        if let Some(theme_name) = object.get("theme_name").and_then(|v| v.as_str()) {
            if !theme_name.trim().is_empty() {
                settings.theme_name = theme_name.to_string();
            }
        }
        if let Some(animate) = object.get("animate").and_then(|v| v.as_bool()) {
            settings.animate = animate;
        }
        if let Some(continuous) = object.get("continuous").and_then(|v| v.as_bool()) {
            settings.continuous = continuous;
        }
        if let Some(webcomics) = object.get("webcomics").and_then(|v| v.as_bool()) {
            settings.webcomics = webcomics;
        }
        if let Some(group_series) = object.get("group_series").and_then(|v| v.as_bool()) {
            settings.group_series = group_series;
        }
        if let Some(fit) = object
            .get("fit")
            .and_then(|v| serde_json::from_value::<FitMode>(v.clone()).ok())
        {
            settings.fit = fit;
        }
        if let Some(opacity) = object.get("opacity").and_then(|v| v.as_f64()) {
            settings.set_opacity(opacity as f32);
        }
        settings
    }
}

fn lenient_settings<'de, D>(deserializer: D) -> Result<Settings, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(Settings::from_value(&value))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Library {
    /// File format version; files from before versioning read as 0.
    #[serde(default)]
    version: u32,
    #[serde(default)]
    books: Vec<Book>,
    #[serde(default, deserialize_with = "lenient_settings")]
    settings: Settings,
}

impl Default for Library {
    fn default() -> Self {
        Self {
            version: LIBRARY_VERSION,
            books: Vec::new(),
            settings: Settings::default(),
        }
    }
}

impl Library {
    /// Bring a library written by an older version up to date.
    fn migrate(&mut self) {
        if self.version < 2 {
            // Reading direction used to default to right-to-left and that default was written
            // into every library file, so a stored `true` cannot be told apart from a deliberate
            // choice. Move everyone to the new left-to-right default; manga readers switch it
            // back once with `r` and it sticks from then on.
            self.settings.rtl = false;
        }
        self.version = LIBRARY_VERSION;
    }

    pub fn load(dirs: &DataDirs) -> Result<Self> {
        let file = dirs.library_file();
        let text = match fs::read_to_string(&file) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(err).with_context(|| format!("reading {}", file.display())),
        };
        match serde_json::from_str::<Library>(&text) {
            Ok(mut lib) => {
                lib.migrate();
                lib.sort();
                Ok(lib)
            }
            Err(err) => {
                // Keep the unreadable file around instead of silently overwriting it.
                let backup = file.with_extension("json.corrupt");
                let _ = fs::rename(&file, &backup);
                bail!(
                    "library.json was unreadable ({err}); moved it to {} and started fresh",
                    backup.display()
                )
            }
        }
    }

    /// Write atomically (temp file + rename) so a crash cannot leave a half-written library.
    pub fn save(&self, dirs: &DataDirs) -> Result<()> {
        let file = dirs.library_file();
        let tmp = file.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(self)?;
        fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &file).with_context(|| format!("replacing {}", file.display()))?;
        Ok(())
    }

    pub fn books(&self) -> &[Book] {
        &self.books
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn settings_mut(&mut self) -> &mut Settings {
        &mut self.settings
    }

    pub fn ids(&self) -> HashSet<String> {
        self.books.iter().map(|b| b.id.clone()).collect()
    }

    pub fn get(&self, id: &str) -> Option<&Book> {
        self.books.iter().find(|b| b.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Book> {
        self.books.iter_mut().find(|b| b.id == id)
    }

    /// Returns `false` (and changes nothing) if a book with the same id already exists.
    pub fn add(&mut self, book: Book) -> bool {
        if self.get(&book.id).is_some() {
            return false;
        }
        self.books.push(book);
        self.sort();
        true
    }

    /// Remove from the library and delete the cached cover. The comic file itself is untouched.
    pub fn remove(&mut self, id: &str, dirs: &DataDirs) {
        self.books.retain(|b| b.id != id);
        let _ = fs::remove_file(dirs.cover(id));
    }

    /// Assign or clear a series for several books. Names are trimmed and matching existing
    /// names are reused case-insensitively so groups do not split over capitalization.
    pub fn set_series(&mut self, ids: &[String], series: Option<String>) {
        let series = series
            .map(|name| name.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|name| !name.is_empty())
            .map(|name| {
                self.books
                    .iter()
                    .filter_map(|book| book.series.as_ref())
                    .find(|existing| existing.eq_ignore_ascii_case(&name))
                    .cloned()
                    .unwrap_or(name)
            });
        for id in ids {
            if let Some(book) = self.get_mut(id) {
                book.series = series.clone();
            }
        }
    }

    fn sort(&mut self) {
        self.books.sort_by(|a, b| natural_cmp(&a.title, &b.title));
    }
}

// ---------------------------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct ImportReport {
    pub added: Vec<Book>,
    /// Already in the library.
    pub skipped: usize,
    pub errors: Vec<String>,
}

impl ImportReport {
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.added.is_empty() {
            parts.push(format!("Imported {}", self.added.len()));
        }
        if self.skipped > 0 {
            parts.push(format!("{} already in library", self.skipped));
        }
        if let Some(first) = self.errors.first() {
            let more = self.errors.len() - 1;
            if more > 0 {
                parts.push(format!("{first} (+{more} more errors)"));
            } else {
                parts.push(first.clone());
            }
        }
        if parts.is_empty() {
            "Nothing to import".to_string()
        } else {
            parts.join(" · ")
        }
    }
}

enum Candidate {
    Archive(PathBuf),
    Folder(PathBuf),
}

/// Import files and folders. Folders are scanned recursively: every `.cbz`/`.zip` becomes a book,
/// and every folder that directly contains images becomes a book too.
///
/// Slow (decodes a cover per book), so call it from a background thread.
pub fn import_paths(
    paths: &[PathBuf],
    dirs: &DataDirs,
    existing: &HashSet<String>,
) -> ImportReport {
    let mut report = ImportReport::default();
    let mut candidates = Vec::new();

    for path in paths {
        collect(path, 0, true, &mut candidates, &mut report.errors);
    }

    let mut seen = existing.clone();
    for candidate in candidates {
        let (path, kind) = match &candidate {
            Candidate::Archive(p) => (p.as_path(), SourceKind::Archive),
            Candidate::Folder(p) => (p.as_path(), SourceKind::Folder),
        };
        let id = book_id(path);
        if !seen.insert(id) {
            report.skipped += 1;
            continue;
        }
        match build_book(path, kind, dirs) {
            Ok(book) => report.added.push(book),
            Err(err) => report
                .errors
                .push(format!("{}: {err:#}", display_name(path))),
        }
    }
    report
}

fn collect(
    path: &Path,
    depth: usize,
    explicit: bool,
    out: &mut Vec<Candidate>,
    errors: &mut Vec<String>,
) {
    let path = match path.canonicalize() {
        Ok(p) => p,
        Err(err) => {
            errors.push(format!("{}: {err}", display_name(path)));
            return;
        }
    };

    if path.is_file() {
        let name = display_name(&path);
        if archive::is_archive_name(&name) {
            out.push(Candidate::Archive(path));
        } else if explicit {
            errors.push(format!(
                "{name}: unsupported file type (CBZ/ZIP archives and image folders only)"
            ));
        }
    } else if path.is_dir() {
        if archive::folder_has_images(&path) {
            out.push(Candidate::Folder(path));
        } else if depth < MAX_SCAN_DEPTH {
            let mut children: Vec<PathBuf> = match fs::read_dir(&path) {
                Ok(entries) => entries.flatten().map(|e| e.path()).collect(),
                Err(err) => {
                    errors.push(format!("{}: {err}", display_name(&path)));
                    return;
                }
            };
            children.sort_by(|a, b| natural_cmp(&display_name(a), &display_name(b)));
            for child in children {
                if display_name(&child).starts_with('.') {
                    continue;
                }
                collect(&child, depth + 1, false, out, errors);
            }
        }
    }
}

fn build_book(path: &Path, kind: SourceKind, dirs: &DataDirs) -> Result<Book> {
    let id = book_id(path);
    let pages = archive::list_pages(path, kind)?;
    if pages.is_empty() {
        bail!("no images found");
    }

    // The first page is usually the cover, but fall back to the next few if it will not decode.
    let mut cover_written = false;
    for name in pages.iter().take(3) {
        let Ok(bytes) = archive::read_page(path, kind, name) else {
            continue;
        };
        if let Ok(jpeg) =
            imaging::make_cover_jpeg(&bytes, imaging::COVER_MAX_W, imaging::COVER_MAX_H)
        {
            fs::write(dirs.cover(&id), jpeg).context("writing cover")?;
            cover_written = true;
            break;
        }
    }
    if !cover_written {
        bail!("could not decode a cover image");
    }

    Ok(Book {
        id,
        title: title_for(path),
        series: None,
        path: path.to_path_buf(),
        kind,
        page_count: pages.len(),
        current_page: 0,
        opened: false,
        added_at: now_secs(),
        last_read: None,
        marks: BTreeMap::new(),
    })
}

/// Stable id derived from the canonical path, so re-importing the same file is a no-op and
/// progress survives the file being replaced by an updated copy.
fn book_id(path: &Path) -> String {
    format!("{:016x}", fnv1a(path.to_string_lossy().as_bytes()))
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn title_for(path: &Path) -> String {
    let raw = if path.is_dir() {
        display_name(path)
    } else {
        path.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| display_name(path))
    };
    let cleaned = raw.replace('_', " ");
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        raw
    } else {
        cleaned.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgba, RgbaImage};
    use std::io::{Cursor, Write};

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = RgbaImage::from_pixel(w, h, Rgba([120, 60, 200, 255]));
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    fn write_cbz(path: &Path, pages: &[&str]) {
        let file = fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for name in pages {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(&png(64, 96)).unwrap();
        }
        zip.finish().unwrap();
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("yomu-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn imports_cbz_with_natural_page_order_and_cover() {
        let tmp = temp_dir("cbz");
        let dirs = DataDirs::at(tmp.join("data")).unwrap();
        let cbz = tmp.join("Blue_Period_v01.cbz");
        write_cbz(
            &cbz,
            &["10.png", "2.png", "1.png", "__MACOSX/._1.png", "notes.txt"],
        );

        let report = import_paths(std::slice::from_ref(&cbz), &dirs, &HashSet::new());
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.added.len(), 1);

        let book = &report.added[0];
        assert_eq!(book.title, "Blue Period v01");
        assert_eq!(book.page_count, 3);
        assert!(dirs.cover(&book.id).exists());

        let pages = archive::list_pages(&book.path, book.kind).unwrap();
        assert_eq!(pages, vec!["1.png", "2.png", "10.png"]);
        let bytes = archive::read_page(&book.path, book.kind, &pages[0]).unwrap();
        assert!(!bytes.is_empty());

        let again = import_paths(
            &[cbz],
            &dirs,
            &report.added.iter().map(|b| b.id.clone()).collect(),
        );
        assert_eq!(again.skipped, 1);
        assert!(again.added.is_empty());
    }

    #[test]
    fn scans_folders_recursively() {
        let tmp = temp_dir("scan");
        let dirs = DataDirs::at(tmp.join("data")).unwrap();
        let root = tmp.join("manga");
        fs::create_dir_all(root.join("Series A")).unwrap();
        fs::create_dir_all(root.join("Loose Chapter")).unwrap();
        write_cbz(&root.join("Series A").join("v1.cbz"), &["1.png"]);
        write_cbz(&root.join("Series A").join("v2.cbz"), &["1.png"]);
        fs::write(root.join("Loose Chapter").join("001.png"), png(32, 48)).unwrap();
        fs::write(root.join("readme.txt"), "hi").unwrap();

        let report = import_paths(&[root], &dirs, &HashSet::new());
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.added.len(), 3);
        assert!(report.added.iter().any(|b| b.kind == SourceKind::Folder));
    }

    #[test]
    fn progress_and_persistence_roundtrip() {
        let tmp = temp_dir("persist");
        let dirs = DataDirs::at(tmp.join("data")).unwrap();
        let mut lib = Library::default();
        let mut book = Book {
            id: "abc".into(),
            title: "Test".into(),
            series: None,
            path: tmp.join("x.cbz"),
            kind: SourceKind::Archive,
            page_count: 10,
            current_page: 0,
            opened: false,
            added_at: 1,
            last_read: None,
            marks: BTreeMap::new(),
        };
        assert_eq!(book.progress(), 0.0);
        book.opened = true;
        book.current_page = 4;
        assert!((book.progress() - 0.5).abs() < 1e-6);
        book.current_page = 9;
        assert!(book.is_finished());

        book.marks.insert("a".into(), 3);
        assert!(lib.add(book.clone()));
        assert!(!lib.add(book));
        lib.settings_mut().rtl = true;
        lib.settings_mut().animate = false;
        lib.settings_mut().fit = FitMode::Width;
        lib.save(&dirs).unwrap();

        let loaded = Library::load(&dirs).unwrap();
        assert_eq!(loaded.books().len(), 1);
        assert_eq!(loaded.get("abc").unwrap().current_page, 9);
        assert_eq!(loaded.get("abc").unwrap().marks.get("a"), Some(&3));
        assert!(
            loaded.settings().rtl,
            "a deliberate right-to-left choice is kept"
        );
        assert!(!loaded.settings().animate);
        assert_eq!(loaded.settings().fit, FitMode::Width);
    }

    #[test]
    fn old_library_files_without_settings_still_load() {
        let tmp = temp_dir("compat");
        let dirs = DataDirs::at(tmp.join("data")).unwrap();
        let json = r#"{"books":[{"id":"x","title":"Old","path":"/tmp/old.cbz","kind":"Archive","page_count":5,"added_at":1}]}"#;
        fs::write(dirs.library_file(), json).unwrap();
        let loaded = Library::load(&dirs).unwrap();
        assert_eq!(loaded.books().len(), 1);
        assert!(!loaded.settings().rtl, "left to right is the default");
        assert!(loaded.settings().animate);
        assert_eq!(loaded.settings().fit, FitMode::Page);
        assert!(loaded.get("x").unwrap().marks.is_empty());
    }

    fn sample_book() -> Book {
        Book {
            id: "b".into(),
            title: "Sample".into(),
            series: None,
            path: PathBuf::from("/tmp/sample.cbz"),
            kind: SourceKind::Archive,
            page_count: 10,
            current_page: 0,
            opened: false,
            added_at: 1,
            last_read: None,
            marks: BTreeMap::new(),
        }
    }

    #[test]
    fn read_status_and_progress_changes() {
        let mut book = sample_book();
        assert_eq!(book.status(), ReadStatus::Unread);
        assert!(!book.has_progress());

        // Opening a book without turning a page is still "unread"
        book.opened = true;
        assert_eq!(book.status(), ReadStatus::Unread);

        book.current_page = 4;
        assert_eq!(book.status(), ReadStatus::InProgress);
        assert!(book.has_progress());

        book.mark_read();
        assert_eq!(book.status(), ReadStatus::Finished);
        assert_eq!(book.current_page, 9);
        assert!((book.progress() - 1.0).abs() < 1e-6);

        book.reset_progress();
        assert_eq!(book.status(), ReadStatus::Unread);
        assert_eq!(book.progress(), 0.0);
        assert!(book.last_read.is_none());

        book.current_page = 6;
        book.opened = true;
        book.start_over();
        assert_eq!(book.current_page, 0);
        assert!(book.opened);
    }

    #[test]
    fn reset_keeps_marks() {
        let mut book = sample_book();
        book.marks.insert("a".into(), 3);
        book.mark_read();
        book.reset_progress();
        assert_eq!(book.marks.get("a"), Some(&3));
    }

    #[test]
    fn opacity_is_clamped_and_rounded() {
        assert_eq!(clamp_opacity(2.0), OPACITY_MAX);
        assert_eq!(clamp_opacity(0.0), OPACITY_MIN);
        assert_eq!(clamp_opacity(-1.0), OPACITY_MIN);
        assert_eq!(clamp_opacity(f32::NAN), OPACITY_MAX);
        assert_eq!(clamp_opacity(0.8049), 0.80);
        assert_eq!(Settings::default().opacity, OPACITY_MAX);
    }

    #[test]
    fn invalid_stored_settings_fall_back_instead_of_breaking_the_library() {
        let tmp = temp_dir("badsettings");
        let dirs = DataDirs::at(tmp.join("data")).unwrap();
        let json = r#"{"books":[],"settings":{"rtl":"yes","fit":"Sideways","opacity":"lots"}}"#;
        fs::write(dirs.library_file(), json).unwrap();
        let loaded = Library::load(&dirs).unwrap();
        assert_eq!(*loaded.settings(), Settings::default());
        assert!(!dirs.library_file().with_extension("json.corrupt").exists());

        let json = r#"{"version":2,"books":[],"settings":{"rtl":true,"animate":"yes","fit":"Width","opacity":7.5}}"#;
        fs::write(dirs.library_file(), json).unwrap();
        let loaded = Library::load(&dirs).unwrap();
        assert!(loaded.settings().rtl);
        assert!(
            loaded.settings().animate,
            "a mistyped value falls back to the default"
        );
        assert_eq!(loaded.settings().fit, FitMode::Width);
        assert_eq!(loaded.settings().opacity, OPACITY_MAX);

        let json = r#"{"books":[],"settings":{"opacity":0.1}}"#;
        fs::write(dirs.library_file(), json).unwrap();
        assert_eq!(
            Library::load(&dirs).unwrap().settings().opacity,
            OPACITY_MIN
        );

        let json = r#"{"books":[],"settings":[1,2,3]}"#;
        fs::write(dirs.library_file(), json).unwrap();
        assert_eq!(
            *Library::load(&dirs).unwrap().settings(),
            Settings::default()
        );
    }

    #[test]
    fn reading_direction_default_moves_to_left_to_right_once() {
        let tmp = temp_dir("direction");
        let dirs = DataDirs::at(tmp.join("data")).unwrap();

        // a file written by the old version: right-to-left was stored as the default
        let old = r#"{"books":[],"settings":{"rtl":true,"fit":"Page","opacity":1.0}}"#;
        fs::write(dirs.library_file(), old).unwrap();
        let mut lib = Library::load(&dirs).unwrap();
        assert!(!lib.settings().rtl, "migrated to the left-to-right default");

        // choosing right-to-left afterwards is saved with the new version and survives reloads
        lib.settings_mut().rtl = true;
        lib.save(&dirs).unwrap();
        let text = fs::read_to_string(dirs.library_file()).unwrap();
        assert!(text.contains(&format!("\"version\": {LIBRARY_VERSION}")));
        assert!(Library::load(&dirs).unwrap().settings().rtl);
        assert!(Library::load(&dirs).unwrap().settings().rtl);

        // a brand new library starts at the current version with left-to-right
        let fresh = Library::default();
        assert!(!fresh.settings().rtl);
        fresh.save(&dirs).unwrap();
        assert!(!Library::load(&dirs).unwrap().settings().rtl);
    }

    #[test]
    fn opacity_persists_across_save_and_load() {
        let tmp = temp_dir("opacity");
        let dirs = DataDirs::at(tmp.join("data")).unwrap();
        let mut lib = Library::default();
        lib.settings_mut().set_opacity(0.65);
        lib.save(&dirs).unwrap();
        assert_eq!(Library::load(&dirs).unwrap().settings().opacity, 0.65);
    }

    #[test]
    fn unsupported_file_reports_error() {
        let tmp = temp_dir("bad");
        let dirs = DataDirs::at(tmp.join("data")).unwrap();
        let rar = tmp.join("book.cbr");
        fs::write(&rar, b"Rar!").unwrap();
        let report = import_paths(&[rar], &dirs, &HashSet::new());
        assert!(report.added.is_empty());
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].contains("unsupported"));
    }
}
