# Agent handoff

Read this after [AGENTS.md](../AGENTS.md). It summarizes the implemented product state and the decisions an agent should carry forward without re-discovering them. User-facing behavior and shortcuts are in [README.md](../README.md); code boundaries are in [architecture.md](architecture.md); GPUI Kit migration and redesign decisions are in [ui-redesign-plan.md](ui-redesign-plan.md).

## Project status

yomu is a Rust 2024 desktop comic reader built with GPUI Kit 0.7.3. The minimum supported Rust version is 1.92. It targets Linux, Wayland first with X11 fallback, and uses Nix/devenv for native dependencies. Main UI and application orchestration are in `src/main.rs`; reusable behavior stays in GPUI-independent modules under `src/`.

The framework migration, built-in theme integration, custom theme file, tabbed settings, command prompt, and first visual redesign pass are implemented. The library toolbar follows the reference layout: centered search, no Yomu title or total book count, transparent top bar, and a top-right hamburger menu. Its actions are Add files, Add folder, Settings, grouping, and Quit. Selection actions remain at the left of the toolbar while a selection is active. The window root, surfaces, controls, cards, and popups use rounded corners. Search has no placeholder/ghost text and shows a blinking caret while active. Menus, settings, dialogs, and the command prompt have short fade/slide entrance animations that reduce to near-instant when reduced motion is detected.

Folder grouping, progressive folder imports, opacity-aware widget surfaces, click-outside/Enter search dismissal, and GPUI Kit menu icons are implemented. Raised widget opacity is 10 percentage points above the window setting. Book right-click handling stops event propagation after opening the native popup; otherwise the root right-click dismissal handler closes it on the same event. Register `gpui_kit::assets::AllAssets` on the application builder: custom-rendered `Icon` elements use SVG paths, and without this asset source toolbar/context-menu glyphs render blank. Icon controls have visible themed surfaces and context-menu glyphs have accent backplates. This path has not yet been visually verified on a real Wayland compositor. `cargo fmt --all -- --check`, `cargo check --offline`, and `git diff --check` passed before this correction; rerun after the latest edits. The full test suite was not run. Do not claim visual verification.

## Implemented behavior

- **Import and library:** CBZ/ZIP archives and image folders are imported by reference. Background import sends each completed book to the UI; it is added and persisted immediately in scan order. Covers and library metadata are stored under XDG data storage. Search matches title, source path, and series, and does not mutate library ordering or selection scope.
- **Search:** click the centered toolbar search, press `/`, or use `Ctrl+F`; `Escape` leaves search. Query state is transient.
- **Grouping:** directory grouping is on by default. Two or more books with the same parent directory form a folder card; the first imported member supplies its cover, and opening it displays the group's books in a second view. A manual `Book.series` assignment overrides directory membership; clearing the assignment restores automatic directory grouping. The persisted `group_series` preference switches between folder cards and individual volumes. Group folders and their contents retain library/import order.
- **Reader:** single-page remains the default. Continuous vertical reading is persisted and can be toggled with `v` or `:continuous [on|off]`. Webcomics mode is separately persisted, forces continuous layout and a zero page gap, and restores the saved normal layout when disabled; use `w` or `:webcomics [on|off]`.
- **Settings:** `Ctrl+,`, `:settings`, or the hamburger menu opens Appearance, Library, Reader, and Keyboard & Commands tabs. Every persisted reader/library preference has a control. Command-backed controls use the same mutation methods as commands.
- **Themes:** Kit built-in themes and app-side custom themes apply live and the selected name persists in `library.json`. `$XDG_CONFIG_HOME/yomu/themes.toml` (usually `~/.config/yomu/themes.toml`) is created from `themes.example.toml` on first use. The parser validates theme names, modes, tokens, and hex colors; a bad reload reports an error and keeps the existing theme. The exact format and supported keys are documented in the example and README.
- **Theme consistency / Noctalia:** custom UI reads semantic `UiColors`; the context-menu glyph uses the same `foreground` token as group and series names. `noctalia/yomu-theme.toml.template` maps Noctalia v5 palette tokens to an importable `Noctalia Dynamic` yomu theme. A background fingerprint watcher reloads the generated custom theme file automatically after it is replaced or edited; the Settings reload button remains available. Context menus use GPUI's anchored popup window (`xdg_popup`) on Wayland, with the old in-window menu as a platform fallback. The parent window dismisses the popup on a click elsewhere inside yomu; Wayland popup grabs handle outside-app dismissal. For Niri popup blur, use a matching `window-rule` with a `popups { background-effect { blur true } }` block.
- **Command prompt:** in the reader `:` opens a centered themed floating panel inside the main window. Its surface uses the same `widget_surface()` rule as Settings and other raised widgets: 10 percentage points more opacity than the window setting, capped at 100%. Do not add a special alpha cap for compositor blur. UI suggestions filter command names and supported arguments; Up/Down selects, Tab/Shift+Tab completes, Enter runs the existing `vim.rs` parser, and Escape cancels. The suggestion catalog lives in `main.rs`, separate from the parser, so keep it synchronized when commands or arguments change. Niri's main `background-effect` applies to the yomu window; `popups {}` applies only to the separate book context popup.
- **Visual design:** use GPUI Kit semantic theme tokens and bundled `IconName` assets, not a hard-coded Yomu palette. The app registers `gpui_kit::assets::AllAssets` with the application so icon SVG paths resolve. Header/back controls render `Icon` directly inside GPUI Kit `Button`s with explicit theme color and visible themed button surfaces; context-menu glyphs use the foreground color over accent backplates. Custom context menus and dialogs intentionally remain custom because they carry Yomu's keyboard navigation, dismissal, selection, and destructive-action behavior; they are styled from active Kit tokens. The book context menu is a native anchored GPUI popup (`xdg_popup`) on Wayland, with an in-window fallback where native popups are unavailable. Stop propagation on the book's opening right-click so the root popup-dismiss handler does not close it immediately. Clicks elsewhere inside yomu explicitly close the popup; the Wayland grab dismisses it outside the app. Niri `popups {}` rules can target the native menu.

