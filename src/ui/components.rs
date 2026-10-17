//! Reusable UI elements, dialog buttons, and layout helpers.

use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonRounded, ButtonVariants};
use gpui_kit::component::{Disableable, Sizable};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, App, ClickEvent, Div, ObjectFit, RenderImage, Rgba, SharedString, Styled,
    StyledImage, Window, div, img, px,
};

use super::theme::UiColors;

pub fn with_alpha(color: gpui_kit::Hsla, alpha: f32) -> Rgba {
    let mut color: Rgba = color.into();
    color.a = alpha;
    color
}

pub fn overlay_shade(colors: UiColors) -> gpui_kit::Hsla {
    if colors.background.l > 0.5 {
        colors.foreground
    } else {
        colors.background
    }
}

/// One page image at an exact position and size (see `render_reader`).
pub fn page_layer(
    image: &Arc<RenderImage>,
    left: f32,
    top: f32,
    display: (f32, f32),
) -> AnyElement {
    img(image.clone())
        .absolute()
        .left(px(left))
        .top(px(top))
        .w(px(display.0))
        .h(px(display.1))
        .object_fit(ObjectFit::Fill)
        .into_any_element()
}

pub fn centered(text: impl Into<SharedString>, colors: UiColors) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(colors.muted)
        .child(text.into())
        .into_any_element()
}

pub fn pill(row: Div, text: impl Into<SharedString>, colors: UiColors) -> Div {
    row.flex().justify_center().child(
        div()
            .px_3()
            .py_1()
            .rounded_full()
            .bg(colors.surface)
            .border_1()
            .border_color(colors.border)
            .shadow_md()
            .text_sm()
            .text_color(colors.foreground)
            .child(text.into()),
    )
}

#[derive(Clone, Copy)]
pub enum ButtonKind {
    Normal,
    Primary,
    Danger,
}

/// A button for dialogs: same shape as the header buttons, on the dialog's raised surface.
pub fn panel_button(
    id: &'static str,
    label: impl Into<SharedString>,
    kind: ButtonKind,
    colors: UiColors,
    enabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let button = Button::new(id)
        .label(label)
        .rounded(ButtonRounded::Medium)
        .small()
        .disabled(!enabled)
        .on_click(on_click);
    let button = match kind {
        ButtonKind::Normal => button
            .secondary()
            .outline()
            .bg(colors.surface)
            .border_color(colors.border),
        ButtonKind::Primary => button.primary().bg(colors.accent),
        ButtonKind::Danger => button.danger().outline(),
    };
    #[cfg(test)]
    {
        div()
            .debug_selector(|| id.into())
            .child(button)
            .into_any_element()
    }
    #[cfg(not(test))]
    {
        button.into_any_element()
    }
}
