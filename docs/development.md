# Development notes

## Environment

Use `devenv shell` on the primary NixOS setup, or `nix develop`. Both provide a current stable Rust toolchain (edition 2024; minimum Rust 1.92), pkg-config, CMake, and GPUI Kit's Linux runtime libraries. The app uses GPUI Kit (GPUI) and Vulkan/Wayland; on NixOS, enable graphics drivers and an XDG desktop portal for file dialogs. See the README for full host setup and packaging instructions.

The code is Linux-targeted. `nix/package.nix` wraps runtime library lookup and installs `packaging/yomu.desktop`. Keep runtime dependencies synchronized between `devenv.nix`, the flake dev shell, and the package when changing native requirements.

## Common commands

```sh
devenv shell
cargo run --release
cargo fmt --check
cargo check
cargo test
cargo test library::tests:: -- --nocapture
cargo test perf_ -- --ignored --nocapture
```

`cargo test` includes gpui UI tests. A display/compositor is not required for the simulated gpui tests, but real rendering, alpha behavior, portals, and compositor protocols need an actual desktop session. Performance tests are ignored by default and should be run explicitly.

## Change recipes

### Add a user preference

Add a field and default in `library::Settings`, parse it leniently in `Settings::from_value`, expose it through the settings UI in `main.rs`, and save through the existing library persistence path. If the field changes the interpretation of old stored data, bump `LIBRARY_VERSION` and add migration coverage. Update README's Settings section and architecture notes.

### Add a reader command or key

Map input in `vim.rs` to an `Action`; keep command parsing independent of the window. Handle the action in `main.rs`, update `README.md`'s key reference, and add parser and behavior coverage. Preserve counts/prefix handling and avoid changing the public key behavior unintentionally.

### Change library search

Matching rules live in `search.rs`; the query and active typing state live in `Yomu` and are not persisted. `/` and `Ctrl+F` activate search, which includes title, source path, and series. Keep filtered rendering separate from `Library` mutations so selection, select-all, and batch removal continue to use the complete library order. Update the README's search behavior when changing matching or keyboard rules.

### Change series grouping

Series names are optional `Book` metadata in `library.rs`; keep older `library.json` files loading as unassigned. Assign or clear names through `Library::set_series`, and persist the `Settings::group_series` display preference using the existing settings path. Assignment from book and selection context menus is edited in the Series overlay in `main.rs`; grouped rendering filters first and leaves book order stable within each naturally ordered series section. Update README and architecture notes when changing this behavior.

### Change image loading or memory use

Start with `archive.rs`, `imaging.rs`, `layout.rs`, and `pagecache.rs`; keep disk reads and decoding off the UI thread. Update cache policy tests and relevant ignored perf measurements. Remember that decoded-page bytes alone do not equal total RSS or GPU memory.

### Change library persistence/import

Keep `Book` and `Library` serialization backward compatible. Consider missing/renamed source paths, corrupt JSON, duplicate imports, old versions, partial import failures, and cover generation failures. Add tests using temporary data directories rather than a user's live library.

### Change native build/runtime needs

Update all relevant dependency lists: `devenv.nix`, `flake.nix`, and `nix/package.nix`. Confirm whether the dependency is build-time, link-time, or runtime-only, and update README setup instructions if users must change their host configuration.

## State of the project

`problems.md` is a user-owned issue notebook; do not edit it as part of unrelated implementation work. It can contain already implemented items, so check the code and README before treating an entry as open. Continuous vertical scrolling is implemented and selectable in Settings or with `v` / `:continuous [on|off]`. Webcomics mode is implemented as a persisted gapless override, toggleable with `w` / `:webcomics [on|off]`. Library search is implemented in a centered transparent toolbar and matches title, source path, or series. The upper-right hamburger provides import, Settings, series grouping, and quit actions; the toolbar omits the app name and total count. Series grouping is an opt-in persisted library view with assignment from single-book and bulk context menus. Current documented product gaps are CBR/CB7, double-page spreads, pinch zoom, sorting options, and very-low-priority inertial scrolling (see README's “Not done yet” and [agent handoff](agent-handoff.md)).

Reader and library preferences are serialized in `library.json`; custom themes are configured separately in `$XDG_CONFIG_HOME/yomu/themes.toml`. Do not move existing preferences into the theme file or create a competing source of truth without a migration and ownership plan. The settings tab state itself is transient.
