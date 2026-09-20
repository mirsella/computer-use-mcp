#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl PixelRect {
    pub fn is_valid_within(self, bounds: (u32, u32)) -> bool {
        self.width > 0
            && self.height > 0
            && self
                .x
                .checked_add(self.width)
                .is_some_and(|right| right <= bounds.0)
            && self
                .y
                .checked_add(self.height)
                .is_some_and(|bottom| bottom <= bounds.1)
    }
}

/// Derive the pre-transform source-pixel rect for an advisory target-window
/// crop from diagnostic KDE window geometry (a hint only, never AT-SPI
/// extents). The window rect is expressed in compositor logical pixels; the
/// monitor position and logical size come from the screencast stream, and the
/// frame size from the negotiated capture format. Any missing input, negative
/// or zero extent, scaling failure, or rect escaping the frame returns a
/// deterministic fallback reason so the caller keeps the full monitor.
pub fn window_crop_in_frame(
    window: (i32, i32, u32, u32),
    monitor_position: Option<(i32, i32)>,
    monitor_logical_size: Option<(i32, i32)>,
    frame_size: (u32, u32),
) -> Result<PixelRect, String> {
    let (window_x, window_y, window_width, window_height) = window;
    if window_width == 0 || window_height == 0 {
        return Err("KDE window geometry has an empty extent".into());
    }
    let Some((monitor_x, monitor_y)) = monitor_position else {
        return Err("screencast stream has no monitor position".into());
    };
    let Some((logical_width, logical_height)) = monitor_logical_size else {
        return Err("screencast stream has no monitor logical size".into());
    };
    if logical_width <= 0 || logical_height <= 0 || frame_size.0 == 0 || frame_size.1 == 0 {
        return Err("screencast stream has an empty monitor size".into());
    }
    let scale_x = f64::from(frame_size.0) / f64::from(logical_width);
    let scale_y = f64::from(frame_size.1) / f64::from(logical_height);
    if !scale_x.is_finite() || !scale_y.is_finite() || scale_x <= 0.0 || scale_y <= 0.0 {
        return Err("cannot map KDE logical geometry to frame pixels".into());
    }
    let relative_x = i64::from(window_x) - i64::from(monitor_x);
    let relative_y = i64::from(window_y) - i64::from(monitor_y);
    if relative_x < 0 || relative_y < 0 {
        return Err("KDE window geometry starts outside this monitor".into());
    }
    let edge = |logical: i64, scale: f64| -> Option<u32> {
        let pixel = (logical as f64) * scale;
        if !pixel.is_finite() || pixel < 0.0 {
            return None;
        }
        u32::try_from(pixel.round() as i64).ok()
    };
    let (Some(x), Some(y), Some(right), Some(bottom)) = (
        edge(relative_x, scale_x),
        edge(relative_y, scale_y),
        edge(
            relative_x
                .checked_add(i64::from(window_width))
                .ok_or_else(|| "KDE window geometry overflows logical coordinates".to_owned())?,
            scale_x,
        ),
        edge(
            relative_y
                .checked_add(i64::from(window_height))
                .ok_or_else(|| "KDE window geometry overflows logical coordinates".to_owned())?,
            scale_y,
        ),
    ) else {
        return Err("cannot map KDE logical geometry to frame pixels".into());
    };
    let (Some(width), Some(height)) = (right.checked_sub(x), bottom.checked_sub(y)) else {
        return Err("cannot map KDE logical geometry to frame pixels".into());
    };
    let rect = PixelRect {
        x,
        y,
        width,
        height,
    };
    if rect.is_valid_within(frame_size) {
        Ok(rect)
    } else {
        Err("KDE window geometry escapes the captured frame".into())
    }
}

/// Intersect two pixel rects. Returns None when either rect is empty or they
/// do not overlap. Deterministic: same inputs always yield the same rect.
pub fn intersect(first: PixelRect, second: PixelRect) -> Option<PixelRect> {
    let left = first.x.max(second.x);
    let top = first.y.max(second.y);
    let right = first
        .x
        .checked_add(first.width)?
        .min(second.x.checked_add(second.width)?);
    let bottom = first
        .y
        .checked_add(first.height)?
        .min(second.y.checked_add(second.height)?);
    let width = right.checked_sub(left)?;
    let height = bottom.checked_sub(top)?;
    if width == 0 || height == 0 {
        return None;
    }
    Some(PixelRect {
        x: left,
        y: top,
        width,
        height,
    })
}

