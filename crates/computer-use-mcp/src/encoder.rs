use image::{
    ExtendedColorType, ImageBuffer, ImageEncoder, Rgba, RgbaImage,
    codecs::png::{CompressionType, FilterType, PngEncoder},
    imageops,
};

use crate::geometry::{PixelRect, Transform};

pub const MAX_LONGEST_DIMENSION: u32 = 1280;
pub const MAX_PNG_BYTES: usize = 900 * 1024;
const MAX_DOWNSCALE_ATTEMPTS: usize = 10;

#[derive(Debug, Clone, PartialEq)]
pub struct EncodedPng {
    pub bytes: Vec<u8>,
    pub size: (u32, u32),
}

pub fn encode(
    rgba: Vec<u8>,
    frame_size: (u32, u32),
    valid_crop: PixelRect,
    transform: Transform,
    output_crop: PixelRect,
) -> Result<EncodedPng, String> {
    encode_with_limits(
        rgba,
        frame_size,
        valid_crop,
        transform,
        output_crop,
        MAX_LONGEST_DIMENSION,
        MAX_PNG_BYTES,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_with_limits(
    rgba: Vec<u8>,
    frame_size: (u32, u32),
    valid_crop: PixelRect,
    transform: Transform,
    output_crop: PixelRect,
    maximum_dimension: u32,
    maximum_bytes: usize,
) -> Result<EncodedPng, String> {
    let image = RgbaImage::from_raw(frame_size.0, frame_size.1, rgba)
        .ok_or_else(|| "PipeWire RGBA frame length does not match its dimensions".to_owned())?;
    let cropped = checked_crop(&image, valid_crop)?;
    let transformed = apply_transform(cropped, transform);
    let mut image = checked_crop(&transformed, output_crop)?;
    fit_longest_dimension(&mut image, maximum_dimension);

    for attempt in 0..MAX_DOWNSCALE_ATTEMPTS {
        let bytes = encode_png(&image)?;
        if bytes.len() <= maximum_bytes {
            return Ok(EncodedPng {
                bytes,
                size: (image.width(), image.height()),
            });
        }
        if image.width() == 1 && image.height() == 1 {
            break;
        }
        let factor = 0.82_f64.powi(i32::try_from(attempt + 1).unwrap_or(i32::MAX));
        let width = ((f64::from(image.width()) * factor).round() as u32).max(1);
        let height = ((f64::from(image.height()) * factor).round() as u32).max(1);
        image = imageops::resize(&image, width, height, imageops::FilterType::Lanczos3);
    }
    Err(format!(
        "PNG remains larger than {} bytes after bounded downscaling",
        maximum_bytes
    ))
}

fn checked_crop(image: &RgbaImage, crop: PixelRect) -> Result<RgbaImage, String> {
    if !crop.is_valid_within((image.width(), image.height())) {
        return Err("pixel crop is outside the image".into());
    }
    Ok(imageops::crop_imm(image, crop.x, crop.y, crop.width, crop.height).to_image())
}

fn apply_transform(image: RgbaImage, transform: Transform) -> RgbaImage {
    match transform {
        Transform::Normal => image,
        // SPA video transforms are counter-clockwise; imageops names clockwise rotations.
        Transform::Rotate90 => imageops::rotate270(&image),
        Transform::Rotate180 => imageops::rotate180(&image),
        Transform::Rotate270 => imageops::rotate90(&image),
        Transform::Flip => imageops::flip_horizontal(&image),
        Transform::FlipRotate90 => imageops::rotate270(&imageops::flip_horizontal(&image)),
        Transform::FlipRotate180 => imageops::rotate180(&imageops::flip_horizontal(&image)),
        Transform::FlipRotate270 => imageops::rotate90(&imageops::flip_horizontal(&image)),
    }
}

fn fit_longest_dimension(image: &mut ImageBuffer<Rgba<u8>, Vec<u8>>, maximum: u32) {
    let longest = image.width().max(image.height());
    if longest <= maximum {
        return;
    }
    let factor = f64::from(maximum) / f64::from(longest);
    let width = (f64::from(image.width()) * factor).round().max(1.0) as u32;
    let height = (f64::from(image.height()) * factor).round().max(1.0) as u32;
    *image = imageops::resize(image, width, height, imageops::FilterType::Lanczos3);
}

fn encode_png(image: &RgbaImage) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    PngEncoder::new_with_quality(&mut bytes, CompressionType::Best, FilterType::Adaptive)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            ExtendedColorType::Rgba8,
        )
        .map_err(|error| format!("PNG encoding failed: {error}"))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_respects_dimension_and_byte_limits() {
        let width = 180;
        let height = 140;
        let mut rgba = vec![0; width * height * 4];
        for (index, byte) in rgba.iter_mut().enumerate() {
            *byte = ((index * 37 + index / 17) & 0xff) as u8;
        }
        let encoded = encode_with_limits(
            rgba,
            (width as u32, height as u32),
            PixelRect {
                x: 0,
                y: 0,
                width: width as u32,
                height: height as u32,
            },
            Transform::Rotate90,
            PixelRect {
                x: 0,
                y: 0,
                width: height as u32,
                height: width as u32,
            },
            128,
            9_000,
        )
        .unwrap();
        assert!(encoded.size.0.max(encoded.size.1) <= 128);
        assert!(encoded.bytes.len() <= 9_000);
        assert_eq!(&encoded.bytes[..8], b"\x89PNG\r\n\x1a\n");
    }

    fn labeled_image() -> RgbaImage {
        RgbaImage::from_raw(
            3,
            2,
            vec![
                1, 0, 0, 255, 2, 0, 0, 255, 3, 0, 0, 255, 4, 0, 0, 255, 5, 0, 0, 255, 6, 0, 0, 255,
            ],
        )
        .unwrap()
    }

    fn labels(image: &RgbaImage) -> Vec<u8> {
        image.pixels().map(|pixel| pixel[0]).collect()
    }

    #[test]
    fn spa_rotations_are_counter_clockwise_by_pixel_orientation() {
        let image = labeled_image();
        assert_eq!(
            labels(&apply_transform(image.clone(), Transform::Rotate90)),
            [3, 6, 2, 5, 1, 4]
        );
        assert_eq!(
            labels(&apply_transform(image.clone(), Transform::Rotate180)),
            [6, 5, 4, 3, 2, 1]
        );
        assert_eq!(
            labels(&apply_transform(image, Transform::Rotate270)),
            [4, 1, 5, 2, 6, 3]
        );
    }

    #[test]
    fn spa_flipped_rotations_flip_then_rotate_counter_clockwise() {
        let image = labeled_image();
        assert_eq!(
            labels(&apply_transform(image.clone(), Transform::Flip)),
            [3, 2, 1, 6, 5, 4]
        );
        assert_eq!(
            labels(&apply_transform(image.clone(), Transform::FlipRotate90)),
            [1, 4, 2, 5, 3, 6]
        );
        assert_eq!(
            labels(&apply_transform(image.clone(), Transform::FlipRotate180)),
            [4, 5, 6, 1, 2, 3]
        );
        assert_eq!(
            labels(&apply_transform(image, Transform::FlipRotate270)),
            [6, 3, 5, 2, 4, 1]
        );
    }
}
