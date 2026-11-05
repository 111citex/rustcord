gpui-pre 0.3.7 and gpui-pre-wgpu 0.3.7 from crates.io (Apache-2.0), with:
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