/// Map an advisory capture frame-space dirty rect to encoded output PNG
/// pixels. Only unrotated output is supported (rotated frames keep the
/// frame-space rect, reported with its space); the visible intersection with
/// the encoded source crop is mapped, or None when nothing visible changed.
pub fn changed_rect_to_output(
    rect: PixelRect,
    source_crop: PixelRect,
    transform: Transform,
    output_size: (u32, u32),
) -> Option<PixelRect> {
    if transform != Transform::Normal {
        return None;
    }
    let visible = intersect(rect, source_crop)?;
    scale_rect_to_output(visible, source_crop, output_size).ok()
}
/// Map a pre-transform source-pixel rect (contained in `source_crop`) to the
/// encoded output PNG pixel space. The encoder downscales uniformly, so
/// fractions are preserved; the result is clamped to the output bounds and
/// rejected when empty. Deterministic: same inputs always yield same rect.
pub fn scale_rect_to_output(
    source_rect: PixelRect,
    source_crop: PixelRect,
    output_size: (u32, u32),
) -> Result<PixelRect, String> {
    if source_crop.width == 0 || source_crop.height == 0 || output_size.0 == 0 || output_size.1 == 0
    {
        return Err("cannot map an empty crop to output pixels".into());
    }
    let contained = source_rect.width > 0
        && source_rect.height > 0
        && source_rect.x >= source_crop.x
        && source_rect.y >= source_crop.y
        && source_rect
            .x
            .checked_add(source_rect.width)
            .is_some_and(|right| right <= source_crop.x.saturating_add(source_crop.width))
        && source_rect
            .y
            .checked_add(source_rect.height)
            .is_some_and(|bottom| bottom <= source_crop.y.saturating_add(source_crop.height));
    if !contained {
        return Err("window rect escapes the encoded source crop".into());
    }
    let scale_x = f64::from(output_size.0) / f64::from(source_crop.width);
    let scale_y = f64::from(output_size.1) / f64::from(source_crop.height);
    if !scale_x.is_finite() || !scale_y.is_finite() || scale_x <= 0.0 || scale_y <= 0.0 {
        return Err("cannot scale the window rect to output pixels".into());
    }
    let map = |value: f64, bound: u32| -> u32 { value.round().clamp(0.0, f64::from(bound)) as u32 };
    let left = map(
        f64::from(source_rect.x - source_crop.x) * scale_x,
        output_size.0,
    );
    let top = map(
        f64::from(source_rect.y - source_crop.y) * scale_y,
        output_size.1,
    );
    let right = map(
        f64::from(source_rect.x - source_crop.x + source_rect.width) * scale_x,
        output_size.0,
    );
    let bottom = map(
        f64::from(source_rect.y - source_crop.y + source_rect.height) * scale_y,
        output_size.1,
    );
    let rect = PixelRect {
        x: left,
        y: top,
        width: right.saturating_sub(left),
        height: bottom.saturating_sub(top),
    };
    if rect.is_valid_within(output_size) {
        Ok(rect)
    } else {
        Err("window rect collapses below one output pixel".into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transform {
    Normal,
    Rotate90,
    Rotate180,
    Rotate270,
    Flip,
    FlipRotate90,
    FlipRotate180,
    FlipRotate270,
}

impl Transform {
    /// Map a point in the encoded, transformed view back to the corresponding
    /// normalized point in the pre-transform source crop. The encoder applies
    /// the SPA transform before downscaling, so this remains exact for every
    /// uniform output scale.
    pub fn source_fraction(self, output_x: f64, output_y: f64) -> (f64, f64) {
        match self {
            Self::Normal => (output_x, output_y),
            Self::Rotate90 => (1.0 - output_y, output_x),
            Self::Rotate180 => (1.0 - output_x, 1.0 - output_y),
            Self::Rotate270 => (output_y, 1.0 - output_x),
            Self::Flip => (1.0 - output_x, output_y),
            Self::FlipRotate90 => (output_y, output_x),
            Self::FlipRotate180 => (output_x, 1.0 - output_y),
            Self::FlipRotate270 => (1.0 - output_y, 1.0 - output_x),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_rect_validity_rejects_empty_overflow_and_out_of_bounds_rects() {
        let valid = PixelRect {
            x: 2,
            y: 3,
            width: 4,
            height: 5,
        };
        assert!(valid.is_valid_within((6, 8)));
        assert!(!PixelRect { width: 0, ..valid }.is_valid_within((6, 8)));
        assert!(
            !PixelRect {
                x: u32::MAX,
                ..valid
            }
            .is_valid_within((u32::MAX, 8))
        );
        assert!(!PixelRect { width: 5, ..valid }.is_valid_within((6, 8)));
    }

    #[test]
    fn window_crop_maps_kde_logical_geometry_to_frame_pixels() {
        let rect = window_crop_in_frame(
            (100, 50, 400, 300),
            Some((0, 0)),
            Some((800, 600)),
            (1600, 1200),
        )
        .unwrap();
        assert_eq!(
            rect,
            PixelRect {
                x: 200,
                y: 100,
                width: 800,
                height: 600,
            }
        );
    }

    #[test]
    fn window_crop_accounts_for_monitor_position() {
        let rect = window_crop_in_frame(
            (1800, 100, 200, 100),
            Some((1600, 0)),
            Some((800, 600)),
            (1600, 1200),
        )
        .unwrap();
        assert_eq!(
            rect,
            PixelRect {
                x: 400,
                y: 200,
                width: 400,
                height: 200,
            }
        );
    }

    #[test]
    fn window_crop_rounds_logical_edges_before_deriving_fractional_extent() {
        let rect = window_crop_in_frame(
            (10, 10, 101, 77),
            Some((0, 0)),
            Some((1920, 1080)),
            (2560, 1440),
        )
        .unwrap();
        assert_eq!(
            rect,
            PixelRect {
                x: 13,
                y: 13,
                width: 135,
                height: 103,
            }
        );
    }

    #[test]
    fn window_crop_fallback_reasons_are_deterministic() {
        assert_eq!(
            window_crop_in_frame((0, 0, 0, 10), Some((0, 0)), Some((8, 6)), (8, 6)),
            Err("KDE window geometry has an empty extent".into())
        );
        assert_eq!(
            window_crop_in_frame((0, 0, 10, 10), None, Some((8, 6)), (8, 6)),
            Err("screencast stream has no monitor position".into())
        );
        assert_eq!(
            window_crop_in_frame((0, 0, 10, 10), Some((0, 0)), None, (8, 6)),
            Err("screencast stream has no monitor logical size".into())
        );
        assert_eq!(
            window_crop_in_frame(
                (700, 0, 200, 100),
                Some((0, 0)),
                Some((800, 600)),
                (1600, 1200)
            ),
            Err("KDE window geometry escapes the captured frame".into())
        );
        assert_eq!(
            window_crop_in_frame(
                (-10, 0, 200, 100),
                Some((0, 0)),
                Some((800, 600)),
                (1600, 1200)
            ),
            Err("KDE window geometry starts outside this monitor".into())
        );
    }

    #[test]
    fn scale_rect_to_output_preserves_fractions() {
        let output = scale_rect_to_output(
            PixelRect {
                x: 400,
                y: 300,
                width: 800,
                height: 600,
            },
            PixelRect {
                x: 0,
                y: 0,
                width: 1600,
                height: 1200,
            },
            (800, 600),
        )
        .unwrap();
        assert_eq!(
            output,
            PixelRect {
                x: 200,
                y: 150,
                width: 400,
                height: 300,
            }
        );
    }

    #[test]
    fn scale_rect_to_output_rejects_escaping_or_empty_rects() {
        let crop = PixelRect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        assert!(
            scale_rect_to_output(
                PixelRect {
                    x: 90,
                    y: 90,
                    width: 20,
                    height: 20,
                },
                crop,
                (50, 50),
            )
            .is_err()
        );
        assert!(
            scale_rect_to_output(
                PixelRect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                },
                crop,
                (0, 50),
            )
            .is_err()
        );
    }

    #[test]
    fn inverse_transform_fractions_match_encoder_orientation() {
        let point = (0.2, 0.7);
        let assert_fraction = |actual: (f64, f64), expected: (f64, f64)| {
            assert!((actual.0 - expected.0).abs() < 1e-12);
            assert!((actual.1 - expected.1).abs() < 1e-12);
        };
        assert_fraction(Transform::Normal.source_fraction(point.0, point.1), point);
        assert_fraction(
            Transform::Rotate90.source_fraction(point.0, point.1),
            (0.3, 0.2),
        );
        assert_fraction(
            Transform::Rotate180.source_fraction(point.0, point.1),
            (0.8, 0.3),
        );
        assert_fraction(
            Transform::Rotate270.source_fraction(point.0, point.1),
            (0.7, 0.8),
        );
        assert_fraction(
            Transform::FlipRotate90.source_fraction(point.0, point.1),
            (0.7, 0.2),
        );
    }

    #[test]
    fn intersect_clamps_to_overlap_and_rejects_misses() {
        let outer = PixelRect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        assert_eq!(
            intersect(
                PixelRect {
                    x: 50,
                    y: 60,
                    width: 80,
                    height: 70,
                },
                outer,
            ),
            Some(PixelRect {
                x: 50,
                y: 60,
                width: 50,
                height: 40,
            })
        );
        assert_eq!(
            intersect(
                PixelRect {
                    x: 200,
                    y: 200,
                    width: 10,
                    height: 10,
                },
                outer,
            ),
            None
        );
        assert_eq!(
            intersect(
                PixelRect {
                    x: 10,
                    y: 10,
                    width: 0,
                    height: 5,
                },
                outer,
            ),
            None
        );
    }

    #[test]
    fn changed_rect_to_output_maps_visible_part_for_normal_transform() {
        let crop = PixelRect {
            x: 0,
            y: 0,
            width: 1600,
            height: 1200,
        };
        assert_eq!(
            changed_rect_to_output(
                PixelRect {
                    x: 320,
                    y: 240,
                    width: 640,
                    height: 480,
                },
                crop,
                Transform::Normal,
                (800, 600),
            ),
            Some(PixelRect {
                x: 160,
                y: 120,
                width: 320,
                height: 240,
            })
        );
        assert_eq!(
            changed_rect_to_output(
                PixelRect {
                    x: 0,
                    y: 0,
                    width: 64,
                    height: 64,
                },
                crop,
                Transform::Rotate90,
                (800, 600),
            ),
            None
        );
        assert_eq!(
            changed_rect_to_output(
                PixelRect {
                    x: 2000,
                    y: 2000,
                    width: 64,
                    height: 64,
                },
                crop,
                Transform::Normal,
                (800, 600),
            ),
            None
        );
    }
}
