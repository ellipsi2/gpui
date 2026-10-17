# GPUI for Draw

This branch (`draw`) holds the GPUI crates that [Draw](https://github.com/ellipsi2/draw) builds against, maintained as a fork of [zed-industries/zed](https://github.com/zed-industries/zed).

It is not a zed checkout. GPUI 0.2.2 was published to crates.io from zed commit `69e2130295c2649963eb639fc70b4f2ee8ea1624` (from a modified tree) as twelve crates with renamed internal dependencies (`gpui_util`, `gpui_collections`, ...). The first commit of this branch is exactly those twelve `.crate` archives, unpacked; their SHA-256 sums match the crates.io index. Building the zed workspace directly would pull in zed's whole dependency set instead, so changes are made on top of the published sources.

The branch shares no history with zed's `main`, so a Cargo git dependency on it fetches only these sources.

## Changes from 0.2.2

- **Windows renders through Blade (Vulkan)** instead of the DirectX 11 renderer, so Draw's renderer features are written once in WGSL. Direct3D 11 remains only to present: Blade draws each frame into a D3D11 texture shared with Vulkan, which a premultiplied DXGI composition swap chain shows through DirectComposition (Vulkan swapchains lose per-pixel window alpha on NVIDIA). See `crates/gpui/src/platform/windows/composition.rs` and Draw's `docs/design/gpui-vulkan.md`.
- **Backdrop blur.** `Window::paint_backdrop_blur` blurs what is already drawn under a rounded rectangle and tints it (opaque glass). The Blade renderer copies the region, averages it to quarter resolution, runs a separable Gaussian and composites it back in scene order (`blade_renderer.rs`, `shaders.wgsl`); the Metal renderer draws nothing for it.
- **External images (Windows).** `Window::paint_external_image` draws an `ExternalImage`, an image another Vulkan device on the same GPU rendered and exported as an `OPAQUE_WIN32` memory handle, in scene order without a copy through the CPU. The Blade renderer imports it once per source, acquires each new frame from the external queue family and samples it with the image sprite shader. `GpuSpecs::adapter_luid` names the adapter, so the other device can open the same GPU.
- **`crates/blade-graphics`** is blade-graphics 0.7.1 (MIT, kvark/blade `88b6c64`), vendored unchanged in its own commit and then patched: D3D11 shared-texture import, the adapter LUID, no destroy of a null swapchain (faults in NVIDIA's Windows driver), and a fallible `OPAQUE_WIN32` texture import with an acquire from the external queue family.

## Crates

| Directory | Package |
| --- | --- |
| `crates/gpui` | `gpui` |
| `crates/gpui-macros` | `gpui-macros` |
| `crates/gpui_collections` | `gpui_collections` |
| `crates/gpui_derive_refineable` | `gpui_derive_refineable` |
| `crates/gpui_http_client` | `gpui_http_client` |
| `crates/gpui_media` | `gpui_media` |
| `crates/gpui_perf` | `gpui_perf` |
| `crates/gpui_refineable` | `gpui_refineable` |
| `crates/gpui_semantic_version` | `gpui_semantic_version` |
| `crates/gpui_sum_tree` | `gpui_sum_tree` |
| `crates/gpui_util` | `gpui_util` |
| `crates/gpui_util_macros` | `gpui_util_macros` |
| `crates/blade-graphics` | `blade-graphics` |

Each crate keeps its published `Cargo.toml`, `Cargo.toml.orig`, `.cargo_vcs_info.json` and `Cargo.lock`. The root `Cargo.toml` is a workspace that patches the crates onto each other, so `cargo check` here builds the local copies.

## Use from Draw

Draw's root `Cargo.toml` replaces the crates.io packages with this branch:

```toml
[patch.crates-io]
gpui = { git = "https://github.com/ellipsi2/gpui", branch = "draw" }
# ... one line per package above, blade-graphics included
```

After pushing here, run `cargo update -p gpui` (and the other packages) in Draw to move its lock file to the new commit.

## Bringing in upstream changes

Fetch zed into a local clone (`git remote add upstream https://github.com/zed-industries/zed`, `git fetch upstream main`) and port the needed commits by hand: paths differ (`crates/gpui/...` is the same, but internal crates are renamed) and the published `Cargo.toml` files use plain crates.io versions instead of `workspace = true`.

## License

Apache-2.0, as upstream (`LICENSE-APACHE` in each crate).
