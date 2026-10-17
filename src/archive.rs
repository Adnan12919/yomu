use std::{
    fs::{self, File},
    io::{self, BufReader, Read},
    path::{Path, PathBuf},
    sync::Mutex,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::util::natural_cmp;

/// Refuse to inflate a single page beyond this size (guards against zip bombs).
const MAX_PAGE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_FOLDER_DEPTH: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceKind {
    Archive,
    Folder,
}

pub fn is_image_name(name: &str) -> bool {
    const EXTENSIONS: [&str; 5] = [".jpg", ".jpeg", ".png", ".webp", ".gif"];
    EXTENSIONS.iter().any(|ext| {
        if name.len() < ext.len() {
            return false;
        }
        name[name.len() - ext.len()..].eq_ignore_ascii_case(ext)
    })
}

pub fn is_archive_name(name: &str) -> bool {
    const EXTENSIONS: [&str; 2] = [".cbz", ".zip"];
    EXTENSIONS.iter().any(|ext| {
        if name.len() < ext.len() {
            return false;
        }
        name[name.len() - ext.len()..].eq_ignore_ascii_case(ext)
    })
}

/// Hidden files and macOS resource-fork folders are never pages.
fn is_junk(name: &str) -> bool {
    name.split('/')
        .any(|part| part.starts_with('.') || part == "__MACOSX")
}

struct CachedArchive {
    path: PathBuf,
    archive: zip::ZipArchive<BufReader<File>>,
}

static ARCHIVE_POOL: Mutex<Vec<CachedArchive>> = Mutex::new(Vec::new());

fn checkout_archive(path: &Path) -> Result<zip::ZipArchive<BufReader<File>>> {
    if let Ok(mut pool) = ARCHIVE_POOL.lock() {
        if let Some(idx) = pool.iter().position(|c| c.path == path) {
            return Ok(pool.swap_remove(idx).archive);
        }
    }
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = BufReader::with_capacity(64 * 1024, file);
    zip::ZipArchive::new(reader)
        .with_context(|| format!("{} is not a valid zip/cbz", path.display()))
}

fn checkin_archive(path: PathBuf, archive: zip::ZipArchive<BufReader<File>>) {
    if let Ok(mut pool) = ARCHIVE_POOL.lock() {
        if pool.len() >= 8 {
            pool.remove(0);
        }
        pool.push(CachedArchive { path, archive });
    }
}

/// Names of every page in reading order.
pub fn list_pages(path: &Path, kind: SourceKind) -> Result<Vec<String>> {
    let mut pages = match kind {
        SourceKind::Archive => {
            let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
            let reader = BufReader::with_capacity(64 * 1024, file);
            let archive = zip::ZipArchive::new(reader)
                .with_context(|| format!("{} is not a valid zip/cbz", path.display()))?;
            archive
                .file_names()
                .filter(|name| !name.ends_with('/') && is_image_name(name) && !is_junk(name))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        }
        SourceKind::Folder => {
            let mut out = Vec::new();
            walk_folder(path, path, 0, &mut out)
                .with_context(|| format!("reading folder {}", path.display()))?;
            out
        }
    };
    pages.sort_by(|a, b| natural_cmp(a, b));
    Ok(pages)
}

fn walk_folder(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if depth < MAX_FOLDER_DEPTH {
                walk_folder(root, &entry.path(), depth + 1, out)?;
            }
        } else if is_image_name(&name) {
            let relative = entry.path();
            let relative = relative.strip_prefix(root).unwrap_or(&relative);
            let s = relative.to_string_lossy();
            let normalized = if s.contains('\\') {
                s.replace('\\', "/")
            } else {
                s.into_owned()
            };
            out.push(normalized);
        }
    }
    Ok(())
}

/// True if the folder directly contains at least one image (so it is a "chapter").
pub fn folder_has_images(dir: &Path) -> bool {
    fs::read_dir(dir)
        .map(|entries| {
            entries.flatten().any(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                !name.starts_with('.') && is_image_name(&name) && e.path().is_file()
            })
        })
        .unwrap_or(false)
}

/// Raw (still encoded) bytes of one page.
pub fn read_page(path: &Path, kind: SourceKind, name: &str) -> Result<Vec<u8>> {
    match kind {
        SourceKind::Archive => {
            let mut archive = checkout_archive(path)?;

            let read_res = (|| -> Result<Vec<u8>> {
                let entry = archive
                    .by_name(name)
                    .with_context(|| format!("page {name} missing from archive"))?;
                if entry.size() > MAX_PAGE_BYTES {
                    bail!("page {name} is unreasonably large");
                }
                let mut buf = Vec::with_capacity(entry.size() as usize);
                entry.take(MAX_PAGE_BYTES).read_to_end(&mut buf)?;
                Ok(buf)
            })();

            checkin_archive(path.to_path_buf(), archive);

            read_res
        }
        SourceKind::Folder => {
            let full = path.join(name);
            fs::read(&full).with_context(|| format!("reading {}", full.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn extension_checks() {
        assert!(is_image_name("page.jpg"));
        assert!(is_image_name("PAGE.JPEG"));
        assert!(is_image_name("test.PNG"));
        assert!(is_image_name("manga.webp"));
        assert!(is_image_name("anim.gif"));
        assert!(!is_image_name("notes.txt"));
        assert!(!is_image_name("archive.zip"));

        assert!(is_archive_name("book.cbz"));
        assert!(is_archive_name("BOOK.ZIP"));
        assert!(!is_archive_name("book.tar"));
    }

    #[test]
    fn archive_cache_reads_repeatedly() {
        let tmp = std::env::temp_dir().join(format!("yomu_test_{}.cbz", crate::util::now_secs()));
        let file = File::create(&tmp).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("001.jpg", options).unwrap();
        zip.write_all(b"page1content").unwrap();
        zip.start_file("002.jpg", options).unwrap();
        zip.write_all(b"page2content").unwrap();
        zip.finish().unwrap();

        let pages = list_pages(&tmp, SourceKind::Archive).unwrap();
        assert_eq!(pages, vec!["001.jpg", "002.jpg"]);

        // First read populates cache
        let p1 = read_page(&tmp, SourceKind::Archive, "001.jpg").unwrap();
        assert_eq!(p1, b"page1content");

        // Second read uses cached archive handle
        let p2 = read_page(&tmp, SourceKind::Archive, "002.jpg").unwrap();
        assert_eq!(p2, b"page2content");

        let _ = fs::remove_file(&tmp);
    }

    #[test]
    fn concurrent_archive_reads() {
        let tmp = std::env::temp_dir().join(format!(
            "yomu_test_concurrent_{}.cbz",
            crate::util::now_secs()
        ));
        let file = File::create(&tmp).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("001.jpg", options).unwrap();
        zip.write_all(b"page1content").unwrap();
        zip.start_file("002.jpg", options).unwrap();
        zip.write_all(b"page2content").unwrap();
        zip.finish().unwrap();

        std::thread::scope(|s| {
            let t1 = s.spawn(|| read_page(&tmp, SourceKind::Archive, "001.jpg").unwrap());
            let t2 = s.spawn(|| read_page(&tmp, SourceKind::Archive, "002.jpg").unwrap());
            assert_eq!(t1.join().unwrap(), b"page1content");
            assert_eq!(t2.join().unwrap(), b"page2content");
        });

        let _ = fs::remove_file(&tmp);
    }
}
