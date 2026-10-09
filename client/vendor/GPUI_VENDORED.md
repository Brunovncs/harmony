# Vendored GPUI

`gpui-pre` and `gpui-pre-windows` 0.3.7, copied from crates.io (less `.cargo-ok`) and patched in
`client/Cargo.toml`'s `[patch.crates-io]`, so video is drawn by the GPU on Windows: the published
crates have the "surface" primitive end to end (`PaintSurface`, `PrimitiveBatch::Surfaces`), but
its only payload is a macOS `CVPixelBuffer` and the DirectX renderer's `draw_surfaces` is an empty
stub. Everything else is the published 0.3.7, byte for byte; to see the changes, diff a directory
against `~/.cargo/registry/src/index.crates.io-*/<crate>-0.3.7`.

To move to a newer GPUI: port the changes below (or drop them, if upstream has its own Windows
surfaces by then), or remove the `[patch.crates-io]` lines and this directory to go back to the
crates as published, which brings back the client's CPU path for video (BGRA `RenderImage`s
through the sprite atlas).

## gpui-pre

- `src/scene.rs`
  - `SurfaceFrame`: one video frame in CPU memory as YUV planes (`SurfaceFormat::I420` or
    `Nv12`, BT.601 limited range) in one shared `Arc<[u8]>`, with each plane's offset and stride
    (`SurfacePlane`), the stream it belongs to (`id`, so the renderer can keep that stream's
    textures) and a serial unique to the frame (so a frame already uploaded is not uploaded again
    when the window redraws for other reasons). `new` checks every plane lies inside the data,
    which the renderer relies on. `mirrored(true)` flips it left to right as it is drawn (your own
    camera). Platform-neutral; only the Windows renderer draws it.
  - `PaintSurface` gains, on Windows only, `frame: SurfaceFrame` and
    `corner_radii: Corners<ScaledPixels>` (rounded like images are). The macOS field is untouched.
- `src/window.rs`: `Window::paint_surface(bounds, corner_radii, frame)` on Windows, beside the
  macOS one (which is untouched): snaps the bounds, takes the content mask, clamps and scales the
  radii, and inserts the `PaintSurface`.
- `src/elements/surface.rs`: `SurfaceSource::Frame(SurfaceFrame)` on Windows (with
  `From<SurfaceFrame>`), and `surface(...)` available on Windows as well as macOS, so a frame is
  drawn with `surface(frame).object_fit(ObjectFit::Contain).rounded(...)` like an `img`. Its
  rounded corners come from the element's style.

## gpui-pre-windows

- `src/directx_renderer.rs`
  - `draw_surfaces`, which was empty, draws each surface of the batch, in the batch's place in the
    scene's order (so tile labels and other overlays painted later stay on top), with its own
    pipeline (`surface_pipeline`, the usual alpha blending) and instance buffer, filled in
    `upload_scene_buffers` like every other primitive's.
  - `SurfaceTextures`: per stream (`SurfaceFrame::id`) an R8 texture per plane (R8 Y, U, V for
    I420; R8 Y and R8G8 UV for NV12) with their views, made when a stream first draws and remade
    only when its size or format changes. A frame is written (`UpdateSubresource`, from the
    frame's own strides) only if its serial is not the one already in the textures. Streams a
    render did not draw are dropped at the end of that render (the window redraws when a tile
    goes away, so an ended stream's GPU memory is freed then), and the plane views are unbound
    after each batch so nothing keeps them alive. The cache is cleared on device loss.
  - A second sampler (`surface_sampler`), linear and clamped: video frames are whole textures,
    and the sprite sampler's wrapping would bleed one edge into the other as they are filtered.
  - `ShaderModule::Surface` ("surface"), for both the runtime-compiled (debug) and the
    precompiled (release) shader paths.
- `src/shaders.hlsl`: `surface_vertex` / `surface_fragment`. The vertex shader places the bounds
  quad, clips with `SV_ClipDistance` against the content mask like the other primitives, and
  mirrors the texture coordinates when asked. The fragment shader samples the planes with linear
  filtering (the GPU's scaling replaces the CPU's), converts BT.601 limited-range YUV to RGB with
  the coefficients of libyuv's `I420ToARGB` (in 1/64ths, blue's capped at 2, so colours match what
  the client used to convert on the CPU) and rounds the corners with `quad_sdf`, as images are.
  Planes are bound to `t2`-`t4` (after the instance buffer in `t1`), the sampler to `s1`.
- `build.rs`: "surface" added to the modules fxc precompiles for release builds.
