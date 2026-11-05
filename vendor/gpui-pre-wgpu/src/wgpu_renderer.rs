use crate::frame_mailbox::FrameMailbox;
use crate::{AtlasFrame, CompositorGpuHint, DeviceErrorState, WgpuAtlas, WgpuContext};
use anyhow::{Context as _, Result};
use bytemuck::{Pod, Zeroable};
use collections::FxHashMap;
use gpui::{
    AtlasTextureId, Background, Bounds, DevicePixels, GpuSpecs, Path, Point, PrimitiveBatch,
    ScaledPixels, Scene, Size, get_gamma_correction_ratios,
};
use log::warn;
use parking_lot::Mutex;
#[cfg(not(target_family = "wasm"))]
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use smallvec::SmallVec;
use std::cell::RefCell;
use std::num::NonZeroU64;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

const MAX_INSTANCE_BUFFER_SIZE: u64 = 256 * 1024 * 1024;

const INSTANCE_TEXTURE_TEXEL_SIZE: u64 = 16;

/// Shader variant for backends with storage buffer support: the shared shader
/// logic plus the storage-buffer instance transport.
const STORAGE_BUFFER_SHADERS: &str = concat!(
    include_str!("shaders.wgsl"),
    include_str!("shaders_storage.wgsl"),
);

/// Shader variant for WebGL2, which has no storage buffers: the shared shader
/// logic plus the texture-based instance transport.
const WEBGL_SHADERS: &str = concat!(
    include_str!("shaders.wgsl"),
    include_str!("shaders_webgl.wgsl"),
);

/// Subpixel text rendering requires dual-source blending, which WebGL2 lacks, so
/// this variant only ever runs with the storage-buffer transport. The `enable`
/// directive must precede all declarations.
const SUBPIXEL_SHADERS: &str = concat!(
    "enable dual_source_blending;\n",
    include_str!("shaders.wgsl"),
    include_str!("shaders_storage.wgsl"),
    include_str!("shaders_subpixel.wgsl"),
);

fn least_common_multiple(left: u64, right: u64) -> u64 {
    let mut first = left;
    let mut second = right;
    while second != 0 {
        let remainder = first % second;
        first = second;
        second = remainder;
    }
    left / first * right
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GlobalParams {
    viewport_size: [f32; 2],
    premultiplied_alpha: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct PodBounds {
    origin: [f32; 2],
    size: [f32; 2],
}

impl From<Bounds<ScaledPixels>> for PodBounds {
    fn from(bounds: Bounds<ScaledPixels>) -> Self {
        Self {
            origin: [bounds.origin.x.0, bounds.origin.y.0],
            size: [bounds.size.width.0, bounds.size.height.0],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct SurfaceParams {
    bounds: PodBounds,
    content_mask: PodBounds,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GammaParams {
    gamma_ratios: [f32; 4],
    grayscale_enhanced_contrast: f32,
    subpixel_enhanced_contrast: f32,
    is_bgr: u32,
    _pad: u32,
}

#[derive(Clone, Debug)]
#[repr(C)]
struct PathSprite {
    bounds: Bounds<ScaledPixels>,
}

#[derive(Clone, Debug)]
#[repr(C)]
struct PathRasterizationVertex {
    xy_position: Point<ScaledPixels>,
    st_position: Point<f32>,
    color: Background,
    bounds: Bounds<ScaledPixels>,
}

pub struct WgpuSurfaceConfig {
    pub size: Size<DevicePixels>,
    pub transparent: bool,
    /// Preferred presentation mode. When `Some`, the renderer will use this
    /// mode if supported by the surface, falling back to `Fifo`.
    /// When `None`, defaults to `Fifo` (VSync).
    ///
    /// Mobile platforms may prefer `Mailbox` (triple-buffering) to avoid
    /// blocking in `get_current_texture()` during lifecycle transitions.
    pub preferred_present_mode: Option<wgpu::PresentMode>,
}

struct WgpuPipelines {
    quads: wgpu::RenderPipeline,
    shadows: wgpu::RenderPipeline,
    path_rasterization: wgpu::RenderPipeline,
    paths: wgpu::RenderPipeline,
    underlines: wgpu::RenderPipeline,
    mono_sprites: wgpu::RenderPipeline,
    subpixel_sprites: Option<wgpu::RenderPipeline>,
    poly_sprites: wgpu::RenderPipeline,
    #[allow(dead_code)]
    surfaces: wgpu::RenderPipeline,
}

/// One frame allocation of instance data, ready to bind.
struct InstanceBinding {
    bind_group: wgpu::BindGroup,
    /// Index of the allocation's first instance within the bound data. Always
    /// zero on the storage-buffer path, where the binding offset already
    /// positions the array; on the WebGL texture path the shader indexes the
    /// shared instance texture absolutely, so draws must offset their
    /// instance (or vertex) ranges by this value.
    first_instance: u32,
}

struct InstanceBindings {
    quads: InstanceBinding,
    shadows: InstanceBinding,
    underlines: InstanceBinding,
    monochrome_sprites: InstanceBinding,
    subpixel_sprites: InstanceBinding,
    polychrome_sprites: InstanceBinding,
}

struct WgpuBindGroupLayouts {
    globals: wgpu::BindGroupLayout,
    instances: wgpu::BindGroupLayout,
    texture: wgpu::BindGroupLayout,
    surfaces: wgpu::BindGroupLayout,
}

/// Shared GPU context reference, used to coordinate device recovery across multiple windows.
pub type GpuContext = Rc<RefCell<Option<WgpuContext>>>;

enum InstanceData {
    Storage(wgpu::Buffer),
    // WebGL2 has no storage buffers. A uint texture keeps the records available to both shader
    // stages while preserving integer and floating-point bit patterns exactly.
    Texture {
        texture: wgpu::Texture,
        view: wgpu::TextureView,
        width: u32,
        height: u32,
    },
}

/// GPU resources that must be dropped together during device recovery.
struct WgpuResources {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    pipelines: WgpuPipelines,
    bind_group_layouts: WgpuBindGroupLayouts,
    atlas_sampler: wgpu::Sampler,
    atlas_texture_bind_groups: FxHashMap<AtlasTextureId, CachedTextureBindGroup>,
    globals_buffer: wgpu::Buffer,
    globals_bind_group: wgpu::BindGroup,
    path_globals_bind_group: wgpu::BindGroup,
    instance_data: InstanceData,
    path_intermediate_texture: Option<wgpu::Texture>,
    path_intermediate_view: Option<wgpu::TextureView>,
    path_msaa_texture: Option<wgpu::Texture>,
    path_msaa_view: Option<wgpu::TextureView>,
}

struct CachedTextureBindGroup {
    texture_generation: u64,
    bind_group: wgpu::BindGroup,
}

impl WgpuResources {
    fn invalidate_intermediate_textures(&mut self) {
        self.path_intermediate_texture = None;
        self.path_intermediate_view = None;
        self.path_msaa_texture = None;
        self.path_msaa_view = None;
    }
}

struct WgpuRendererCore {
    resources: WgpuResources,
    path_globals_offset: u64,
    gamma_offset: u64,
    instance_data_capacity: u64,
    max_instance_data_size: u64,
    instance_data_alignment: u64,
    uses_webgl_instance_data: bool,
    rendering_params: RenderingParameters,
    is_bgr: bool,
    dual_source_blending: bool,
    adapter_info: wgpu::AdapterInfo,
    target_format: wgpu::TextureFormat,
    max_texture_size: u32,
}

/// GPU resources of a windowed renderer. A surface is only ever configured against the
/// device that owns `core`, so it cannot outlive it: there is no surface-only state.
enum RendererState {
    /// Frames can be drawn.
    Ready {
        surface: wgpu::Surface<'static>,
        core: WgpuRendererCore,
    },
    /// The native surface is gone (Android `TerminateWindow`, browser context loss) but
    /// the device, pipelines, and atlas remain so `replace_surface` can resume without
    /// re-uploading cached textures.
    Unconfigured { core: WgpuRendererCore },
    /// Released by `destroy`, or dropped for a device recovery that has not succeeded yet.
    Released,
}

/// The surface and everything that draws to it. Rustcord: the render thread holds this lock
/// for as long as it works on a frame, so when the main thread takes it to change the
/// surface, no frame is half done. The render thread never waits for the main thread with
/// this held, which is what keeps that from deadlocking.
struct RenderTarget {
    state: RendererState,
    surface_config: wgpu::SurfaceConfiguration,
    transparent_alpha_mode: wgpu::CompositeAlphaMode,
    opaque_alpha_mode: wgpu::CompositeAlphaMode,
    is_bgr: bool,
    failed_frame_count: u32,
    device_errors: Arc<DeviceErrorState>,
    observed_error_generation: u64,
    last_surface_error: Option<String>,
}

/// Rustcord: a finished frame on its way to the render thread.
#[derive(Default)]
struct FramePacket {
    scene: Scene,
    atlas: AtlasFrame,
    /// See [`RenderShared::surface_generation`].
    surface_generation: u64,
}

/// Rustcord: what the main thread and the render thread share.
struct RenderShared {
    target: Mutex<RenderTarget>,
    mailbox: FrameMailbox<FramePacket>,
    /// Goes up when the surface changes in a way that makes frames painted before it wrong
    /// to show, like a resize. Those frames are then never presented, so nothing of the old
    /// size can reach the compositor once the main thread has acked the new size.
    surface_generation: AtomicU64,
    /// The last frame didn't reach the screen.
    frame_failed: AtomicBool,
    /// GPU errors kept coming: the atlas should be emptied, which is for the main thread to
    /// do because that's where tiles get handed out.
    atlas_reset_wanted: AtomicBool,
    thread_died: AtomicBool,
    /// Called when the main thread should look at the flags above.
    feedback_waker: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// Paces frames that only the render thread knows about. See [`FramePacer`].
    pacer: Mutex<Option<Box<dyn FramePacer>>>,
}

/// Rustcord: tells the render thread when the compositor wants another frame, with no help from
/// the main thread, which may be busy just then. Frames for a glide are made at that pace.
pub trait FramePacer: Send {
    /// Asks to hear about the frame after the next commit. A request not answered yet still holds.
    fn request_frame(&mut self);
    /// Blocks until it's heard about, for `timeout` at most, since it won't be for a surface
    /// that isn't showing. No frame comes of waiting here, so this can't busy-loop.
    fn wait_for_frame(&mut self, timeout: Duration);
}

/// How long to wait for the compositor's word before moving on anyway: just over a 30 Hz frame.
/// A glide is over in moments, so a hidden window doesn't spend long at this.
#[cfg(not(target_family = "wasm"))]
const LONGEST_FRAME_WAIT: Duration = Duration::from_millis(34);

impl RenderShared {
    fn wake_main_thread(&self) {
        if let Some(waker) = self.feedback_waker.lock().as_ref() {
            waker();
        }
    }
}

/// Rustcord: lets the main thread know if the render thread goes down with a panic.
/// Without this a window would just freeze.
#[cfg(not(target_family = "wasm"))]
struct RenderThreadWatch(Arc<RenderShared>);

#[cfg(not(target_family = "wasm"))]
impl Drop for RenderThreadWatch {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.thread_died.store(true, Ordering::Release);
            self.0.wake_main_thread();
        }
    }
}

/// Rustcord: the render thread. It owns every GPU step of a frame, from uploads to present,
/// so that a slow driver or a wait for a free surface image never holds up the main thread.
#[cfg(not(target_family = "wasm"))]
fn render_thread(shared: Arc<RenderShared>) {
    let _watch = RenderThreadWatch(shared.clone());
    // The frame on screen, kept for as long as its live layers may have to move, and the scene
    // composed from it for each showing.
    let mut shown: Option<FramePacket> = None;
    let mut composed = Scene::default();
    let mut gliding = false;
    loop {
        let arrived = if gliding {
            // Pace with the compositor: a new frame from the main thread waits for this too,
            // which is the same frame it'd have made it into at the earliest.
            if let Some(pacer) = shared.pacer.lock().as_mut() {
                pacer.wait_for_frame(LONGEST_FRAME_WAIT);
            }
            if shared.mailbox.is_closed() {
                return;
            }
            shared.mailbox.take_pending()
        } else {
            match shared.mailbox.wait_or_nudge() {
                Some(arrived) => arrived,
                None => return,
            }
        };

        let fresh = arrived.is_some();
        if let Some(frame) = arrived {
            if let Some(older) = shown.replace(frame) {
                recycle_frame(older, &shared);
            }
        }
        // Painted for a surface that's gone: only the next paint can show anything.
        let generation = shared.surface_generation.load(Ordering::Acquire);
        if !fresh && shown.as_ref().is_some_and(|frame| frame.surface_generation != generation) {
            recycle_frame(shown.take().unwrap(), &shared);
        }
        let Some(frame) = shown.as_mut() else {
            gliding = false;
            continue;
        };

        let advance = frame.scene.advance_live_layers(std::time::Instant::now());
        gliding = advance.active;
        if !fresh && !advance.changed {
            continue;
        }
        if gliding && let Some(pacer) = shared.pacer.lock().as_mut() {
            pacer.request_frame();
        }
        let presented = shared.target.lock().render_packet(frame, &mut composed, &shared);
        if presented {
            frame.scene.note_live_presented(std::time::Instant::now());
        }
        if !frame.scene.has_live_layers() {
            recycle_frame(shown.take().unwrap(), &shared);
        }
        // Without a commit the compositor won't send the frame callback that the main thread
        // is waiting for, so it has to hear about this some other way. A frame shown again is
        // nothing the main thread is waiting on.
        if fresh && !presented {
            shared.frame_failed.store(true, Ordering::Release);
            shared.wake_main_thread();
        }
    }
}

#[cfg(not(target_family = "wasm"))]
fn recycle_frame(mut frame: FramePacket, shared: &RenderShared) {
    frame.scene.clear();
    frame.atlas.reset();
    shared.mailbox.recycle(frame);
}

/// The main thread's handle on a window's rendering. Rustcord: frames are drawn by
/// [`render_thread`]. All the methods here belong to the main thread.
pub struct WgpuRenderer {
    /// Shared GPU context for device recovery coordination (unused on WASM).
    #[allow(dead_code)]
    context: Option<GpuContext>,
    /// Compositor GPU hint for adapter selection (unused on WASM).
    #[allow(dead_code)]
    compositor_gpu: Option<CompositorGpuHint>,
    atlas: Arc<WgpuAtlas>,
    max_texture_size: u32,
    needs_redraw: bool,
    /// Whether frames can be drawn, so `draw` can answer without taking the target's lock.
    /// Only the main thread changes that state.
    surface_ready: bool,
    /// Asked on every frame, so this stays off the target's lock too.
    device_errors: Arc<DeviceErrorState>,
    shared: Arc<RenderShared>,
    has_pacer: bool,
    #[cfg(not(target_family = "wasm"))]
    render_thread: Option<std::thread::JoinHandle<()>>,
}

impl WgpuRenderer {
    /// Creates a new WgpuRenderer from raw window handles.
    ///
    /// The `gpu_context` is a shared reference that coordinates GPU context across
    /// multiple windows. The first window to create a renderer will initialize the
    /// context; subsequent windows will share it.
    ///
    /// # Safety
    /// The caller must ensure that the window handle remains valid for the lifetime
    /// of the returned renderer.
    #[cfg(not(target_family = "wasm"))]
    pub fn new<W>(
        gpu_context: GpuContext,
        window: &W,
        config: WgpuSurfaceConfig,
        compositor_gpu: Option<CompositorGpuHint>,
    ) -> anyhow::Result<Self>
    where
        W: HasWindowHandle + HasDisplayHandle + std::fmt::Debug + Send + Sync + Clone + 'static,
    {
        let window_handle = window
            .window_handle()
            .map_err(|e| anyhow::anyhow!("Failed to get window handle: {e}"))?;

        let target = wgpu::SurfaceTargetUnsafe::RawHandle {
            // Fall back to the display handle already provided via InstanceDescriptor::display.
            raw_display_handle: None,
            raw_window_handle: window_handle.as_raw(),
        };

        // Use the existing context's instance if available, otherwise create a new one.
        // The surface must be created with the same instance that will be used for
        // adapter selection, otherwise wgpu will panic.
        let instance = gpu_context
            .borrow()
            .as_ref()
            .map(|ctx| ctx.instance.clone())
            .unwrap_or_else(|| WgpuContext::instance(Some(Box::new(window.clone()))));

        // Safety: The caller guarantees that the window handle is valid for the
        // lifetime of this renderer. In practice, the RawWindow struct is created
        // from the native window handles and the surface is dropped before the window.
        let surface = unsafe {
            instance
                .create_surface_unsafe(target)
                .map_err(|e| anyhow::anyhow!("Failed to create surface: {e}"))?
        };

        let mut ctx_ref = gpu_context.borrow_mut();
        let context = match ctx_ref.as_mut() {
            Some(context) => {
                context.check_compatible_with_surface(&surface)?;
                context
            }
            None => ctx_ref.insert(WgpuContext::new(instance, &surface, compositor_gpu)?),
        };

        let atlas = Arc::new(WgpuAtlas::from_context(context));

        Self::new_internal(
            Some(Rc::clone(&gpu_context)),
            context,
            surface,
            config,
            compositor_gpu,
            atlas,
        )
    }

    #[cfg(target_family = "wasm")]
    pub fn new_from_canvas(
        context: &WgpuContext,
        canvas: &web_sys::HtmlCanvasElement,
        config: WgpuSurfaceConfig,
    ) -> anyhow::Result<Self> {
        let surface = context
            .instance
            .create_surface(wgpu::SurfaceTarget::Canvas(canvas.clone()))
            .map_err(|e| anyhow::anyhow!("Failed to create surface: {e}"))?;
        Self::new_from_surface(context, surface, config)
    }

    #[cfg(target_family = "wasm")]
    #[allow(clippy::arc_with_non_send_sync)]
    pub fn new_from_surface(
        context: &WgpuContext,
        surface: wgpu::Surface<'static>,
        config: WgpuSurfaceConfig,
    ) -> anyhow::Result<Self> {
        let atlas = Arc::new(WgpuAtlas::from_context(context));
        Self::new_internal(None, context, surface, config, None, atlas)
    }

    #[allow(clippy::arc_with_non_send_sync)]
    fn new_internal(
        gpu_context: Option<GpuContext>,
        context: &WgpuContext,
        surface: wgpu::Surface<'static>,
        config: WgpuSurfaceConfig,
        compositor_gpu: Option<CompositorGpuHint>,
        atlas: Arc<WgpuAtlas>,
    ) -> anyhow::Result<Self> {
        let target = RenderTarget::new(context, surface, config)?;
        let max_texture_size = context.device.limits().max_texture_dimension_2d;
        let shared = Arc::new(RenderShared {
            target: Mutex::new(target),
            mailbox: FrameMailbox::new(),
            surface_generation: AtomicU64::new(0),
            frame_failed: AtomicBool::new(false),
            atlas_reset_wanted: AtomicBool::new(false),
            thread_died: AtomicBool::new(false),
            feedback_waker: Mutex::new(None),
            pacer: Mutex::new(None),
        });
        #[cfg(not(target_family = "wasm"))]
        let render_thread = {
            gpui::frame_stats::PRESENTED_BY_RENDER_THREAD.store(true, std::sync::atomic::Ordering::Relaxed);
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("gpui-render".to_string())
                // GPU drivers can be hungry for stack, and this is only address space until used.
                .stack_size(8 * 1024 * 1024)
                .spawn(move || render_thread(shared))
                .context("Failed to start the render thread")?
        };

        Ok(Self {
            context: gpu_context,
            compositor_gpu,
            atlas,
            max_texture_size,
            needs_redraw: false,
            surface_ready: true,
            device_errors: Arc::clone(context.errors()),
            shared,
            has_pacer: false,
            #[cfg(not(target_family = "wasm"))]
            render_thread: Some(render_thread),
        })
    }
}

impl RenderTarget {
    fn new(
        context: &WgpuContext,
        surface: wgpu::Surface<'static>,
        config: WgpuSurfaceConfig,
    ) -> anyhow::Result<Self> {
        let surface_caps = surface.get_capabilities(&context.adapter);
        let preferred_formats = [
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Rgba8Unorm,
        ];
        let surface_format = preferred_formats
            .iter()
            .find(|f| surface_caps.formats.contains(f))
            .copied()
            .or_else(|| surface_caps.formats.iter().find(|f| !f.is_srgb()).copied())
            .or_else(|| surface_caps.formats.first().copied())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Surface reports no supported texture formats for adapter {:?}",
                    context.adapter.get_info().name
                )
            })?;

        let pick_alpha_mode =
            |preferences: &[wgpu::CompositeAlphaMode]| -> anyhow::Result<wgpu::CompositeAlphaMode> {
                preferences
                    .iter()
                    .find(|p| surface_caps.alpha_modes.contains(p))
                    .copied()
                    .or_else(|| surface_caps.alpha_modes.first().copied())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "Surface reports no supported alpha modes for adapter {:?}",
                            context.adapter.get_info().name
                        )
                    })
            };

        let transparent_alpha_mode = pick_alpha_mode(&[
            wgpu::CompositeAlphaMode::PreMultiplied,
            wgpu::CompositeAlphaMode::Inherit,
        ])?;

        let opaque_alpha_mode = pick_alpha_mode(&[
            wgpu::CompositeAlphaMode::Opaque,
            wgpu::CompositeAlphaMode::Inherit,
        ])?;

        let alpha_mode = if config.transparent {
            transparent_alpha_mode
        } else {
            opaque_alpha_mode
        };

        let device = Arc::clone(&context.device);
        let max_texture_size = device.limits().max_texture_dimension_2d;

        let requested_width = config.size.width.0 as u32;
        let requested_height = config.size.height.0 as u32;
        let clamped_width = requested_width.min(max_texture_size);
        let clamped_height = requested_height.min(max_texture_size);

        if clamped_width != requested_width || clamped_height != requested_height {
            warn!(
                "Requested surface size ({}, {}) exceeds maximum texture dimension {}. \
                 Clamping to ({}, {}). Window content may not fill the entire window.",
                requested_width, requested_height, max_texture_size, clamped_width, clamped_height
            );
        }

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width: clamped_width.max(1),
            height: clamped_height.max(1),
            present_mode: config
                .preferred_present_mode
                .filter(|mode| surface_caps.present_modes.contains(mode))
                .unwrap_or(wgpu::PresentMode::Fifo),
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: vec![],
        };
        // Configure the surface immediately. The adapter selection process already validated
        // that this adapter can successfully configure this surface.
        surface.configure(&context.device, &surface_config);

        let core = WgpuRendererCore::new(context, surface_format, alpha_mode);

        Ok(Self {
            state: RendererState::Ready { surface, core },
            surface_config,
            transparent_alpha_mode,
            opaque_alpha_mode,
            is_bgr: false,
            failed_frame_count: 0,
            device_errors: Arc::clone(context.errors()),
            observed_error_generation: 0,
            last_surface_error: None,
        })
    }

    fn core(&self) -> Option<&WgpuRendererCore> {
        match &self.state {
            RendererState::Ready { core, .. } | RendererState::Unconfigured { core } => Some(core),
            RendererState::Released => None,
        }
    }

    fn core_mut(&mut self) -> Option<&mut WgpuRendererCore> {
        match &mut self.state {
            RendererState::Ready { core, .. } | RendererState::Unconfigured { core } => Some(core),
            RendererState::Released => None,
        }
    }

    /// Writes a frame's uploads even when the frame itself won't be drawn. Its tiles count as
    /// uploaded from now on, and later frames will draw them.
    fn flush_uploads(&mut self, frame: &mut FramePacket) {
        if let Some(core) = self.core() {
            frame.atlas.flush_uploads(&core.resources.queue);
        }
    }

    /// Draws a frame and presents it. Returns whether it reached the compositor. The scene is
    /// left as it came if it has live layers, for showing it again, and `composed` is drawn.
    fn render_packet(
        &mut self,
        frame: &mut FramePacket,
        composed: &mut Scene,
        shared: &RenderShared,
    ) -> bool {
        self.flush_uploads(frame);
        if frame.surface_generation != shared.surface_generation.load(Ordering::Acquire) {
            return false;
        }

        #[cfg(target_family = "wasm")]
        if self.device_errors.device_lost() {
            if matches!(self.state, RendererState::Ready { .. }) {
                log::error!(
                    "Browser graphics context was lost; rendering has stopped. Reload the page to recover."
                );
                self.state = match std::mem::replace(&mut self.state, RendererState::Released) {
                    RendererState::Ready { core, .. } => RendererState::Unconfigured { core },
                    state => state,
                };
            }
            return false;
        }

        // Bail out early if the surface has been unconfigured (e.g. during
        // Android background/rotation transitions).  Attempting to acquire
        // a texture from an unconfigured surface can block indefinitely on
        // some drivers (Adreno).
        let RendererState::Ready { surface, core } = &mut self.state else {
            return false;
        };

        if let Some(error) = self.last_surface_error.take().or_else(|| {
            self.device_errors
                .observe_error(&mut self.observed_error_generation)
        }) {
            self.failed_frame_count += 1;
            log::error!(
                "GPU error during frame (failure {} of 10): {error}",
                self.failed_frame_count
            );

            // TBD. Does retrying more actually help?
            if self.failed_frame_count > 10 {
                panic!("Too many consecutive GPU errors. Last error: {error}");
            } else if self.failed_frame_count > 5 {
                core.resources.invalidate_intermediate_textures();
                shared.atlas_reset_wanted.store(true, Ordering::Release);
                self.failed_frame_count = 0;
                return false;
            }
        } else {
            self.failed_frame_count = 0;
        }

        // Rustcord: part of drawing, so that it costs the main thread nothing.
        let scene = if frame.scene.has_live_layers() {
            frame.scene.copy_primitives_into(composed);
            composed.compose_scroll_layers();
            &*composed
        } else {
            frame.scene.compose_scroll_layers();
            &frame.scene
        };

        let render_start = std::time::Instant::now();
        // Rustcord: reconfiguring and trying again right here saves the main thread a trip
        // through the frame loop just because the surface went out of date.
        let mut can_retry = true;
        let surface_texture = loop {
            match surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(surface_texture) => break surface_texture,
                wgpu::CurrentSurfaceTexture::Suboptimal(surface_texture) => {
                    // Textures must be destroyed before the surface can be reconfigured.
                    drop(surface_texture);
                    surface.configure(&core.resources.device, &self.surface_config);
                }
                wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                    surface.configure(&core.resources.device, &self.surface_config);
                }
                wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                    return false;
                }
                wgpu::CurrentSurfaceTexture::Validation => {
                    self.last_surface_error = Some("Surface texture validation error".to_string());
                    return false;
                }
            }
            if !std::mem::take(&mut can_retry) {
                return false;
            }
        };
        gpui::frame_stats::since(&gpui::frame_stats::ACQUIRE_NS, render_start);

        // The acquired texture is the authority on frame dimensions; the surface
        // configuration is only a request.
        let size = Size {
            width: DevicePixels(surface_texture.texture.width() as i32),
            height: DevicePixels(surface_texture.texture.height() as i32),
        };
        let frame_view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let premultiplied_alpha =
            self.surface_config.alpha_mode == wgpu::CompositeAlphaMode::PreMultiplied;
        if let Err(error) = core.render_frame(
            scene,
            &mut frame.atlas,
            &frame_view,
            size,
            premultiplied_alpha,
            wgpu::Color::TRANSPARENT,
        ) {
            log::error!("{error:#}");
            return false;
        }

        let present_start = std::time::Instant::now();
        surface_texture.present();
        gpui::frame_stats::presented(present_start, std::time::Instant::now());
        gpui::frame_stats::since(&gpui::frame_stats::RENDER_NS, render_start);
        gpui::frame_stats::add(&gpui::frame_stats::RENDER_FRAMES, 1);
        true
    }
}

