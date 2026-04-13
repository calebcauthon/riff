use crate::models::ShotMeta;
use image::DynamicImage;

pub(crate) fn render(img: &DynamicImage, _shot: &ShotMeta) -> DynamicImage {
    img.clone()
}