## Persistent state and paths

- `$XDG_DATA_HOME/yomu/library.json` (normally `~/.local/share/yomu/library.json`) stores books, progress, marks, and app preferences. `YOMU_DATA_DIR` overrides the data root.
- The library schema is version 3. `Settings::from_value` accepts older/missing fields leniently and clamps opacity. Version 3 defaults folder grouping on while honoring explicit saved values. Add serde-compatible defaults and a deliberate migration if changing stored meaning.
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

## Recent work & agent handoff log

### 1. Codex recovery and Turn 40 rollback
- **Context:** An external coding assistant (Codex) executed 40 turns across prior sessions. Turn 40 attempted to convert the command prompt into a native Wayland popup (`NativeCommandPrompt`), but was interrupted by rate limits mid-execution, leaving orphaned code, syntax errors, and inaccurate documentation.
- **Action taken:**
  - Audited full conversation history (stored in `~/.codex/thread_history_1.sqlite` and exported to the conversation artifacts).
  - Completely excised Turn 40's unfinished native command prompt code, restoring the clean, working in-window floating command prompt with `widget_surface()` (+10% opacity over window alpha).
  - Preserved and verified all valid features from Turns 26–39:
    - Libadwaita-style rounded window and widget geometry.
    - Active search bar with blinking caret and outside-click dismissal.
    - Progressive folder imports yielding immediate UI feedback per book.
    - Automatic directory-based folder grouping (with persisted override via `Book.series`).
    - Noctalia v5 theme template (`noctalia/yomu-theme.toml.template`) and background SHA-256 fingerprinting theme reloader.
    - Bundled SVG asset registration via `gpui_kit::assets::AllAssets`.
    - Wayland native `xdg_popup` book context menu.

### 2. Context menu outside-click dismissal bug & fix
- **Issue:** Right-clicking a book opened the native `xdg_popup` context menu, but clicking anywhere outside it in the main window did not dismiss the menu.
- **Root cause:** In GPUI on Linux Wayland, compositor grabs for popups automatically dismiss the popup when clicking *outside the application window*. However, clicks *inside* the parent application window were not closing the popup because Turn 39 added `Overlay::Menu(_) if self.native_context_popup => return None;` in `render_overlay`, which completely removed the modal scrim in the parent window.
- **Fix:**
  - In `src/main.rs:render_overlay`, changed the condition to keep rendering `deferred(self.scrim(false, cx))` even when `self.native_context_popup` is active. This invisible scrim captures any mouse click across the parent window and safely triggers `close_native_context_popup(cx)`.
  - Added parent window mouse down handlers to intercept and close open native popups.
  - Added an <kbd>Esc</kbd> key handler inside `NativeBookMenu::render`.
  - In `open_context_menu`, ensured any existing popup window is closed before opening a new one.

### 3. Critical GPUI Kit traps & patterns
- **`Button` hover style panic:** In GPUI Kit, calling `.hover(...)` on a `Button` that already has a style variant applied (e.g. `.ghost()`, `.secondary()`, `.primary()`, `.danger()`) panics with `assertion failed: self.hover_style.is_none(): hover style already set`. Never chain `.hover(...)` onto GPUI Kit `Button`s.
- **Asset provider requirement:** Custom `Icon` elements use SVG glyphs from GPUI Kit. If `gpui_kit::assets::AllAssets` is not registered on the application builder (`App::new().with_assets(gpui_kit::assets::AllAssets)`), all icon glyphs in toolbars and context menus will render blank.
- **Default folder grouping in tests:** Library schema version 3 defaults `group_series` to true. When test fixtures place multiple archives in the same temporary directory without distinct parent paths or series tags, they are grouped into a single folder card. Legacy UI tests expecting flat book cards must account for folder grouping or explicitly set `group_series: false`.
- **Raised surface opacity rule:** All elevated surfaces (dialogs, menus, settings, floating command prompt) use `widget_surface(window_opacity, colors)` to add +10 percentage points of opacity (capped at 1.0) above the main window background.

## Build and verification

On the configured NixOS system, use `devenv shell` (or `nix develop`) before launching. Useful commands:

```sh
cargo fmt --all -- --check
cargo check
cargo test --lib
```

The current toolchain minimum is Rust 1.92. UI tests use GPUI's simulated platform; they do not validate actual compositor appearance, window transparency, portals, or Wayland behavior. Report precisely which commands ran and whether a real compositor was used. `cargo test perf_ -- --ignored --nocapture` runs opt-in measurements only.

