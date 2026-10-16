//! Reader geometry: fit modes, zoom and panning. Pure math with no gpui types, so it is unit tested.
//!
//! Terminology:
//! * *viewport*: the window area the page is shown in, in logical pixels.
//! * *display size*: how big the page is drawn, in logical pixels (may exceed the viewport).
//! * *offset*: how far the page has been scrolled from its top-left corner (always `>= 0`).
//! * *origin*: where the page's top-left corner lands relative to the viewport. When the page is
//!   smaller than the viewport on an axis it is centred; otherwise it is `-offset`.

use serde::{Deserialize, Serialize};

pub const ZOOM_MIN: f32 = 0.25;
pub const ZOOM_MAX: f32 = 8.0;
/// Multiplier for one zoom-in / zoom-out step.
pub const ZOOM_STEP: f32 = 1.25;

/// Decode sizes (longest side, in device pixels). Decoding at a few fixed tiers instead of the
/// exact size means zooming does not trigger a re-decode on every step.
pub const TIERS: [u32; 6] = [1024, 1536, 2048, 3072, 4096, 6144];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum FitMode {
    /// The whole page is visible (best fit).
    #[default]
    Page,
    /// The page is as wide as the window; taller pages scroll vertically.
    Width,
    /// The page is as tall as the window; wider pages scroll horizontally.
    Height,
    /// One image pixel per screen pixel.
    Original,
}

impl FitMode {
    pub fn label(self) -> &'static str {
        match self {
            FitMode::Page => "fit page",
            FitMode::Width => "fit width",
            FitMode::Height => "fit height",
            FitMode::Original => "original size",
        }
    }

    pub fn next(self) -> Self {
        match self {
            FitMode::Page => FitMode::Width,
            FitMode::Width => FitMode::Height,
            FitMode::Height => FitMode::Original,
            FitMode::Original => FitMode::Page,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub width: f32,
    pub height: f32,
    /// Device pixels per logical pixel (the window's scale factor).
    pub scale: f32,
}

impl Viewport {
    pub const EMPTY: Viewport = Viewport {
        width: 0.0,
        height: 0.0,
        scale: 1.0,
    };
}

/// Where to put the scroll position after a page turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrive {
    /// Where reading starts: the top, on the side the reading direction starts from.
    Start,
    /// Where reading ends: the bottom, on the side the reading direction ends at.
    End,
}

/// Scale (display pixels per image pixel) for a fit mode at zoom 1.0.
pub fn base_scale(fit: FitMode, orig: (f32, f32), vp: &Viewport) -> f32 {
    let (ow, oh) = (orig.0.max(1.0), orig.1.max(1.0));
    match fit {
        FitMode::Page => (vp.width / ow).min(vp.height / oh),
        FitMode::Width => vp.width / ow,
        FitMode::Height => vp.height / oh,
        FitMode::Original => 1.0 / vp.scale.max(0.01),
    }
}

pub fn display_size(fit: FitMode, zoom: f32, orig: (f32, f32), vp: &Viewport) -> (f32, f32) {
    let scale = base_scale(fit, orig, vp) * zoom;
    (orig.0 * scale, orig.1 * scale)
}

/// Furthest the page can be scrolled on each axis.
pub fn max_offset(display: (f32, f32), vp: &Viewport) -> (f32, f32) {
    (
        (display.0 - vp.width).max(0.0),
        (display.1 - vp.height).max(0.0),
    )
}

pub fn clamp_offset(off: (f32, f32), display: (f32, f32), vp: &Viewport) -> (f32, f32) {
    let max = max_offset(display, vp);
    (off.0.clamp(0.0, max.0), off.1.clamp(0.0, max.1))
}

/// Top-left corner of the page relative to the viewport.
pub fn origin(off: (f32, f32), display: (f32, f32), vp: &Viewport) -> (f32, f32) {
    fn axis(off: f32, display: f32, view: f32) -> f32 {
        if display <= view {
            (view - display) / 2.0
        } else {
            -off.clamp(0.0, display - view)
        }
    }
    (
        axis(off.0, display.0, vp.width),
        axis(off.1, display.1, vp.height),
    )
}

/// The offset that keeps the image point under `anchor` (viewport coordinates) fixed while the
/// display size changes from `old_display` to `new_display`. Not clamped.
pub fn zoom_anchor_offset(
    old_origin: (f32, f32),
    old_display: (f32, f32),
    new_display: (f32, f32),
    anchor: (f32, f32),
) -> (f32, f32) {
    fn axis(old_origin: f32, old_display: f32, new_display: f32, anchor: f32) -> f32 {
        let fraction = if old_display > 0.0 {
            (anchor - old_origin) / old_display
        } else {
            0.5
        };
        -(anchor - fraction * new_display)
    }
    (
        axis(old_origin.0, old_display.0, new_display.0, anchor.0),
        axis(old_origin.1, old_display.1, new_display.1, anchor.1),
    )
}

