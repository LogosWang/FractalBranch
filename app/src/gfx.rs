//! 渲染设备。用 WARP（软件 D3D）+ DirectComposition：
//! 第 0 阶段实测，同样的面板硬件路径私有内存约 57 MB，这条路径约 5 MB，挂载快约 10 倍。

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::*;

pub struct Gfx {
    pub dwrite: IDWriteFactory,
    _d2d: ID2D1Factory1,
    pub ctx: ID2D1DeviceContext,
    dxgi: IDXGIDevice,
    factory: IDXGIFactory2,
    comp: IDCompositionDevice,
    _d3d: ID3D11Device,
    device: ID2D1Device,
    context3: Option<ID3D11DeviceContext>,
}

pub struct Surface {
    swap: IDXGISwapChain1,
    bitmap: Option<ID2D1Bitmap1>,
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
    pub w: u32,
    pub h: u32,
}

impl Gfx {
    pub fn new() -> Result<Self> {
        unsafe {
            let mut device: Option<ID3D11Device> = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_SINGLETHREADED,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )?;
            let d3d = device.ok_or_else(|| Error::from(E_FAIL))?;
            let dxgi: IDXGIDevice = d3d.cast()?;
            let factory: IDXGIFactory2 = dxgi.GetAdapter()?.GetParent()?;
            let d2d: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let d2d_device = d2d.CreateDevice(&dxgi)?;
            let ctx = d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
            let context3 = d3d.GetImmediateContext().ok();
                        let comp: IDCompositionDevice = DCompositionCreateDevice(&dxgi)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            crate::view::apply_text_params(&ctx, &dwrite);
            Ok(Self { dwrite, _d2d: d2d, ctx, dxgi, factory, comp, _d3d: d3d, device: d2d_device, context3 })
        }
    }

    pub fn surface(&self, hwnd: HWND, w: u32, h: u32) -> Result<Surface> {
        let (w, h) = (w.max(1), h.max(1));
        unsafe {
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: w,
                Height: h,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                ..Default::default()
            };
            let swap = self.factory.CreateSwapChainForComposition(&self.dxgi, &desc, None)?;
            let target = self.comp.CreateTargetForHwnd(hwnd, false)?;
            let visual = self.comp.CreateVisual()?;
            visual.SetContent(&swap)?;
            target.SetRoot(&visual)?;
            let mut s = Surface { swap, bitmap: None, _target: target, _visual: visual, w, h };
            self.bind(&mut s)?;
            Ok(s)
        }
    }

    fn bind(&self, s: &mut Surface) -> Result<()> {
        unsafe {
            let buffer: IDXGISurface = s.swap.GetBuffer(0)?;
            let bitmap = self.ctx.CreateBitmapFromDxgiSurface(
                &buffer,
                Some(&D2D1_BITMAP_PROPERTIES1 {
                    pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                    dpiX: 96.0,
                    dpiY: 96.0,
                    bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                    colorContext: ManuallyDrop::new(None),
                }),
            )?;
            s.bitmap = Some(bitmap);
        }
        Ok(())
    }

    pub fn resize(&self, s: &mut Surface, w: u32, h: u32) -> Result<()> {
        let (w, h) = (w.max(1), h.max(1));
        if (w, h) == (s.w, s.h) {
            return Ok(());
        }
        unsafe {
            // ResizeBuffers 要求先放掉所有对缓冲区的引用。
            self.ctx.SetTarget(None);
            s.bitmap = None;
            s.swap.ResizeBuffers(0, w, h, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0))?;
        }
        s.w = w;
        s.h = h;
        self.bind(s)
    }

    /// 空闲时调用：放掉 D2D 的字形和纹理缓存，以及驱动里的临时分配。下次绘制时会按需重建。
    pub fn trim(&self) {
        unsafe {
            self.device.ClearResources(0);
            if let Some(c) = &self.context3 {
                c.ClearState();
                c.Flush();
            }
            if let Ok(d) = self.dxgi.cast::<IDXGIDevice3>() {
                d.Trim();
            }
        }
    }

    /// 在表面上绘制一帧。`dpi` 决定绘制坐标的单位：传实际 DPI，绘制代码就可以一律用 DIP。
    pub fn frame(&self, s: &mut Surface, dpi: f32, draw: impl FnOnce(&ID2D1RenderTarget)) -> Result<()> {
        let Some(bitmap) = s.bitmap.as_ref() else { return Ok(()) };
        unsafe {
            self.ctx.SetTarget(bitmap);
            self.ctx.SetDpi(dpi, dpi);
            self.ctx.BeginDraw();
            self.ctx.Clear(Some(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }));
            draw(&self.ctx);
            let r = self.ctx.EndDraw(None, None);
            self.ctx.SetTarget(None);
            r?;
            s.swap.Present(0, DXGI_PRESENT(0)).ok()?;
            self.comp.Commit()?;
        }
        Ok(())
    }
}
