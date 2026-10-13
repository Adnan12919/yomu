# Agent handoff

Read this after [AGENTS.md](../AGENTS.md). It summarizes the implemented product state and the decisions an agent should carry forward without re-discovering them. User-facing behavior and shortcuts are in [README.md](../README.md); code boundaries are in [architecture.md](architecture.md); GPUI Kit migration and redesign decisions are in [ui-redesign-plan.md](ui-redesign-plan.md).

## Project status

yomu is a Rust 2024 desktop comic reader built with GPUI Kit 0.7.3. The minimum supported Rust version is 1.92. It targets Linux, Wayland first with X11 fallback, and uses Nix/devenv for native dependencies. Main UI and application orchestration are in `src/main.rs`; reusable behavior stays in GPUI-independent modules under `src/`.

The framework migration, built-in theme integration, custom theme file, tabbed settings, command prompt, and first visual redesign pass are implemented. The most recent code change moved the library toolbar to the reference layout: centered search, no Yomu title or total book count, transparent top bar, and a top-right hamburger menu. Its actions are Add files, Add folder, Settings, series grouping, and Quit. Selection actions remain at the left of the toolbar while a selection is active.

The latest code checks run for that UI pass were `cargo fmt --all -- --check`, `cargo check --offline`, and `git diff --check`; all passed. The full test suite was not run, and the new UI has not been visually inspected on a real Wayland compositor. Do not claim visual verification. Recheck appearance on the user's compositor when possible.

## Implemented behavior

- **Import and library:** CBZ/ZIP archives and image folders are imported by reference. Covers and library metadata are stored under XDG data storage. Search matches title, source path, and series, and does not mutate library ordering or selection scope.
- **Search:** click the centered toolbar search, press `/`, or use `Ctrl+F`; `Escape` leaves search. Query state is transient.
- **Series:** optional persisted series names; assignment and clearing work for one or multiple books. Grouping is an opt-in persisted preference that filters first, uses natural ordering for headings, retains within-series book order, and places unassigned books last.
- **Reader:** single-page remains the default. Continuous vertical reading is persisted and can be toggled with `v` or `:continuous [on|off]`. Webcomics mode is separately persisted, forces continuous layout and a zero page gap, and restores the saved normal layout when disabled; use `w` or `:webcomics [on|off]`.
- **Settings:** `Ctrl+,`, `:settings`, or the hamburger menu opens Appearance, Library, Reader, and Keyboard & Commands tabs. Every persisted reader/library preference has a control. Command-backed controls use the same mutation methods as commands.
- **Themes:** Kit built-in themes and app-side custom themes apply live and the selected name persists in `library.json`. `$XDG_CONFIG_HOME/yomu/themes.toml` (usually `~/.config/yomu/themes.toml`) is created from `themes.example.toml` on first use. The parser validates theme names, modes, tokens, and hex colors; a bad reload reports an error and keeps the existing theme. The exact format and supported keys are documented in the example and README.
- **Command prompt:** in the reader `:` opens a centered themed floating panel. UI suggestions filter command names and supported arguments; Up/Down selects, Tab/Shift+Tab completes, Enter runs the existing `vim.rs` parser, and Escape cancels. The suggestion catalog lives in `main.rs`, separate from the parser, so keep it synchronized when commands or arguments change.
- **Visual design:** use GPUI Kit semantic theme tokens, not a hard-coded Yomu palette. GPUI Kit `Button` components are used for app actions. The library header and book cards use active theme surfaces. Custom context menus and dialogs intentionally remain custom because they carry Yomu's keyboard navigation, dismissal, selection, and destructive-action behavior; they are still styled from active Kit tokens.

## Persistent state and paths

- `$XDG_DATA_HOME/yomu/library.json` (normally `~/.local/share/yomu/library.json`) stores books, progress, marks, and app preferences. `YOMU_DATA_DIR` overrides the data root.
- The library schema is version 2. `Settings::from_value` accepts older/missing fields leniently and clamps opacity. Add serde-compatible defaults and a deliberate migration if changing stored meaning.
- `themes.toml` belongs in the XDG **config** directory, separate from the library data directory. It is user-editable; never overwrite an existing file when loading the app.
- Comic source archives/folders are not copied, renamed, or deleted by yomu. Removal deletes yomu metadata/covers only.
- `problems.md` is user-owned. Do not edit it as part of feature work. Check source and README before treating its entries as open issues.

## Code map and invariants

- `src/main.rs`: GPUI Kit window, `Yomu` state, reader and library rendering, overlay state, event routing, persistence calls, background task coordination. This file is large; move pure/reusable logic to existing core modules where practical.
- `src/library.rs`: `Book`, `Settings`, XDG paths, import orchestration and JSON persistence.
- `src/archive.rs` / `src/imaging.rs`: archive/folder listing and background decode/resize primitives. Do not decode or resize images on the UI thread.
- `src/layout.rs` / `src/motion.rs` / `src/pagecache.rs`: pure geometry, animation, and cache policy.
- `src/vim.rs`: reader tokens, `:` command-line input and `Action` parsing. It must remain independent of GPUI.
- `src/search.rs` / `src/selection.rs` / `src/menu.rs`: GPUI-independent query, selection and context-menu policies.
- Page indices are zero-based and always in archive reading order. RTL affects input/visual travel, never archive order or cache order.
- Keep continuous scrolling compatible with jumps, marks, zoom, saved progress, and bounded visible-page caching. Webcomics is an override on top of the saved continuous preference.
- Keep async results tied to the book/page/request that created them; stale tasks must not replace the current reader content.

## UI reference decisions

The root `gpui-kit_inspiration.jpg` is the visual reference supplied by the user. For the library, preserve these explicit requests:

1. Search stays horizontally centered at the top.
2. Do not show the Yomu name or total book count in the toolbar.
3. Keep the top toolbar transparent, with no full-width colored strip or bottom rule.
4. Put a small hamburger at the upper right. Its menu contains library actions such as importing files/folders, settings, series grouping, and quitting.
5. Keep the book grid as the main content; do not replace working library behavior to imitate a static screenshot.

Settings uses active Kit themes rather than a Yomu palette. Tabs are open to future real preferences, but do not add controls for features that do not exist. Menus/dialogs may stay custom if Kit components cannot preserve current interaction semantics.

## Remaining work and roadmap

- **Next redesign work:** verify the current appearance on a real Wayland session against the reference and refine actual spacing/sizing based on the rendered result. The first visual pass is compile-checked, not visually approved.
- **Sorting:** not implemented. Do not add a Settings sorting option until sorting behavior exists.
- **Inertial scrolling:** explicitly requested as a very-low-priority roadmap item; it is not implemented. If revisiting it, first check GPUI's current scroll input/animation support and preserve reduced-motion preferences.
- Other product gaps currently listed in README include CBR/CB7, double-page spreads, and pinch zoom.

## Build and verification

On the configured NixOS system, use `devenv shell` (or `nix develop`) before launching. Useful commands:

```sh
cargo fmt --all -- --check
cargo check
cargo test
```

The current toolchain minimum is Rust 1.92. UI tests use GPUI's simulated platform; they do not validate actual compositor appearance, window transparency, portals, or Wayland behavior. Report precisely which commands ran and whether a real compositor was used. `cargo test perf_ -- --ignored --nocapture` runs opt-in measurements only.
