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
    pub fn swaps_axes(self) -> bool {
        matches!(
            self,
            Self::Rotate90 | Self::Rotate270 | Self::FlipRotate90 | Self::FlipRotate270
        )
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
}