impl WgpuRendererCore {
    fn create_bind_group_layouts(
        device: &wgpu::Device,
        uses_webgl_instance_data: bool,
    ) -> WgpuBindGroupLayouts {
        let globals =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("globals_layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: NonZeroU64::new(
                                std::mem::size_of::<GlobalParams>() as u64
                            ),
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: NonZeroU64::new(
                                std::mem::size_of::<GammaParams>() as u64
                            ),
                        },
                        count: None,
                    },
                ],
            });

        let instance_data_entry = wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: if uses_webgl_instance_data {
                wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Uint,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                }
            } else {
                wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                }
            },
            count: None,
        };

        let instances = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("instances_layout"),
            entries: &[instance_data_entry],
        });

        let texture = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("texture_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let surfaces = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("surfaces_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: NonZeroU64::new(
                            std::mem::size_of::<SurfaceParams>() as u64
                        ),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        WgpuBindGroupLayouts {
            globals,
            instances,
            texture,
            surfaces,
        }
    }

    fn create_instance_texture(
        device: &wgpu::Device,
        requested_capacity: u64,
        max_texture_dimension: u32,
    ) -> (InstanceData, u64) {
        let texel_count = requested_capacity.div_ceil(INSTANCE_TEXTURE_TEXEL_SIZE);
        let width = texel_count.min(u64::from(max_texture_dimension)).max(1) as u32;
        let height = texel_count
            .div_ceil(u64::from(width))
            .min(u64::from(max_texture_dimension))
            .max(1) as u32;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("instance_texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Uint,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let capacity = u64::from(width) * u64::from(height) * INSTANCE_TEXTURE_TEXEL_SIZE;
        (
            InstanceData::Texture {
                texture,
                view,
                width,
                height,
            },
            capacity,
        )
    }

    fn create_pipelines(
        device: &wgpu::Device,
        layouts: &WgpuBindGroupLayouts,
        surface_format: wgpu::TextureFormat,
        alpha_mode: wgpu::CompositeAlphaMode,
        path_sample_count: u32,
        dual_source_blending: bool,
        uses_webgl_instance_data: bool,
    ) -> WgpuPipelines {
        // Diagnostic guard: verify the device actually has
        // DUAL_SOURCE_BLENDING. We have a crash report (ZED-5G1) where a
        // feature mismatch caused a wgpu-hal abort, but we haven't
        // identified the code path that produces the mismatch. This
        // guard prevents the crash and logs more evidence.
        // Remove this check once:
        // a) We find and fix the root cause, or
        // b) There are no reports of this warning appearing for some time.
        let device_has_feature = device
            .features()
            .contains(wgpu::Features::DUAL_SOURCE_BLENDING);
        if dual_source_blending && !device_has_feature {
            log::error!(
                "BUG: dual_source_blending flag is true but device does not \
                 have DUAL_SOURCE_BLENDING enabled (device features: {:?}). \
                 Falling back to mono text rendering. Please report this at \
                 https://github.com/zed-industries/zed/issues",
                device.features(),
            );
        }
        let dual_source_blending =
            dual_source_blending && device_has_feature && !uses_webgl_instance_data;

        let shader_source = if uses_webgl_instance_data {
            WEBGL_SHADERS
        } else {
            STORAGE_BUFFER_SHADERS
        };
        let shader_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("gpui_shaders"),
            source: wgpu::ShaderSource::Wgsl(shader_source.into()),
        });

        let subpixel_shader_module = if dual_source_blending {
            Some(device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("gpui_subpixel_shaders"),
                source: wgpu::ShaderSource::Wgsl(SUBPIXEL_SHADERS.into()),
            }))
        } else {
            None
        };

        let blend_mode = match alpha_mode {
            wgpu::CompositeAlphaMode::PreMultiplied => {
                wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING
            }
            _ => wgpu::BlendState::ALPHA_BLENDING,
        };

        let color_target = wgpu::ColorTargetState {
            format: surface_format,
            blend: Some(blend_mode),
            write_mask: wgpu::ColorWrites::ALL,
        };

        let create_pipeline = |name: &str,
                               vs_entry: &str,
                               fs_entry: &str,
                               globals_layout: &wgpu::BindGroupLayout,
                               data_layout: &wgpu::BindGroupLayout,
                               texture_layout: Option<&wgpu::BindGroupLayout>,
                               topology: wgpu::PrimitiveTopology,
                               color_targets: &[Option<wgpu::ColorTargetState>],
                               sample_count: u32,
                               module: &wgpu::ShaderModule| {
            let mut bind_group_layouts = vec![Some(globals_layout), Some(data_layout)];
            bind_group_layouts.extend(texture_layout.map(Some));
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(&format!("{name}_layout")),
                bind_group_layouts: &bind_group_layouts,
                immediate_size: 0,
            });

            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(name),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module,
                    entry_point: Some(vs_entry),
                    buffers: &[],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module,
                    entry_point: Some(fs_entry),
                    targets: color_targets,
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    topology,
                    strip_index_format: None,
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: None,
                    polygon_mode: wgpu::PolygonMode::Fill,
                    unclipped_depth: false,
                    conservative: false,
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState {
                    count: sample_count,
                    mask: !0,
                    alpha_to_coverage_enabled: false,
                },
                multiview_mask: None,
                cache: None,
            })
        };

        let quads = create_pipeline(
            "quads",
            "vs_quad",
            "fs_quad",
            &layouts.globals,
            &layouts.instances,
            None,
            wgpu::PrimitiveTopology::TriangleStrip,
            &[Some(color_target.clone())],
            1,
            &shader_module,
        );

        let shadows = create_pipeline(
            "shadows",
            "vs_shadow",
            "fs_shadow",
            &layouts.globals,
            &layouts.instances,
            None,
            wgpu::PrimitiveTopology::TriangleStrip,
            &[Some(color_target.clone())],
            1,
            &shader_module,
        );

        let path_rasterization = create_pipeline(
            "path_rasterization",
            "vs_path_rasterization",
            "fs_path_rasterization",
            &layouts.globals,
            &layouts.instances,
            None,
            wgpu::PrimitiveTopology::TriangleList,
            &[Some(wgpu::ColorTargetState {
                format: surface_format,
                blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            path_sample_count,
            &shader_module,
        );

        let paths_blend = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
        };

        let paths = create_pipeline(
            "paths",
            "vs_path",
            "fs_path",
            &layouts.globals,
            &layouts.instances,
            Some(&layouts.texture),
            wgpu::PrimitiveTopology::TriangleStrip,
            &[Some(wgpu::ColorTargetState {
                format: surface_format,
                blend: Some(paths_blend),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            1,
            &shader_module,
        );

        let underlines = create_pipeline(
            "underlines",
            "vs_underline",
            "fs_underline",
            &layouts.globals,
            &layouts.instances,
            None,
            wgpu::PrimitiveTopology::TriangleStrip,
            &[Some(color_target.clone())],
            1,
            &shader_module,
        );

        let mono_sprites = create_pipeline(
            "mono_sprites",
            "vs_mono_sprite",
            "fs_mono_sprite",
            &layouts.globals,
            &layouts.instances,
            Some(&layouts.texture),
            wgpu::PrimitiveTopology::TriangleStrip,
            &[Some(color_target.clone())],
            1,
            &shader_module,
        );

        let subpixel_sprites = if let Some(subpixel_module) = &subpixel_shader_module {
            let subpixel_blend = wgpu::BlendState {
                color: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::Src1,
                    dst_factor: wgpu::BlendFactor::OneMinusSrc1,
                    operation: wgpu::BlendOperation::Add,
                },
                alpha: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::One,
                    dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                    operation: wgpu::BlendOperation::Add,
                },
            };

            Some(create_pipeline(
                "subpixel_sprites",
                "vs_subpixel_sprite",
                "fs_subpixel_sprite",
                &layouts.globals,
                &layouts.instances,
                Some(&layouts.texture),
                wgpu::PrimitiveTopology::TriangleStrip,
                &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(subpixel_blend),
                    write_mask: wgpu::ColorWrites::COLOR,
                })],
                1,
                subpixel_module,
            ))
        } else {
            None
        };

        let poly_sprites = create_pipeline(
            "poly_sprites",
            "vs_poly_sprite",
            "fs_poly_sprite",
            &layouts.globals,
            &layouts.instances,
            Some(&layouts.texture),
            wgpu::PrimitiveTopology::TriangleStrip,
            &[Some(color_target.clone())],
            1,
            &shader_module,
        );

        let surfaces = create_pipeline(
            "surfaces",
            "vs_surface",
            "fs_surface",
            &layouts.globals,
            &layouts.surfaces,
            None,
            wgpu::PrimitiveTopology::TriangleStrip,
            &[Some(color_target)],
            1,
            &shader_module,
        );

        WgpuPipelines {
            quads,
            shadows,
            path_rasterization,
            paths,
            underlines,
            mono_sprites,
            subpixel_sprites,
            poly_sprites,
            surfaces,
        }
    }

    fn create_path_intermediate(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> (wgpu::Texture, wgpu::TextureView) {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("path_intermediate"),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        (texture, view)
    }

    fn create_msaa_if_needed(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        sample_count: u32,
    ) -> Option<(wgpu::Texture, wgpu::TextureView)> {
        if sample_count <= 1 {
            return None;
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("path_msaa"),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Some((texture, view))
    }
}

impl WgpuRenderer {
    /// Records the new drawable size and reconfigures the surface when one is present.
    /// The size is kept even without GPU resources so that recovery restores it.
    ///
    /// Rustcord: this waits for a frame being drawn, and any frame that's painted but not yet
    /// drawn is never presented. So after this returns, the next thing to reach the
    /// compositor is the first frame painted at the new size.
    pub fn update_drawable_size(&mut self, size: Size<DevicePixels>) {
        let width = size.width.0 as u32;
        let height = size.height.0 as u32;

        let mut guard = self.shared.target.lock();
        let target = &mut *guard;
        if width == target.surface_config.width && height == target.surface_config.height {
            return;
        }

        let clamped_width = width.min(self.max_texture_size);
        let clamped_height = height.min(self.max_texture_size);
        if clamped_width != width || clamped_height != height {
            warn!(
                "Requested surface size ({}, {}) exceeds maximum texture dimension {}. \
                 Clamping to ({}, {}). Window content may not fill the entire window.",
                width, height, self.max_texture_size, clamped_width, clamped_height
            );
        }
        target.surface_config.width = clamped_width.max(1);
        target.surface_config.height = clamped_height.max(1);
        self.shared
            .surface_generation
            .fetch_add(1, Ordering::AcqRel);

        let Some(core) = target.core_mut() else {
            return;
        };
        let resources = &mut core.resources;

        // Wait for any in-flight GPU work to complete before destroying textures
        if let Err(e) = resources.device.poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        }) {
            warn!("Failed to poll device during resize: {e:?}");
        }

        // Destroy old textures before allocating new ones to avoid GPU memory spikes
        if let Some(ref texture) = resources.path_intermediate_texture {
            texture.destroy();
        }
        if let Some(ref texture) = resources.path_msaa_texture {
            texture.destroy();
        }

        // Invalidate intermediate textures - they will be lazily recreated
        // in draw() after we confirm the surface is healthy. This avoids
        // panics when the device/surface is in an invalid state during resize.
        resources.invalidate_intermediate_textures();

        if let RendererState::Ready { surface, core } = &target.state {
            surface.configure(&core.resources.device, &target.surface_config);
        }
    }

    pub fn set_subpixel_layout(&mut self, is_bgr: bool) {
        let mut target = self.shared.target.lock();
        target.is_bgr = is_bgr;
        if let Some(core) = target.core_mut() {
            core.is_bgr = is_bgr;
        }
    }

    pub fn update_transparency(&mut self, transparent: bool) {
        let mut guard = self.shared.target.lock();
        let target = &mut *guard;
        let new_alpha_mode = if transparent {
            target.transparent_alpha_mode
        } else {
            target.opaque_alpha_mode
        };
        if new_alpha_mode == target.surface_config.alpha_mode {
            return;
        }
        target.surface_config.alpha_mode = new_alpha_mode;
        let format = target.surface_config.format;

        let Some(core) = target.core_mut() else {
            return;
        };
        let resources = &mut core.resources;
        resources.pipelines = WgpuRendererCore::create_pipelines(
            &resources.device,
            &resources.bind_group_layouts,
            format,
            new_alpha_mode,
            core.rendering_params.path_sample_count,
            core.dual_source_blending,
            core.uses_webgl_instance_data,
        );

        if let RendererState::Ready { surface, core } = &target.state {
            surface.configure(&core.resources.device, &target.surface_config);
        }
    }

    #[allow(dead_code)]
    pub fn viewport_size(&self) -> Size<DevicePixels> {
        let target = self.shared.target.lock();
        Size {
            width: DevicePixels(target.surface_config.width as i32),
            height: DevicePixels(target.surface_config.height as i32),
        }
    }

    pub fn sprite_atlas(&self) -> &Arc<WgpuAtlas> {
        &self.atlas
    }

    pub fn supports_dual_source_blending(&self) -> bool {
        self.shared
            .target
            .lock()
            .core()
            .is_some_and(|core| core.dual_source_blending)
    }

    /// Returns `None` once GPU resources have been released by `destroy` or a pending
    /// device recovery.
    pub fn gpu_specs(&self) -> Option<GpuSpecs> {
        let target = self.shared.target.lock();
        let adapter_info = &target.core()?.adapter_info;
        Some(GpuSpecs {
            is_software_emulated: adapter_info.device_type == wgpu::DeviceType::Cpu,
            device_name: adapter_info.name.clone(),
            driver_name: adapter_info.driver.clone(),
            driver_info: adapter_info.driver_info.clone(),
        })
    }

    pub fn max_texture_size(&self) -> u32 {
        self.max_texture_size
    }

    /// Hands a finished frame to the render thread and returns without waiting for it.
    /// `true` means the frame is on its way, not that it's on screen. If it doesn't make it,
    /// the waker from [`set_feedback_waker`](Self::set_feedback_waker) is called and
    /// [`take_failed_frame`](Self::take_failed_frame) says so.
    pub fn draw(&mut self, scene: &Scene) -> bool {
        self.apply_render_feedback();
        if !self.surface_ready {
            return false;
        }

        let mut frame = self.shared.mailbox.take_spare().unwrap_or_default();
        scene.copy_primitives_into(&mut frame.scene);
        self.atlas.capture_frame(&mut frame.atlas);
        frame.surface_generation = self.shared.surface_generation.load(Ordering::Acquire);

        // Whatever failed before this frame, this one will put right.
        self.shared.frame_failed.store(false, Ordering::Release);

        #[cfg(target_family = "wasm")]
        {
            let shared = self.shared.clone();
            let presented = shared
                .target
                .lock()
                .render_packet(&mut frame, &mut Scene::default(), &shared);
            self.apply_render_feedback();
            frame.scene.clear();
            frame.atlas.reset();
            shared.mailbox.recycle(frame);
            presented
        }

        #[cfg(not(target_family = "wasm"))]
        {
            let replaced = self.shared.mailbox.post(frame, |older, newer| {
                newer.atlas.take_uploads_from(&mut older.atlas);
                older.scene.clear();
                older.atlas.reset();
            });
            if replaced {
                gpui::frame_stats::add(&gpui::frame_stats::RENDER_SUPERSEDED, 1);
            }
            true
        }
    }

    /// Sets what the render thread calls when the main thread should come and look:
    /// a frame didn't reach the screen, or a redraw is needed. It's called from the render
    /// thread, and only in those cases, never for a frame that went fine.
    pub fn set_feedback_waker(&mut self, waker: Box<dyn Fn() + Send + Sync>) {
        *self.shared.feedback_waker.lock() = Some(waker);
    }

    /// Sets what paces the frames the render thread makes by itself. Without one it won't.
    pub fn set_frame_pacer(&mut self, pacer: Box<dyn FramePacer>) {
        *self.shared.pacer.lock() = Some(pacer);
        self.has_pacer = true;
    }

    /// What wakes the render thread to move a scroll layer by itself, if it can do that.
    #[cfg(not(target_family = "wasm"))]
    pub fn scroll_compositor_waker(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let shared = self.shared.clone();
        self.has_pacer.then(|| Arc::new(move || shared.mailbox.nudge()) as Arc<dyn Fn() + Send + Sync>)
    }

    /// Whether a frame failed since the last [`draw`](Self::draw). Calling this clears it.
    pub fn take_failed_frame(&mut self) -> bool {
        self.apply_render_feedback();
        self.shared.frame_failed.swap(false, Ordering::AcqRel)
    }

    /// Whether a frame is waiting for the render thread, with one being drawn not counted.
    pub fn has_queued_frame(&self) -> bool {
        self.shared.mailbox.has_pending()
    }

    fn apply_render_feedback(&mut self) {
        if self.shared.thread_died.load(Ordering::Acquire) {
            panic!("The render thread died, see its panic above");
        }
        if self.shared.atlas_reset_wanted.swap(false, Ordering::AcqRel) {
            self.atlas.clear();
            self.needs_redraw = true;
        }
    }
}

