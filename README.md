# yomu

A minimal manga reader written entirely in Rust. It is built on [gpui](https://crates.io/crates/gpui), so
rendering is GPU-accelerated, and it talks to Wayland directly (no webview, no Tauri, no GTK/Qt).

- Library grid with cover art and a reading-progress bar per book
- Import CBZ/ZIP archives or folders of images (files, folders, or drag and drop)
- Chrome-free reader: one page, a 2 px progress line at the bottom edge, nothing else until you ask
- Fit page / width / height / original size, zoom in and out, pan by dragging
- Vim motions (counts, `gg`, `G`, marks, `:` commands) next to the normal keys
- Right-to-left (manga) or left-to-right reading order
- Progress is saved on every page turn and restored when you reopen a book
- Pages are decoded on background threads at the resolution the current zoom needs, with the next pages prefetched

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

The reader is modal in the vim sense: counts (`5j`), two-key sequences (`gg`, `zw`, `ma`) and a `:`
command line all work, and the usual keys (arrows, PageUp/PageDown, Space, Home/End, `+`/`-`,
Ctrl+wheel) work too. Press `?` in the reader for a cheat sheet.

**Library**

| Key | Action |
|-----|--------|
| Click a book | Open it where you left off |
| `Ctrl+O` / `Ctrl+Shift+O` | Add files / Add folder |
| Shift + right-click a book | Remove it from the library (the file is kept) |

**Moving**

| Key | Action |
|-----|--------|
| `j` `k` `h` `l`, arrows | Scroll. At the edge of the page they turn the page, so on a page that fits the window they simply turn pages |
| `Space` / `Shift+Space`, `Ctrl+F` / `Ctrl+B` | Scroll a screen down / up |
| `Ctrl+D` / `Ctrl+U` | Scroll half a screen |
| `J` `K`, `PageDown` `PageUp` | Next / previous page |
| `gg`, `G`, `12G`, `:12`, `Home`, `End` | First page, last page, page 12 |
| `ma`, `'a`, `''` | Set mark `a`, jump to it, jump back to where you were |
| Click left / right third of the window | Turn the page toward that side |
| Mouse wheel | Scroll; at the edge it turns the page |
| Drag | Pan a zoomed or width-fit page |

Counts work on motions: `5j` scrolls five steps, `3J` skips three pages. Marks are saved with the book.

**View**

| Key | Action |
|-----|--------|
| `a` or `zp` | Fit page (the whole page is visible) |
| `s` or `zw` | Fit width |
| `zh` | Fit height |
| `z1` | Original size (one image pixel per screen pixel) |
| `Tab` | Cycle the fit modes |
| `+` / `=`, `zi` | Zoom in |
| `-`, `zo` | Zoom out |
| `0`, `z0` | Reset zoom to the fit size |
| `Ctrl` + wheel | Zoom at the pointer |
| `zt` `zz` `zb` | Scroll to the top / middle / bottom of the page |
| `r` | Toggle right-to-left / left-to-right |
| `i` | Show page number, fit mode and zoom |
| `f`, `F11` | Fullscreen |
| `?` | Help |
| `Esc`, `q` | Back to the library (`Esc` first cancels a half-typed command) |
| `Ctrl+Q` | Quit |

**Command line** (`:` then Enter)

| Command | Action |
|---------|--------|
| `:12` | Go to page 12 |
| `:fit width`, `:fit page`, `:fit height`, `:fit original` | Set the fit mode |
| `:zoom 150` | Zoom to 150% of the fit size (`:zoom` resets) |
| `:rtl`, `:ltr` | Reading direction |
| `:q` | Back to the library |
| `:qa` | Quit |

In right-to-left mode `Left` and `h` go forward, like turning a printed manga. The fit mode and
reading direction are remembered between books.

## Where data lives

`$XDG_DATA_HOME/yomu/` (usually `~/.local/share/yomu/`), or `$YOMU_DATA_DIR` if set:

- `library.json` holds the books and reading progress
- `covers/` holds generated cover thumbnails

Importing never copies or modifies your comic files.

## Development

```sh
cargo test       # archives, import, covers, progress, layout math, vim parser
```

The code is split so that everything except the window lives in the library crate:

- `src/archive.rs` lists and reads pages from CBZ files and folders
- `src/imaging.rs` decodes and resizes pages, and builds cover thumbnails
- `src/layout.rs` is the fit-mode, zoom and pan math
- `src/vim.rs` turns keys into actions: counts, `gg`/`zw`/`ma` sequences and the `:` command line
- `src/library.rs` is the library model, JSON persistence, and the import pipeline
- `src/main.rs` is the gpui UI

## Not done yet

- CBR/CB7 (needs a pure-Rust RAR/7z reader; CBR files are rejected with a clear message)
- Double-page spreads, continuous scroll, pinch zoom
- Search, sorting options, series grouping
