// Doing `if let` gives you nice scoping with passes/encoders
#![allow(irrefutable_let_patterns)]

use super::{BladeAtlas, BladeContext};
use crate::{
    BackdropBlur, Background, Bounds, DevicePixels, GpuSpecs, MonochromeSprite, Path, Point,
    PolychromeSprite, PrimitiveBatch, Quad, Rgba, ScaledPixels, Scene, Shadow, Size, Underline,
};
use blade_graphics as gpu;
use blade_util::{BufferBelt, BufferBeltDescriptor};
use bytemuck::{Pod, Zeroable};
#[cfg(target_os = "macos")]
use media::core_video::CVMetalTextureCache;
use std::sync::Arc;

const MAX_FRAME_TIME_MS: u32 = 10000;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GlobalParams {
    viewport_size: [f32; 2],
    premultiplied_alpha: u32,
    pad: u32,
}

//Note: we can't use `Bounds` directly here because
// it doesn't implement Pod + Zeroable
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

/// One pass of a backdrop blur (see `BackdropParams` in the shaders).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct BackdropParams {
    dest: [f32; 4],
    source: [f32; 4],
    tint: [f32; 4],
    bounds: [f32; 4],
    content_mask: [f32; 4],
    corner_radii: [f32; 4],
    target_size: [f32; 2],
    direction: [f32; 2],
    sigma: f32,
    opacity: f32,
    pad: [f32; 2],
}

#[derive(blade_macros::ShaderData)]
struct ShaderBackdropData {
    backdrop: BackdropParams,
    t_backdrop: gpu::TextureView,
}

#[derive(blade_macros::ShaderData)]
struct ShaderBackdropCompositeData {
    backdrop: BackdropParams,
    t_backdrop: gpu::TextureView,
    t_backdrop_frame: gpu::TextureView,
}

#[derive(blade_macros::ShaderData)]
struct ShaderQuadsData {
    globals: GlobalParams,
    b_quads: gpu::BufferPiece,
}

#[derive(blade_macros::ShaderData)]
struct ShaderShadowsData {
    globals: GlobalParams,
    b_shadows: gpu::BufferPiece,
}

#[derive(blade_macros::ShaderData)]
struct ShaderPathRasterizationData {
    globals: GlobalParams,
    b_path_vertices: gpu::BufferPiece,
}

#[derive(blade_macros::ShaderData)]
struct ShaderPathsData {
    globals: GlobalParams,
    t_sprite: gpu::TextureView,
    s_sprite: gpu::Sampler,
    b_path_sprites: gpu::BufferPiece,
}

#[derive(blade_macros::ShaderData)]
struct ShaderUnderlinesData {
    globals: GlobalParams,
    b_underlines: gpu::BufferPiece,
}

#[derive(blade_macros::ShaderData)]
struct ShaderMonoSpritesData {
    globals: GlobalParams,
    gamma_ratios: [f32; 4],
    grayscale_enhanced_contrast: f32,
    t_sprite: gpu::TextureView,
    s_sprite: gpu::Sampler,
    b_mono_sprites: gpu::BufferPiece,
}

#[derive(blade_macros::ShaderData)]
struct ShaderPolySpritesData {
    globals: GlobalParams,
    t_sprite: gpu::TextureView,
    s_sprite: gpu::Sampler,
    b_poly_sprites: gpu::BufferPiece,
}

#[derive(blade_macros::ShaderData)]
struct ShaderSurfacesData {
    globals: GlobalParams,
    surface_locals: SurfaceParams,
    t_y: gpu::TextureView,
    t_cb_cr: gpu::TextureView,
    s_surface: gpu::Sampler,
}

#[derive(Clone, Debug, Eq, PartialEq)]
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

struct BladePipelines {
    quads: gpu::RenderPipeline,
    shadows: gpu::RenderPipeline,
    path_rasterization: gpu::RenderPipeline,
    paths: gpu::RenderPipeline,
    underlines: gpu::RenderPipeline,
    mono_sprites: gpu::RenderPipeline,
    poly_sprites: gpu::RenderPipeline,
    surfaces: gpu::RenderPipeline,
    backdrop_downsample: gpu::RenderPipeline,
    backdrop_blur: gpu::RenderPipeline,
    backdrop_composite: gpu::RenderPipeline,
}

