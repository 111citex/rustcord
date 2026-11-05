gpui-pre 0.3.7, gpui-pre-wgpu 0.3.7 and gpui-pre-linux 0.3.7 from crates.io (Apache-2.0), with:
- `gpui-pre-wgpu/wgpu_renderer.rs`, `frame_mailbox.rs`: every window has a render thread that does all the GPU work of a
  frame, from uploads to present. `WgpuRenderer::draw` copies the scene (`Scene::copy_primitives_into`) and hands it over
  without waiting. The newest frame wins if the thread is behind. The surface and the things drawing to it sit behind one
  lock, which is how a resize knows no frame is half done; frames painted before a resize are never presented.
- `wgpu_atlas.rs`: a frame takes its uploads and textures with it (`AtlasFrame`), because the next paint can reuse a tile's
  space or a texture's id while it waits to be drawn.
- `scene.rs`, `window.rs`, `elements/list.rs`, `elements/div.rs`: scroll layers. A `ListState` or `ScrollHandle` given a
  `ScrollLayerSpec` paints its content with `overscan` more of it into a layer of the scene, which `Window::move_scroll_layer`
  then moves with no render. The render thread composes that into a frame (`Scene::compose_scroll_layers`). A layer that's
  moved gets rendered on the next real draw, and input other than more wheel on the same spot gets a real draw first.
- `gpui-pre-linux/wayland`: only `draw` going off the main thread needs this crate changed. A frame that fails to reach
  the compositor gets no frame callback, so the render thread pings the loop (`render_feedback_fired`).
  `x11/window.rs`: redraw requests from the render thread are picked up in `refresh`.
- `bounds_tree.rs`: `Send`, so that a `Scene` can go to the render thread.
- `cosmic_text_system.rs`: its tests want fonts from Zed's tree and are off, so that this crate's other tests can be built
  (`cargo test -p gpui-pre-wgpu --features test-support`).
- `frame_stats`: counters on what a frame costs (draw phases, layout nodes, atlas size).
- `TaffyLayoutEngine::remember_next_layout`: a list row's layout is reused on its next frame when its
  styles, its tree and what its measured nodes say are all unchanged. `layout_items` in `elements/list.rs` asks for it.
- `elements/text.rs`: text already fitted to a width answers "how wide do you want to be" with its
  real width, not the fitted one. Boxes sized by their text no longer get stuck narrow.
- `assets.rs`, `Window::paint_image`: an image's pixels move to the GPU when first painted, instead of
  being held in memory as well. `frame_stats::ATLAS_RESETS` says when they're gone from there too.
- `view.rs`: a cached view being redrawn no longer forces the cached views inside it to redraw too,
  as long as they were drawn in the frame before (`prepainted_in`).
- `elements/img.rs`: GIFs are scaled to the `#fit=WxH` on their address, and big ones stay files (`StreamSource`).
- `wgpu_atlas.rs`: big pictures keep to their own textures; `frame_stats::ATLAS_USED` counts what's really in them.
- `ListState::rows_near_view`: the rows within some distance of the screen.

cosmic-text 0.19.0 from crates.io (MIT OR Apache-2.0), with: the last step of font fallback asks fontconfig
which font has a character (`font/fallback/fontconfig.rs`) instead of opening every font on the system.
- `wgpu_context.rs` (gpui-pre-wgpu): OpenGL is only asked for when there is no Vulkan.
