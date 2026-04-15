use crate::models::ShotMeta;
use image::DynamicImage;

pub(crate) fn render(img: &DynamicImage, _shot: &ShotMeta) -> DynamicImage {
    img.brighten(8).adjust_contrast(18.0).unsharpen(1.2, 1)
}