impl WgpuRendererCore {
    fn new(
        context: &WgpuContext,
        target_format: wgpu::TextureFormat,
        alpha_mode: wgpu::CompositeAlphaMode,
    ) -> Self {
        let device = Arc::clone(&context.device);
        let queue = Arc::clone(&context.queue);
        let rendering_params = RenderingParameters::new(&context.adapter, target_format);
        let uses_webgl_instance_data = context.uses_webgl_instance_data();
        let dual_source_blending =
            context.supports_dual_source_blending() && !uses_webgl_instance_data;
        let bind_group_layouts = Self::create_bind_group_layouts(&device, uses_webgl_instance_data);
        let pipelines = Self::create_pipelines(
            &device,
            &bind_group_layouts,
            target_format,
            alpha_mode,
            rendering_params.path_sample_count,
            dual_source_blending,
            uses_webgl_instance_data,
        );
        let atlas_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("atlas_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let uniform_alignment = device.limits().min_uniform_buffer_offset_alignment as u64;
        let globals_size = std::mem::size_of::<GlobalParams>() as u64;
        let gamma_size = std::mem::size_of::<GammaParams>() as u64;
        let path_globals_offset = globals_size.next_multiple_of(uniform_alignment);
        let gamma_offset = (path_globals_offset + globals_size).next_multiple_of(uniform_alignment);
        let globals_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals_buffer"),
            size: gamma_offset + gamma_size,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let (
            instance_data,
            instance_data_capacity,
            max_instance_data_size,
            instance_data_alignment,
        ) = if uses_webgl_instance_data {
            let max_texture_dimension = device.limits().max_texture_dimension_2d;
            let max_instance_data_size = (u64::from(max_texture_dimension).pow(2)
                * INSTANCE_TEXTURE_TEXEL_SIZE)
                .min(MAX_INSTANCE_BUFFER_SIZE);
            let initial_capacity = (2 * 1024 * 1024).min(max_instance_data_size);
            let (instance_data, capacity) =
                Self::create_instance_texture(&device, initial_capacity, max_texture_dimension);
            (
                instance_data,
                capacity,
                max_instance_data_size,
                INSTANCE_TEXTURE_TEXEL_SIZE,
            )
        } else {
            let max_buffer_size = device
                .limits()
                .max_buffer_size
                .min(device.limits().max_storage_buffer_binding_size)
                .min(MAX_INSTANCE_BUFFER_SIZE);
            let initial_capacity = (2 * 1024 * 1024).min(max_buffer_size);
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("instance_buffer"),
                size: initial_capacity,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            (
                InstanceData::Storage(buffer),
                initial_capacity,
                max_buffer_size,
                device.limits().min_storage_buffer_offset_alignment as u64,
            )
        };
        let globals_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("globals_bind_group"),
            layout: &bind_group_layouts.globals,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &globals_buffer,
                        offset: 0,
                        size: NonZeroU64::new(globals_size),
                    }),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &globals_buffer,
                        offset: gamma_offset,
                        size: NonZeroU64::new(gamma_size),
                    }),
                },
            ],
        });
        let path_globals_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("path_globals_bind_group"),
            layout: &bind_group_layouts.globals,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &globals_buffer,
                        offset: path_globals_offset,
                        size: NonZeroU64::new(globals_size),
                    }),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &globals_buffer,
                        offset: gamma_offset,
                        size: NonZeroU64::new(gamma_size),
                    }),
                },
            ],
        });
        let max_texture_size = device.limits().max_texture_dimension_2d;

        Self {
            resources: WgpuResources {
                device,
                queue,
                pipelines,
                bind_group_layouts,
                atlas_sampler,
                atlas_texture_bind_groups: FxHashMap::default(),
                globals_buffer,
                globals_bind_group,
                path_globals_bind_group,
                instance_data,
                path_intermediate_texture: None,
                path_intermediate_view: None,
                path_msaa_texture: None,
                path_msaa_view: None,
            },
            path_globals_offset,
            gamma_offset,
            instance_data_capacity,
            max_instance_data_size,
            instance_data_alignment,
            uses_webgl_instance_data,
            rendering_params,
            is_bgr: false,
            dual_source_blending,
            adapter_info: context.adapter.get_info(),
            target_format,
            max_texture_size,
        }
    }

    fn resources(&self) -> &WgpuResources {
        &self.resources
    }

    fn resources_mut(&mut self) -> &mut WgpuResources {
        &mut self.resources
    }

    /// Path rendering goes through an intermediate texture that must match the frame
    /// target's dimensions, so the textures are keyed by size and rebuilt on mismatch.
    fn ensure_intermediate_textures(&mut self, size: Size<DevicePixels>) {
        let width = size.width.0 as u32;
        let height = size.height.0 as u32;
        if self
            .resources
            .path_intermediate_texture
            .as_ref()
            .is_some_and(|texture| texture.width() == width && texture.height() == height)
        {
            return;
        }

        let format = self.target_format;
        let path_sample_count = self.rendering_params.path_sample_count;
        let resources = &mut self.resources;

        let (texture, view) =
            Self::create_path_intermediate(&resources.device, format, width, height);
        resources.path_intermediate_texture = Some(texture);
        resources.path_intermediate_view = Some(view);

        let (path_msaa_texture, path_msaa_view) = Self::create_msaa_if_needed(
            &resources.device,
            format,
            width,
            height,
            path_sample_count,
        )
        .map(|(texture, view)| (Some(texture), Some(view)))
        .unwrap_or((None, None));
        resources.path_msaa_texture = path_msaa_texture;
        resources.path_msaa_view = path_msaa_view;
    }

    fn render_frame(
        &mut self,
        scene: &Scene,
        atlas_frame: &mut AtlasFrame,
        target_view: &wgpu::TextureView,
        size: Size<DevicePixels>,
        premultiplied_alpha: bool,
        clear_color: wgpu::Color,
    ) -> Result<wgpu::SubmissionIndex> {
        anyhow::ensure!(
            size.width.0 > 0 && size.height.0 > 0,
            "invalid render target size: {size:?}"
        );
        anyhow::ensure!(
            size.width.0 as u32 <= self.max_texture_size
                && size.height.0 as u32 <= self.max_texture_size,
            "render target size {size:?} exceeds maximum texture dimension {}",
            self.max_texture_size
        );

        atlas_frame.flush_uploads(&self.resources.queue);
        self.ensure_intermediate_textures(size);

        let gamma_params = GammaParams {
            gamma_ratios: self.rendering_params.gamma_ratios,
            grayscale_enhanced_contrast: self.rendering_params.grayscale_enhanced_contrast,
            subpixel_enhanced_contrast: self.rendering_params.subpixel_enhanced_contrast,
            is_bgr: self.is_bgr as u32,
            _pad: 0,
        };
        let globals = GlobalParams {
            viewport_size: [size.width.0 as f32, size.height.0 as f32],
            premultiplied_alpha: premultiplied_alpha as u32,
            pad: 0,
        };
        let path_globals = GlobalParams {
            premultiplied_alpha: 0,
            ..globals
        };
        self.resources.queue.write_buffer(
            &self.resources.globals_buffer,
            0,
            bytemuck::bytes_of(&globals),
        );
        self.resources.queue.write_buffer(
            &self.resources.globals_buffer,
            self.path_globals_offset,
            bytemuck::bytes_of(&path_globals),
        );
        self.resources.queue.write_buffer(
            &self.resources.globals_buffer,
            self.gamma_offset,
            bytemuck::bytes_of(&gamma_params),
        );

        self.record_frame(scene, atlas_frame, target_view, clear_color)
            .inspect_err(|_| {
                // Queue writes are staged before encoding; flush them even if the frame fails.
                self.resources.queue.submit(std::iter::empty());
            })
    }

    fn record_frame(
        &mut self,
        scene: &Scene,
        atlas_frame: &AtlasFrame,
        frame_view: &wgpu::TextureView,
        clear_color: wgpu::Color,
    ) -> Result<wgpu::SubmissionIndex> {
        let mut instance_offset = 0;
        let instance_bindings = self
            .write_instances(scene, &mut instance_offset)
            .with_context(|| {
                format!(
                    "scene too large: {} paths, {} shadows, {} quads, {} underlines, {} monochrome sprites, {} subpixel sprites, {} polychrome sprites",
                    scene.paths.len(),
                    scene.shadows.len(),
                    scene.quads.len(),
                    scene.underlines.len(),
                    scene.monochrome_sprites.len(),
                    scene.subpixel_sprites.len(),
                    scene.polychrome_sprites.len(),
                )
            })?;
        self.prepare_texture_bind_groups(scene, atlas_frame);

        let mut encoder =
            self.resources()
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("main_encoder"),
                });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("main_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: frame_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_color),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });

            for batch in scene.batches() {
                match batch {
                    PrimitiveBatch::Quads(range) => self.draw_instances(
                        &instance_bindings.quads,
                        &self.resources().pipelines.quads,
                        instance_range(range),
                        &mut pass,
                    ),
                    PrimitiveBatch::Shadows(range) => self.draw_instances(
                        &instance_bindings.shadows,
                        &self.resources().pipelines.shadows,
                        instance_range(range),
                        &mut pass,
                    ),
                    PrimitiveBatch::Paths(range) => {
                        let paths = &scene.paths[range];
                        if paths.is_empty() {
                            continue;
                        }

                        drop(pass);
                        let rasterized = self.draw_paths_to_intermediate(
                            &mut encoder,
                            paths,
                            &mut instance_offset,
                        )?;

                        pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: Some("main_pass_continued"),
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: frame_view,
                                resolve_target: None,
                                ops: wgpu::Operations {
                                    load: wgpu::LoadOp::Load,
                                    store: wgpu::StoreOp::Store,
                                },
                                depth_slice: None,
                            })],
                            depth_stencil_attachment: None,
                            ..Default::default()
                        });

                        if rasterized {
                            self.draw_paths_from_intermediate(
                                paths,
                                &mut instance_offset,
                                &mut pass,
                            )?;
                        }
                    }
                    PrimitiveBatch::Underlines(range) => self.draw_instances(
                        &instance_bindings.underlines,
                        &self.resources().pipelines.underlines,
                        instance_range(range),
                        &mut pass,
                    ),
                    PrimitiveBatch::MonochromeSprites { texture_id, range } => {
                        self.draw_sprites(
                            &instance_bindings.monochrome_sprites,
                            texture_id,
                            &self.resources().pipelines.mono_sprites,
                            instance_range(range),
                            &mut pass,
                        )?;
                    }
                    PrimitiveBatch::SubpixelSprites { texture_id, range } => {
                        let resources = self.resources();
                        self.draw_sprites(
                            &instance_bindings.subpixel_sprites,
                            texture_id,
                            resources
                                .pipelines
                                .subpixel_sprites
                                .as_ref()
                                .unwrap_or(&resources.pipelines.mono_sprites),
                            instance_range(range),
                            &mut pass,
                        )?;
                    }
                    PrimitiveBatch::PolychromeSprites { texture_id, range } => {
                        self.draw_sprites(
                            &instance_bindings.polychrome_sprites,
                            texture_id,
                            &self.resources().pipelines.poly_sprites,
                            instance_range(range),
                            &mut pass,
                        )?;
                    }
                    // Surfaces are macOS-only for video playback and are not
                    // implemented by the WGPU renderer.
                    PrimitiveBatch::Surfaces(_surfaces) => {}
                }
            }
        }

        let submission = self
            .resources()
            .queue
            .submit(std::iter::once(encoder.finish()));
        Ok(submission)
    }

    fn write_instances(
        &mut self,
        scene: &Scene,
        instance_offset: &mut u64,
    ) -> Result<InstanceBindings> {
        Ok(InstanceBindings {
            quads: self.write_instance_binding(
                "quads_bind_group",
                instance_offset,
                &scene.quads,
            )?,
            shadows: self.write_instance_binding(
                "shadows_bind_group",
                instance_offset,
                &scene.shadows,
            )?,
            underlines: self.write_instance_binding(
                "underlines_bind_group",
                instance_offset,
                &scene.underlines,
            )?,
            monochrome_sprites: self.write_instance_binding(
                "monochrome_sprites_bind_group",
                instance_offset,
                &scene.monochrome_sprites,
            )?,
            subpixel_sprites: self.write_instance_binding(
                "subpixel_sprites_bind_group",
                instance_offset,
                &scene.subpixel_sprites,
            )?,
            polychrome_sprites: self.write_instance_binding(
                "polychrome_sprites_bind_group",
                instance_offset,
                &scene.polychrome_sprites,
            )?,
        })
    }

    fn create_texture_bind_group(
        &self,
        label: &str,
        texture_view: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        let resources = self.resources();
        resources
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout: &resources.bind_group_layouts.texture,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(texture_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&resources.atlas_sampler),
                    },
                ],
            })
    }

    fn prepare_texture_bind_groups(&mut self, scene: &Scene, atlas_frame: &AtlasFrame) {
        let mut texture_ids = SmallVec::<[AtlasTextureId; 8]>::new();
        for batch in scene.batches() {
            let texture_id = match batch {
                PrimitiveBatch::MonochromeSprites { texture_id, .. }
                | PrimitiveBatch::SubpixelSprites { texture_id, .. }
                | PrimitiveBatch::PolychromeSprites { texture_id, .. } => texture_id,
                _ => continue,
            };
            if !texture_ids.contains(&texture_id) {
                texture_ids.push(texture_id);
            }
        }

        self.resources_mut()
            .atlas_texture_bind_groups
            .retain(|texture_id, _| texture_ids.contains(texture_id));

        for texture_id in texture_ids {
            let Some(texture_info) = atlas_frame.texture_info(texture_id) else {
                self.resources_mut()
                    .atlas_texture_bind_groups
                    .remove(&texture_id);
                continue;
            };
            let is_current = self
                .resources()
                .atlas_texture_bind_groups
                .get(&texture_id)
                .is_some_and(|cached| cached.texture_generation == texture_info.generation);
            if is_current {
                continue;
            }

            let bind_group =
                self.create_texture_bind_group("atlas_texture_bind_group", &texture_info.view);
            self.resources_mut().atlas_texture_bind_groups.insert(
                texture_id,
                CachedTextureBindGroup {
                    texture_generation: texture_info.generation,
                    bind_group,
                },
            );
        }
    }

    fn draw_instances(
        &self,
        instances: &InstanceBinding,
        pipeline: &wgpu::RenderPipeline,
        range: Range<u32>,
        pass: &mut wgpu::RenderPass<'_>,
    ) {
        if range.is_empty() {
            return;
        }
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &self.resources().globals_bind_group, &[]);
        pass.set_bind_group(1, &instances.bind_group, &[]);
        pass.draw(
            0..4,
            instances.first_instance + range.start..instances.first_instance + range.end,
        );
    }

    fn draw_sprites(
        &self,
        sprite_instances: &InstanceBinding,
        texture_id: AtlasTextureId,
        pipeline: &wgpu::RenderPipeline,
        range: Range<u32>,
        pass: &mut wgpu::RenderPass<'_>,
    ) -> Result<()> {
        if range.is_empty() {
            return Ok(());
        }
        let resources = self.resources();
        // The atlas has released this texture; the batch belongs to a stale
        // paint that will be replaced once its view re-renders.
        let Some(texture) = resources.atlas_texture_bind_groups.get(&texture_id) else {
            return Ok(());
        };
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &resources.globals_bind_group, &[]);
        pass.set_bind_group(1, &sprite_instances.bind_group, &[]);
        pass.set_bind_group(2, &texture.bind_group, &[]);
        pass.draw(
            0..4,
            sprite_instances.first_instance + range.start
                ..sprite_instances.first_instance + range.end,
        );
        Ok(())
    }

    unsafe fn instance_bytes<T>(instances: &[T]) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(
                instances.as_ptr() as *const u8,
                std::mem::size_of_val(instances),
            )
        }
    }

    fn draw_paths_from_intermediate(
        &mut self,
        paths: &[Path<ScaledPixels>],
        instance_offset: &mut u64,
        pass: &mut wgpu::RenderPass<'_>,
    ) -> Result<()> {
        let first_path = &paths[0];
        let sprites: Vec<PathSprite> = if paths.last().map(|p| &p.order) == Some(&first_path.order)
        {
            paths
                .iter()
                .map(|p| PathSprite {
                    bounds: p.clipped_bounds(),
                })
                .collect()
        } else {
            let mut bounds = first_path.clipped_bounds();
            for path in paths.iter().skip(1) {
                bounds = bounds.union(&path.clipped_bounds());
            }
            vec![PathSprite { bounds }]
        };

        let Some(path_intermediate_view) = self.resources().path_intermediate_view.clone() else {
            return Ok(());
        };
        let instances =
            self.write_instance_binding("path_sprites_bind_group", instance_offset, &sprites)?;
        let texture = self.create_texture_bind_group(
            "path_intermediate_texture_bind_group",
            &path_intermediate_view,
        );
        let resources = self.resources();
        pass.set_pipeline(&resources.pipelines.paths);
        pass.set_bind_group(0, &resources.globals_bind_group, &[]);
        pass.set_bind_group(1, &instances.bind_group, &[]);
        pass.set_bind_group(2, &texture, &[]);
        pass.draw(
            0..4,
            instances.first_instance..instances.first_instance + sprites.len() as u32,
        );
        Ok(())
    }

    fn draw_paths_to_intermediate(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        paths: &[Path<ScaledPixels>],
        instance_offset: &mut u64,
    ) -> Result<bool> {
        let mut vertices = Vec::new();
        for path in paths {
            let bounds = path.clipped_bounds();
            vertices.extend(path.vertices.iter().map(|v| PathRasterizationVertex {
                xy_position: v.xy_position,
                st_position: v.st_position,
                color: path.color,
                bounds,
            }));
        }

        if vertices.is_empty() {
            return Ok(false);
        }

        let vertex_binding = self.write_instance_binding(
            "path_rasterization_bind_group",
            instance_offset,
            &vertices,
        )?;

        let resources = self.resources();
        let Some(path_intermediate_view) = resources.path_intermediate_view.as_ref() else {
            return Ok(false);
        };

        let (target_view, resolve_target) = if let Some(ref msaa_view) = resources.path_msaa_view {
            (msaa_view, Some(path_intermediate_view))
        } else {
            (path_intermediate_view, None)
        };

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("path_rasterization_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
                    resolve_target,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });

            pass.set_pipeline(&resources.pipelines.path_rasterization);
            pass.set_bind_group(0, &resources.path_globals_bind_group, &[]);
            pass.set_bind_group(1, &vertex_binding.bind_group, &[]);
            // The path rasterization shader loads records by vertex index
            // rather than instance index, so the allocation's base shifts the
            // vertex range here.
            pass.draw(
                vertex_binding.first_instance
                    ..vertex_binding.first_instance + vertices.len() as u32,
                0..1,
            );
        }

        Ok(true)
    }

    fn write_instance_binding<T>(
        &mut self,
        label: &str,
        instance_offset: &mut u64,
        instances: &[T],
    ) -> Result<InstanceBinding> {
        let data = unsafe { Self::instance_bytes(instances) };
        // wgpu rejects zero-sized bindings, so empty primitive arrays still
        // reserve the 16-byte minimum.
        let size = (data.len() as u64).max(16);
        let stride = (std::mem::size_of::<T>() as u64).max(1);
        let (alignment, allocation_size) = if self.uses_webgl_instance_data {
            // The texture transport has no binding offset: the shader indexes
            // the instance texture absolutely, so each allocation must start on
            // a whole instance (a stride multiple) and a whole texel, and must
            // end on a texel boundary so the zero padding of its final partial
            // texel cannot overlap the next allocation.
            (
                least_common_multiple(self.instance_data_alignment, stride),
                size.next_multiple_of(INSTANCE_TEXTURE_TEXEL_SIZE),
            )
        } else {
            (self.instance_data_alignment.max(1), size)
        };
        let mut offset = (*instance_offset).next_multiple_of(alignment);
        if offset + allocation_size > self.instance_data_capacity {
            self.grow_instance_data(allocation_size)?;
            offset = 0;
        }
        *instance_offset = offset + allocation_size;

        let first_instance = if self.uses_webgl_instance_data {
            u32::try_from(offset / stride).context("instance index exceeds u32 range")?
        } else {
            0
        };

        let resources = self.resources();
        if !data.is_empty() {
            match &resources.instance_data {
                InstanceData::Storage(buffer) => resources.queue.write_buffer(buffer, offset, data),
                InstanceData::Texture { .. } => {
                    Self::write_instance_texture(resources, offset, data)
                }
            }
        }
        let bind_group = resources
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout: &resources.bind_group_layouts.instances,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: match &resources.instance_data {
                        InstanceData::Storage(buffer) => {
                            wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                buffer,
                                offset,
                                size: NonZeroU64::new(size),
                            })
                        }
                        InstanceData::Texture { view, .. } => {
                            wgpu::BindingResource::TextureView(view)
                        }
                    },
                }],
            });
        Ok(InstanceBinding {
            bind_group,
            first_instance,
        })
    }

    fn write_instance_texture(resources: &WgpuResources, offset: u64, data: &[u8]) {
        let InstanceData::Texture {
            texture,
            width,
            height,
            ..
        } = &resources.instance_data
        else {
            return;
        };
        let mut byte_offset = 0usize;
        let mut texel_offset = offset / INSTANCE_TEXTURE_TEXEL_SIZE;
        while byte_offset < data.len() {
            let x = (texel_offset % u64::from(*width)) as u32;
            let y = (texel_offset / u64::from(*width)) as u32;
            if y >= *height {
                // The capacity check in write_instance_binding should make this
                // unreachable. Truncating silently would leave stale bytes in the
                // texture and draw garbage for the remaining instances.
                debug_assert!(
                    false,
                    "instance texture write out of bounds: row {y} >= height {}",
                    *height
                );
                log::error!(
                    "instance texture write out of bounds; dropping {} bytes of instance data",
                    data.len() - byte_offset
                );
                return;
            }
            let available_texels = u64::from(*width - x);
            let remaining_bytes = data.len() - byte_offset;
            let complete_texels = remaining_bytes as u64 / INSTANCE_TEXTURE_TEXEL_SIZE;
            let texels = complete_texels.min(available_texels);
            if texels > 0 {
                let byte_count = (texels * INSTANCE_TEXTURE_TEXEL_SIZE) as usize;
                resources.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d { x, y, z: 0 },
                        aspect: wgpu::TextureAspect::All,
                    },
                    &data[byte_offset..byte_offset + byte_count],
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(byte_count as u32),
                        rows_per_image: None,
                    },
                    wgpu::Extent3d {
                        width: texels as u32,
                        height: 1,
                        depth_or_array_layers: 1,
                    },
                );
                byte_offset += byte_count;
                texel_offset += texels;
                continue;
            }

            let mut final_texel = [0; INSTANCE_TEXTURE_TEXEL_SIZE as usize];
            final_texel[..remaining_bytes].copy_from_slice(&data[byte_offset..]);
            resources.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x, y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                &final_texel,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(INSTANCE_TEXTURE_TEXEL_SIZE as u32),
                    rows_per_image: None,
                },
                wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
            );
            break;
        }
    }

    fn grow_instance_data(&mut self, required: u64) -> Result<()> {
        let capacity = (self.instance_data_capacity * 2)
            .max(required.next_power_of_two())
            .min(self.max_instance_data_size);
        anyhow::ensure!(
            capacity >= required,
            "instance data needs {required} bytes, above the maximum of {}",
            self.max_instance_data_size
        );
        anyhow::ensure!(
            capacity > self.instance_data_capacity,
            "frame instance data exceeds the {}-byte maximum",
            self.max_instance_data_size
        );
        log::debug!(
            "instance data grown from {} to {capacity}",
            self.instance_data_capacity
        );
        // Bind groups created earlier in the frame keep the previous buffer or
        // texture alive, so allocations written before the grow remain valid;
        // only subsequent writes land in the new allocation.
        let uses_webgl_instance_data = self.uses_webgl_instance_data;
        let resources = self.resources_mut();
        if uses_webgl_instance_data {
            let max_texture_dimension = resources.device.limits().max_texture_dimension_2d;
            let (instance_data, actual_capacity) =
                Self::create_instance_texture(&resources.device, capacity, max_texture_dimension);
            resources.instance_data = instance_data;
            self.instance_data_capacity = actual_capacity;
        } else {
            resources.instance_data =
                InstanceData::Storage(resources.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("instance_buffer"),
                    size: capacity,
                    usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }));
            self.instance_data_capacity = capacity;
        }
        Ok(())
    }
}

