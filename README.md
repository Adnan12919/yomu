# yomu

A minimal manga reader written entirely in Rust. It is built on [gpui](https://crates.io/crates/gpui), so
rendering is GPU-accelerated, and it talks to Wayland directly (no webview, no Tauri, no GTK/Qt).

- Library grid with cover art and a reading-progress bar per book
- Import CBZ/ZIP archives or folders of images (files, folders, or drag and drop)
- Chrome-free reader: one page, fit to the window, a 2 px progress line at the bottom edge
- Right-to-left (manga) or left-to-right reading order
- Progress is saved on every page turn and restored when you reopen a book
- Pages are decoded on background threads, sized to your window, with the next pages prefetched

## Build and run (NixOS)

```sh
git init && git add .      # flakes only see files that git tracks
nix develop                # Rust toolchain + every native library gpui needs
cargo run --release
```

Or without entering the shell: `nix run .` (builds the packaged, wrapped binary).

With direnv: `direnv allow` (the `.envrc` is already there).

NixOS prerequisites, if you do not have them already:

```nix
hardware.graphics.enable = true;          # Vulkan drivers (/run/opengl-driver)
xdg.portal.enable = true;                 # the Add files / Add folder dialogs use the portal
xdg.portal.extraPortals = [ pkgs.xdg-desktop-portal-gtk ];   # or the GNOME portal
```

## Keys

| Where   | Key                                  | Action                                    |
|---------|--------------------------------------|-------------------------------------------|
| Library | Click a book                         | Open it where you left off                |
| Library | `Ctrl+O` / `Ctrl+Shift+O`            | Add files / Add folder                    |
| Library | Shift + right-click a book           | Remove it from the library (file is kept) |
| Reader  | `Left` / `Right`                     | Turn the page toward that side            |
| Reader  | `Space`, `PageDown`, `Down`          | Next page                                 |
| Reader  | `Shift+Space`, `PageUp`, `Up`        | Previous page                             |
| Reader  | Click left / right third of window   | Same as `Left` / `Right`                  |
| Reader  | Mouse wheel                          | Next / previous page                      |
| Reader  | `Home` / `End`                       | First / last page                         |
| Reader  | `r`                                  | Toggle right-to-left / left-to-right      |
| Reader  | `i`                                  | Toggle the page-number overlay            |
| Reader  | `f` or `F11`                         | Fullscreen                                |
| Reader  | `Esc` or `q`                         | Back to the library                       |
| Any     | `Ctrl+Q`                             | Quit                                      |

In right-to-left mode `Left` goes forward, like turning a printed manga.

## Where data lives

`$XDG_DATA_HOME/yomu/` (usually `~/.local/share/yomu/`), or `$YOMU_DATA_DIR` if set:

- `library.json` holds the books and reading progress
- `covers/` holds generated cover thumbnails

Importing never copies or modifies your comic files.

## Development

```sh
cargo test       # archive handling, import, covers, progress persistence
```

The code is split so that everything except the window lives in the library crate:

- `src/archive.rs` lists and reads pages from CBZ files and folders
- `src/imaging.rs` decodes and resizes pages, and builds cover thumbnails
- `src/library.rs` is the library model, JSON persistence, and the import pipeline
- `src/main.rs` is the gpui UI

## Not done yet

- Vim motions (the action layer in `main.rs` is where a modal key handler would plug in)
- CBR/CB7 (needs a pure-Rust RAR/7z reader; CBR files are rejected with a clear message)
- Double-page spreads, zoom and pan
- Search, sorting options, series grouping
