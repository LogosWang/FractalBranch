//! 把当前布局画到一张 PNG。不创建窗口、不隐藏图标，用于在不打扰桌面的情况下检查界面。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::*;
use windows::core::*;

use crate::app::{load_layout, refresh_meta, system_scale, work_area};
use crate::icons;
use crate::layout;
use crate::util;
use crate::view::{self, Hit, PanelState, Res};

pub fn run(out: &str) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let roots = util::roots();
        let dir = util::data_dir();
        let mut l = load_layout(&dir.join("layout.json"), &roots);
        let entries = layout::scan(&roots).map_err(|_| Error::from(E_FAIL))?;
        l.reconcile(&entries, &HashMap::new());
        let meta = refresh_meta(&entries);

        let scale = system_scale();
        let work = work_area(scale);
        let (wpx, hpx) = ((work.0 * scale) as u32, (work.1 * scale) as u32);

        let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let bitmap = wic.CreateBitmap(wpx, hpx, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnDemand)?;
        let d2d: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0 * scale,
            dpiY: 96.0 * scale,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let target = d2d.CreateWicBitmapRenderTarget(&bitmap, &props)?;
        let rt: &ID2D1RenderTarget = &target;

        let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        view::apply_text_params(rt, &dwrite);
        let mut res = Res::new(dwrite, scale, roots, &l.style)?;
        let loader = icons::spawn(dir.join("icons.cache"), None);

        let empty = Default::default();
        let state = |scroll: f32| PanelState { hover: Hit::None, selected: &empty, scroll, drop_hot: false, band: None };
        let cells = view::desktop_cells(l.desktop.items.len(), &l.desktop, &l.style, work, &l.partitions);

        let draw_all = |rt: &ID2D1RenderTarget, res: &mut Res| -> Result<()> {
            rt.BeginDraw();
            // 明暗相间的条纹：可读性要在最不利的背景上也过关。
            rt.Clear(Some(&D2D1_COLOR_F { r: 0.30, g: 0.55, b: 0.85, a: 1.0 }));
            rt.SetTransform(&windows_numerics_translate(0.0, 0.0));
            if let Ok(white) = rt.CreateSolidColorBrush(&D2D1_COLOR_F { r: 0.97, g: 0.97, b: 0.95, a: 1.0 }, None) {
                for i in 0..8 {
                    let x = i as f32 * 300.0;
                    rt.FillRectangle(&D2D_RECT_F { left: x, top: 0.0, right: x + 150.0, bottom: work.1 }, &white);
                }
            }
            view::draw_desktop(rt, res, &l.desktop, &cells, &meta, &state(0.0), &loader)?;
            for p in &l.partitions {
                draw_tree(rt, res, &l.style, p, p.x, p.y, &meta, &state(0.0), &loader)?;
            }
            rt.EndDraw(None, None)
        };

        // 第一遍发出图标请求，等结果到齐再画第二遍。
        draw_all(rt, &mut res)?;
        // 超出分区可视范围的图标不会被请求，所以没有确定的总数，安静一会儿就算到齐。
        let mut got = 0;
        let deadline = Instant::now() + Duration::from_secs(240);
        while Instant::now() < deadline {
            let Ok(i) = loader.rx.recv_timeout(Duration::from_secs(6)) else { break };
            res.accept(i);
            got += 1;
        }
        util::log(format!("预览：收到图标 {got} 个"));
        draw_all(rt, &mut res)?;

        let name = util::wide(out);
        let stream = wic.CreateStream()?;
        stream.InitializeFromFilename(PCWSTR(name.as_ptr()), GENERIC_WRITE.0)?;
        let encoder = wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
        let mut frame = None;
        encoder.CreateNewFrame(&mut frame, std::ptr::null_mut())?;
        let frame = frame.ok_or_else(|| Error::from(E_FAIL))?;
        frame.Initialize(None)?;
        frame.WriteSource(&bitmap, std::ptr::null())?;
        frame.Commit()?;
        encoder.Commit()?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_tree(rt: &ID2D1RenderTarget, res: &mut Res, style: &layout::Style, p: &layout::Partition, x: f32, y: f32, meta: &view::MetaMap, st: &PanelState, loader: &icons::Loader) -> Result<()> {
    unsafe { rt.SetTransform(&windows_numerics_translate(x, y)) };
    let h = if p.collapsed { view::HEADER_H } else { p.h };
    view::draw_partition(rt, res, style, p, meta, st, loader, p.w, h)?;
    if !p.collapsed {
        let top = view::content_top(p);
        for c in &p.tabs[p.active_tab()].children {
            unsafe { draw_tree(rt, res, style, c, x + c.x, y + top + c.y, meta, st, loader)? };
        }
    }
    Ok(())
}

fn windows_numerics_translate(x: f32, y: f32) -> windows_numerics::Matrix3x2 {
    windows_numerics::Matrix3x2 { M11: 1.0, M12: 0.0, M21: 0.0, M22: 1.0, M31: x, M32: y }
}
