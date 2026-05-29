use crate::models::ShotMeta;
use image::imageops;
use image::{DynamicImage, GenericImageView, ImageBuffer, Rgba};

pub(crate) fn render(img: &DynamicImage, _shot: &ShotMeta) -> DynamicImage {
    let (w, h) = img.dimensions();
    let border = ((w.max(h) as f32) * 0.05).round().max(16.0) as u32;
    let framed_w = w + border * 2;
    let framed_h = h + border * 2;
    let mut out = ImageBuffer::from_pixel(framed_w, framed_h, Rgba([236, 241, 248, 255]));
    imageops::overlay(&mut out, img, border as i64, border as i64);
    DynamicImage::ImageRgba8(out)
}