impl WgpuRenderer {
    /// Mark the surface as unconfigured so rendering is skipped until a new
    /// surface is provided via [`replace_surface`](Self::replace_surface).
    ///
    /// This does **not** drop the renderer — the device, queue, atlas, and
    /// pipelines stay alive.  Use this when the native window is destroyed
    /// (e.g. Android `TerminateWindow`) but you intend to re-create the
    /// surface later without losing cached atlas textures.
    pub fn unconfigure_surface(&mut self) {
        self.surface_ready = false;
        let mut target = self.shared.target.lock();
        self.drop_queued_frame(&mut target);
        target.state = match std::mem::replace(&mut target.state, RendererState::Released) {
            RendererState::Ready { core, surface } => {
                drop(surface);
                RendererState::Unconfigured { core }
            }
            state @ (RendererState::Unconfigured { .. } | RendererState::Released) => state,
        };
        // Drop intermediate textures since they reference the old surface size.
        if let Some(core) = target.core_mut() {
            core.resources.invalidate_intermediate_textures();
        }
    }

    /// Takes a frame back before it's drawn, keeping its uploads because the atlas lives on.
    fn drop_queued_frame(&self, target: &mut RenderTarget) {
        if let Some(mut frame) = self.shared.mailbox.take_pending() {
            target.flush_uploads(&mut frame);
        }
    }