impl BladePipelines {
    fn new(gpu: &gpu::Context, surface_info: gpu::SurfaceInfo, path_sample_count: u32) -> Self {
        use gpu::ShaderData as _;

        log::info!(
            "Initializing Blade pipelines for surface {:?}",
            surface_info
        );
        let shader = gpu.create_shader(gpu::ShaderDesc {
            source: include_str!("shaders.wgsl"),
        });
        shader.check_struct_size::<GlobalParams>();
        shader.check_struct_size::<SurfaceParams>();
        shader.check_struct_size::<Quad>();
        shader.check_struct_size::<Shadow>();
        shader.check_struct_size::<PathRasterizationVertex>();
        shader.check_struct_size::<PathSprite>();
        shader.check_struct_size::<Underline>();
        shader.check_struct_size::<MonochromeSprite>();
        shader.check_struct_size::<PolychromeSprite>();
        shader.check_struct_size::<BackdropParams>();

        // See https://apoorvaj.io/alpha-compositing-opengl-blending-and-premultiplied-alpha/
        let blend_mode = match surface_info.alpha {
            gpu::AlphaMode::Ignored => gpu::BlendState::ALPHA_BLENDING,
            gpu::AlphaMode::PreMultiplied => gpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING,
            gpu::AlphaMode::PostMultiplied => gpu::BlendState::ALPHA_BLENDING,
        };
        let color_targets = &[gpu::ColorTargetState {
            format: surface_info.format,
            blend: Some(blend_mode),
            write_mask: gpu::ColorWrites::default(),
        }];
        // Backdrop blur passes write their texels as they are.
        let backdrop_pipeline = |name, fragment, layout, format| {
            gpu.create_render_pipeline(gpu::RenderPipelineDesc {
                name,
                data_layouts: &[layout],
                vertex: shader.at("vs_backdrop"),
                vertex_fetches: &[],
                primitive: gpu::PrimitiveState {
                    topology: gpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                fragment: Some(shader.at(fragment)),
                color_targets: &[gpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: gpu::ColorWrites::default(),
                }],
                multisample_state: gpu::MultisampleState::default(),
            })
        };

        Self {
            quads: gpu.create_render_pipeline(gpu::RenderPipelineDesc {
                name: "quads",
                data_layouts: &[&ShaderQuadsData::layout()],
                vertex: shader.at("vs_quad"),
                vertex_fetches: &[],
                primitive: gpu::PrimitiveState {
                    topology: gpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                fragment: Some(shader.at("fs_quad")),
                color_targets,
                multisample_state: gpu::MultisampleState::default(),
            }),
            shadows: gpu.create_render_pipeline(gpu::RenderPipelineDesc {
                name: "shadows",
                data_layouts: &[&ShaderShadowsData::layout()],
                vertex: shader.at("vs_shadow"),
                vertex_fetches: &[],
                primitive: gpu::PrimitiveState {
                    topology: gpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                fragment: Some(shader.at("fs_shadow")),
                color_targets,
                multisample_state: gpu::MultisampleState::default(),
            }),
            path_rasterization: gpu.create_render_pipeline(gpu::RenderPipelineDesc {
                name: "path_rasterization",
                data_layouts: &[&ShaderPathRasterizationData::layout()],
                vertex: shader.at("vs_path_rasterization"),
                vertex_fetches: &[],
                primitive: gpu::PrimitiveState {
                    topology: gpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: None,
                fragment: Some(shader.at("fs_path_rasterization")),
                // The original implementation was using ADDITIVE blende mode,
                // I don't know why
                // color_targets: &[gpu::ColorTargetState {
                //     format: PATH_TEXTURE_FORMAT,
                //     blend: Some(gpu::BlendState::ADDITIVE),
                //     write_mask: gpu::ColorWrites::default(),
                // }],
                color_targets: &[gpu::ColorTargetState {
                    format: surface_info.format,
                    blend: Some(gpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: gpu::ColorWrites::default(),
                }],
                multisample_state: gpu::MultisampleState {
                    sample_count: path_sample_count,
                    ..Default::default()
                },
            }),
            paths: gpu.create_render_pipeline(gpu::RenderPipelineDesc {
                name: "paths",
                data_layouts: &[&ShaderPathsData::layout()],
                vertex: shader.at("vs_path"),
                vertex_fetches: &[],
                primitive: gpu::PrimitiveState {
                    topology: gpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                fragment: Some(shader.at("fs_path")),
                color_targets: &[gpu::ColorTargetState {
                    format: surface_info.format,
                    blend: Some(gpu::BlendState {
                        color: gpu::BlendComponent::OVER,
                        alpha: gpu::BlendComponent::ADDITIVE,
                    }),
                    write_mask: gpu::ColorWrites::default(),
                }],
                multisample_state: gpu::MultisampleState::default(),
            }),
            underlines: gpu.create_render_pipeline(gpu::RenderPipelineDesc {
                name: "underlines",
                data_layouts: &[&ShaderUnderlinesData::layout()],
                vertex: shader.at("vs_underline"),
                vertex_fetches: &[],
                primitive: gpu::PrimitiveState {
                    topology: gpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                fragment: Some(shader.at("fs_underline")),
                color_targets,
                multisample_state: gpu::MultisampleState::default(),
            }),
            mono_sprites: gpu.create_render_pipeline(gpu::RenderPipelineDesc {
                name: "mono-sprites",
                data_layouts: &[&ShaderMonoSpritesData::layout()],
                vertex: shader.at("vs_mono_sprite"),
                vertex_fetches: &[],
                primitive: gpu::PrimitiveState {
                    topology: gpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                fragment: Some(shader.at("fs_mono_sprite")),
                color_targets,
                multisample_state: gpu::MultisampleState::default(),
            }),
            poly_sprites: gpu.create_render_pipeline(gpu::RenderPipelineDesc {
                name: "poly-sprites",
                data_layouts: &[&ShaderPolySpritesData::layout()],
                vertex: shader.at("vs_poly_sprite"),
                vertex_fetches: &[],
                primitive: gpu::PrimitiveState {
                    topology: gpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                fragment: Some(shader.at("fs_poly_sprite")),
                color_targets,
                multisample_state: gpu::MultisampleState::default(),
            }),
            surfaces: gpu.create_render_pipeline(gpu::RenderPipelineDesc {
                name: "surfaces",
                data_layouts: &[&ShaderSurfacesData::layout()],
                vertex: shader.at("vs_surface"),
                vertex_fetches: &[],
                primitive: gpu::PrimitiveState {
                    topology: gpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                fragment: Some(shader.at("fs_surface")),
                color_targets,
                multisample_state: gpu::MultisampleState::default(),
            }),
            backdrop_downsample: backdrop_pipeline(
                "backdrop-downsample",
                "fs_backdrop_downsample",
                &ShaderBackdropData::layout(),
                BACKDROP_FORMAT,
            ),
            backdrop_blur: backdrop_pipeline(
                "backdrop-blur",
                "fs_backdrop_blur",
                &ShaderBackdropData::layout(),
                BACKDROP_FORMAT,
            ),
            backdrop_composite: backdrop_pipeline(
                "backdrop-composite",
                "fs_backdrop_composite",
                &ShaderBackdropCompositeData::layout(),
                surface_info.format,
            ),
        }
    }

    fn destroy(&mut self, gpu: &gpu::Context) {
        gpu.destroy_render_pipeline(&mut self.quads);
        gpu.destroy_render_pipeline(&mut self.shadows);
        gpu.destroy_render_pipeline(&mut self.path_rasterization);
        gpu.destroy_render_pipeline(&mut self.paths);
        gpu.destroy_render_pipeline(&mut self.underlines);
        gpu.destroy_render_pipeline(&mut self.mono_sprites);
        gpu.destroy_render_pipeline(&mut self.poly_sprites);
        gpu.destroy_render_pipeline(&mut self.surfaces);
        gpu.destroy_render_pipeline(&mut self.backdrop_downsample);
        gpu.destroy_render_pipeline(&mut self.backdrop_blur);
        gpu.destroy_render_pipeline(&mut self.backdrop_composite);
    }
}

/// The quarter-resolution images of a backdrop blur.
const BACKDROP_FORMAT: gpu::TextureFormat = gpu::TextureFormat::Rgba16Float;
/// Frame pixels per quarter-resolution texel along each axis.
const BACKDROP_SCALE: u32 = 4;

/// Scratch textures of the backdrop blur, made at the first blur and dropped on resize.
struct BackdropTextures {
    /// The frame as it was before a blur, at full resolution.
    frame: (gpu::Texture, gpu::TextureView),
    /// Two quarter-resolution images the passes alternate between.
    quarter: [(gpu::Texture, gpu::TextureView); 2],
    quarter_size: [u32; 2],
}

impl BackdropTextures {
    fn new(gpu: &gpu::Context, format: gpu::TextureFormat, size: gpu::Extent) -> Self {
        let make = |name, format, width, height, usage| {
            let texture = gpu.create_texture(gpu::TextureDesc {
                name,
                format,
                size: gpu::Extent {
                    width,
                    height,
                    depth: 1,
                },
                array_layer_count: 1,
                mip_level_count: 1,
                sample_count: 1,
                dimension: gpu::TextureDimension::D2,
                usage,
                external: None,
            });
            let view = gpu.create_texture_view(
                texture,
                gpu::TextureViewDesc {
                    name,
                    format,
                    dimension: gpu::ViewDimension::D2,
                    subresources: &Default::default(),
                },
            );
            (texture, view)
        };
        let quarter_size = [
            size.width.div_ceil(BACKDROP_SCALE),
            size.height.div_ceil(BACKDROP_SCALE),
        ];
        Self {
            frame: make(
                "backdrop frame",
                format,
                size.width,
                size.height,
                gpu::TextureUsage::COPY | gpu::TextureUsage::RESOURCE,
            ),
            quarter: [0, 1].map(|_| {
                make(
                    "backdrop quarter",
                    BACKDROP_FORMAT,
                    quarter_size[0],
                    quarter_size[1],
                    gpu::TextureUsage::RESOURCE | gpu::TextureUsage::TARGET,
                )
            }),
            quarter_size,
        }
    }

    fn destroy(self, gpu: &gpu::Context) {
        for (texture, view) in [self.frame, self.quarter[0], self.quarter[1]] {
            gpu.destroy_texture_view(view);
            gpu.destroy_texture(texture);
        }
    }
}

#[cfg_attr(target_os = "windows", allow(dead_code))]
pub struct BladeSurfaceConfig {
    pub size: gpu::Extent,
    pub transparent: bool,
}

//Note: we could see some of these fields moved into `BladeContext`
// so that they are shared between windows. E.g. `pipelines`.
// But that is complicated by the fact that pipelines depend on
// the format and alpha mode.
/// Where frames go: a window surface, or on Windows a texture shared with the compositor
/// (the window presents it through DirectComposition, which keeps per-pixel alpha).
enum FrameTarget {
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    Surface(gpu::Surface),
    #[cfg(target_os = "windows")]
    Shared {
        texture: gpu::Texture,
        view: gpu::TextureView,
    },
}

impl FrameTarget {
    fn info(&self) -> gpu::SurfaceInfo {
        match self {
            Self::Surface(surface) => surface.info(),
            #[cfg(target_os = "windows")]
            Self::Shared { .. } => gpu::SurfaceInfo {
                format: SHARED_FORMAT,
                alpha: gpu::AlphaMode::PreMultiplied,
            },
        }
    }
}

/// The format of a shared frame texture (`DXGI_FORMAT_B8G8R8A8_UNORM` on the other side).
#[cfg(target_os = "windows")]
const SHARED_FORMAT: gpu::TextureFormat = gpu::TextureFormat::Bgra8Unorm;

pub struct BladeRenderer {
    gpu: Arc<gpu::Context>,
    target: FrameTarget,
    surface_config: gpu::SurfaceConfig,
    command_encoder: gpu::CommandEncoder,
    last_sync_point: Option<gpu::SyncPoint>,
    pipelines: BladePipelines,
    instance_belt: BufferBelt,
    atlas: Arc<BladeAtlas>,
    atlas_sampler: gpu::Sampler,
    #[cfg(target_os = "macos")]
    core_video_texture_cache: CVMetalTextureCache,
    path_intermediate_texture: gpu::Texture,
    path_intermediate_texture_view: gpu::TextureView,
    path_intermediate_msaa_texture: Option<gpu::Texture>,
    path_intermediate_msaa_texture_view: Option<gpu::TextureView>,
    backdrop_textures: Option<BackdropTextures>,
    rendering_parameters: RenderingParameters,
    /// Imports of `ExternalImage`s, by the address of their source.
    #[cfg(target_os = "windows")]
    external_images: std::collections::HashMap<usize, ExternalImport>,
}

/// An imported `ExternalImage`; no texture when its import failed (it is not retried).
#[cfg(target_os = "windows")]
struct ExternalImport {
    source: std::sync::Weak<dyn std::any::Any + Send + Sync>,
    texture: Option<(gpu::Texture, gpu::TextureView)>,
    /// The `in_use` address of the frame last acquired: each new frame of the source was
    /// released by the exporter and is acquired once.
    frame: usize,
}

impl BladeRenderer {
    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub fn new<I: raw_window_handle::HasWindowHandle + raw_window_handle::HasDisplayHandle>(
        context: &BladeContext,
        window: &I,
        config: BladeSurfaceConfig,
    ) -> anyhow::Result<Self> {
        // Backdrop blurs copy from frames; a surface that cannot be copied from draws none.
        let mut surface_config = gpu::SurfaceConfig {
            size: config.size,
            usage: gpu::TextureUsage::TARGET | gpu::TextureUsage::COPY,
            display_sync: gpu::DisplaySync::Recent,
            color_space: gpu::ColorSpace::Srgb,
            allow_exclusive_full_screen: false,
            transparent: config.transparent,
        };
        let surface = match context
            .gpu
            .create_surface_configured(window, surface_config)
        {
            Ok(surface) => surface,
            Err(_) => {
                surface_config.usage = gpu::TextureUsage::TARGET;
                context
                    .gpu
                    .create_surface_configured(window, surface_config)
                    .map_err(|err| anyhow::anyhow!("Failed to create surface: {err:?}"))?
            }
        };
        Ok(Self::with_target(
            context,
            FrameTarget::Surface(surface),
            surface_config,
        ))
    }

    /// A renderer drawing into the Direct3D 11 texture behind `handle` (an NT handle of a
    /// `D3D11_RESOURCE_MISC_SHARED_NTHANDLE` texture of `size`, `SHARED_FORMAT`). Each `draw`
    /// returns once the frame is complete in the texture.
    #[cfg(target_os = "windows")]
    pub fn new_shared(context: &BladeContext, size: gpu::Extent, handle: isize) -> Self {
        let (texture, view) = import_shared_texture(&context.gpu, size, handle);
        let surface_config = gpu::SurfaceConfig {
            size,
            usage: gpu::TextureUsage::TARGET | gpu::TextureUsage::COPY,
            display_sync: gpu::DisplaySync::Recent,
            color_space: gpu::ColorSpace::Srgb,
            allow_exclusive_full_screen: false,
            transparent: true,
        };
        Self::with_target(
            context,
            FrameTarget::Shared { texture, view },
            surface_config,
        )
    }

    /// Draw into a new shared texture (see `new_shared`), as after a resize.
    #[cfg(target_os = "windows")]
    pub fn set_shared_target(&mut self, size: gpu::Extent, handle: isize) {
        self.wait_for_gpu();
        let (texture, view) = import_shared_texture(&self.gpu, size, handle);
        if let FrameTarget::Shared {
            texture: old_texture,
            view: old_view,
        } = std::mem::replace(&mut self.target, FrameTarget::Shared { texture, view })
        {
            self.gpu.destroy_texture_view(old_view);
            self.gpu.destroy_texture(old_texture);
        }
        self.surface_config.size = size;
        self.recreate_intermediate_textures(size);
    }

    /// Wait until the last frame is complete.
    #[cfg(target_os = "windows")]
    pub fn wait_for_frame(&mut self) {
        self.wait_for_gpu();
    }

    fn with_target(
        context: &BladeContext,
        target: FrameTarget,
        surface_config: gpu::SurfaceConfig,
    ) -> Self {
        let config = &surface_config;
        let command_encoder = context.gpu.create_command_encoder(gpu::CommandEncoderDesc {
            name: "main",
            buffer_count: 2,
        });
        let rendering_parameters = RenderingParameters::from_env(context);
        let pipelines = BladePipelines::new(
            &context.gpu,
            target.info(),
            rendering_parameters.path_sample_count,
        );
        let instance_belt = BufferBelt::new(BufferBeltDescriptor {
            memory: gpu::Memory::Shared,
            min_chunk_size: 0x1000,
            alignment: 0x40, // Vulkan `minStorageBufferOffsetAlignment` on Intel Xe
        });
        let atlas = Arc::new(BladeAtlas::new(&context.gpu));
        let atlas_sampler = context.gpu.create_sampler(gpu::SamplerDesc {
            name: "path rasterization sampler",
            mag_filter: gpu::FilterMode::Linear,
            min_filter: gpu::FilterMode::Linear,
            ..Default::default()
        });

        let (path_intermediate_texture, path_intermediate_texture_view) =
            create_path_intermediate_texture(
                &context.gpu,
                target.info().format,
                config.size.width,
                config.size.height,
            );
        let (path_intermediate_msaa_texture, path_intermediate_msaa_texture_view) =
            create_msaa_texture_if_needed(
                &context.gpu,
                target.info().format,
                config.size.width,
                config.size.height,
                rendering_parameters.path_sample_count,
            )
            .unzip();

        #[cfg(target_os = "macos")]
        let core_video_texture_cache = unsafe {
            CVMetalTextureCache::new(
                objc2::rc::Retained::as_ptr(&context.gpu.metal_device()) as *mut _
            )
            .unwrap()
        };

        Self {
            gpu: Arc::clone(&context.gpu),
            target,
            surface_config,
            command_encoder,
            last_sync_point: None,
            pipelines,
            instance_belt,
            atlas,
            atlas_sampler,
            #[cfg(target_os = "macos")]
            core_video_texture_cache,
            path_intermediate_texture,
            path_intermediate_texture_view,
            path_intermediate_msaa_texture,
            path_intermediate_msaa_texture_view,
            backdrop_textures: None,
            rendering_parameters,
            #[cfg(target_os = "windows")]
            external_images: Default::default(),
        }
    }

    fn wait_for_gpu(&mut self) {
        if let Some(last_sp) = self.last_sync_point.take()
            && !self.gpu.wait_for(&last_sp, MAX_FRAME_TIME_MS)
        {
            log::error!("GPU hung");
            #[cfg(target_os = "linux")]
            if self.gpu.device_information().driver_name == "radv" {
                log::error!(
                    "there's a known bug with amdgpu/radv, try setting ZED_PATH_SAMPLE_COUNT=0 as a workaround"
                );
                log::error!(
                    "if that helps you're running into https://github.com/zed-industries/zed/issues/26143"
                );
            }
            log::error!(
                "your device information is: {:?}",
                self.gpu.device_information()
            );
            while !self.gpu.wait_for(&last_sp, MAX_FRAME_TIME_MS) {}
        }
    }

    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub fn update_drawable_size(&mut self, size: Size<DevicePixels>) {
        self.update_drawable_size_impl(size, false);
    }

    /// Like `update_drawable_size` but skips the check that the size has changed. This is useful in
    /// cases like restoring a window from minimization where the size is the same but the
    /// renderer's swap chain needs to be recreated.
    #[cfg_attr(
        any(
            target_os = "macos",
            target_os = "linux",
            target_os = "freebsd",
            target_os = "windows"
        ),
        allow(dead_code)
    )]
    pub fn update_drawable_size_even_if_unchanged(&mut self, size: Size<DevicePixels>) {
        self.update_drawable_size_impl(size, true);
    }

    #[cfg_attr(target_os = "windows", allow(dead_code))]
    fn update_drawable_size_impl(&mut self, size: Size<DevicePixels>, always_resize: bool) {
        let gpu_size = gpu::Extent {
            width: size.width.0 as u32,
            height: size.height.0 as u32,
            depth: 1,
        };

        if always_resize || gpu_size != self.surface_config.size {
            self.wait_for_gpu();
            self.surface_config.size = gpu_size;
            if let FrameTarget::Surface(surface) = &mut self.target {
                self.gpu.reconfigure_surface(surface, self.surface_config);
            }
            self.recreate_intermediate_textures(gpu_size);
        }
    }

    fn recreate_intermediate_textures(&mut self, gpu_size: gpu::Extent) {
        if let Some(textures) = self.backdrop_textures.take() {
            textures.destroy(&self.gpu);
        }
        self.gpu.destroy_texture(self.path_intermediate_texture);
        self.gpu
            .destroy_texture_view(self.path_intermediate_texture_view);
        if let Some(msaa_texture) = self.path_intermediate_msaa_texture {
            self.gpu.destroy_texture(msaa_texture);
        }
        if let Some(msaa_view) = self.path_intermediate_msaa_texture_view {
            self.gpu.destroy_texture_view(msaa_view);
        }
        let (path_intermediate_texture, path_intermediate_texture_view) =
            create_path_intermediate_texture(
                &self.gpu,
                self.target.info().format,
                gpu_size.width,
                gpu_size.height,
            );
        self.path_intermediate_texture = path_intermediate_texture;
        self.path_intermediate_texture_view = path_intermediate_texture_view;
        let (path_intermediate_msaa_texture, path_intermediate_msaa_texture_view) =
            create_msaa_texture_if_needed(
                &self.gpu,
                self.target.info().format,
                gpu_size.width,
                gpu_size.height,
                self.rendering_parameters.path_sample_count,
            )
            .unzip();
        self.path_intermediate_msaa_texture = path_intermediate_msaa_texture;
        self.path_intermediate_msaa_texture_view = path_intermediate_msaa_texture_view;
    }

    #[cfg_attr(target_os = "windows", allow(dead_code))]
    pub fn update_transparency(&mut self, transparent: bool) {
        // A shared target is always premultiplied: the compositor keeps its alpha.
        if !matches!(self.target, FrameTarget::Surface(_)) {
            return;
        }
        if transparent != self.surface_config.transparent {
            self.wait_for_gpu();
            self.surface_config.transparent = transparent;
            if let FrameTarget::Surface(surface) = &mut self.target {
                self.gpu.reconfigure_surface(surface, self.surface_config);
            }
            self.pipelines.destroy(&self.gpu);
            self.pipelines = BladePipelines::new(
                &self.gpu,
                self.target.info(),
                self.rendering_parameters.path_sample_count,
            );
        }
    }

    #[cfg_attr(
        any(target_os = "macos", feature = "wayland", target_os = "windows"),
        allow(dead_code)
    )]
    pub fn viewport_size(&self) -> gpu::Extent {
        self.surface_config.size
    }

    pub fn sprite_atlas(&self) -> &Arc<BladeAtlas> {
        &self.atlas
    }

    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub fn gpu_specs(&self) -> GpuSpecs {
        let info = self.gpu.device_information();

        GpuSpecs {
            is_software_emulated: info.is_software_emulated,
            device_name: info.device_name.clone(),
            driver_name: info.driver_name.clone(),
            driver_info: info.driver_info.clone(),
            adapter_luid: info.luid,
        }
    }

    #[cfg(target_os = "macos")]
    pub fn layer(&self) -> metal::MetalLayer {
        unsafe { foreign_types::ForeignType::from_ptr(self.layer_ptr()) }
    }

    #[cfg(target_os = "macos")]
    pub fn layer_ptr(&self) -> *mut metal::CAMetalLayer {
        let FrameTarget::Surface(surface) = &self.target;
        objc2::rc::Retained::as_ptr(&surface.metal_layer()) as *mut _
    }

    #[profiling::function]
    fn draw_paths_to_intermediate(
        &mut self,
        paths: &[Path<ScaledPixels>],
        width: f32,
        height: f32,
    ) {
        self.command_encoder
            .init_texture(self.path_intermediate_texture);
        if let Some(msaa_texture) = self.path_intermediate_msaa_texture {
            self.command_encoder.init_texture(msaa_texture);
        }

        let target = if let Some(msaa_view) = self.path_intermediate_msaa_texture_view {
            gpu::RenderTarget {
                view: msaa_view,
                init_op: gpu::InitOp::Clear(gpu::TextureColor::TransparentBlack),
                finish_op: gpu::FinishOp::ResolveTo(self.path_intermediate_texture_view),
            }
        } else {
            gpu::RenderTarget {
                view: self.path_intermediate_texture_view,
                init_op: gpu::InitOp::Clear(gpu::TextureColor::TransparentBlack),
                finish_op: gpu::FinishOp::Store,
            }
        };
        if let mut pass = self.command_encoder.render(
            "rasterize paths",
            gpu::RenderTargetSet {
                colors: &[target],
                depth_stencil: None,
            },
        ) {
            let globals = GlobalParams {
                viewport_size: [width, height],
                premultiplied_alpha: 0,
                pad: 0,
            };
            let mut encoder = pass.with(&self.pipelines.path_rasterization);

            let mut vertices = Vec::new();
            for path in paths {
                vertices.extend(path.vertices.iter().map(|v| PathRasterizationVertex {
                    xy_position: v.xy_position,
                    st_position: v.st_position,
                    color: path.color,
                    bounds: path.clipped_bounds(),
                }));
            }
            let vertex_buf = unsafe { self.instance_belt.alloc_typed(&vertices, &self.gpu) };
            encoder.bind(
                0,
                &ShaderPathRasterizationData {
                    globals,
                    b_path_vertices: vertex_buf,
                },
            );
            encoder.draw(0, vertices.len() as u32, 0, 1);
        }
    }

    pub fn destroy(&mut self) {
        self.wait_for_gpu();
        self.atlas.destroy();
        self.gpu.destroy_sampler(self.atlas_sampler);
        self.instance_belt.destroy(&self.gpu);
        self.gpu.destroy_command_encoder(&mut self.command_encoder);
        self.pipelines.destroy(&self.gpu);
        match &mut self.target {
            FrameTarget::Surface(surface) => self.gpu.destroy_surface(surface),
            #[cfg(target_os = "windows")]
            FrameTarget::Shared { texture, view } => {
                self.gpu.destroy_texture_view(*view);
                self.gpu.destroy_texture(*texture);
            }
        }
        self.gpu.destroy_texture(self.path_intermediate_texture);
        self.gpu
            .destroy_texture_view(self.path_intermediate_texture_view);
        if let Some(msaa_texture) = self.path_intermediate_msaa_texture {
            self.gpu.destroy_texture(msaa_texture);
        }
        if let Some(msaa_view) = self.path_intermediate_msaa_texture_view {
            self.gpu.destroy_texture_view(msaa_view);
        }
        if let Some(textures) = self.backdrop_textures.take() {
            textures.destroy(&self.gpu);
        }
        #[cfg(target_os = "windows")]
        for (_, import) in self.external_images.drain() {
            destroy_external_import(&self.gpu, import);
        }
    }

    /// Import the external images `scene` shows that are not imported yet, and set their
    /// layout, before the frame's first pass. Imports whose source died are freed first: the
    /// previous frame finished (every frame is waited on), so nothing reads them any more.
    #[cfg(target_os = "windows")]
    fn import_external_images(&mut self, scene: &Scene) {
        let dead: Vec<usize> = self
            .external_images
            .iter()
            .filter(|(_, import)| import.source.strong_count() == 0)
            .map(|(key, _)| *key)
            .collect();
        for key in dead {
            if let Some(import) = self.external_images.remove(&key) {
                destroy_external_import(&self.gpu, import);
            }
        }
        for surface in &scene.surfaces {
            let image = &surface.image;
            let key = external_key(image);
            let frame = Arc::as_ptr(&image.in_use) as *const () as usize;
            let family = self.gpu.queue_family_index();
            if let Some(import) = self.external_images.get_mut(&key) {
                if let Some((texture, _)) = import.texture
                    && import.frame != frame
                {
                    self.command_encoder
                        .acquire_external_texture(texture, family);
                    import.frame = frame;
                }
                continue;
            }
            let desc = gpu::TextureDesc {
                name: "external image",
                format: EXTERNAL_FORMAT,
                size: gpu::Extent {
                    width: image.size.width.0.max(0) as u32,
                    height: image.size.height.0.max(0) as u32,
                    depth: 1,
                },
                array_layer_count: 1,
                mip_level_count: 1,
                sample_count: 1,
                dimension: gpu::TextureDimension::D2,
                usage: gpu::TextureUsage::TARGET
                    | gpu::TextureUsage::RESOURCE
                    | gpu::TextureUsage::COPY,
                external: None,
            };
            let texture = match self.gpu.import_win32_texture(
                desc,
                image.handle,
                image.memory_type,
                image.allocation_size,
            ) {
                Ok(texture) => {
                    // Blade keeps textures in GENERAL; the exporter released the image in it.
                    // No barrier from UNDEFINED, which may discard the contents.
                    self.command_encoder
                        .acquire_external_texture(texture, family);
                    let view = self.gpu.create_texture_view(
                        texture,
                        gpu::TextureViewDesc {
                            name: "external image view",
                            format: EXTERNAL_FORMAT,
                            dimension: gpu::ViewDimension::D2,
                            subresources: &Default::default(),
                        },
                    );
                    Some((texture, view))
                }
                Err(e) => {
                    log::error!("external image import failed: {e}");
                    None
                }
            };
            self.external_images.insert(
                key,
                ExternalImport {
                    source: Arc::downgrade(&image.source),
                    texture,
                    frame,
                },
            );
        }
    }

    /// Blur and tint what the frame holds under `blur`: copy that region, average it to
    /// quarter resolution, blur it along each axis, and composite it back upsampled. Runs
    /// between render passes.
    #[profiling::function]
    fn draw_backdrop_blur(
        &mut self,
        blur: &BackdropBlur,
        frame_texture: gpu::Texture,
        frame_view: gpu::TextureView,
    ) {
        let size = self.surface_config.size;
        let clip = blur.bounds.intersect(&blur.content_mask.bounds);
        let region = [
            clip.origin.x.0.floor().max(0.0) as u32,
            clip.origin.y.0.floor().max(0.0) as u32,
            (clip.origin.x.0 + clip.size.width.0)
                .ceil()
                .min(size.width as f32) as u32,
            (clip.origin.y.0 + clip.size.height.0)
                .ceil()
                .min(size.height as f32) as u32,
        ];
        if region[0] >= region[2] || region[1] >= region[3] {
            return;
        }
        let quarter = [
            region[0] / BACKDROP_SCALE,
            region[1] / BACKDROP_SCALE,
            region[2].div_ceil(BACKDROP_SCALE),
            region[3].div_ceil(BACKDROP_SCALE),
        ]
        .map(|v| v as f32);
        if self.backdrop_textures.is_none() {
            let textures = BackdropTextures::new(&self.gpu, self.target.info().format, size);
            // Once, when made: initializing again would discard, without waiting, what an
            // earlier blur of the same frame still reads.
            for (texture, _) in [textures.frame, textures.quarter[0], textures.quarter[1]] {
                self.command_encoder.init_texture(texture);
            }
            self.backdrop_textures = Some(textures);
        }
        let Some(textures) = &self.backdrop_textures else {
            return;
        };
        let (copy, copy_view) = textures.frame;
        let [(_, a_view), (_, b_view)] = textures.quarter;
        let quarter_size = textures.quarter_size.map(|v| v as f32);

        let piece = |texture| gpu::TexturePiece {
            texture,
            mip_level: 0,
            array_layer: 0,
            origin: [region[0], region[1], 0],
        };
        if let mut transfer = self.command_encoder.transfer("backdrop copy") {
            transfer.copy_texture_to_texture(
                piece(frame_texture),
                piece(copy),
                gpu::Extent {
                    width: region[2] - region[0],
                    height: region[3] - region[1],
                    depth: 1,
                },
            );
        }

        let tint = Rgba::from(blur.tint);
        let rect = |b: Bounds<ScaledPixels>| {
            [
                b.origin.x.0,
                b.origin.y.0,
                b.origin.x.0 + b.size.width.0,
                b.origin.y.0 + b.size.height.0,
            ]
        };
        let region = region.map(|v| v as f32);
        let params = BackdropParams {
            dest: quarter,
            source: region,
            tint: [tint.r, tint.g, tint.b, tint.a],
            bounds: rect(blur.bounds),
            content_mask: rect(blur.content_mask.bounds),
            corner_radii: [
                blur.corner_radii.top_left.0,
                blur.corner_radii.top_right.0,
                blur.corner_radii.bottom_right.0,
                blur.corner_radii.bottom_left.0,
            ],
            target_size: quarter_size,
            direction: [0.0; 2],
            // ponytail: a pass reads 3 x 16 texels each way at most, so radii past 64 device
            // pixels stop growing; a coarser quarter step would lift that.
            sigma: (blur.blur_radius.0 / BACKDROP_SCALE as f32).clamp(0.5, 16.0),
            opacity: blur.opacity.clamp(0.0, 1.0),
            pad: [0.0; 2],
        };
        let blur_along = |direction| BackdropParams {
            source: quarter,
            direction,
            ..params
        };
        let passes = [
            (
                &self.pipelines.backdrop_downsample,
                a_view,
                copy_view,
                params,
            ),
            (
                &self.pipelines.backdrop_blur,
                b_view,
                a_view,
                blur_along([1.0, 0.0]),
            ),
            (
                &self.pipelines.backdrop_blur,
                a_view,
                b_view,
                blur_along([0.0, 1.0]),
            ),
        ];
        for (pipeline, target, input, params) in passes {
            if let mut pass = self.command_encoder.render(
                "backdrop blur",
                gpu::RenderTargetSet {
                    colors: &[gpu::RenderTarget {
                        view: target,
                        init_op: gpu::InitOp::Load,
                        finish_op: gpu::FinishOp::Store,
                    }],
                    depth_stencil: None,
                },
            ) {
                let mut encoder = pass.with(pipeline);
                encoder.bind(
                    0,
                    &ShaderBackdropData {
                        backdrop: params,
                        t_backdrop: input,
                    },
                );
                encoder.draw(0, 4, 0, 1);
            }
        }

        if let mut pass = self.command_encoder.render(
            "backdrop composite",
            gpu::RenderTargetSet {
                colors: &[gpu::RenderTarget {
                    view: frame_view,
                    init_op: gpu::InitOp::Load,
                    finish_op: gpu::FinishOp::Store,
                }],
                depth_stencil: None,
            },
        ) {
            let mut encoder = pass.with(&self.pipelines.backdrop_composite);
            encoder.bind(
                0,
                &ShaderBackdropCompositeData {
                    backdrop: BackdropParams {
                        dest: region,
                        source: quarter,
                        target_size: [size.width as f32, size.height as f32],
                        ..params
                    },
                    t_backdrop: a_view,
                    t_backdrop_frame: copy_view,
                },
            );
            encoder.draw(0, 4, 0, 1);
        }
    }

    pub fn draw(&mut self, scene: &Scene) {
        self.command_encoder.start();
        self.atlas.before_frame(&mut self.command_encoder);

        let (frame, frame_texture, frame_view) = match &mut self.target {
            FrameTarget::Surface(surface) => {
                profiling::scope!("acquire frame");
                let frame = surface.acquire_frame();
                let (texture, view) = (frame.texture(), frame.texture_view());
                (Some(frame), texture, view)
            }
            #[cfg(target_os = "windows")]
            FrameTarget::Shared { texture, view } => (None, *texture, *view),
        };
        self.command_encoder.init_texture(frame_texture);
        #[cfg(target_os = "windows")]
        self.import_external_images(scene);

        let globals = GlobalParams {
            viewport_size: [
                self.surface_config.size.width as f32,
                self.surface_config.size.height as f32,
            ],
            premultiplied_alpha: match self.target.info().alpha {
                gpu::AlphaMode::Ignored | gpu::AlphaMode::PostMultiplied => 0,
                gpu::AlphaMode::PreMultiplied => 1,
            },
            pad: 0,
        };

        let mut pass = self.command_encoder.render(
            "main",
            gpu::RenderTargetSet {
                colors: &[gpu::RenderTarget {
                    view: frame_view,
                    init_op: gpu::InitOp::Clear(gpu::TextureColor::TransparentBlack),
                    finish_op: gpu::FinishOp::Store,
                }],
                depth_stencil: None,
            },
        );

        profiling::scope!("render pass");
        for batch in scene.batches() {
            match batch {
                PrimitiveBatch::Quads(quads) => {
                    let instance_buf = unsafe { self.instance_belt.alloc_typed(quads, &self.gpu) };
                    let mut encoder = pass.with(&self.pipelines.quads);
                    encoder.bind(
                        0,
                        &ShaderQuadsData {
                            globals,
                            b_quads: instance_buf,
                        },
                    );
                    encoder.draw(0, 4, 0, quads.len() as u32);
                }
                PrimitiveBatch::Shadows(shadows) => {
                    let instance_buf =
                        unsafe { self.instance_belt.alloc_typed(shadows, &self.gpu) };
                    let mut encoder = pass.with(&self.pipelines.shadows);
                    encoder.bind(
                        0,
                        &ShaderShadowsData {
                            globals,
                            b_shadows: instance_buf,
                        },
                    );
                    encoder.draw(0, 4, 0, shadows.len() as u32);
                }
                PrimitiveBatch::Paths(paths) => {
                    let Some(first_path) = paths.first() else {
                        continue;
                    };
                    drop(pass);
                    self.draw_paths_to_intermediate(
                        paths,
                        self.surface_config.size.width as f32,
                        self.surface_config.size.height as f32,
                    );
                    pass = self.command_encoder.render(
                        "main",
                        gpu::RenderTargetSet {
                            colors: &[gpu::RenderTarget {
                                view: frame_view,
                                init_op: gpu::InitOp::Load,
                                finish_op: gpu::FinishOp::Store,
                            }],
                            depth_stencil: None,
                        },
                    );
                    let mut encoder = pass.with(&self.pipelines.paths);
                    // When copying paths from the intermediate texture to the drawable,
                    // each pixel must only be copied once, in case of transparent paths.
                    //
                    // If all paths have the same draw order, then their bounds are all
                    // disjoint, so we can copy each path's bounds individually. If this
                    // batch combines different draw orders, we perform a single copy
                    // for a minimal spanning rect.
                    let sprites = if paths.last().unwrap().order == first_path.order {
                        paths
                            .iter()
                            .map(|path| PathSprite {
                                bounds: path.clipped_bounds(),
                            })
                            .collect()
                    } else {
                        let mut bounds = first_path.clipped_bounds();
                        for path in paths.iter().skip(1) {
                            bounds = bounds.union(&path.clipped_bounds());
                        }
                        vec![PathSprite { bounds }]
                    };
                    let instance_buf =
                        unsafe { self.instance_belt.alloc_typed(&sprites, &self.gpu) };
                    encoder.bind(
                        0,
                        &ShaderPathsData {
                            globals,
                            t_sprite: self.path_intermediate_texture_view,
                            s_sprite: self.atlas_sampler,
                            b_path_sprites: instance_buf,
                        },
                    );
                    encoder.draw(0, 4, 0, sprites.len() as u32);
                }
                PrimitiveBatch::Underlines(underlines) => {
                    let instance_buf =
                        unsafe { self.instance_belt.alloc_typed(underlines, &self.gpu) };
                    let mut encoder = pass.with(&self.pipelines.underlines);
                    encoder.bind(
                        0,
                        &ShaderUnderlinesData {
                            globals,
                            b_underlines: instance_buf,
                        },
                    );
                    encoder.draw(0, 4, 0, underlines.len() as u32);
                }
                PrimitiveBatch::MonochromeSprites {
                    texture_id,
                    sprites,
                } => {
                    let tex_info = self.atlas.get_texture_info(texture_id);
                    let instance_buf =
                        unsafe { self.instance_belt.alloc_typed(sprites, &self.gpu) };
                    let mut encoder = pass.with(&self.pipelines.mono_sprites);
                    encoder.bind(
                        0,
                        &ShaderMonoSpritesData {
                            globals,
                            gamma_ratios: self.rendering_parameters.gamma_ratios,
                            grayscale_enhanced_contrast: self
                                .rendering_parameters
                                .grayscale_enhanced_contrast,
                            t_sprite: tex_info.raw_view,
                            s_sprite: self.atlas_sampler,
                            b_mono_sprites: instance_buf,
                        },
                    );
                    encoder.draw(0, 4, 0, sprites.len() as u32);
                }
                PrimitiveBatch::PolychromeSprites {
                    texture_id,
                    sprites,
                } => {
                    let tex_info = self.atlas.get_texture_info(texture_id);
                    let instance_buf =
                        unsafe { self.instance_belt.alloc_typed(sprites, &self.gpu) };
                    let mut encoder = pass.with(&self.pipelines.poly_sprites);
                    encoder.bind(
                        0,
                        &ShaderPolySpritesData {
                            globals,
                            t_sprite: tex_info.raw_view,
                            s_sprite: self.atlas_sampler,
                            b_poly_sprites: instance_buf,
                        },
                    );
                    encoder.draw(0, 4, 0, sprites.len() as u32);
                }
                PrimitiveBatch::BackdropBlurs(blurs) => {
                    if !self.surface_config.usage.contains(gpu::TextureUsage::COPY) {
                        continue;
                    }
                    drop(pass);
                    for blur in blurs {
                        self.draw_backdrop_blur(blur, frame_texture, frame_view);
                    }
                    pass = self.command_encoder.render(
                        "main",
                        gpu::RenderTargetSet {
                            colors: &[gpu::RenderTarget {
                                view: frame_view,
                                init_op: gpu::InitOp::Load,
                                finish_op: gpu::FinishOp::Store,
                            }],
                            depth_stencil: None,
                        },
                    );
                }
                #[cfg(target_os = "windows")]
                PrimitiveBatch::Surfaces(surfaces) => {
                    let mut encoder = pass.with(&self.pipelines.poly_sprites);
                    for surface in surfaces {
                        let Some((_, view)) = self
                            .external_images
                            .get(&external_key(&surface.image))
                            .and_then(|import| import.texture)
                        else {
                            continue;
                        };
                        // The whole image as one polychrome sprite (straight alpha, as the
                        // atlas holds), untransformed and unmasked.
                        let sprite = PolychromeSprite {
                            order: surface.order,
                            pad: 0,
                            grayscale: false,
                            opacity: 1.0,
                            bounds: surface.bounds,
                            content_mask: surface.content_mask.clone(),
                            corner_radii: Default::default(),
                            tile: crate::AtlasTile {
                                texture_id: crate::AtlasTextureId {
                                    index: 0,
                                    kind: crate::AtlasTextureKind::Polychrome,
                                },
                                tile_id: crate::TileId(0),
                                padding: 0,
                                bounds: Bounds {
                                    origin: Point::default(),
                                    size: surface.image.size,
                                },
                            },
                            mask: surface.bounds,
                            transformation: crate::TransformationMatrix::unit(),
                        };
                        let instance_buf =
                            unsafe { self.instance_belt.alloc_typed(&[sprite], &self.gpu) };
                        encoder.bind(
                            0,
                            &ShaderPolySpritesData {
                                globals,
                                t_sprite: view,
                                s_sprite: self.atlas_sampler,
                                b_poly_sprites: instance_buf,
                            },
                        );
                        encoder.draw(0, 4, 0, 1);
                    }
                }
                #[cfg(not(target_os = "windows"))]
                PrimitiveBatch::Surfaces(surfaces) => {
                    let mut _encoder = pass.with(&self.pipelines.surfaces);

                    for surface in surfaces {
                        #[cfg(not(target_os = "macos"))]
                        {
                            let _ = surface;
                            continue;
                        };

                        #[cfg(target_os = "macos")]
                        {
                            let (t_y, t_cb_cr) = unsafe {
                                use core_foundation::base::TCFType as _;
                                use std::ptr;

                                assert_eq!(
                                        surface.image_buffer.get_pixel_format(),
                                        core_video::pixel_buffer::kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
                                    );

                                let y_texture = self
                                    .core_video_texture_cache
                                    .create_texture_from_image(
                                        surface.image_buffer.as_concrete_TypeRef(),
                                        ptr::null(),
                                        metal::MTLPixelFormat::R8Unorm,
                                        surface.image_buffer.get_width_of_plane(0),
                                        surface.image_buffer.get_height_of_plane(0),
                                        0,
                                    )
                                    .unwrap();
                                let cb_cr_texture = self
                                    .core_video_texture_cache
                                    .create_texture_from_image(
                                        surface.image_buffer.as_concrete_TypeRef(),
                                        ptr::null(),
                                        metal::MTLPixelFormat::RG8Unorm,
                                        surface.image_buffer.get_width_of_plane(1),
                                        surface.image_buffer.get_height_of_plane(1),
                                        1,
                                    )
                                    .unwrap();
                                (
                                    gpu::TextureView::from_metal_texture(
                                        &objc2::rc::Retained::retain(
                                            foreign_types::ForeignTypeRef::as_ptr(
                                                y_texture.as_texture_ref(),
                                            )
                                                as *mut objc2::runtime::ProtocolObject<
                                                    dyn objc2_metal::MTLTexture,
                                                >,
                                        )
                                        .unwrap(),
                                        gpu::TexelAspects::COLOR,
                                    ),
                                    gpu::TextureView::from_metal_texture(
                                        &objc2::rc::Retained::retain(
                                            foreign_types::ForeignTypeRef::as_ptr(
                                                cb_cr_texture.as_texture_ref(),
                                            )
                                                as *mut objc2::runtime::ProtocolObject<
                                                    dyn objc2_metal::MTLTexture,
                                                >,
                                        )
                                        .unwrap(),
                                        gpu::TexelAspects::COLOR,
                                    ),
                                )
                            };

                            _encoder.bind(
                                0,
                                &ShaderSurfacesData {
                                    globals,
                                    surface_locals: SurfaceParams {
                                        bounds: surface.bounds.into(),
                                        content_mask: surface.content_mask.bounds.into(),
                                    },
                                    t_y,
                                    t_cb_cr,
                                    s_surface: self.atlas_sampler,
                                },
                            );

                            _encoder.draw(0, 4, 0, 1);
                        }
                    }
                }
            }
        }
        drop(pass);

        if let Some(frame) = frame {
            self.command_encoder.present(frame);
        }
        let sync_point = self.gpu.submit(&mut self.command_encoder);

        profiling::scope!("finish");
        self.instance_belt.flush(&sync_point);
        self.atlas.after_frame(&sync_point);

        self.wait_for_gpu();
        self.last_sync_point = Some(sync_point);
        // The compositor reads a shared target as soon as `draw` returns.
        // ponytail: a CPU wait per frame; a shared fence or keyed mutex would overlap the
        // copy with the next frame if the wait shows up in profiles.
        #[cfg(target_os = "windows")]
        if matches!(self.target, FrameTarget::Shared { .. }) {
            self.wait_for_gpu();
        }
    }
}

/// The format of an `ExternalImage` (`VK_FORMAT_B8G8R8A8_UNORM` on the exporter's side).
#[cfg(target_os = "windows")]
const EXTERNAL_FORMAT: gpu::TextureFormat = gpu::TextureFormat::Bgra8Unorm;

/// The import cache key of an external image: its source's address.
#[cfg(target_os = "windows")]
fn external_key(image: &crate::ExternalImage) -> usize {
    Arc::as_ptr(&image.source) as *const () as usize
}

#[cfg(target_os = "windows")]
fn destroy_external_import(gpu: &gpu::Context, import: ExternalImport) {
    if let Some((texture, view)) = import.texture {
        gpu.destroy_texture_view(view);
        gpu.destroy_texture(texture);
    }
}

/// Import the shared Direct3D 11 frame texture behind `handle` as a render target.
#[cfg(target_os = "windows")]
fn import_shared_texture(
    gpu: &gpu::Context,
    size: gpu::Extent,
    handle: isize,
) -> (gpu::Texture, gpu::TextureView) {
    let texture = gpu.create_texture(gpu::TextureDesc {
        name: "shared frame",
        format: SHARED_FORMAT,
        size,
        array_layer_count: 1,
        mip_level_count: 1,
        sample_count: 1,
        dimension: gpu::TextureDimension::D2,
        // Backdrop blurs copy from it.
        usage: gpu::TextureUsage::TARGET | gpu::TextureUsage::COPY,
        external: Some(gpu::ExternalMemorySource::D3D11Texture(handle)),
    });
    let view = gpu.create_texture_view(
        texture,
        gpu::TextureViewDesc {
            name: "shared frame view",
            format: SHARED_FORMAT,
            dimension: gpu::ViewDimension::D2,
            subresources: &Default::default(),
        },
    );
    (texture, view)
}

fn create_path_intermediate_texture(
    gpu: &gpu::Context,
    format: gpu::TextureFormat,
    width: u32,
    height: u32,
) -> (gpu::Texture, gpu::TextureView) {
    let texture = gpu.create_texture(gpu::TextureDesc {
        name: "path intermediate",
        format,
        size: gpu::Extent {
            width,
            height,
            depth: 1,
        },
        array_layer_count: 1,
        mip_level_count: 1,
        sample_count: 1,
        dimension: gpu::TextureDimension::D2,
        usage: gpu::TextureUsage::COPY | gpu::TextureUsage::RESOURCE | gpu::TextureUsage::TARGET,
        external: None,
    });
    let texture_view = gpu.create_texture_view(
        texture,
        gpu::TextureViewDesc {
            name: "path intermediate view",
            format,
            dimension: gpu::ViewDimension::D2,
            subresources: &Default::default(),
        },
    );
    (texture, texture_view)
}

fn create_msaa_texture_if_needed(
    gpu: &gpu::Context,
    format: gpu::TextureFormat,
    width: u32,
    height: u32,
    sample_count: u32,
) -> Option<(gpu::Texture, gpu::TextureView)> {
    if sample_count <= 1 {
        return None;
    }
    let texture_msaa = gpu.create_texture(gpu::TextureDesc {
        name: "path intermediate msaa",
        format,
        size: gpu::Extent {
            width,
            height,
            depth: 1,
        },
        array_layer_count: 1,
        mip_level_count: 1,
        sample_count,
        dimension: gpu::TextureDimension::D2,
        usage: gpu::TextureUsage::TARGET,
        external: None,
    });
    let texture_view_msaa = gpu.create_texture_view(
        texture_msaa,
        gpu::TextureViewDesc {
            name: "path intermediate msaa view",
            format,
            dimension: gpu::ViewDimension::D2,
            subresources: &Default::default(),
        },
    );

    Some((texture_msaa, texture_view_msaa))
}

/// A set of parameters that can be set using a corresponding environment variable.
struct RenderingParameters {
    // Env var: ZED_PATH_SAMPLE_COUNT
    // workaround for https://github.com/zed-industries/zed/issues/26143
    path_sample_count: u32,

    // Env var: ZED_FONTS_GAMMA
    // Allowed range [1.0, 2.2], other values are clipped
    // Default: 1.8
    gamma_ratios: [f32; 4],
    // Env var: ZED_FONTS_GRAYSCALE_ENHANCED_CONTRAST
    // Allowed range: [0.0, ..), other values are clipped
    // Default: 1.0
    grayscale_enhanced_contrast: f32,
}

impl RenderingParameters {
    fn from_env(context: &BladeContext) -> Self {
        use std::env;

        let path_sample_count = env::var("ZED_PATH_SAMPLE_COUNT")
            .ok()
            .and_then(|v| v.parse().ok())
            .or_else(|| {
                [4, 2, 1]
                    .into_iter()
                    .find(|&n| (context.gpu.capabilities().sample_count_mask & n) != 0)
            })
            .unwrap_or(1);
        // Windows defaults to the system's DirectWrite text gamma and contrast.
        #[cfg(target_os = "windows")]
        let (default_gamma, default_contrast) = crate::system_text_gamma_and_contrast();
        #[cfg(not(target_os = "windows"))]
        let (default_gamma, default_contrast) = (1.8_f32, 1.0_f32);
        let gamma = env::var("ZED_FONTS_GAMMA")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default_gamma)
            .clamp(1.0, 2.2);
        let gamma_ratios = Self::get_gamma_ratios(gamma);
        let grayscale_enhanced_contrast = env::var("ZED_FONTS_GRAYSCALE_ENHANCED_CONTRAST")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default_contrast)
            .max(0.0);

        Self {
            path_sample_count,
            gamma_ratios,
            grayscale_enhanced_contrast,
        }
    }

    // Gamma ratios for brightening/darkening edges for better contrast
    // https://github.com/microsoft/terminal/blob/1283c0f5b99a2961673249fa77c6b986efb5086c/src/renderer/atlas/dwrite.cpp#L50
    fn get_gamma_ratios(gamma: f32) -> [f32; 4] {
        const GAMMA_INCORRECT_TARGET_RATIOS: [[f32; 4]; 13] = [
            [0.0000 / 4.0, 0.0000 / 4.0, 0.0000 / 4.0, 0.0000 / 4.0], // gamma = 1.0
            [0.0166 / 4.0, -0.0807 / 4.0, 0.2227 / 4.0, -0.0751 / 4.0], // gamma = 1.1
            [0.0350 / 4.0, -0.1760 / 4.0, 0.4325 / 4.0, -0.1370 / 4.0], // gamma = 1.2
            [0.0543 / 4.0, -0.2821 / 4.0, 0.6302 / 4.0, -0.1876 / 4.0], // gamma = 1.3
            [0.0739 / 4.0, -0.3963 / 4.0, 0.8167 / 4.0, -0.2287 / 4.0], // gamma = 1.4
            [0.0933 / 4.0, -0.5161 / 4.0, 0.9926 / 4.0, -0.2616 / 4.0], // gamma = 1.5
            [0.1121 / 4.0, -0.6395 / 4.0, 1.1588 / 4.0, -0.2877 / 4.0], // gamma = 1.6
            [0.1300 / 4.0, -0.7649 / 4.0, 1.3159 / 4.0, -0.3080 / 4.0], // gamma = 1.7
            [0.1469 / 4.0, -0.8911 / 4.0, 1.4644 / 4.0, -0.3234 / 4.0], // gamma = 1.8
            [0.1627 / 4.0, -1.0170 / 4.0, 1.6051 / 4.0, -0.3347 / 4.0], // gamma = 1.9
            [0.1773 / 4.0, -1.1420 / 4.0, 1.7385 / 4.0, -0.3426 / 4.0], // gamma = 2.0
            [0.1908 / 4.0, -1.2652 / 4.0, 1.8650 / 4.0, -0.3476 / 4.0], // gamma = 2.1
            [0.2031 / 4.0, -1.3864 / 4.0, 1.9851 / 4.0, -0.3501 / 4.0], // gamma = 2.2
        ];

        const NORM13: f32 = ((0x10000 as f64) / (255.0 * 255.0) * 4.0) as f32;
        const NORM24: f32 = ((0x100 as f64) / (255.0) * 4.0) as f32;

        let index = ((gamma * 10.0).round() as usize).clamp(10, 22) - 10;
        let ratios = GAMMA_INCORRECT_TARGET_RATIOS[index];

        [
            ratios[0] * NORM13,
            ratios[1] * NORM24,
            ratios[2] * NORM13,
            ratios[3] * NORM24,
        ]
    }
}
