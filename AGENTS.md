# Agent guide

This file is the starting point for coding agents working on yomu. Read [README.md](README.md) for the complete user-facing behavior and key reference, [docs/architecture.md](docs/architecture.md) for code boundaries and important invariants, and [docs/ui-redesign-plan.md](docs/ui-redesign-plan.md) before changing the GPUI UI or adopting GPUI Kit.

## Project at a glance

yomu is a Rust desktop comic reader built on gpui. It imports CBZ/ZIP archives and image folders, stores library metadata locally, and decodes/scales pages off the UI thread. It targets Linux (Wayland, with X11 fallback) and uses Nix/devenv for native build dependencies.

## Start here

- Core library and unit tests: `src/lib.rs` and modules under `src/`.
- Window, event handling, overlays, rendering, and gpui UI tests: `src/main.rs`.
- Build metadata and Rust dependencies: `Cargo.toml`, `Cargo.lock`.
- Development environments: `devenv.nix`, `devenv.yaml`, `flake.nix`.
- Nix package: `nix/package.nix`.
- `problems.md` is user-owned. Do not edit it as part of feature work; verify feature status against source and README.

## Build and run

On NixOS, enter the configured shell and run the app:

```sh
devenv shell
cargo run
```

`nix develop` is the alternative shell. The package is Linux-only and needs Wayland/Vulkan runtime libraries. Rust edition is 2024 and the minimum compiler is 1.89. See README for NixOS graphics and portal prerequisites.

Useful project commands:

```sh
cargo fmt --check
cargo check
cargo test
cargo test perf_ -- --ignored --nocapture
```

The last command runs opt-in performance measurements, not the normal suite. UI tests use gpui's simulated platform and do not validate appearance on a real compositor. Run focused tests while iterating, then the full suite for behavior changes when verification is requested or appropriate.

## Working conventions

- Keep reusable logic independent of gpui. Put parsing, persistence, archive handling, geometry, selection rules, animation math, and cache planning in their existing core modules.
- Keep filesystem, archive, image decode, and resize failures as `anyhow::Result` with useful context. Avoid silently replacing unreadable user data; library loading preserves a corrupt file as a backup.
- Treat `library.json` as user data. Add serde defaults for new fields, read settings leniently where possible, and add a migration when stored meaning or defaults change. Preserve old data and document format changes.
- Page indices are zero-based internally and ordered in reading order regardless of RTL. Direction maps input and visual travel; it must not reverse archive ordering or cache indices.
- Continuous vertical mode is an opt-in persisted reader setting. Keep it working with page-number jumps, marks, zoom, input scrolling, saved progress, and bounded visible-page caching; single-page mode remains the default.
- Library search is transient UI state. `/` and `Ctrl+F` activate it; it matches a case-insensitive substring of title, source path, or series and filters only the rendered cards. Bulk selection and actions still operate on the full library.
- Reader key `v` toggles continuous vertical mode; `:continuous [on|off]` and the Settings option remain available. Single-page mode stays the default.
- Webcomics mode is separately persisted; it forces continuous layout with zero gap between pages and restores the saved continuous/single-page preference when disabled. Reader key `w` and `:webcomics [on|off]` control it.
- Series is optional persisted book metadata (missing values are unassigned). Series grouping is an opt-in persisted library setting; preserve normal grid mode, search, and bulk assignment behavior when changing it.
- Do not do expensive page decode/resizing on the UI thread. Keep decoded pages bounded by the cache policy and free gpui image resources when no longer used.
- Preserve accessibility/input behavior and the README's documented shortcuts when editing UI interactions.
- Vendor patches under `vendor/` are deliberate compatibility fixes; inspect their comments and `Cargo.toml` rationale before changing dependency patches.
- Avoid unrelated formatting or broad UI rewrites when addressing a focused issue.

## Before handing work back

Summarize changed files, behavior, and any tests or commands actually run. If UI behavior depends on a real Wayland compositor, state whether it was visually checked. Do not claim commands passed unless they were run.
