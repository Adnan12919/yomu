//! Reading pages out of a comic source: a `.cbz`/`.zip` archive or a plain folder of images.

use std::{
    fs::{self, File},
    io::{self, Read},
    path::Path,
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
    let lower = name.to_ascii_lowercase();
    [".jpg", ".jpeg", ".png", ".webp", ".gif"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

pub fn is_archive_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".cbz") || lower.ends_with(".zip")
}

/// Hidden files and macOS resource-fork folders are never pages.
fn is_junk(name: &str) -> bool {
    name.split('/')
        .any(|part| part.starts_with('.') || part == "__MACOSX")
}

/// Names of every page in reading order.
pub fn list_pages(path: &Path, kind: SourceKind) -> Result<Vec<String>> {
    let mut pages = match kind {
        SourceKind::Archive => {
            let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
            let archive = zip::ZipArchive::new(file)
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
            let joined = relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.push(joined);
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
            let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
            let mut archive = zip::ZipArchive::new(file)?;
            let entry = archive
                .by_name(name)
                .with_context(|| format!("page {name} missing from archive"))?;
            if entry.size() > MAX_PAGE_BYTES {
                bail!("page {name} is unreasonably large");
            }
            let mut buf = Vec::with_capacity(entry.size() as usize);
            entry.take(MAX_PAGE_BYTES).read_to_end(&mut buf)?;
            Ok(buf)
        }
        SourceKind::Folder => {
            let full = path.join(name);
            fs::read(&full).with_context(|| format!("reading {}", full.display()))
        }
    }
}
