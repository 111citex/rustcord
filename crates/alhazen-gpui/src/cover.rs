//! Cover art: an embedded picture (JPEG/PNG) decoded for painting.

use std::sync::Arc;

use gpui::RenderImage;

/// Covers larger than this (on their longer side) are scaled down to it.
const MAX_SIDE: u32 = 1024;

/// Decodes `data` into a gpui image (BGRA, like video frames); `None` when it is not an image.
pub(crate) fn decode_cover(data: &[u8]) -> Option<Arc<RenderImage>> {
    let mut image = image::load_from_memory(data).ok()?;
    // Covers are shown small: a huge one would otherwise be a huge texture.
    if image.width().max(image.height()) > MAX_SIDE {
        image = image.resize(MAX_SIDE, MAX_SIDE, image::imageops::FilterType::Triangle);
    }
    let mut image = image.into_rgba8();
    for px in image.pixels_mut() {
        px.0.swap(0, 2);
    }
    Some(Arc::new(RenderImage::new([image::Frame::new(image)])))
}