pub fn arrival_offset(arrive: Arrive, max: (f32, f32)) -> (f32, f32) {
    match arrive {
        Arrive::Start => (0.0, 0.0),
        Arrive::End => (max.0, max.1),
    }
}

pub fn clamp_zoom(zoom: f32) -> f32 {
    zoom.clamp(ZOOM_MIN, ZOOM_MAX)
}

/// Smallest decode tier that covers `long_side_px` device pixels.
pub fn tier_for(long_side_px: f32) -> u32 {
    TIERS
        .iter()
        .copied()
        .find(|tier| *tier as f32 >= long_side_px)
        .unwrap_or(TIERS[TIERS.len() - 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    const VP: Viewport = Viewport {
        width: 800.0,
        height: 600.0,
        scale: 1.0,
    };

    fn close(a: (f32, f32), b: (f32, f32)) -> bool {
        (a.0 - b.0).abs() < 0.01 && (a.1 - b.1).abs() < 0.01
    }

    #[test]
    fn fit_modes_pick_the_right_scale() {
        let orig = (1000.0, 2000.0);
        assert!(close(
            display_size(FitMode::Page, 1.0, orig, &VP),
            (300.0, 600.0)
        ));
        assert!(close(
            display_size(FitMode::Width, 1.0, orig, &VP),
            (800.0, 1600.0)
        ));
        assert!(close(
            display_size(FitMode::Height, 1.0, orig, &VP),
            (300.0, 600.0)
        ));
        let hidpi = Viewport { scale: 2.0, ..VP };
        assert!(close(
            display_size(FitMode::Original, 1.0, orig, &hidpi),
            (500.0, 1000.0)
        ));
    }

    #[test]
    fn wide_page_fits_differently() {
        let orig = (3000.0, 1000.0);
        assert!(close(
            display_size(FitMode::Page, 1.0, orig, &VP),
            (800.0, 266.666)
        ));
        assert!(close(
            display_size(FitMode::Height, 1.0, orig, &VP),
            (1800.0, 600.0)
        ));
    }

    #[test]
    fn zoom_scales_display() {
        let d = display_size(FitMode::Page, 2.0, (1000.0, 2000.0), &VP);
        assert!(close(d, (600.0, 1200.0)));
    }

    #[test]
    fn small_pages_are_centred_large_pages_scroll() {
        assert!(close(origin((0.0, 0.0), (300.0, 600.0), &VP), (250.0, 0.0)));
        assert!(close(
            origin((0.0, 100.0), (800.0, 1600.0), &VP),
            (0.0, -100.0)
        ));
        // an offset beyond the end is clamped
        assert!(close(
            origin((0.0, 5000.0), (800.0, 1600.0), &VP),
            (0.0, -1000.0)
        ));
        assert!(close(max_offset((800.0, 1600.0), &VP), (0.0, 1000.0)));
        assert!(close(
            clamp_offset((50.0, -9.0), (800.0, 1600.0), &VP),
            (0.0, 0.0)
        ));
    }

    #[test]
    fn zoom_keeps_the_anchor_point_fixed() {
        let old_display = (800.0, 1600.0);
        let off = (0.0, 400.0);
        let old_origin = origin(off, old_display, &VP);
        let anchor = (200.0, 150.0);
        let image_point = (
            (anchor.0 - old_origin.0) / old_display.0,
            (anchor.1 - old_origin.1) / old_display.1,
        );

        let new_display = (1600.0, 3200.0);
        let new_off = zoom_anchor_offset(old_origin, old_display, new_display, anchor);
        let new_origin = origin(new_off, new_display, &VP);
        let after = (
            (anchor.0 - new_origin.0) / new_display.0,
            (anchor.1 - new_origin.1) / new_display.1,
        );
        assert!(
            (image_point.1 - after.1).abs() < 1e-4,
            "{image_point:?} vs {after:?}"
        );
    }

    #[test]
    fn arrival_offsets_match_bounds() {
        let max = (500.0, 900.0);
        assert_eq!(arrival_offset(Arrive::Start, max), (0.0, 0.0));
        assert_eq!(arrival_offset(Arrive::End, max), (500.0, 900.0));
    }

    #[test]
    fn tiers_round_up_and_cap() {
        assert_eq!(tier_for(900.0), 1024);
        assert_eq!(tier_for(1025.0), 1536);
        assert_eq!(tier_for(2048.0), 2048);
        assert_eq!(tier_for(99999.0), 6144);
    }

    #[test]
    fn fit_cycle_visits_every_mode() {
        let mut mode = FitMode::Page;
        let mut seen = vec![mode];
        for _ in 0..3 {
            mode = mode.next();
            seen.push(mode);
        }
        assert_eq!(mode.next(), FitMode::Page);
        assert_eq!(seen.len(), 4);
    }
}
