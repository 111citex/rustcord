use crate::{DevicePixels, Pixels, Result, SharedString, Size, size};
use smallvec::SmallVec;

use image::{Delay, Frame};
use std::sync::Arc;
use std::{
    borrow::Cow,
    fmt,
    hash::Hash,
    sync::atomic::{AtomicUsize, Ordering::SeqCst},
};

/// A source of assets for this app to use.
pub trait AssetSource: 'static + Send + Sync {
    /// Load the given asset from the source path.
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>>;

    /// List the assets at the given path.
    fn list(&self, path: &str) -> Result<Vec<SharedString>>;
}

impl AssetSource for () {
    fn load(&self, _path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(None)
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(vec![])
    }
}

/// A unique identifier for the image cache
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ImageId(pub usize);

#[derive(PartialEq, Eq, Hash, Clone)]
#[expect(missing_docs)]
pub struct RenderImageParams {
    pub image_id: ImageId,
    pub frame_index: usize,
}

/// A cached and processed image, in BGRA format
pub struct RenderImage {
    /// The ID associated with this image
    pub id: ImageId,
    /// The scale factor of this image on render.
    pub(crate) scale_factor: f32,
    data: SmallVec<[StoredFrame; 1]>,
    stream: Option<StreamSource>,
}

/// Rustcord: a long animation, kept as its file. This image is its first frame, and
/// the rest are decoded as they come up, by whoever's showing it.
#[derive(Clone)]
pub struct StreamSource {
    /// The file.
    pub bytes: Arc<[u8]>,
    /// The size to scale its frames to fit, if any.
    pub fit: Option<(u32, u32)>,
    /// An animated WebP rather than a GIF.
    pub webp: bool,
}

/// Rustcord: a frame's pixels are only held until they're on the GPU.
struct StoredFrame {
    width: u32,
    height: u32,
    delay: Delay,
    pixels: parking_lot::Mutex<Option<Vec<u8>>>,
}

impl From<Frame> for StoredFrame {
    fn from(frame: Frame) -> Self {
        let delay = frame.delay();
        let buffer = frame.into_buffer();
        let (width, height) = buffer.dimensions();
        StoredFrame { width, height, delay, pixels: parking_lot::Mutex::new(Some(buffer.into_raw())) }
    }
}

impl PartialEq for RenderImage {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for RenderImage {}

impl RenderImage {
    /// Create a new image from the given data.
    pub fn new(data: impl Into<SmallVec<[Frame; 1]>>) -> Self {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

        Self {
            id: ImageId(NEXT_ID.fetch_add(1, SeqCst)),
            scale_factor: 1.0,
            data: data.into().into_iter().map(StoredFrame::from).collect(),
            stream: None,
        }
    }

    /// This image as the first frame of a long animation.
    pub fn with_stream(mut self, stream: StreamSource) -> Self {
        self.stream = Some(stream);
        self
    }

    /// Where the rest of its frames are, if it's the first frame of a long animation.
    pub fn stream(&self) -> Option<&StreamSource> {
        self.stream.as_ref()
    }

    /// Rustcord: whether a frame's pixels are still here, not yet on the GPU.
    pub fn has_pixels(&self, frame_index: usize) -> bool {
        self.data.get(frame_index).is_some_and(|f| f.pixels.lock().is_some())
    }

    /// Takes a frame's pixels: they're the GPU's from here on. `None` if they've
    /// already been taken.
    pub fn take_pixels(&self, frame_index: usize) -> Option<Vec<u8>> {
        self.data.get(frame_index)?.pixels.lock().take()
    }

    /// Get the size of this image, in pixels.
    pub fn size(&self, frame_index: usize) -> Size<DevicePixels> {
        self.data
            .get(frame_index)
            .map(|frame| size(frame.width.into(), frame.height.into()))
            .unwrap_or_default()
    }

    /// Get the size of this image, in pixels for display, adjusted for the scale factor.
    pub(crate) fn render_size(&self, frame_index: usize) -> Size<Pixels> {
        self.size(frame_index)
            .map(|v| (v.0 as f32 / self.scale_factor).into())
    }

    /// Get the delay of this frame from the previous
    pub fn delay(&self, frame_index: usize) -> Delay {
        self.data
            .get(frame_index)
            .map(|frame| frame.delay)
            .unwrap_or(Delay::from_numer_denom_ms(100, 1))
    }

    /// Get the number of frames for this image.
    pub fn frame_count(&self) -> usize {
        self.data.len()
    }
}

impl fmt::Debug for RenderImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImageData")
            .field("id", &self.id)
            .field("size", &self.data.first().map(|f| (f.width, f.height)))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smallvec::SmallVec;

    #[test]
    fn empty_render_image_does_not_panic() {
        let image = RenderImage::new(SmallVec::new());
        assert_eq!(image.frame_count(), 0);
        assert_eq!(image.size(0), Size::default());
        assert_eq!(image.take_pixels(0), None);
        assert_eq!(image.render_size(0), Size::default());
        assert_eq!(image.delay(0), Delay::from_numer_denom_ms(100, 1));
        let _ = format!("{image:?}");
    }
}
