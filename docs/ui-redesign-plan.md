# Themed UI and Command Prompt Plan

This document records the product direction, implementation plan, and GPUI Kit research for the UI redesign request. Read it before changing the app UI or replacing GPUI dependencies. It is intended to keep later implementation work grounded in this repository and avoid repeating the initial research.

## User direction

- Use the `gpui-kit_inspiration.jpg` image in the project root as a reference for component style: dark surfaces, fine borders, compact menus, tabbed settings, clear switches, and focused confirmation dialogs. It is a style reference, not a final settings layout or a full app mockup.
- Do not define a separate yomu color palette. Let the user select GPUI Kit themes, and read UI colors from the active Kit theme throughout the application.
- Offer GPUI Kit's built-in themes and a user-editable `themes.toml` file with hex color values and other supported theme options.
- Design the settings tabs around the real app preferences. Include settings for persistent modes and preferences that currently can only be changed with commands or key bindings. The tabs are open to expansion; the screenshot's Appearance/Reader tabs are not a fixed requirement.
- Replace the black bottom command strip with a floating command box with contextual completions and descriptions. The interaction should feel like a lightweight Noice/command-prompt panel: type after `:`, inspect suggestions, complete or navigate them, run with Enter, and dismiss with Escape.

## Current project facts

- The app currently depends directly on `gpui = "0.2.2"` and declares Rust 1.89 in `Cargo.toml`.
- The latest GPUI Kit docs checked for this plan are version 0.7.3. Its installation guide describes Kit as the entry point that brings in a matching GPUI snapshot (`gpui-pre = 0.3.9` in the checked guide), and notes the current locked Linux dependency graph requires Rust 1.92. Do not add Kit beside the existing GPUI dependency and assume the types interoperate. First confirm a coherent Kit/runtime version, then migrate the GPUI entry point and update the project's minimum Rust and Nix/devenv toolchains if needed.
- GPUI Kit documents more than 20 built-in themes and a `ThemeRegistry` for looking up and applying them. Theme entries include a name and light/dark mode, along with colors and optional font, radius, and shadow settings.
- The documented `ThemeRegistry` directory watcher reads JSON theme files. Kit's theme schema and examples describe JSON theme sets. Kit does not natively promise a `themes.toml` loader.
- The app can meet the TOML requirement with an application-side adapter: read and parse `themes.toml`, validate the supported Yomu format, convert its theme set to the JSON representation expected by the pinned Kit version, then register/apply it using that version's public API. Keep the TOML contract documented and include a sample. Do not claim Kit itself parses TOML.
- Verify the exact `ThemeRegistry`, `Theme`, initialization, and theme-update APIs against the GPUI Kit version selected in the compatibility spike. Kit's APIs and version pins can change.
- Existing UI and behavior are concentrated in `src/main.rs`; parsing and command actions are in `src/vim.rs`; reusable menu policy is in `src/menu.rs`; persisted preferences are in `src/library.rs`'s `Settings`.
- The current persisted settings include reading direction (`rtl`), fit mode (`fit`), animations (`animate`), normal continuous scrolling (`continuous`), gapless webcomics override (`webcomics`), series grouping (`group_series`), and window opacity. Theme selection is not yet persisted.
- `render_settings` is currently a small custom panel; `render_menu` and `render_confirm` are custom overlays. The reader currently renders `vim_status` as a black strip at the bottom. The `Vim` command line stores text and parses commands, but it does not provide command suggestions/completion.
- The project is Linux/Wayland-targeted today. Preserve its file dialog, image resource, reduced-motion, keyboard, and compositor behavior during a framework migration.

## Research references

These are the official references checked while writing this brief. Recheck APIs only when the selected Kit version differs or its pinned documentation says an interface changed.

