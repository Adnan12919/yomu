# yomu

A minimal manga reader written entirely in Rust. It is built on [gpui](https://crates.io/crates/gpui), so
rendering is GPU-accelerated, and it talks to Wayland directly (no webview, no Tauri, no GTK/Qt).

- Library grid with cover art and a reading-progress bar per book
- Import CBZ/ZIP archives or folders of images (files, folders, or drag and drop)
- Chrome-free reader: one page, a 2 px progress line at the bottom edge, nothing else until you ask
- Right-click menu on books (resume, mark read/unread, reset progress, open file location, properties, remove)
- Multiple selection (Ctrl/Shift-click, Ctrl+A) for removing many books at once
- Settings panel with a live background-opacity slider (`Ctrl+,`)
- Fit page / width / height / original size, zoom in and out, pan by dragging
- Vim motions (counts, `gg`, `G`, marks, `:` commands) next to the normal keys
- Right-to-left (manga) or left-to-right reading order
- Progress is saved on every page turn and restored when you reopen a book
- Pages are decoded on background threads at the resolution the current zoom needs, with the next pages prefetched

## Build and run (NixOS)

With [devenv](https://devenv.sh) (`devenv.nix` and `devenv.yaml` are in the repo):

```sh
devenv shell               # Rust toolchain + every native library gpui needs
cargo run --release
```

- `devenv test` runs the whole test suite; with direnv, `direnv allow` loads the shell automatically
  (the `.envrc` is already there).
- `nix develop` still works if you prefer it (same toolchain, same libraries), and `nix run .` builds and runs
  the packaged, wrapped binary. Flakes only see files that git tracks, so run `git init && git add .` first.

NixOS prerequisites, if you do not have them already:

```nix
hardware.graphics.enable = true;          # Vulkan drivers (/run/opengl-driver)
xdg.portal.enable = true;                 # the Add files / Add folder dialogs use the portal
xdg.portal.extraPortals = [ pkgs.xdg-desktop-portal-gtk ];   # or the GNOME portal
```

## Installing yomu on NixOS

The package definition is `nix/package.nix`; the flake exposes it as `packages.<system>.default` and as an overlay.
It is built with nixpkgs' own Rust (the project needs 1.89 or newer; nixos-unstable has 1.98 at the time of writing),
wraps the binary so it finds Wayland and Vulkan, and installs a desktop entry.

**With a flake-based system configuration (recommended).** Add yomu as an input of your system flake:

```nix
inputs.yomu = {
  url = "git+file:///home/you/Projects/yomu";   # or github:you/yomu once it is pushed
  inputs.nixpkgs.follows = "nixpkgs";
};
```

then, in one of your NixOS modules (`inputs` must be passed through `specialArgs`):

```nix
{ inputs, pkgs, ... }: {
  nixpkgs.overlays = [ inputs.yomu.overlays.default ];
  environment.systemPackages = [ pkgs.yomu ];      # or home.packages with Home Manager
}
```

Without the overlay, use the package directly:
`environment.systemPackages = [ inputs.yomu.packages.${pkgs.stdenv.hostPlatform.system}.default ];`

Commit (or at least `git add`) your changes in the yomu repository, then `nix flake update yomu` in the system flake
and rebuild. Try it first without installing: `nix run git+file:///home/you/Projects/yomu`.

**Without flakes**, point `callPackage` at the file (an absolute path, so this does not work in a pure flake
evaluation):

```nix
environment.systemPackages = [ (pkgs.callPackage /home/you/Projects/yomu/nix/package.nix { }) ];
```

## Keys

The reader is modal in the vim sense: counts (`5j`), two-key sequences (`gg`, `zw`, `ma`) and a `:`
command line all work, and the usual keys (arrows, PageUp/PageDown, Space, Home/End, `+`/`-`,
Ctrl+wheel) work too. Press `?` in the reader for a cheat sheet.

**Library**

| Key | Action |
|-----|--------|
| Click a book | Open it where you left off |
| Right-click a book | Context menu (see below) |
| `Ctrl`+click, `Shift`+click | Select books (see below) |
| `Ctrl+A`, `Delete`, `Esc` | Select all, remove the selection, clear the selection |
| `Ctrl+O` / `Ctrl+Shift+O` | Add files / Add folder |
| `Ctrl+,` or the **Settings** button | Settings (works in the reader too) |

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

## Selecting several books

- **Ctrl+click** toggles a book. **Shift+click** selects the range from the last book you clicked.
- Once anything is selected, plain clicks toggle books instead of opening them, so you can just keep clicking.
  Deselect the last one (or press `Esc` / **Cancel**) and clicking opens books again.
- While books are selected the header shows `N of M selected` with **Select all**, **Remove** and **Cancel**
  in place of the import and settings buttons. `Ctrl+A` selects everything and `Delete` removes the selection.
- Right-click a selected book for a small bulk menu (**Clear Selection**, **Remove N Books from Library**).
  Right-clicking a book that is not selected drops the selection and shows that book's normal menu.
- Removal only forgets the books; the comic files are never deleted. It asks first only if some of the selected
  books have progress or marks that would be lost, and it saves once for the whole batch.

## Context menu

Right-click a book. Only the actions that make sense for its state are shown:

| Book state | Menu |
|------------|------|
| Not started | Start Reading · Mark as Read · Open File Location · Properties · Remove from Library |
| In progress | Resume Reading · Start from Beginning · Mark as Read · Reset Progress… · Open File Location · Properties · Remove from Library |
| Finished | Start from Beginning · Mark as Unread · Open File Location · Properties · Remove from Library |

- Navigate with `Up`/`Down` (or `j`/`k`, `Tab`), `Home`/`End`, `Enter` to run, `Esc` to close. Clicking outside closes it
  without clicking the book underneath. The menu slides back inside the window near the edges.
- **Remove from Library** never touches the comic file; it only forgets the book, its progress and its marks.
  It asks first only if there is progress or marks to lose. **Reset Progress** always asks (marks are kept).
- If the file has gone missing, the actions that need it are greyed out; Mark as Read/Unread, Properties and
  Remove still work.
- **Open File Location** uses the desktop's file manager through `xdg-desktop-portal`, falling back to `xdg-open`
  on the containing folder. It needs the same portal setup as the file dialogs.
- Dates in **Properties** are UTC.

## Settings

`Ctrl+,` (or `:settings` in the reader, or the Settings button in the library header):

- **Window background opacity**, 30% to 100%, default 100%. Drag the slider or use `Left`/`Right` (5% steps),
  `Home`/`End`. It applies immediately and is saved when you let go. **Reset to Defaults** asks first and also
  restores reading direction and page fit; it is greyed out when everything is already at the defaults.

Stored preferences are read leniently: a missing or invalid value falls back to its default (out-of-range numbers
are clamped) instead of affecting your library.

### How opacity works, and its limits

yomu asks the compositor for a window with an alpha channel and lowers the alpha of its **background** only.
Pages, covers, text and menus stay fully opaque, so letterbox margins around a page and the empty library
background become see-through. At exactly 100% the window switches back to a fully opaque surface, which lets
the compositor skip blending.

- Needs a compositing window system. Every Wayland compositor qualifies; on X11 you need a compositor running.
- There is no blur: gpui's blur support depends on compositor extensions, and a plain opacity slider does not need it.
- I could not look at a real compositor while building this, so the actual translucent look (including the colour
  of semi-transparent areas) is the one thing not verified by the tests.

## Where data lives

`$XDG_DATA_HOME/yomu/` (usually `~/.local/share/yomu/`), or `$YOMU_DATA_DIR` if set:

- `library.json` holds the books and reading progress
- `covers/` holds generated cover thumbnails

Importing never copies or modifies your comic files.

## Development

```sh
cargo test       # core logic plus UI tests that drive the real views with simulated mouse and keyboard input
```

The code is split so that everything except the window lives in the library crate:

- `src/archive.rs` lists and reads pages from CBZ files and folders
- `src/imaging.rs` decodes and resizes pages, and builds cover thumbnails
- `src/layout.rs` is the fit-mode, zoom and pan math
- `src/vim.rs` turns keys into actions: counts, `gg`/`zw`/`ma` sequences and the `:` command line
- `src/menu.rs` decides which context-menu actions apply to a book, plus confirmations and the Properties rows
- `src/selection.rs` holds the multi-select rules (toggle, range, select all)
- `nix/package.nix` is the Nix package; `devenv.nix` / `devenv.yaml` and `flake.nix` are the dev environments
- `src/library.rs` is the library model, JSON persistence, and the import pipeline
- `src/main.rs` is the gpui UI

## Not done yet

- CBR/CB7 (needs a pure-Rust RAR/7z reader; CBR files are rejected with a clear message)
- Double-page spreads, continuous scroll, pinch zoom
- Search, sorting options, series grouping