    /// Replace the wgpu surface with a new one (e.g. after Android destroys
    /// and recreates the native window).  Keeps the device, queue, atlas, and
    /// all pipelines intact so cached `AtlasTextureId`s remain valid.
    ///
    /// The `instance` **must** be the same [`wgpu::Instance`] that was used to
    /// create the adapter and device (i.e. from the [`WgpuContext`]).  Using a
    /// different instance will cause a "Device does not exist" panic because
    /// the wgpu device is bound to its originating instance.
    #[cfg(not(target_family = "wasm"))]
    pub fn replace_surface<W: HasWindowHandle>(
        &mut self,
        window: &W,
        config: WgpuSurfaceConfig,
        instance: &wgpu::Instance,
    ) -> anyhow::Result<()> {
        let window_handle = window
            .window_handle()
            .map_err(|e| anyhow::anyhow!("Failed to get window handle: {e}"))?;

        let surface = create_surface(instance, window_handle.as_raw())?;

        let width = (config.size.width.0 as u32).max(1);
        let height = (config.size.height.0 as u32).max(1);

        let mut guard = self.shared.target.lock();
        let target = &mut *guard;
        self.drop_queued_frame(target);
        self.shared
            .surface_generation
            .fetch_add(1, Ordering::AcqRel);

        let alpha_mode = if config.transparent {
            target.transparent_alpha_mode
        } else {
            target.opaque_alpha_mode
        };

        target.surface_config.width = width;
        target.surface_config.height = height;
        target.surface_config.alpha_mode = alpha_mode;
        if let Some(mode) = config.preferred_present_mode {
            target.surface_config.present_mode = mode;
        }

        let mut core = match std::mem::replace(&mut target.state, RendererState::Released) {
            RendererState::Ready {
                core,
                surface: old_surface,
            } => {
                drop(old_surface);
                core
            }
            RendererState::Unconfigured { core } => core,
            RendererState::Released => {
                self.surface_ready = false;
                anyhow::bail!("Cannot replace the surface: GPU resources have been released")
            }
        };
        surface.configure(&core.resources.device, &target.surface_config);
        core.resources.invalidate_intermediate_textures();
        target.state = RendererState::Ready { surface, core };
        self.surface_ready = true;

        Ok(())
    }

    pub fn destroy(&mut self) {
        // Release surface-bound GPU resources eagerly so the underlying native
        // window can be destroyed before the renderer itself is dropped.
        self.surface_ready = false;
        self.stop_render_thread();
        self.shared.target.lock().state = RendererState::Released;
    }

    /// Returns once the render thread is gone. Nothing it could be waiting for needs the
    /// main thread, so this can't hang on it.
    fn stop_render_thread(&mut self) {
        self.shared.mailbox.close();
        #[cfg(not(target_family = "wasm"))]
        if let Some(thread) = self.render_thread.take() {
            // If it panicked, that panic has already been reported.
            thread.join().ok();
        }
    }

    /// Returns true if the GPU device was lost and recovery is needed.
    pub fn device_lost(&self) -> bool {
        self.device_errors.device_lost()
    }

    /// Returns true if a redraw is needed because GPU state was cleared.
    /// Calling this method clears the flag.
    pub fn needs_redraw(&mut self) -> bool {
        self.apply_render_feedback();
        std::mem::take(&mut self.needs_redraw)
    }

    /// Recovers from a lost GPU device by recreating the renderer with a new context.
    ///
    /// Call this after detecting `device_lost()` returns true.
    ///
    /// This method coordinates recovery across multiple windows:
    /// - The first window to call this will recreate the shared context
    /// - Subsequent windows will adopt the already-recovered context
    #[cfg(not(target_family = "wasm"))]
    pub fn recover<W>(&mut self, window: &W) -> anyhow::Result<()>
    where
        W: HasWindowHandle + HasDisplayHandle + std::fmt::Debug + Send + Sync + Clone + 'static,
    {
        let gpu_context = self.context.as_ref().expect("recover requires gpu_context");

        // Rustcord: the frame waiting for the render thread was meant for the lost device,
        // and so are its uploads. The render thread stays, it finds no work until the first
        // frame for the new device.
        self.shared.mailbox.take_pending();

        // Check if another window already recovered the context
        let needs_new_context = gpu_context
            .borrow()
            .as_ref()
            .is_none_or(|ctx| ctx.device_lost());

        let window_handle = window
            .window_handle()
            .map_err(|e| anyhow::anyhow!("Failed to get window handle: {e}"))?;

        let surface = if needs_new_context {
            log::warn!("GPU device lost, recreating context...");

            // Drop old resources to release Arc<Device>/Arc<Queue> and GPU resources
            self.surface_ready = false;
            self.shared.target.lock().state = RendererState::Released;
            *gpu_context.borrow_mut() = None;

            // Wait briefly for the GPU driver to stabilize, then try to
            // recreate the context without software renderers. If this fails
            // the caller should request another frame and retry — the real GPU
            // may need more time to come back (e.g. after suspend/resume).
            std::thread::sleep(std::time::Duration::from_millis(350));

            let instance = WgpuContext::instance(Some(Box::new(window.clone())));
            let surface = create_surface(&instance, window_handle.as_raw())?;
            let new_context =
                WgpuContext::new_rejecting_software(instance, &surface, self.compositor_gpu)?;
            *gpu_context.borrow_mut() = Some(new_context);
            surface
        } else {
            let ctx_ref = gpu_context.borrow();
            let instance = &ctx_ref.as_ref().unwrap().instance;
            create_surface(instance, window_handle.as_raw())?
        };

        let gpu_context = Rc::clone(gpu_context);
        let ctx_ref = gpu_context.borrow();
        let context = ctx_ref.as_ref().expect("context should exist");

        let mut target = self.shared.target.lock();
        let config = WgpuSurfaceConfig {
            size: gpui::Size {
                width: gpui::DevicePixels(target.surface_config.width as i32),
                height: gpui::DevicePixels(target.surface_config.height as i32),
            },
            transparent: target.surface_config.alpha_mode != wgpu::CompositeAlphaMode::Opaque,
            preferred_present_mode: Some(target.surface_config.present_mode),
        };
        let is_bgr = target.is_bgr;

        self.surface_ready = false;
        target.state = RendererState::Released;
        self.atlas.handle_device_lost(context);

        let mut recovered = RenderTarget::new(context, surface, config)?;
        recovered.is_bgr = is_bgr;
        if let Some(core) = recovered.core_mut() {
            core.is_bgr = is_bgr;
        }
        *target = recovered;
        self.max_texture_size = context.device.limits().max_texture_dimension_2d;
        self.device_errors = Arc::clone(context.errors());
        self.surface_ready = true;
        self.shared
            .surface_generation
            .fetch_add(1, Ordering::AcqRel);
        self.shared.frame_failed.store(false, Ordering::Release);
        self.shared.atlas_reset_wanted.store(false, Ordering::Release);

        log::info!("GPU recovery complete");
        Ok(())
    }
}

impl Drop for WgpuRenderer {
    fn drop(&mut self) {
        self.stop_render_thread();
    }
}

#[cfg(all(
    not(target_family = "wasm"),
    any(test, feature = "bench-support", feature = "test-support")
))]
struct HeadlessRenderTarget {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

#[cfg(all(
    not(target_family = "wasm"),
    any(test, feature = "bench-support", feature = "test-support")
))]
impl HeadlessRenderTarget {
    fn size(&self) -> Size<DevicePixels> {
        Size {
            width: DevicePixels(self.texture.width() as i32),
            height: DevicePixels(self.texture.height() as i32),
        }
    }
}

#[cfg(all(
    not(target_family = "wasm"),
    any(test, feature = "bench-support", feature = "test-support")
))]
pub struct WgpuHeadlessRenderer {
    context: WgpuContext,
    core: WgpuRendererCore,
    atlas: Arc<WgpuAtlas>,
    render_target: Option<HeadlessRenderTarget>,
    observed_error_generation: u64,
}

#[cfg(all(
    not(target_family = "wasm"),
    any(test, feature = "bench-support", feature = "test-support")
))]
impl WgpuHeadlessRenderer {
    pub fn new() -> anyhow::Result<Self> {
        // Rustcord: the Vulkan loader crashes in the driver's setup when two threads start it at
        // once, which parallel tests would otherwise do now and then.
        static STARTING: Mutex<()> = Mutex::new(());
        let (context, target_format) = {
            let _starting = STARTING.lock();
            WgpuContext::new_headless()?
        };
        let atlas = Arc::new(WgpuAtlas::from_context(&context));
        let core =
            WgpuRendererCore::new(&context, target_format, wgpu::CompositeAlphaMode::Opaque);

        Ok(Self {
            context,
            core,
            atlas,
            render_target: None,
            observed_error_generation: 0,
        })
    }

    fn ensure_render_target(&mut self, size: Size<DevicePixels>) -> anyhow::Result<()> {
        anyhow::ensure!(
            size.width.0 > 0 && size.height.0 > 0,
            "invalid headless render target size: {size:?}"
        );
        anyhow::ensure!(
            size.width.0 as u32 <= self.core.max_texture_size
                && size.height.0 as u32 <= self.core.max_texture_size,
            "headless render target size {size:?} exceeds maximum texture dimension {}",
            self.core.max_texture_size
        );
        if self
            .render_target
            .as_ref()
            .is_some_and(|target| target.size() == size)
        {
            return Ok(());
        }

        let texture = self
            .core
            .resources
            .device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("headless_render_target"),
                size: wgpu::Extent3d {
                    width: size.width.0 as u32,
                    height: size.height.0 as u32,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: self.core.target_format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.render_target = Some(HeadlessRenderTarget { texture, view });
        Ok(())
    }

    fn check_gpu_errors(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.context.device_lost(),
            "GPU device was lost during headless rendering"
        );
        if let Some(error) = self
            .context
            .errors()
            .observe_error(&mut self.observed_error_generation)
        {
            anyhow::bail!("GPU error during headless rendering: {error}");
        }
        Ok(())
    }

