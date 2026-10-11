//! The library: which books exist, where reading progress is, and how new ones get imported.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    archive::{self, SourceKind},
    imaging,
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
}

impl Book {
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

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Library {
    #[serde(default)]
    books: Vec<Book>,
}

impl Library {
    pub fn load(dirs: &DataDirs) -> Result<Self> {
        let file = dirs.library_file();
        let text = match fs::read_to_string(&file) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(err).with_context(|| format!("reading {}", file.display())),
        };
        match serde_json::from_str::<Library>(&text) {
            Ok(mut lib) => {
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
pub fn import_paths(paths: &[PathBuf], dirs: &DataDirs, existing: &HashSet<String>) -> ImportReport {
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
        if let Ok(jpeg) = imaging::make_cover_jpeg(&bytes, imaging::COVER_MAX_W, imaging::COVER_MAX_H)
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
        path: path.to_path_buf(),
        kind,
        page_count: pages.len(),
        current_page: 0,
        opened: false,
        added_at: now_secs(),
        last_read: None,
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
    if cleaned.is_empty() { raw } else { cleaned.to_string() }
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
        write_cbz(&cbz, &["10.png", "2.png", "1.png", "__MACOSX/._1.png", "notes.txt"]);

        let report = import_paths(&[cbz.clone()], &dirs, &HashSet::new());
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

        let again = import_paths(&[cbz], &dirs, &report.added.iter().map(|b| b.id.clone()).collect());
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
            path: tmp.join("x.cbz"),
            kind: SourceKind::Archive,
            page_count: 10,
            current_page: 0,
            opened: false,
            added_at: 1,
            last_read: None,
        };
        assert_eq!(book.progress(), 0.0);
        book.opened = true;
        book.current_page = 4;
        assert!((book.progress() - 0.5).abs() < 1e-6);
        book.current_page = 9;
        assert!(book.is_finished());

        assert!(lib.add(book.clone()));
        assert!(!lib.add(book));
        lib.save(&dirs).unwrap();

        let loaded = Library::load(&dirs).unwrap();
        assert_eq!(loaded.books().len(), 1);
        assert_eq!(loaded.get("abc").unwrap().current_page, 9);
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
