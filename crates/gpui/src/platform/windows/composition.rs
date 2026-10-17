//! Presents the frames Blade renders on Vulkan through DirectComposition.
//!
//! A Vulkan swapchain does not keep per-pixel window alpha on every driver (NVIDIA's drops
//! premultiplied composite alpha once the window's background appearance changes), while a
//! DXGI composition swap chain does. So Blade draws each frame into a Direct3D 11 texture shared
//! with Vulkan, and this presenter copies it into the swap chain's back buffer and presents it
//! on the window's composition visual. Direct3D does no rendering here.

use anyhow::{Context as _, Result};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HMODULE, HWND, LUID},
        Graphics::{
            Direct3D::{
                D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_11_1,
            },
            Direct3D11::*,
            DirectComposition::*,
            Dxgi::{Common::*, *},
        },
    },
    core::Interface,
};

/// The frame format on both sides of the shared texture (`Bgra8Unorm` in Blade).
const FORMAT: DXGI_FORMAT = DXGI_FORMAT_B8G8R8A8_UNORM;

pub(crate) struct CompositionPresenter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swap_chain: IDXGISwapChain1,
    shared: ID3D11Texture2D,
    /// NT handle of `shared`, for the renderer to import.
    shared_handle: HANDLE,
    // Kept alive: the window shows the swap chain through them.
    _comp_device: IDCompositionDevice,
    _comp_target: IDCompositionTarget,
    _comp_visual: IDCompositionVisual,
}

impl CompositionPresenter {
    /// A presenter for `hwnd` (created with `WS_EX_NOREDIRECTIONBITMAP`) of `width` x `height`
    /// pixels, on the adapter with `luid` (the one Vulkan renders on) or the default adapter.
    pub(crate) fn new(hwnd: HWND, luid: Option<[u8; 8]>, width: u32, height: u32) -> Result<Self> {
        let factory: IDXGIFactory4 =
            unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS::default()) }
                .context("Creating DXGI factory")?;
        let adapter: IDXGIAdapter = match luid {
            Some(bytes) => unsafe {
                factory.EnumAdapterByLuid(LUID {
                    LowPart: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
                    HighPart: i32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
                })
            }
            .context("Finding the Vulkan adapter through DXGI")?,
            None => unsafe { factory.EnumAdapters(0) }.context("Finding a DXGI adapter")?,
        };
        let (mut device, mut context) = (None, None);
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[
                    D3D_FEATURE_LEVEL_11_1,
                    D3D_FEATURE_LEVEL_11_0,
                    D3D_FEATURE_LEVEL_10_1,
                ]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .context("Creating the Direct3D 11 presentation device")?;
        let (device, context): (ID3D11Device, ID3D11DeviceContext) =
            (device.unwrap(), context.unwrap());

        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width,
            Height: height,
            Format: FORMAT,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            // Composition swap chains only support stretch scaling.
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
            Flags: 0,
        };
        let swap_chain = unsafe { factory.CreateSwapChainForComposition(&device, &desc, None) }
            .context("Creating the composition swap chain")?;

        let dxgi_device: IDXGIDevice = device.cast()?;
        let comp_device: IDCompositionDevice = unsafe { DCompositionCreateDevice(&dxgi_device) }
            .context("Creating the DirectComposition device")?;
        let comp_target = unsafe { comp_device.CreateTargetForHwnd(hwnd, true) }?;
        let comp_visual = unsafe { comp_device.CreateVisual() }?;
        unsafe {
            comp_visual.SetContent(&swap_chain)?;
            comp_target.SetRoot(&comp_visual)?;
            comp_device.Commit()?;
        }

        let (shared, shared_handle) = create_shared_texture(&device, width, height)?;
        Ok(Self {
            device,
            context,
            swap_chain,
            shared,
            shared_handle,
            _comp_device: comp_device,
            _comp_target: comp_target,
            _comp_visual: comp_visual,
        })
    }

    /// The NT handle of the texture the renderer draws frames into.
    pub(crate) fn shared_handle(&self) -> isize {
        self.shared_handle.0 as isize
    }

    /// Resize the swap chain and replace the shared texture; the renderer imports the new
    /// `shared_handle`.
    pub(crate) fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        unsafe {
            self.context.ClearState();
            self.swap_chain.ResizeBuffers(
                0,
                width,
                height,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(0),
            )
        }
        .context("Resizing the composition swap chain")?;
        let (shared, handle) = create_shared_texture(&self.device, width, height)?;
        self.shared = shared;
        // Imported memory holds its own reference, so the old handle can go.
        unsafe { CloseHandle(std::mem::replace(&mut self.shared_handle, handle)) }?;
        Ok(())
    }

    /// Present the frame the renderer finished in the shared texture.
    pub(crate) fn present(&self) -> Result<()> {
        unsafe {
            let back_buffer: ID3D11Texture2D = self.swap_chain.GetBuffer(0)?;
            self.context.CopyResource(&back_buffer, &self.shared);
            self.swap_chain.Present(0, DXGI_PRESENT(0)).ok()?;
        }
        Ok(())
    }
}

impl Drop for CompositionPresenter {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.shared_handle);
        }
    }
}

/// A texture Vulkan can render into and Direct3D 11 can copy from, with its NT handle.
fn create_shared_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<(ID3D11Texture2D, HANDLE)> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: FORMAT,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0) as u32,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .context("Creating the shared frame texture")?;
    let texture = texture.unwrap();
    let resource: IDXGIResource1 = texture.cast()?;
    let handle = unsafe {
        resource.CreateSharedHandle(
            None,
            (DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE).0,
            None,
        )
    }
    .context("Sharing the frame texture")?;
    Ok((texture, handle))
}
