use std::time::Duration;
use gpui::{
    AbsoluteLength, App, Bounds, Corners, Element, ElementId, Entity, GlobalElementId, InspectorElementId,
    IntoElement, LayoutId, ObjectFit, Pixels, Refineable, Style, StyleRefinement, Styled, Window,
};

use crate::VideoPlayer;

/// Renders the current frame of a [`VideoPlayer`]. Style it like a `div` (`.size_full()`, etc.).
pub fn video_view(player: Entity<VideoPlayer>) -> VideoView {
    VideoView {
        player,
        style: StyleRefinement::default(),
        object_fit: ObjectFit::Contain,
        corner_radius: None,
    }
}

pub struct VideoView {
    player: Entity<VideoPlayer>,
    style: StyleRefinement,
    object_fit: ObjectFit,
    corner_radius: Option<AbsoluteLength>,
}

impl VideoView {
    pub fn object_fit(mut self, fit: ObjectFit) -> Self {
        self.object_fit = fit;
        self
    }

    /// Rounds the painted frame's corners (rustcord addition).
    pub fn corner_radius(mut self, radius: impl Into<AbsoluteLength>) -> Self {
        self.corner_radius = Some(radius.into());
        self
    }
}

impl Styled for VideoView {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl IntoElement for VideoView {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for VideoView {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.refine(&self.style);
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let start = std::time::Instant::now();
        // Frames larger than the area they are shown in are scaled down on the decode thread.
        let scale = window.scale_factor();
        let device = |px: gpui::Pixels| (f32::from(px) * scale).round().max(1.0) as u32;
        let display = (device(bounds.size.width), device(bounds.size.height));
        let (image, retired, new_frame) = self.player.update(cx, |p, _| {
            p.set_display_size(display);
            p.frame_image()
        });
        for old in retired {
            // Without this the sprite atlas grows by one frame per frame.
            let _ = window.drop_image(old);
        }
        let corners = Corners::all(self.corner_radius.map_or(gpui::px(0.), |r| r.to_pixels(window.rem_size())));
        if let Some(image) = image {
            let fit = self.object_fit.get_bounds(bounds, image.size(0));
            let _ = window.paint_image(bounds, fit, corners, image, 0, false);
        } else if !self.player.read(cx).has_video()
            && let Some(cover) = self.player.read(cx).cover()
        {
            // Audio only: the album art where the picture would be.
            let fit = self.object_fit.get_bounds(bounds, cover.size(0));
            let _ = window.paint_image(bounds, fit, Corners::default(), cover, 0, false);
        }
        let cost = start.elapsed();
        self.player
            .update(cx, |p, _| p.debug_paint(new_frame, cost));
        // Asking every display refresh would redraw the whole window at 180Hz
        // for a 15fps GIF. Silent loops take 30 a second, real videos 60.
        if self.player.read(cx).is_playing() {
            let wait = if self.player.read(cx).is_muted() { 33 } else { 16 };
            redraw_after(Duration::from_millis(wait), window, cx);
        }
    }
}

/// Views already waiting on a redraw, so many pictures in one view make one wake-up.
#[derive(Default)]
struct PendingRedraws(std::collections::HashSet<gpui::EntityId>);

impl gpui::Global for PendingRedraws {}

/// Redraws the view being painted after `wait`.
pub fn redraw_after(wait: Duration, window: &gpui::Window, cx: &mut gpui::App) {
    let view = window.current_view();
    if !cx.has_global::<PendingRedraws>() {
        cx.set_global(PendingRedraws::default());
    }
    if !cx.global_mut::<PendingRedraws>().0.insert(view) {
        return;
    }
    cx.spawn(async move |cx| {
        cx.background_executor().timer(wait).await;
        let _ = cx.update(|cx| {
            cx.global_mut::<PendingRedraws>().0.remove(&view);
            cx.notify(view);
        });
    })
    .detach();
}
