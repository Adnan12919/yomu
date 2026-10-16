//! Theme configuration, parsing, custom themes, and UI color tokens.

use std::{
    collections::{HashMap, HashSet},
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    rc::Rc,
};

use anyhow::{Result, anyhow};
use gpui_kit::App;
use gpui_kit::component::ColorTokens;
use gpui_kit::component::{Theme as KitTheme, ThemeConfig, ThemeRegistry};
use serde::Deserialize;
use yomu::library::DataDirs;

#[derive(Clone, Copy)]
pub struct UiColors {
    pub background: gpui_kit::Hsla,
    pub surface: gpui_kit::Hsla,
    pub surface_hover: gpui_kit::Hsla,
    pub border: gpui_kit::Hsla,
    pub foreground: gpui_kit::Hsla,
    pub muted: gpui_kit::Hsla,
    pub accent: gpui_kit::Hsla,
    pub accent_foreground: gpui_kit::Hsla,
    pub danger: gpui_kit::Hsla,
    pub danger_hover: gpui_kit::Hsla,
    pub widget_opacity: f32,
}

impl UiColors {
    pub fn from_tokens(tokens: ColorTokens) -> Self {
        Self {
            background: tokens.background,
            surface: tokens.surface,
            surface_hover: tokens.secondary,
            border: tokens.border,
            foreground: tokens.foreground,
            muted: tokens.muted_foreground,
            accent: tokens.primary,
            accent_foreground: tokens.primary_foreground,
            danger: tokens.destructive,
            danger_hover: tokens.destructive,
            widget_opacity: 1.0,
        }
    }