    fn render(&mut self, scene: &Scene, size: Size<DevicePixels>) -> anyhow::Result<()> {
        self.check_gpu_errors()?;
        self.ensure_render_target(size)?;
        let view = self
            .render_target
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Headless render target was not created"))?
            .view
            .clone();
        let mut atlas_frame = AtlasFrame::default();
        self.atlas.capture_frame(&mut atlas_frame);
        let mut composed = Scene::default();
        scene.copy_primitives_into(&mut composed);
        composed.compose_scroll_layers();
        self.core.render_frame(
            &composed,
            &mut atlas_frame,
            &view,
            size,
            false,
            wgpu::Color::BLACK,
        )?;
        Ok(())
    }

    /// Copies the current render target back to the CPU. Dimensions come from the
    /// target texture itself, so the copy can never disagree with what was rendered.
    fn read_image(&mut self) -> anyhow::Result<image::RgbaImage> {
        let target = self
            .render_target
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Headless render target was not created"))?;
        let width = target.texture.width();
        let height = target.texture.height();
        let bytes_per_row = width
            .checked_mul(4)
            .ok_or_else(|| anyhow::anyhow!("Headless render target row size overflowed"))?;
        let padded_bytes_per_row = bytes_per_row
            .checked_next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            .ok_or_else(|| anyhow::anyhow!("Headless padded row size overflowed"))?;
        let buffer_size = u64::from(padded_bytes_per_row)
            .checked_mul(u64::from(height))
            .ok_or_else(|| anyhow::anyhow!("Headless readback buffer size overflowed"))?;
        anyhow::ensure!(
            buffer_size <= self.core.resources.device.limits().max_buffer_size,
            "Headless readback buffer size {buffer_size} exceeds maximum buffer size {}",
            self.core.resources.device.limits().max_buffer_size
        );
        let readback_buffer = self
            .core
            .resources
            .device
            .create_buffer(&wgpu::BufferDescriptor {
                label: Some("headless_readback_buffer"),
                size: buffer_size,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
        let mut encoder =
            self.core
                .resources
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("headless_readback_encoder"),
                });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback_buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        let submission = self
            .core
            .resources
            .queue
            .submit(std::iter::once(encoder.finish()));
        let (sender, receiver) = std::sync::mpsc::channel();
        readback_buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                if sender.send(result).is_err() {
                    log::error!("Headless readback receiver was dropped before mapping completed");
                }
            });
        self.core
            .resources
            .device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(std::time::Duration::from_secs(30)),
            })
            .map_err(|error| anyhow::anyhow!("Failed to wait for headless rendering: {error}"))?;
        receiver
            .recv_timeout(std::time::Duration::from_secs(30))
            .map_err(|error| anyhow::anyhow!("Failed to receive headless mapping result: {error}"))?
            .map_err(|error| anyhow::anyhow!("Failed to map headless readback buffer: {error}"))?;
        self.check_gpu_errors()?;

        let mapped_data = readback_buffer.slice(..).get_mapped_range();
        let pixel_capacity = usize::try_from(u64::from(bytes_per_row) * u64::from(height))
            .map_err(|_| anyhow::anyhow!("Headless image size exceeds addressable memory"))?;
        let mut pixels = Vec::with_capacity(pixel_capacity);
        for row in mapped_data
            .chunks_exact(padded_bytes_per_row as usize)
            .take(height as usize)
        {
            pixels.extend_from_slice(&row[..bytes_per_row as usize]);
        }
        drop(mapped_data);
        readback_buffer.unmap();

        if self.core.target_format == wgpu::TextureFormat::Bgra8Unorm {
            for pixel in pixels.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
        }

        image::RgbaImage::from_raw(width, height, pixels)
            .ok_or_else(|| anyhow::anyhow!("Failed to create image from headless pixel data"))
    }
}

#[cfg(all(
    not(target_family = "wasm"),
    any(test, feature = "bench-support", feature = "test-support")
))]
impl gpui::PlatformHeadlessRenderer for WgpuHeadlessRenderer {
    fn render_scene_to_image(
        &mut self,
        scene: &Scene,
        size: Size<DevicePixels>,
    ) -> anyhow::Result<image::RgbaImage> {
        self.render(scene, size)?;
        self.read_image()
    }

    fn render_scene(&mut self, scene: &Scene, size: Size<DevicePixels>) -> anyhow::Result<()> {
        self.render(scene, size)
    }

    fn sprite_atlas(&self) -> Arc<dyn gpui::PlatformAtlas> {
        self.atlas.clone()
    }
}

fn instance_range(range: Range<usize>) -> Range<u32> {
    range.start as u32..range.end as u32
}

#[cfg(not(target_family = "wasm"))]
fn create_surface(
    instance: &wgpu::Instance,
    raw_window_handle: raw_window_handle::RawWindowHandle,
) -> anyhow::Result<wgpu::Surface<'static>> {
    unsafe {
        instance
            .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                // Fall back to the display handle already provided via InstanceDescriptor::display.
                raw_display_handle: None,
                raw_window_handle,
            })
            .map_err(|e| anyhow::anyhow!("{e}"))
    }
}

struct RenderingParameters {
    path_sample_count: u32,
    gamma_ratios: [f32; 4],
    grayscale_enhanced_contrast: f32,
    subpixel_enhanced_contrast: f32,
}

impl RenderingParameters {
    fn new(adapter: &wgpu::Adapter, surface_format: wgpu::TextureFormat) -> Self {
        use std::env;

        let format_features = adapter.get_texture_format_features(surface_format);
        let path_sample_count = [4, 2, 1]
            .into_iter()
            .find(|&n| format_features.flags.sample_count_supported(n))
            .unwrap_or(1);

        let gamma = env::var("ZED_FONTS_GAMMA")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1.8_f32)
            .clamp(1.0, 2.2);
        let gamma_ratios = get_gamma_correction_ratios(gamma);