- [GPUI Kit installation and version alignment](https://gpui-kit.com/docs/installation/) — current documented Kit version, matching GPUI snapshot, Rust toolchain notes, and dependency usage.
- [GPUI Kit theme guide](https://gpui-kit.com/docs/theme/) — active theme access, built-in theme registry, and theme directory registration example.
- [GPUI Kit theme schema](https://github.com/longbridge/gpui-kit/blob/main/.theme-schema.json) — supported theme metadata, light/dark modes, color tokens, font fields, radii, and shadows.
- [Theme registry implementation reference](https://docs.rs/gpui-component/latest/src/gpui_component/theme/registry.rs.html) — registry behavior and its JSON theme loading path. Confirm the same behavior in the selected Kit release.
- [GPUI Kit tabs](https://gpui-kit.com/docs/components/tabs) and [switches](https://gpui-kit.com/component/switch/) — useful for settings navigation and immediate boolean settings.
- [GPUI Kit menus](https://gpui-kit.com/component/menu/) and [dialogs](https://gpui-kit.com/component/dialog/) — useful for book context actions and confirmation overlays.
- [GPUI Kit design guidance](https://gpui-kit.com/docs/design-guides/) — component composition, native interaction, and accessibility guidance.

## Proposed settings structure

Keep each tab populated with real app preferences and reference material. Do not add fake or nonfunctional controls to fill space.

| Tab | Initial contents |
|---|---|
| **Appearance** | Searchable theme picker for built-in and custom themes; light/dark theme entries; window background opacity; custom theme file location and reload/error status. Theme colors come from Kit's active theme. |
| **Library** | Series grouping preference; future library sorting preferences only after sorting is implemented. Preserve the regular ungrouped grid mode. |
| **Reader** | Reading direction; fit mode; animation preference; reading layout selection for single page, normal continuous scrolling, and webcomics. Keep the existing semantics where webcomics forces continuous layout and turning it off restores the saved normal mode. |
| **Keyboard & Commands** | Searchable shortcut/command reference, including available `:` commands and their arguments. This tab can also explain completion keys. Do not imply key remapping exists unless it is separately implemented. |

Use the settings screen to expose persistent choices currently reachable only by reader commands, including direction (`:rtl` / `:ltr`), fit (`:fit`), animation (`:animate`), continuous mode (`:continuous`), and webcomics mode (`:webcomics`). Commands and settings must call the same state mutation paths so they cannot drift. Transient actions such as showing the info HUD do not need to become persisted preferences just because they have a command.

### Theme selection and custom TOML

- Persist the chosen theme name in the existing settings file unless migration work establishes a better app-wide configuration owner. Older libraries must default to a safe Kit theme.
- Keep the user's custom theme file under the XDG config directory, at `$XDG_CONFIG_HOME/yomu/themes.toml` (normally `~/.config/yomu/themes.toml`). Do not put editable configuration in the app's install directory or the user data directory.
- Define a Yomu TOML theme-set format with a top-level set name and one or more `[[themes]]`, each with a name, `mode = "light" | "dark"`, and a `[themes.colors]` table. Keys should map to the pinned Kit theme schema; hex colors should accept documented `#RRGGBB` and, if the pinned API supports alpha, `#RRGGBBAA` values. Optional Kit font, radius, and shadow fields may be exposed after confirming their exact representation.
- Include a commented sample and document token names. Prefer partial overrides that inherit Kit defaults if the pinned `ThemeConfig` supports them. Validate names, mode, token names, and hex values. On parse/validation failure, retain the active theme and show a useful message.
- Start with an explicit **Reload custom themes** action in Appearance settings. Add live file watching only if it works reliably with the selected Kit release and can report errors without repeatedly replacing a good active theme.
- Theme selection should apply immediately and update all windows/components. Use Kit's supported theme-update/change path so its component and base theme projections stay in sync; do not mutate theme globals directly without synchronizing them.

## Floating command box

Replace the black bottom status strip used for `Vim::status()` with a themed overlay anchored near the bottom center of the reader. Keep the page visible behind the box. A suitable first layout is a compact input row and a suggestion panel directly above it, with a subtle border/surface from the active Kit theme.

Implement command descriptions and completion metadata from one command catalog shared with parsing where practical. Suggestions should filter as input changes, show a short description and valid arguments, and account for current mode/state. Examples include `:fit` → `page`, `width`, `height`, `original`; `:continuous` → `on`/`off`; `:webcomics` → `on`/`off`; and `:zoom` → reset or a percentage. Unknown commands should remain editable and produce the existing useful error feedback on Enter.

Initial interactions:

- `:` opens the prompt; focus goes directly to text entry.
- Typing updates matching command/argument suggestions.
- Up/Down and Tab/Shift+Tab move the selected suggestion; Tab may insert the selected completion while preserving further argument entry.
- Enter runs the command or selected completion as appropriate.
- Escape closes the prompt and cancels its draft without running it.
- Backspace edits the draft; existing command-line key behavior and reader shortcuts outside the prompt remain intact.

Use the pinned Kit's themed input and accessible list/menu primitives where they fit. A small custom composition is acceptable if the command prompt's placement and behavior do not fit a general command palette component. The current parser remains the source of command semantics; the prompt adds input presentation and completion behavior.

## Implementation phases

1. **Compatibility spike:** build a small Kit window using the candidate version and project's Linux toolchain. Confirm Rust minimum, window creation, `gpui-kit` initialization, root view setup, keyboard dispatch, file dialogs, image display/drop lifecycle, UI test support, and the `ThemeRegistry`/`Theme` APIs. Check whether existing vendor patches still apply. Decide the pinned version before broad code edits.
2. **Framework migration:** replace the direct GPUI entry dependency with the aligned Kit dependency and adapt app/window initialization and UI tests. Update Cargo lock, minimum Rust, `devenv.nix`, `flake.nix`, and `nix/package.nix` as needed. Keep core modules independent of GPUI/Kit.
3. **Theme plumbing:** initialize Kit's theme/component layers; add safe persisted theme selection; remove app-owned UI color constants and route surfaces/text/accents through Kit theme values. Add TOML parsing, validation, JSON-to-registry adapter, sample config, and reload feedback.
4. **Settings:** build the tabbed settings shell and implement the preference inventory above. Wire each command-backed preference to the same mutation path and persistence behavior used by commands.
5. **Command prompt:** model editable draft, candidate list, selection, and argument completion; render the floating themed box; preserve `:` command behavior and Escape cancellation.
6. **Menus and app-wide presentation:** migrate context menus, dialogs, search controls, and buttons to Kit components, then apply active theme values to library cards, grouped sections, reader chrome, status HUD, and scrollbars. Keep yomu-specific behavior and selection/removal policies.
7. **Verification:** run formatting, `cargo check`, and the existing tests after migration; add focused interaction coverage for theme choice, TOML errors, settings/command parity, command completion, Enter/Escape, and preserved reader/library interactions. Visually inspect built-in light/dark themes and a custom theme on a real Wayland compositor.

## Invariants and acceptance criteria

- No hardcoded Yomu UI palette remains as the source of component colors; all UI surfaces and text use active Kit theme tokens. Intentional colors in comic artwork remain unchanged.
- Built-in theme choice applies live and survives restart. Existing preference fields and older `library.json` files continue loading safely.
- Custom `themes.toml` uses hex values, validates against the documented supported token set, can be reloaded from the settings UI, and a broken file does not blank/reset the current theme.
- Every implemented persistent reader/library preference has a settings control and uses the same implementation/persistence path as its command or key binding.
- The command box is not a full-width black bottom strip. It shows useful dynamic command/argument suggestions, supports completion/navigation, runs on Enter, and cancels on Escape.
- Existing open-book behavior, page order, continuous/webcomics behavior, progress, marks, caching, import, selection, and destructive confirmation semantics remain intact.
- UI behavior is checked both with the available simulated platform tests and visually on Wayland; report which was actually run.