    pub fn from_app(cx: &App) -> Self {
        cx.try_global::<KitTheme>()
            .map(|theme| {
                let mut colors = Self::from_tokens(theme.semantic_tokens().colors);
                colors.danger = theme.danger;
                colors.danger_hover = theme.danger_hover;
                colors
            })
            .unwrap_or_else(|| Self::from_tokens(ColorTokens::dark()))
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct CustomThemeFile {
    name: String,
    themes: Vec<CustomThemeDefinition>,
}

impl Default for CustomThemeFile {
    fn default() -> Self {
        Self {
            name: "yomu custom themes".to_string(),
            themes: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct CustomThemeDefinition {
    name: String,
    mode: String,
    colors: std::collections::BTreeMap<String, String>,
    font_size: Option<f32>,
    font_family: Option<String>,
    mono_font_size: Option<f32>,
    mono_font_family: Option<String>,
    radius: Option<usize>,
    radius_lg: Option<usize>,
    shadow: Option<bool>,
}

impl Default for CustomThemeDefinition {
    fn default() -> Self {
        Self {
            name: String::new(),
            mode: "dark".to_string(),
            colors: std::collections::BTreeMap::new(),
            font_size: None,
            font_family: None,
            mono_font_size: None,
            mono_font_family: None,
            radius: None,
            radius_lg: None,
            shadow: None,
        }
    }
}

pub const CUSTOM_THEME_COLOR_KEYS: &[&str] = &[
    "accent.background",
    "accent.foreground",
    "background",
    "border",
    "button.background",
    "button.foreground",
    "button.hover.background",
    "button.primary.background",
    "button.primary.foreground",
    "button.primary.hover.background",
    "danger.background",
    "danger.foreground",
    "foreground",
    "input.border",
    "link",
    "muted.background",
    "muted.foreground",
    "popover.background",
    "popover.foreground",
    "primary.background",
    "primary.foreground",
    "primary.hover.background",
    "ring",
    "scrollbar.background",
    "secondary.background",
    "secondary.foreground",
    "secondary.hover.background",
    "tab.active.background",
    "tab.active.foreground",
    "tab.background",
    "tab.foreground",
    "tab_bar.background",
    "tab_bar.segmented.background",
];

pub fn parse_custom_themes(
    contents: &str,
    built_in_names: &HashSet<String>,
) -> Result<HashMap<String, Rc<ThemeConfig>>> {
    let file: CustomThemeFile =
        toml::from_str(contents).map_err(|error| anyhow!("parsing themes.toml: {error}"))?;
    let mut names = HashSet::new();
    let mut themes = HashMap::new();

    for definition in file.themes {
        let name = definition.name.trim();
        if name.is_empty() {
            return Err(anyhow!("each custom theme needs a non-empty name"));
        }
        if built_in_names.contains(name) {
            return Err(anyhow!(
                "custom theme name “{name}” conflicts with a built-in theme"
            ));
        }
        if !names.insert(name.to_string()) {
            return Err(anyhow!("duplicate custom theme name “{name}”"));
        }
        if definition.mode != "dark" && definition.mode != "light" {
            return Err(anyhow!("theme “{name}” mode must be `dark` or `light`"));
        }
        if definition.colors.is_empty() {
            return Err(anyhow!("theme “{name}” needs at least one color override"));
        }
        for (key, value) in &definition.colors {
            if !CUSTOM_THEME_COLOR_KEYS.contains(&key.as_str()) {
                return Err(anyhow!("unsupported color token `{key}` in theme “{name}”"));
            }
            let digits = value.strip_prefix('#').unwrap_or("");
            if !matches!(digits.len(), 6 | 8)
                || !digits.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(anyhow!(
                    "color `{key}` in theme “{name}” must use #RRGGBB or #RRGGBBAA"
                ));
            }
        }

        let config_value = serde_json::json!({
            "name": name,
            "mode": definition.mode,
            "colors": definition.colors,
            "font.size": definition.font_size,
            "font.family": definition.font_family,
            "mono_font.size": definition.mono_font_size,
            "mono_font.family": definition.mono_font_family,
            "radius": definition.radius,
            "radius.lg": definition.radius_lg,
            "shadow": definition.shadow,
        });
        let config: ThemeConfig = serde_json::from_value(config_value)
            .map_err(|error| anyhow!("invalid theme “{name}”: {error}"))?;
        themes.insert(name.to_string(), Rc::new(config));
    }

    if themes.is_empty() {
        let set_name = file.name.trim();
        let set_name = if set_name.is_empty() {
            "custom theme set"
        } else {
            set_name
        };
        return Err(anyhow!(
            "{set_name} must define at least one [[themes]] entry"
        ));
    }
    Ok(themes)
}

pub fn custom_theme_path(_data_dirs: &DataDirs) -> Result<PathBuf> {
    #[cfg(test)]
    {
        Ok(_data_dirs
            .library_file()
            .parent()
            .unwrap_or(Path::new("."))
            .join("themes.toml"))
    }
    #[cfg(not(test))]
    {
        let config_dir = dirs::config_dir()
            .ok_or_else(|| anyhow!("could not determine the user's configuration directory"))?;
        Ok(config_dir.join("yomu").join("themes.toml"))
    }
}

#[allow(dead_code)]
pub fn custom_theme_fingerprint(path: &Path) -> Option<u64> {
    let contents = fs::read(path).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    contents.hash(&mut hasher);
    Some(hasher.finish())
}

pub fn ensure_custom_theme_file(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("custom theme path has no parent directory"))?;
    fs::create_dir_all(parent)?;
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => {
            use std::io::Write as _;
            file.write_all(include_bytes!("../../themes.example.toml"))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub fn built_in_theme_names(cx: &App) -> HashSet<String> {
    cx.try_global::<ThemeRegistry>()
        .map(|registry| registry.themes().keys().map(ToString::to_string).collect())
        .unwrap_or_default()
}

pub fn load_custom_themes(path: &Path, cx: &App) -> Result<HashMap<String, Rc<ThemeConfig>>> {
    let contents =
        fs::read_to_string(path).map_err(|error| anyhow!("reading {}: {error}", path.display()))?;
    let built_in_names = built_in_theme_names(cx);
    parse_custom_themes(&contents, &built_in_names)
}