        let grayscale_enhanced_contrast = env::var("ZED_FONTS_GRAYSCALE_ENHANCED_CONTRAST")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1.0_f32)
            .max(0.0);

        let subpixel_enhanced_contrast = env::var("ZED_FONTS_SUBPIXEL_ENHANCED_CONTRAST")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.5_f32)
            .max(0.0);

        Self {
            path_sample_count,
            gamma_ratios,
            grayscale_enhanced_contrast,
            subpixel_enhanced_contrast,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        BorderStyle, ColorSpace, ContentMask, Corners, Edges, Hsla, MonochromeSprite,
        PolychromeSprite, Quad, Shadow, Size, SubpixelSprite, Underline, linear_color_stop,
        linear_gradient,
    };
    #[cfg(target_os = "linux")]
    use gpui::{DevicePixels, PlatformHeadlessRenderer, Scene};

    #[cfg(target_os = "linux")]
    fn device_size(width: i32, height: i32) -> Size<DevicePixels> {
        Size {
            width: DevicePixels(width),
            height: DevicePixels(height),
        }
    }

    #[cfg(target_os = "linux")]
    fn solid_quad(x: f32, y: f32, width: f32, height: f32, color: Hsla) -> Quad {
        let bounds = Bounds {
            origin: Point {
                x: x.into(),
                y: y.into(),
            },
            size: Size {
                width: width.into(),
                height: height.into(),
            },
        };
        Quad {
            order: 0,
            border_style: BorderStyle::Solid,
            bounds,
            content_mask: ContentMask { bounds },
            background: color.into(),
            border_color: color,
            corner_radii: Corners::default(),
            border_widths: Edges::default(),
        }
    }

    /// Channels are compared with a small tolerance so the assertions hold across
    /// drivers without pinning exact rasterizer output.
    #[cfg(target_os = "linux")]
    fn assert_pixel(image: &image::RgbaImage, x: u32, y: u32, expected: [u8; 4]) {
        let actual = image.get_pixel(x, y).0;
        assert!(
            actual
                .iter()
                .zip(expected)
                .all(|(actual, expected)| actual.abs_diff(expected) <= 3),
            "pixel ({x}, {y}) was {actual:?}, expected {expected:?}"
        );
    }

    #[cfg(target_os = "linux")]
    const RED: [u8; 4] = [255, 0, 0, 255];
    #[cfg(target_os = "linux")]
    const BLUE: [u8; 4] = [0, 0, 255, 255];
    #[cfg(target_os = "linux")]
    const BLACK: [u8; 4] = [0, 0, 0, 255];

    #[cfg(target_os = "linux")]
    #[test]
    fn headless_renderer_draws_quads_with_distinct_colors() -> anyhow::Result<()> {
        let mut renderer = WgpuHeadlessRenderer::new()?;
        let mut scene = Scene::default();
        scene.insert_primitive(solid_quad(0.0, 0.0, 32.0, 32.0, gpui::red()));
        scene.insert_primitive(solid_quad(32.0, 0.0, 32.0, 32.0, gpui::blue()));
        scene.finish();

        let image = renderer.render_scene_to_image(&scene, device_size(64, 32))?;
        assert_eq!(image.dimensions(), (64, 32));
        assert_pixel(&image, 8, 16, RED);
        assert_pixel(&image, 24, 16, RED);
        assert_pixel(&image, 40, 16, BLUE);
        assert_pixel(&image, 56, 16, BLUE);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    const SPRITE_SIDE: i32 = 300;

    /// Puts a picture of one colour in the atlas, `pixel` being in the atlas's BGRA order.
    #[cfg(target_os = "linux")]
    fn insert_picture(
        atlas: &WgpuAtlas,
        image_id: usize,
        pixel: [u8; 4],
    ) -> (gpui::AtlasKey, gpui::AtlasTile) {
        use gpui::PlatformAtlas as _;
        let key = gpui::AtlasKey::Image(gpui::RenderImageParams {
            image_id: gpui::ImageId(image_id),
            frame_index: 0,
        });
        let bytes = pixel.repeat((SPRITE_SIDE * SPRITE_SIDE) as usize);
        let tile = atlas
            .get_or_insert_with(key.clone(), &mut || {
                Ok(Some((
                    device_size(SPRITE_SIDE, SPRITE_SIDE),
                    std::borrow::Cow::Owned(bytes.clone()),
                )))
            })
            .expect("allocation should succeed")
            .expect("callback returns Some");
        (key, tile)
    }

    #[cfg(target_os = "linux")]
    fn scene_with_sprite(tile: gpui::AtlasTile) -> Scene {
        let bounds = Bounds {
            origin: Point::default(),
            size: Size {
                width: (SPRITE_SIDE as f32).into(),
                height: (SPRITE_SIDE as f32).into(),
            },
        };
        let mut scene = Scene::default();
        scene.insert_primitive(PolychromeSprite {
            order: 0,
            pad: 0,
            grayscale: false.into(),
            opacity: 1.0,
            bounds,
            content_mask: ContentMask { bounds },
            corner_radii: Corners::default(),
            tile,
        });
        scene.finish();
        scene
    }

    /// Draws the way the render thread does: with the atlas as the frame captured it, not as
    /// it is by now.
    #[cfg(target_os = "linux")]
    fn render_frame_to_image(
        renderer: &mut WgpuHeadlessRenderer,
        scene: &Scene,
        atlas_frame: &mut AtlasFrame,
    ) -> anyhow::Result<image::RgbaImage> {
        let size = device_size(SPRITE_SIDE, SPRITE_SIDE);
        renderer.ensure_render_target(size)?;
        let view = renderer.render_target.as_ref().unwrap().view.clone();
        renderer
            .core
            .render_frame(scene, atlas_frame, &view, size, false, wgpu::Color::BLACK)?;
        renderer.read_image()
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn copied_scene_draws_the_same_and_buffers_get_reused() -> anyhow::Result<()> {
        let mut renderer = WgpuHeadlessRenderer::new()?;
        let mut scene = Scene::default();
        scene.insert_primitive(solid_quad(0.0, 0.0, 32.0, 32.0, gpui::red()));
        scene.insert_primitive(solid_quad(8.0, 8.0, 16.0, 16.0, gpui::blue()));
        scene.finish();

        let mut copy = Scene::default();
        scene.copy_primitives_into(&mut copy);
        let size = device_size(32, 32);
        let original = renderer.render_scene_to_image(&scene, size)?;
        let copied = renderer.render_scene_to_image(&copy, size)?;
        assert_eq!(original.as_raw(), copied.as_raw());

        let quads = copy.quads.as_ptr();
        scene.copy_primitives_into(&mut copy);
        assert_eq!(copy.quads.as_ptr(), quads);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn frame_shows_its_own_picture_after_the_next_paint_replaced_it() -> anyhow::Result<()> {
        let mut renderer = WgpuHeadlessRenderer::new()?;
        let atlas = renderer.atlas.clone();

        let (red_key, red_tile) = insert_picture(&atlas, 1, [0, 0, 255, 255]);
        let red_scene = scene_with_sprite(red_tile);
        let mut red_frame = AtlasFrame::default();
        atlas.capture_frame(&mut red_frame);

        // While that frame waits for the render thread, the main thread paints on: the picture
        // goes and another takes the same texture id.
        gpui::PlatformAtlas::remove(&*atlas, &red_key);
        let (_, blue_tile) = insert_picture(&atlas, 2, [255, 0, 0, 255]);
        assert_eq!(blue_tile.texture_id, red_tile.texture_id);
        let blue_scene = scene_with_sprite(blue_tile);
        let mut blue_frame = AtlasFrame::default();
        atlas.capture_frame(&mut blue_frame);

        let image = render_frame_to_image(&mut renderer, &red_scene, &mut red_frame)?;
        assert_pixel(&image, 150, 150, RED);
        let image = render_frame_to_image(&mut renderer, &blue_scene, &mut blue_frame)?;
        assert_pixel(&image, 150, 150, BLUE);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn frame_drawn_instead_of_another_gets_its_uploads() -> anyhow::Result<()> {
        let mut renderer = WgpuHeadlessRenderer::new()?;
        let atlas = renderer.atlas.clone();

        let (_, tile) = insert_picture(&atlas, 1, [0, 0, 255, 255]);
        let mut replaced = AtlasFrame::default();
        atlas.capture_frame(&mut replaced);

        // The next paint uploads nothing, but its scene still uses that tile.
        let scene = scene_with_sprite(tile);
        let mut drawn = AtlasFrame::default();
        atlas.capture_frame(&mut drawn);
        drawn.take_uploads_from(&mut replaced);

        let image = render_frame_to_image(&mut renderer, &scene, &mut drawn)?;
        assert_pixel(&image, 150, 150, RED);
        Ok(())
    }

    /// The main thread paints as fast as it can, swapping a picture in the atlas every frame,
    /// while another thread draws what comes through the mailbox the way the render thread
    /// does. Every frame that gets drawn must show its own picture, whatever was replaced or
    /// reused in the meantime, and nothing may hang or upset wgpu's validation.
    #[cfg(target_os = "linux")]
    #[test]
    fn frames_drawn_on_another_thread_show_their_own_pictures() -> anyhow::Result<()> {
        struct TestFrame {
            scene: Scene,
            atlas: AtlasFrame,
            expected: [u8; 4],
            number: u32,
        }
        const FRAMES: u32 = 300;

        let mut renderer = WgpuHeadlessRenderer::new()?;
        let atlas = renderer.atlas.clone();
        let mailbox = Arc::new(FrameMailbox::<TestFrame>::new());
        let drawing = {
            let mailbox = mailbox.clone();
            std::thread::spawn(move || -> anyhow::Result<(WgpuHeadlessRenderer, u32)> {
                let mut drawn = 0;
                while let Some(mut frame) = mailbox.wait() {
                    let image =
                        render_frame_to_image(&mut renderer, &frame.scene, &mut frame.atlas)?;
                    assert_pixel(&image, 150, 150, frame.expected);
                    renderer.check_gpu_errors()?;
                    drawn += 1;
                    if frame.number == FRAMES - 1 {
                        break;
                    }
                }
                Ok((renderer, drawn))
            })
        };

        let mut previous = None;
        for number in 0..FRAMES {
            if let Some(key) = previous.take() {
                gpui::PlatformAtlas::remove(&*atlas, &key);
            }
            let (pixel, expected) = if number % 2 == 0 {
                ([0, 0, 255, 255], RED)
            } else {
                ([255, 0, 0, 255], BLUE)
            };
            let (key, tile) = insert_picture(&atlas, number as usize, pixel);
            previous = Some(key);

            let mut frame = TestFrame {
                scene: scene_with_sprite(tile),
                atlas: AtlasFrame::default(),
                expected,
                number,
            };
            atlas.capture_frame(&mut frame.atlas);
            mailbox.post(frame, |older, newer| {
                newer.atlas.take_uploads_from(&mut older.atlas);
            });
        }

        let (_, drawn) = drawing.join().expect("the drawing thread panicked")?;
        assert!(drawn >= 1 && drawn <= FRAMES);
        Ok(())
    }

    // Rustcord: composing a scene's scroll layers, one kind of primitive at a time.
    #[cfg(target_os = "linux")]
    mod composing {
        use super::*;
        use gpui::{AtlasTile, AtlasTextureKind, EntityId, PathVertex, ScrollLayerId, TileId, TransformationMatrix, point, px};

        fn rect(x: f32, y: f32, width: f32, height: f32) -> Bounds<ScaledPixels> {
            Bounds {
                origin: point(ScaledPixels(x), ScaledPixels(y)),
                size: Size { width: ScaledPixels(width), height: ScaledPixels(height) },
            }
        }

        fn quad(bounds: Bounds<ScaledPixels>, mask: Bounds<ScaledPixels>) -> Quad {
            Quad { bounds, content_mask: ContentMask { bounds: mask }, ..Quad::default() }
        }

        fn view() -> EntityId {
            EntityId::from(1u64)
        }

        fn composed(scene: &Scene) -> Scene {
            let mut composed = Scene::default();
            scene.copy_primitives_into(&mut composed);
            composed.compose_scroll_layers();
            composed
        }

        #[test]
        fn every_kind_of_primitive_moves_with_its_mask() {
            let id = ScrollLayerId::new();
            let clip = rect(0., 100., 200., 100.);
            let painted = rect(0., 0., 200., 300.);
            let row = rect(10., 90., 50., 40.);
            let mask = ContentMask { bounds: rect(10., 95., 50., 30.) };
            let tile = AtlasTile {
                texture_id: AtlasTextureId { index: 0, kind: AtlasTextureKind::Monochrome },
                tile_id: TileId(0),
                padding: 0,
                bounds: Bounds::default(),
            };
            let mut path = Path::new(point(px(0.), px(0.))).scale(1.);
            path.bounds = row;
            path.content_mask = mask.clone();
            path.vertices.push(PathVertex {
                xy_position: point(ScaledPixels(20.), ScaledPixels(100.)),
                st_position: point(0., 0.),
                content_mask: ContentMask::default(),
            });

            let mut scene = Scene::default();
            scene.begin_scroll_layer(id, view(), clip, painted);
            scene.insert_primitive(Shadow {
                order: 0,
                blur_radius: ScaledPixels(2.),
                bounds: row,
                corner_radii: Corners::default(),
                content_mask: mask.clone(),
                color: Hsla::default(),
                element_bounds: row,
                element_corner_radii: Corners::default(),
                inset: 1,
                pad: 0,
            });
            scene.insert_primitive(quad(row, mask.bounds));
            scene.insert_primitive(Underline {
                order: 0,
                pad: 0,
                bounds: row,
                content_mask: mask.clone(),
                color: Hsla::default(),
                thickness: ScaledPixels(1.),
                wavy: false.into(),
            });
            scene.insert_primitive(MonochromeSprite {
                order: 0,
                pad: 0,
                bounds: row,
                content_mask: mask.clone(),
                color: Hsla::default(),
                tile: tile.clone(),
                transformation: TransformationMatrix::unit(),
            });
            scene.insert_primitive(SubpixelSprite {
                order: 0,
                pad: 0,
                bounds: row,
                content_mask: mask.clone(),
                color: Hsla::default(),
                tile: tile.clone(),
                transformation: TransformationMatrix::unit(),
            });
            scene.insert_primitive(PolychromeSprite {
                order: 0,
                pad: 0,
                grayscale: false.into(),
                opacity: 1.,
                bounds: row,
                content_mask: mask.clone(),
                corner_radii: Corners::default(),
                tile,
            });
            scene.insert_primitive(path);
            scene.end_scroll_layer();
            assert!(scene.quads.is_empty(), "a layer stays out of the lists until composed");

            scene.set_scroll_layer_offset(id, ScaledPixels(40.));
            let composed = composed(&scene);

            let expected_bounds = rect(10., 130., 50., 40.);
            let expected_mask = rect(10., 135., 50., 30.);
            macro_rules! check {
                ($list:ident) => {
                    assert_eq!(composed.$list.len(), 1, stringify!($list));
                    assert_eq!(composed.$list[0].bounds, expected_bounds, stringify!($list));
                    assert_eq!(composed.$list[0].content_mask.bounds, expected_mask, stringify!($list));
                };
            }
            check!(shadows);
            check!(quads);
            check!(underlines);
            check!(monochrome_sprites);
            check!(subpixel_sprites);
            check!(polychrome_sprites);
            check!(paths);
            assert_eq!(composed.shadows[0].element_bounds, expected_bounds);
            assert_eq!(composed.paths[0].vertices[0].xy_position.y, ScaledPixels(140.));
        }

        #[test]
        fn what_ends_up_outside_the_view_is_dropped_and_masks_stop_at_its_edge() {
            let id = ScrollLayerId::new();
            let clip = rect(0., 100., 200., 100.);
            let painted = rect(0., 0., 200., 300.);
            let mut scene = Scene::default();
            scene.begin_scroll_layer(id, view(), clip, painted);
            // Above the view, moved into it, partly.
            scene.insert_primitive(quad(rect(0., 50., 200., 30.), painted));
            // Below the view, staying there.
            scene.insert_primitive(quad(rect(0., 250., 200., 30.), painted));
            scene.end_scroll_layer();
            scene.set_scroll_layer_offset(id, ScaledPixels(40.));
            let composed = composed(&scene);
            assert_eq!(composed.quads.len(), 1);
            assert_eq!(composed.quads[0].bounds, rect(0., 90., 200., 30.));
            assert_eq!(composed.quads[0].content_mask.bounds, clip);
        }

        #[test]
        fn what_paints_after_a_layer_stays_above_it_wherever_it_moves() {
            let id = ScrollLayerId::new();
            let clip = rect(0., 100., 200., 100.);
            let painted = rect(0., 0., 200., 300.);
            let mut scene = Scene::default();
            scene.insert_primitive(quad(clip, clip));
            scene.begin_scroll_layer(id, view(), clip, painted);
            for y in 0..10 {
                // Overlapping, so that the orders inside climb.
                scene.insert_primitive(quad(rect(0., y as f32 * 20., 200., 40.), painted));
            }
            scene.end_scroll_layer();
            // Not touching where rows happened to be doesn't matter: it's over the view.
            let over = rect(190., 110., 10., 10.);
            scene.insert_primitive(quad(over, over));
            scene.finish();
            scene.set_scroll_layer_offset(id, ScaledPixels(20.));
            let composed = composed(&scene);
            assert_eq!(composed.quads.first().unwrap().bounds, clip, "what's under it is drawn first");
            assert_eq!(composed.quads.last().unwrap().bounds, over, "the last painted is drawn last");
        }

        #[test]
        fn replaying_a_paint_keeps_the_layer() {
            let id = ScrollLayerId::new();
            let clip = rect(0., 100., 200., 100.);
            let painted = rect(0., 0., 200., 300.);
            let mut scene = Scene::default();
            scene.begin_scroll_layer(id, view(), clip, painted);
            scene.insert_primitive(quad(rect(0., 120., 200., 30.), painted));
            scene.end_scroll_layer();
            scene.set_scroll_layer_offset(id, ScaledPixels(10.));

            let mut replayed = Scene::default();
            replayed.replay(0..scene.len(), &scene);
            assert_eq!(replayed.scroll_layer_offset(id), Some(0.), "a replay is back where it was painted");
            assert!(replayed.quads.is_empty());
            assert_eq!(composed(&replayed).quads.len(), 1);
        }
    }

    // Rustcord: a moved scroll layer against really rebuilding, compared in pixels off the GPU.
    #[cfg(target_os = "linux")]
    mod scroll_layers {
        use super::*;
        use gpui::{
            Context, InteractiveElement as _, IntoElement, ListAlignment,
            ListState, MouseButton, ParentElement as _, Render, ScrollHandle, ScrollLayerId,
            ScrollLayerSpec, StatefulInteractiveElement as _, Styled as _, TestAppContext,
            VisualTestContext, Window, div, list, point, px, rgb,
        };
        use std::cell::Cell;
        use std::rc::Rc;

        fn row_color(ix: usize) -> u32 {
            0x102030 + (ix as u32 * 0x0B1705) % 0xEFCFCF
        }

        /// Rows of different heights with what tends to go wrong: their own clips, rounded corners
        /// and borders, something sticking out, and a shadow.
        fn row(ix: usize, clicked: &Rc<Cell<Option<usize>>>) -> impl IntoElement {
            let clicked = clicked.clone();
            div()
                .id(ix)
                // A quarter pixel lands on half a device pixel in these tests' 2x windows.
                .h(px(if ix % 3 == 0 { 30.25 } else { 47. }))
                .w_full()
                .flex_shrink_0()
                .relative()
                .bg(rgb(row_color(ix)))
                .on_click(move |_, _, _| clicked.set(Some(ix)))
                .child(div().absolute().left(px(4.)).top(px(-3.)).size(px(12.)).rounded(px(6.)).bg(rgb(0xFFFFFF)))
                .child(
                    div()
                        .absolute()
                        .left(px(30.))
                        .top(px(4.))
                        .h(px(16.))
                        .w(px(60.))
                        .overflow_hidden()
                        .child(div().h(px(50.)).w(px(40.)).border_2().border_color(rgb(0x00FF00)).bg(rgb(0x202020))),
                )
                .child(div().absolute().right(px(4.)).top(px(6.)).size(px(14.)).rounded(px(3.)).bg(rgb(0x4040FF)).shadow_md())
                .child(
                    div().absolute().right(px(30.)).top(px(5.)).size(px(16.)).rounded(px(4.)).bg(rgb(0xC0C0C0)).shadow(vec![gpui::BoxShadow {
                        color: rgb(0x000000).into(),
                        offset: point(px(0.), px(2.)),
                        blur_radius: px(3.),
                        spread_radius: px(0.),
                        inset: true,
                    }]),
                )
        }

        /// Things around a scroll view: above, below, and over it after it's painted.
        fn around(content: impl IntoElement, header_clicked: &Rc<Cell<bool>>) -> impl IntoElement {
            let header_clicked = header_clicked.clone();
            div()
                .size_full()
                .relative()
                .flex()
                .flex_col()
                .bg(rgb(0x000000))
                .child(
                    div()
                        .id("header")
                        .h(px(100.3))
                        .w_full()
                        .flex_shrink_0()
                        .bg(rgb(0x800000))
                        .on_click(move |_, _, _| header_clicked.set(true)),
                )
                .child(content)
                .child(div().h(px(100.)).w_full().flex_shrink_0().bg(rgb(0x008000)))
                .child(div().absolute().right_0().top(px(150.)).w(px(30.)).h(px(60.)).bg(rgb(0xFFFF00)).opacity(0.5))
        }

        struct ListView {
            state: ListState,
            clicked: Rc<Cell<Option<usize>>>,
            header_clicked: Rc<Cell<bool>>,
        }

        impl Render for ListView {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let clicked = self.clicked.clone();
                around(
                    div().flex_1().min_h_0().child(
                        list(self.state.clone(), move |ix, _, _| row(ix, &clicked).into_any_element()).size_full(),
                    ),
                    &self.header_clicked,
                )
            }
        }

        struct HandleView {
            handle: ScrollHandle,
            clicked: Rc<Cell<Option<usize>>>,
            header_clicked: Rc<Cell<bool>>,
        }

        impl Render for HandleView {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let clicked = self.clicked.clone();
                around(
                    div().flex_1().min_h_0().child(
                        div()
                            .id("rows")
                            .size_full()
                            .flex()
                            .flex_col()
                            .overflow_y_scroll()
                            .track_scroll(&self.handle)
                            .children((0..200).map(|ix| row(ix, &clicked).into_any_element())),
                    ),
                    &self.header_clicked,
                )
            }
        }

        fn spec() -> ScrollLayerSpec {
            ScrollLayerSpec { id: ScrollLayerId::new(), overscan: px(300.) }
        }

        fn rebuild(cx: &mut VisualTestContext) {
            cx.update(|window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            });
        }

        fn image_of(cx: &mut VisualTestContext, renderer: &mut WgpuHeadlessRenderer) -> image::RgbaImage {
            cx.update(|window, _| {
                let scale = window.scale_factor();
                let viewport = window.viewport_size();
                let size = device_size((f32::from(viewport.width) * scale) as i32, (f32::from(viewport.height) * scale) as i32);
                renderer.render_scene_to_image(window.rendered_scene_for_test(), size)
            })
            .expect("rendering the scene")
        }

        fn assert_same_picture(moved: &image::RgbaImage, rebuilt: &image::RgbaImage, what: &str) {
            assert_eq!(moved.dimensions(), rebuilt.dimensions());
            let mut wrong = 0;
            let mut first = None;
            for (x, y, pixel) in moved.enumerate_pixels() {
                let other = rebuilt.get_pixel(x, y);
                // Rounding in the last bit of a coordinate can tip an edge's coverage by a step.
                if pixel.0.iter().zip(other.0).any(|(a, b)| a.abs_diff(b) > 2) {
                    wrong += 1;
                    first.get_or_insert((x, y, pixel.0, other.0));
                }
            }
            assert_eq!(wrong, 0, "{what}: {wrong} pixels differ, first {first:?}");
        }

        /// Scrolls by each of `steps`, checks that a moved layer gives the same picture as really
        /// rebuilding, and that a rebuild is asked for exactly when the view leaves what's painted.
        fn check_steps(
            cx: &mut VisualTestContext,
            renderer: &mut WgpuHeadlessRenderer,
            id: ScrollLayerId,
            steps: &[f32],
            top: impl Fn() -> f32,
            scroll: impl Fn(f32),
        ) {
            rebuild(cx);
            for (ix, step) in steps.iter().enumerate() {
                let before = top();
                scroll(*step);
                let moved = before - top();
                let accepted = cx.update(|window, _| window.move_scroll_layer(id, px(moved)));
                assert!(accepted, "step {ix} by {step}");
                let picture = image_of(cx, renderer);
                rebuild(cx);
                let rebuilt = image_of(cx, renderer);
                assert_same_picture(&picture, &rebuilt, &format!("step {ix} by {step}, moved {moved}"));
            }
            // Several steps without a rebuild between.
            rebuild(cx);
            for step in [60., 90.5, 50.] {
                let before = top();
                scroll(step);
                let moved = before - top();
                assert!(cx.update(|window, _| window.move_scroll_layer(id, px(moved))));
            }
            let picture = image_of(cx, renderer);
            rebuild(cx);
            assert_same_picture(&picture, &image_of(cx, renderer), "several steps");

            // On until the view leaves what's painted, and no further.
            let mut gone = 0.;
            loop {
                let before = top();
                scroll(50.);
                let moved = before - top();
                gone += moved;
                if !cx.update(|window, _| window.move_scroll_layer(id, px(moved))) {
                    break;
                }
                assert!(gone >= -300., "the view left what was painted, {gone} down");
            }
            assert!(gone < -300., "asked for a rebuild too early, {gone} down");
            rebuild(cx);
        }

        const STEPS: &[f32] = &[1., 0.5, 7.5, 40., 96.5, -30., -100., 250., 290., -290., 13.];

        #[gpui::test]
        fn moved_list_looks_like_a_rebuilt_one(cx: &mut TestAppContext) {
            let mut renderer = WgpuHeadlessRenderer::new().expect("a GPU");
            let spec = spec();
            let state = ListState::new(200, ListAlignment::Top, px(50.));
            state.set_scroll_layer(Some(spec));
            let view_state = state.clone();
            let (_, cx) = cx.add_window_view(move |_, _| ListView {
                state: view_state,
                clicked: Default::default(),
                header_clicked: Default::default(),
            });
            rebuild(cx);
            let top = || f32::from(-state.scroll_px_offset_for_scrollbar().y);
            let scroll = |dy: f32| state.set_offset_from_scrollbar(point(px(0.), -(state_top(&state) + px(dy))));
            check_steps(cx, &mut renderer, spec.id, STEPS, top, scroll);
        }

        fn state_top(state: &ListState) -> gpui::Pixels {
            -state.scroll_px_offset_for_scrollbar().y
        }

        #[gpui::test]
        fn moved_scroll_view_looks_like_a_rebuilt_one(cx: &mut TestAppContext) {
            let mut renderer = WgpuHeadlessRenderer::new().expect("a GPU");
            let spec = spec();
            let handle = ScrollHandle::new();
            handle.set_scroll_layer(Some(spec));
            let view_handle = handle.clone();
            let (_, cx) = cx.add_window_view(move |_, _| HandleView {
                handle: view_handle,
                clicked: Default::default(),
                header_clicked: Default::default(),
            });
            rebuild(cx);
            let top = || f32::from(-handle.offset().y);
            let scroll = |dy: f32| {
                let mut offset = handle.offset();
                offset.y = (offset.y - px(dy)).min(px(0.));
                handle.set_offset(offset);
            };
            check_steps(cx, &mut renderer, spec.id, STEPS, top, scroll);
        }

        #[gpui::test]
        fn clicks_land_on_what_is_shown_and_never_on_rows_beyond_the_view(cx: &mut TestAppContext) {
            let spec = spec();
            let state = ListState::new(200, ListAlignment::Top, px(50.));
            state.set_scroll_layer(Some(spec));
            let clicked: Rc<Cell<Option<usize>>> = Default::default();
            let header_clicked: Rc<Cell<bool>> = Default::default();
            let (view_state, view_clicked, view_header) = (state.clone(), clicked.clone(), header_clicked.clone());
            let (_, cx) = cx.add_window_view(move |_, _| ListView {
                state: view_state,
                clicked: view_clicked,
                header_clicked: view_header,
            });
            rebuild(cx);

            // Rows are painted behind the header for the layer to move into. They mustn't get its clicks.
            state.set_offset_from_scrollbar(point(px(0.), px(-200.)));
            rebuild(cx);
            let click = |cx: &mut VisualTestContext, y: f32| {
                let position = point(px(200.), px(y));
                cx.simulate_mouse_move(position, None, gpui::Modifiers::default());
                cx.simulate_mouse_down(position, MouseButton::Left, gpui::Modifiers::default());
                cx.simulate_mouse_up(position, MouseButton::Left, gpui::Modifiers::default());
            };
            click(cx, 50.);
            assert!(header_clicked.get(), "the header takes clicks over rows painted behind it");
            assert_eq!(clicked.get(), None);

            // The list has moved as a layer, with the pointer still: a click goes to the row shown
            // there now, the same as clicking there after really rebuilding.
            rebuild(cx);
            let before = state_top(&state);
            state.set_offset_from_scrollbar(point(px(0.), -(before + px(120.))));
            let moved = before - state_top(&state);
            assert!(cx.update(|window, _| window.move_scroll_layer(spec.id, moved)));
            click(cx, 300.);
            let on_moved = clicked.take();
            assert!(on_moved.is_some());
            rebuild(cx);
            click(cx, 300.);
            assert_eq!(on_moved, clicked.take());
        }

        #[gpui::test]
        fn anything_changing_content_rebuilds_and_the_layer_starts_over(cx: &mut TestAppContext) {
            let spec = spec();
            let state = ListState::new(200, ListAlignment::Top, px(50.));
            state.set_scroll_layer(Some(spec));
            let view_state = state.clone();
            let (_, cx) = cx.add_window_view(move |_, _| ListView {
                state: view_state,
                clicked: Default::default(),
                header_clicked: Default::default(),
            });
            rebuild(cx);
            let before = state_top(&state);
            state.set_offset_from_scrollbar(point(px(0.), -(before + px(100.))));
            let moved = before - state_top(&state);
            assert!(cx.update(|window, _| window.move_scroll_layer(spec.id, moved)));

            // The view isn't dirty, only moved: a draw for any other reason still has to render it.
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let offset_after =
                cx.update(|window, _| window.rendered_scene_for_test().scroll_layer_offset(spec.id));
            assert_eq!(offset_after, Some(0.));
        }

        #[gpui::test]
        fn refuses_a_move_that_is_not_whole_pixels_or_with_no_such_layer(cx: &mut TestAppContext) {
            let spec = spec();
            let state = ListState::new(200, ListAlignment::Top, px(50.));
            state.set_scroll_layer(Some(spec));
            let view_state = state.clone();
            let (_, cx) = cx.add_window_view(move |_, _| ListView {
                state: view_state,
                clicked: Default::default(),
                header_clicked: Default::default(),
            });
            rebuild(cx);
            let scale = cx.update(|window, _| window.scale_factor());
            assert!(!cx.update(|window, _| window.move_scroll_layer(spec.id, px(0.3 / scale))));
            rebuild(cx);
            assert!(!cx.update(|window, _| window.move_scroll_layer(ScrollLayerId::new(), px(10.))));
        }
    }

    #[cfg(target_os = "linux")]
    fn point_f(x: f32, y: f32) -> Point<ScaledPixels> {
        Point { x: ScaledPixels(x), y: ScaledPixels(y) }
    }

    /// What `WgpuRenderer::draw` costs the main thread, on a scene the size of a busy message
    /// list. `cargo test --lib handoff_cost -- --ignored --nocapture`
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "a measurement, not a check"]
    fn handoff_cost() -> anyhow::Result<()> {
        let renderer = WgpuHeadlessRenderer::new()?;
        let (_, tile) = insert_picture(&renderer.atlas, 1, [0, 0, 255, 255]);
        let mut scene = Scene::default();
        for i in 0..4000 {
            let mut quad = solid_quad(i as f32, 0.0, 1.0, 1.0, gpui::red());
            quad.order = i;
            scene.quads.push(quad);
        }
        let sprite = scene_with_sprite(tile).polychrome_sprites[0].clone();
        scene.polychrome_sprites = vec![sprite; 8000];

        let mut frame = FramePacket::default();
        let rounds = 500;
        let start = std::time::Instant::now();
        for _ in 0..rounds {
            scene.copy_primitives_into(&mut frame.scene);
            renderer.atlas.capture_frame(&mut frame.atlas);
            frame.atlas.reset();
        }
        println!(
            "hand-off of {} quads and {} sprites: {:?} per frame",
            scene.quads.len(),
            scene.polychrome_sprites.len(),
            start.elapsed() / rounds
        );

        // The same with most of it on a scroll layer, as a list scrolling is.
        let id = gpui::ScrollLayerId::new();
        let everywhere = Bounds {
            origin: point_f(0., -5000.),
            size: Size { width: ScaledPixels(20000.), height: ScaledPixels(20000.) },
        };
        let mut layered = Scene::default();
        layered.begin_scroll_layer(id, gpui::EntityId::from(1u64), everywhere, everywhere);
        for quad in &scene.quads {
            let mut quad = *quad;
            quad.content_mask = ContentMask { bounds: everywhere };
            layered.insert_primitive(quad);
        }
        for sprite in &scene.polychrome_sprites {
            let mut sprite = sprite.clone();
            sprite.content_mask = ContentMask { bounds: everywhere };
            layered.insert_primitive(sprite);
        }
        layered.end_scroll_layer();
        let (mut copying, mut composing) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
        for _ in 0..rounds {
            let start = std::time::Instant::now();
            layered.copy_primitives_into(&mut frame.scene);
            copying += start.elapsed();
            let start = std::time::Instant::now();
            frame.scene.compose_scroll_layers();
            composing += start.elapsed();
            frame.scene.clear();
        }
        println!(
            "with a layer: {:?} on the main thread, {:?} on the render thread, per frame",
            copying / rounds,
            composing / rounds
        );
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn headless_renderer_captures_each_requested_size() -> anyhow::Result<()> {
        let mut renderer = WgpuHeadlessRenderer::new()?;
        let mut scene = Scene::default();
        scene.insert_primitive(solid_quad(2.0, 2.0, 4.0, 3.0, gpui::red()));
        scene.finish();

        // 13 px rows are 52 bytes, forcing readback to strip copy-row padding.
        let image = renderer.render_scene_to_image(&scene, device_size(13, 7))?;
        assert_eq!(image.dimensions(), (13, 7));
        assert_pixel(&image, 0, 0, BLACK);
        assert_pixel(&image, 3, 3, RED);
        assert_pixel(&image, 12, 6, BLACK);

        let image = renderer.render_scene_to_image(&scene, device_size(17, 9))?;
        assert_eq!(image.dimensions(), (17, 9));
        assert_pixel(&image, 3, 3, RED);
        assert_pixel(&image, 16, 8, BLACK);

        let image = renderer.render_scene_to_image(&Scene::default(), device_size(13, 7))?;
        assert_eq!(image.dimensions(), (13, 7));
        assert!(image.pixels().all(|pixel| pixel.0 == BLACK));
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn headless_renderer_reuses_target_for_same_size() -> anyhow::Result<()> {
        let mut renderer = WgpuHeadlessRenderer::new()?;
        let target_texture = |renderer: &WgpuHeadlessRenderer| {
            renderer
                .render_target
                .as_ref()
                .map(|target| target.texture.clone())
        };

        renderer.render_scene(&Scene::default(), device_size(16, 16))?;
        let first = target_texture(&renderer);
        assert!(first.is_some());

        renderer.render_scene(&Scene::default(), device_size(16, 16))?;
        assert_eq!(target_texture(&renderer), first);

        renderer.render_scene(&Scene::default(), device_size(16, 17))?;
        assert_ne!(target_texture(&renderer), first);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn headless_renderer_rejects_invalid_sizes() -> anyhow::Result<()> {
        let mut renderer = WgpuHeadlessRenderer::new()?;
        let too_large = renderer.core.max_texture_size as i32 + 1;

        for size in [
            device_size(0, 8),
            device_size(8, 0),
            device_size(-1, 8),
            device_size(too_large, 8),
        ] {
            assert!(
                renderer.render_scene(&Scene::default(), size).is_err(),
                "{size:?} should be rejected"
            );
            assert!(
                renderer
                    .render_scene_to_image(&Scene::default(), size)
                    .is_err(),
                "{size:?} should be rejected"
            );
        }

        // Rejection must leave the renderer usable.
        let image = renderer.render_scene_to_image(&Scene::default(), device_size(4, 4))?;
        assert_eq!(image.dimensions(), (4, 4));
        Ok(())
    }

    #[test]
    fn webgl_shader_is_valid_wgsl_without_storage_buffers() {
        assert!(!WEBGL_SHADERS.contains("var<storage"));
        validate_wgsl(WEBGL_SHADERS, naga::valid::Capabilities::empty());
    }

    #[test]
    fn storage_buffer_shader_is_valid_wgsl() {
        validate_wgsl(STORAGE_BUFFER_SHADERS, naga::valid::Capabilities::empty());
    }

    #[test]
    fn subpixel_shader_is_valid_wgsl() {
        validate_wgsl(
            SUBPIXEL_SHADERS,
            naga::valid::Capabilities::DUAL_SOURCE_BLENDING,
        );
    }

    fn validate_wgsl(source: &str, capabilities: naga::valid::Capabilities) {
        let module = naga::front::wgsl::parse_str(source).expect("shader should parse");
        naga::valid::Validator::new(naga::valid::ValidationFlags::all(), capabilities)
            .validate(&module)
            .expect("shader should validate");
    }

    #[test]
    fn webgl_record_sizes_match_shader_word_strides() {
        assert_eq!(std::mem::size_of::<Quad>(), 40 * 4);
        assert_eq!(std::mem::size_of::<Shadow>(), 28 * 4);
        assert_eq!(std::mem::size_of::<PathRasterizationVertex>(), 26 * 4);
        assert_eq!(std::mem::size_of::<PathSprite>(), 4 * 4);
        assert_eq!(std::mem::size_of::<Underline>(), 16 * 4);
        assert_eq!(std::mem::size_of::<MonochromeSprite>(), 28 * 4);
        assert_eq!(std::mem::size_of::<SubpixelSprite>(), 28 * 4);
        assert_eq!(std::mem::size_of::<PolychromeSprite>(), 24 * 4);
    }

    #[test]
    fn webgl_quad_layout_matches_fixed_decoder() {
        let quad = Quad {
            order: 41,
            border_style: BorderStyle::Dashed,
            bounds: Bounds {
                origin: Point {
                    x: 1.0.into(),
                    y: 2.0.into(),
                },
                size: Size {
                    width: 3.0.into(),
                    height: 4.0.into(),
                },
            },
            content_mask: ContentMask {
                bounds: Bounds {
                    origin: Point {
                        x: 5.0.into(),
                        y: 6.0.into(),
                    },
                    size: Size {
                        width: 7.0.into(),
                        height: 8.0.into(),
                    },
                },
            },
            background: linear_gradient(
                11.0,
                linear_color_stop(
                    Hsla {
                        h: 12.0,
                        s: 13.0,
                        l: 14.0,
                        a: 15.0,
                    },
                    16.0,
                ),
                linear_color_stop(
                    Hsla {
                        h: 17.0,
                        s: 18.0,
                        l: 19.0,
                        a: 20.0,
                    },
                    21.0,
                ),
            )
            .color_space(ColorSpace::Oklab),
            border_color: Hsla {
                h: 22.0,
                s: 23.0,
                l: 24.0,
                a: 25.0,
            },
            corner_radii: Corners {
                top_left: 26.0.into(),
                top_right: 27.0.into(),
                bottom_right: 28.0.into(),
                bottom_left: 29.0.into(),
            },
            border_widths: Edges {
                top: 30.0.into(),
                right: 31.0.into(),
                bottom: 32.0.into(),
                left: 33.0.into(),
            },
        };

        let bytes = unsafe { WgpuRendererCore::instance_bytes(std::slice::from_ref(&quad)) };
        let words: &[u32] = bytemuck::cast_slice(bytes);
        assert_eq!(
            words,
            &[
                41,
                1,
                1.0_f32.to_bits(),
                2.0_f32.to_bits(),
                3.0_f32.to_bits(),
                4.0_f32.to_bits(),
                5.0_f32.to_bits(),
                6.0_f32.to_bits(),
                7.0_f32.to_bits(),
                8.0_f32.to_bits(),
                1,
                1,
                0,
                0,
                0,
                0,
                11.0_f32.to_bits(),
                12.0_f32.to_bits(),
                13.0_f32.to_bits(),
                14.0_f32.to_bits(),
                15.0_f32.to_bits(),
                16.0_f32.to_bits(),
                17.0_f32.to_bits(),
                18.0_f32.to_bits(),
                19.0_f32.to_bits(),
                20.0_f32.to_bits(),
                21.0_f32.to_bits(),
                0,
                22.0_f32.to_bits(),
                23.0_f32.to_bits(),
                24.0_f32.to_bits(),
                25.0_f32.to_bits(),
                26.0_f32.to_bits(),
                27.0_f32.to_bits(),
                28.0_f32.to_bits(),
                29.0_f32.to_bits(),
                30.0_f32.to_bits(),
                31.0_f32.to_bits(),
                32.0_f32.to_bits(),
                33.0_f32.to_bits(),
            ]
        );
    }
}
