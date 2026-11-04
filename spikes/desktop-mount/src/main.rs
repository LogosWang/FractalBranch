//! 第 0 阶段技术验证：比较几种把透明分区挂到桌面层的方式。
//!
//! 用法：desktop-mount --mode <child-layered|child-plain|child-dcomp|popup-owned|progman-layered|progman-dcomp>
//!                     [--seconds N] [--x N --y N --w N --h N] [--log PATH] [--software] [--warp]
//!                     [--region] [--hide-icons]
//!
//! 安全约束：不碰桌面文件；到时自动退出，进程结束后窗口随之消失。
//! 只有传 --hide-icons 才隐藏原生图标层，此时会同时启动守护进程，主进程无论如何退出都由它恢复。
#![windows_subsystem = "windows"]

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::mem::{ManuallyDrop, size_of};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::Instant;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::ProcessStatus::*;
use windows::Win32::System::Threading::{GetCurrentProcess, INFINITE, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

const TIMER_EXIT: usize = 1;
const TIMER_REMOUNT: usize = 2;
const TIMER_STATS: usize = 3;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    ChildLayered,
    ChildPlain,
    ChildDcomp,
    PopupOwned,
    ProgmanLayered,
    ProgmanDcomp,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::ChildLayered => "child-layered",
            Mode::ChildPlain => "child-plain",
            Mode::ChildDcomp => "child-dcomp",
            Mode::PopupOwned => "popup-owned",
            Mode::ProgmanLayered => "progman-layered",
            Mode::ProgmanDcomp => "progman-dcomp",
        }
    }
}

/// 每种模式各自持有的绘制资源。窗口重建时整体丢弃。
enum Gfx {
    None,
    Dib {
        mem_dc: HDC,
        bitmap: HBITMAP,
        old: HGDIOBJ,
    },
    Comp {
        _device: IDCompositionDevice,
        _target: IDCompositionTarget,
        _visual: IDCompositionVisual,
        _swap: IDXGISwapChain1,
    },
}

struct App {
    mode: Mode,
    started: Instant,
    log: File,
    host: HWND,
    panel: HWND,
    gfx: Gfx,
    pos: (i32, i32),
    size: (i32, i32),
    drag: Option<POINT>,
    shutting_down: bool,
    mount_count: u32,
    paint_count: u32,
    d2d: ID2D1Factory1,
    dwrite: IDWriteFactory,
    taskbar_created: u32,
    software: bool,
    warp: bool,
    region: bool,
    hide_icons: bool,
}

static APP: AtomicPtr<App> = AtomicPtr::new(std::ptr::null_mut());

/// 只在唯一的 UI 线程上访问。
fn app() -> &'static mut App {
    unsafe { &mut *APP.load(Ordering::Relaxed) }
}

fn log(msg: impl AsRef<str>) {
    let a = app();
    let _ = writeln!(a.log, "{:>8} ms  {}", a.started.elapsed().as_millis(), msg.as_ref());
    let _ = a.log.flush();
}

fn class_name(h: HWND) -> String {
    let mut buf = [0u16; 128];
    let n = unsafe { GetClassNameW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// 定位当前桌面窗口树。24H2 起 DefView 直接在 Progman 下，更早的系统在某个 WorkerW 下。
fn find_desktop() -> Option<(HWND, HWND)> {
    unsafe {
        let progman = FindWindowW(w!("Progman"), None).ok()?;
        if let Ok(view) = FindWindowExW(Some(progman), None, w!("SHELLDLL_DefView"), None) {
            return Some((progman, view));
        }
        let mut after = None;
        while let Ok(w) = FindWindowExW(None, after, w!("WorkerW"), None) {
            if let Ok(view) = FindWindowExW(Some(w), None, w!("SHELLDLL_DefView"), None) {
                return Some((progman, view));
            }
            after = Some(w);
        }
        None
    }
}

fn color(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

fn rect(l: f32, t: f32, r: f32, b: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: l, top: t, right: r, bottom: b }
}

/// 测试图案：30% 黑底（对应小智现在的分区背景）、全透明的“洞”、不透明色块和文字。
/// 单位是物理像素。透明是否正确，一眼就能从洞里的壁纸和半透明底看出来。
fn draw(rt: &ID2D1RenderTarget, w: f32, h: f32) -> Result<()> {
    let a = app();
    unsafe {
        rt.SetDpi(96.0, 96.0);
        rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        rt.Clear(Some(&color(0.0, 0.0, 0.0, 0.0)));

        let bg = rt.CreateSolidColorBrush(&color(0.0, 0.0, 0.0, 0.30), None)?;
        let edge = rt.CreateSolidColorBrush(&color(1.0, 1.0, 1.0, 0.45), None)?;
        let white = rt.CreateSolidColorBrush(&color(1.0, 1.0, 1.0, 1.0), None)?;
        let round = D2D1_ROUNDED_RECT { rect: rect(1.0, 1.0, w - 1.0, h - 1.0), radiusX: 16.0, radiusY: 16.0 };
        rt.FillRoundedRectangle(&round, &bg);
        rt.DrawRoundedRectangle(&round, &edge, 2.0, None);

        // 色块：不透明、50%、不透明。
        for (i, c) in [color(0.85, 0.2, 0.2, 1.0), color(0.2, 0.75, 0.3, 0.5), color(0.2, 0.4, 0.9, 1.0)].iter().enumerate() {
            let b = rt.CreateSolidColorBrush(c, None)?;
            let x = 40.0 + i as f32 * 150.0;
            rt.FillRectangle(&rect(x, 150.0, x + 130.0, 270.0), &b);
        }
        // 洞：把这块擦回全透明。应当看到壁纸，鼠标应当穿透（分层模式）。
        rt.PushAxisAlignedClip(&rect(w - 240.0, 150.0, w - 40.0, 270.0), D2D1_ANTIALIAS_MODE_ALIASED);
        rt.Clear(Some(&color(0.0, 0.0, 0.0, 0.0)));
        rt.PopAxisAlignedClip();

        let title = a.dwrite.CreateTextFormat(
            w!("Microsoft YaHei UI"),
            None,
            DWRITE_FONT_WEIGHT_SEMI_BOLD,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            44.0,
            w!("zh-CN"),
        )?;
        let text: Vec<u16> = format!("分形树枝 · {} · 第 {} 次挂载", a.mode.name(), a.mount_count).encode_utf16().collect();
        rt.DrawText(&text, &title, &rect(40.0, 40.0, w - 40.0, 130.0), &white, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
        let body = a.dwrite.CreateTextFormat(
            w!("Microsoft YaHei UI"),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            30.0,
            w!("zh-CN"),
        )?;
        let tip: Vec<u16> = "按住拖动可移动。右上方的空洞里应当能看到壁纸。".encode_utf16().collect();
        rt.DrawText(&tip, &body, &rect(40.0, 300.0, w - 40.0, h - 20.0), &white, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
    }
    Ok(())
}

fn dc_target_props() -> D2D1_RENDER_TARGET_PROPERTIES {
    D2D1_RENDER_TARGET_PROPERTIES {
        r#type: if app().software { D2D1_RENDER_TARGET_TYPE_SOFTWARE } else { D2D1_RENDER_TARGET_TYPE_DEFAULT },
        pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
        dpiX: 96.0,
        dpiY: 96.0,
        usage: D2D1_RENDER_TARGET_USAGE_NONE,
        minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
    }
}

fn paint_layered() -> Result<()> {
    let a = app();
    let (w, h) = a.size;
    unsafe {
        if matches!(a.gfx, Gfx::None) {
            let bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut c_void = std::ptr::null_mut();
            let bitmap = CreateDIBSection(None, &bi, DIB_RGB_COLORS, &mut bits, None, 0)?;
            let mem_dc = CreateCompatibleDC(None);
            let old = SelectObject(mem_dc, bitmap.into());
            a.gfx = Gfx::Dib { mem_dc, bitmap, old };
        }
        let Gfx::Dib { mem_dc, .. } = a.gfx else { unreachable!() };
        let rt = a.d2d.CreateDCRenderTarget(&dc_target_props())?;
        rt.BindDC(mem_dc, &RECT { left: 0, top: 0, right: w, bottom: h })?;
        rt.BeginDraw();
        draw(&rt, w as f32, h as f32)?;
        rt.EndDraw(None, None)?;

        let size = SIZE { cx: w, cy: h };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        UpdateLayeredWindow(a.panel, None, None, Some(&size), Some(mem_dc), Some(&src), COLORREF(0), Some(&blend), ULW_ALPHA)?;
    }
    Ok(())
}

fn paint_plain(hdc: HDC) -> Result<()> {
    let a = app();
    let (w, h) = a.size;
    unsafe {
        let rt = a.d2d.CreateDCRenderTarget(&dc_target_props())?;
        rt.BindDC(hdc, &RECT { left: 0, top: 0, right: w, bottom: h })?;
        rt.BeginDraw();
        draw(&rt, w as f32, h as f32)?;
        rt.EndDraw(None, None)?;
    }
    Ok(())
}

fn init_dcomp() -> Result<()> {
    let a = app();
    let (w, h) = a.size;
    unsafe {
        let mut device: Option<ID3D11Device> = None;
        D3D11CreateDevice(
            None,
            if a.warp { D3D_DRIVER_TYPE_WARP } else { D3D_DRIVER_TYPE_HARDWARE },
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
        let device = device.ok_or_else(|| Error::from(E_FAIL))?;
        let dxgi: IDXGIDevice = device.cast()?;
        let adapter = dxgi.GetAdapter()?;
        let factory: IDXGIFactory2 = adapter.GetParent()?;
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: w as u32,
            Height: h as u32,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
            ..Default::default()
        };
        let swap = factory.CreateSwapChainForComposition(&dxgi, &desc, None)?;

        let d2d_device = a.d2d.CreateDevice(&dxgi)?;
        let ctx = d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
        let surface: IDXGISurface = swap.GetBuffer(0)?;
        let bitmap = ctx.CreateBitmapFromDxgiSurface(
            &surface,
            Some(&D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                dpiX: 96.0,
                dpiY: 96.0,
                bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                colorContext: ManuallyDrop::new(None),
            }),
        )?;
        ctx.SetTarget(&bitmap);
        ctx.BeginDraw();
        draw(&ctx, w as f32, h as f32)?;
        ctx.EndDraw(None, None)?;
        swap.Present(1, DXGI_PRESENT(0)).ok()?;

        let comp: IDCompositionDevice = DCompositionCreateDevice(&dxgi)?;
        let target = comp.CreateTargetForHwnd(a.panel, true)?;
        let visual = comp.CreateVisual()?;
        visual.SetContent(&swap)?;
        target.SetRoot(&visual)?;
        comp.Commit()?;
        a.gfx = Gfx::Comp { _device: comp, _target: target, _visual: visual, _swap: swap };
    }
    Ok(())
}

fn release_gfx() {
    let a = app();
    if let Gfx::Dib { mem_dc, bitmap, old } = a.gfx {
        unsafe {
            SelectObject(mem_dc, old);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(mem_dc);
        }
    }
    a.gfx = Gfx::None;
}

fn mem_mb() -> (f64, f64) {
    unsafe {
        let mut c = PROCESS_MEMORY_COUNTERS_EX::default();
        let _ = K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
            size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        );
        (c.WorkingSetSize as f64 / 1048576.0, c.PrivateUsage as f64 / 1048576.0)
    }
}

fn log_siblings(h: HWND) {
    unsafe {
        let parent = GetAncestor(h, GA_PARENT);
        let mut names = Vec::new();
        let mut cur = GetWindow(parent, GW_CHILD).ok();
        while let Some(w) = cur {
            names.push(if w == h { "<<本窗口>>".to_string() } else { class_name(w) });
            cur = GetWindow(w, GW_HWNDNEXT).ok();
        }
        log(format!("父窗口={} 同级 z 序（上到下）: {}", class_name(parent), names.join(" > ")));
    }
}

/// 顶层窗口的 z 序自检：面板和 Progman 之间不应夹着任何可见窗口。
fn log_z(h: HWND) {
    if app().mode != Mode::PopupOwned {
        return;
    }
    unsafe {
        let Ok(progman) = FindWindowW(w!("Progman"), None) else { return };
        let mut between = Vec::new();
        let mut cur = GetWindow(h, GW_HWNDNEXT).ok();
        let mut reached = false;
        while let Some(w) = cur {
            if w == progman {
                reached = true;
                break;
            }
            if IsWindowVisible(w).as_bool() {
                between.push(class_name(w));
            }
            cur = GetWindow(w, GW_HWNDNEXT).ok();
        }
        let mut r = RECT::default();
        let _ = GetWindowRect(h, &mut r);
        let center = POINT { x: (r.left + r.right) / 2, y: (r.top + r.bottom) / 2 };
        let at = GetAncestor(WindowFromPoint(center), GA_ROOT);
        log(format!(
            "z 自检：在 Progman 之上={reached} 中间可见窗口 {} 个 {:?}；面板中心最上层窗口={}",
            between.len(),
            between,
            if at == h { "<<本窗口>>".to_string() } else { class_name(at) }
        ));
    }
}

fn mount() -> Result<()> {
    let a = app();
    release_gfx();
    let (progman, view) = find_desktop().ok_or_else(|| Error::new(E_FAIL, "未找到桌面窗口树"))?;
    a.mount_count += 1;
    let t0 = Instant::now();
    let (x, y) = a.pos;
    let (w, h) = a.size;
    let hinst: HINSTANCE = unsafe { GetModuleHandleW(None)? }.into();
    let (ex, style, parent) = match a.mode {
        Mode::ChildLayered => (WS_EX_LAYERED | WS_EX_NOACTIVATE, WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS, view),
        Mode::ChildPlain => (WS_EX_NOACTIVATE, WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS, view),
        Mode::ChildDcomp => (WS_EX_NOREDIRECTIONBITMAP | WS_EX_NOACTIVATE, WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS, view),
        Mode::PopupOwned => (WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, WS_POPUP | WS_VISIBLE, progman),
        Mode::ProgmanLayered => (WS_EX_LAYERED | WS_EX_NOACTIVATE, WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS, progman),
        Mode::ProgmanDcomp => (WS_EX_NOREDIRECTIONBITMAP | WS_EX_NOACTIVATE, WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS, progman),
    };
    let panel = unsafe {
        CreateWindowExW(ex, w!("FractalBranchSpikePanel"), w!("分形树枝验证"), style, x, y, w, h, Some(parent), None, Some(hinst), None)?
    };
    a.panel = panel;
    unsafe {
        let z = if a.mode == Mode::PopupOwned { HWND_BOTTOM } else { HWND_TOP };
        let _ = SetWindowPos(panel, Some(z), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    }
    match a.mode {
        Mode::ChildLayered | Mode::PopupOwned | Mode::ProgmanLayered => paint_layered()?,
        Mode::ChildPlain => unsafe {
            let _ = InvalidateRect(Some(panel), None, true);
            let _ = UpdateWindow(panel);
        },
        Mode::ChildDcomp | Mode::ProgmanDcomp => init_dcomp()?,
    }
    let (ws, private) = mem_mb();
    log(format!(
        "挂载 #{} 完成，用时 {} ms；panel={:?} dpi={} 可见={} 工作集={:.1} MB 私有={:.1} MB",
        a.mount_count,
        t0.elapsed().as_millis(),
        panel.0,
        unsafe { GetDpiForWindow(panel) },
        unsafe { IsWindowVisible(panel).as_bool() },
        ws,
        private
    ));
    unsafe {
        let (mut r, mut pr) = (RECT::default(), RECT::default());
        let _ = GetWindowRect(panel, &mut r);
        let _ = GetWindowRect(GetAncestor(panel, GA_PARENT), &mut pr);
        log(format!("面板屏幕矩形 {},{} {}x{}；父窗口 {},{} {}x{}", r.left, r.top, r.right - r.left, r.bottom - r.top, pr.left, pr.top, pr.right - pr.left, pr.bottom - pr.top));
    }
    log_siblings(panel);
    log_z(panel);
    if a.region {
        apply_hole_region(panel, w, h);
    }
    if a.hide_icons {
        set_icons_visible(false);
    }
    Ok(())
}

/// 把测试图案里的“洞”从窗口区域挖掉。分层窗口靠 alpha 决定点击穿透，DComp 子窗口只能靠区域。
fn apply_hole_region(panel: HWND, w: i32, h: i32) {
    let _ = h;
    unsafe {
        let whole = CreateRectRgn(0, 0, w, h);
        let hole = CreateRectRgn(w - 240, 150, w - 40, 270);
        let _ = CombineRgn(Some(whole), Some(whole), Some(hole), RGN_DIFF);
        let _ = DeleteObject(hole.into());
        // 成功后区域归系统所有，不能再释放 whole。
        let ok = SetWindowRgn(panel, Some(whole), true);
        log(format!("窗口区域已设置，结果={ok}"));
    }
}

fn find_listview() -> Option<HWND> {
    let (_, view) = find_desktop()?;
    unsafe { FindWindowExW(Some(view), None, w!("SysListView32"), None).ok() }
}

/// 用异步调用，资源管理器卡住时不会把本进程一起卡住。
fn set_icons_visible(visible: bool) {
    if let Some(lv) = find_listview() {
        unsafe {
            let _ = ShowWindowAsync(lv, if visible { SW_SHOW } else { SW_HIDE });
        }
        log(format!("原生图标层设为可见={visible}"));
    } else {
        log("未找到原生图标层");
    }
}

/// 守护模式：等主进程退出（包括被强杀），然后恢复原生图标。
fn run_guard(pid: u32, log_path: &str) {
    let mut f = OpenOptions::new().create(true).write(true).truncate(true).open(log_path).ok();
    let started = Instant::now();
    let mut say = |s: String| {
        if let Some(f) = f.as_mut() {
            let _ = writeln!(f, "{:>8} ms  {s}", started.elapsed().as_millis());
            let _ = f.flush();
        }
    };
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) };
    let Ok(handle) = handle else {
        say(format!("无法打开进程 {pid}"));
        return;
    };
    let (ws, private) = mem_mb();
    say(format!("守护启动，等待进程 {pid}；工作集={ws:.1} MB 私有={private:.1} MB"));
    unsafe {
        WaitForSingleObject(handle, INFINITE);
        let _ = CloseHandle(handle);
    }
    let t0 = Instant::now();
    match find_listview() {
        Some(lv) => unsafe {
            let was = IsWindowVisible(lv).as_bool();
            let _ = ShowWindowAsync(lv, SW_SHOW);
            say(format!("主进程已退出；原生图标层原可见={was}，已恢复，用时 {} ms", t0.elapsed().as_millis()));
        },
        None => say("主进程已退出；未找到原生图标层".to_string()),
    }
}

fn try_mount() {
    if let Err(e) = mount() {
        log(format!("挂载失败：{e}，稍后重试"));
        unsafe {
            SetTimer(Some(app().host), TIMER_REMOUNT, 1000, None);
        }
    }
}

unsafe extern "system" fn host_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let a = app();
        if msg == a.taskbar_created {
            log("收到 TaskbarCreated：资源管理器已重启");
            SetTimer(Some(h), TIMER_REMOUNT, 300, None);
            return LRESULT(0);
        }
        match msg {
            WM_TIMER => {
                match wp.0 {
                    TIMER_EXIT => {
                        log("到时自动退出");
                        a.shutting_down = true;
                        if a.hide_icons {
                            set_icons_visible(true);
                        }
                        PostQuitMessage(0);
                    }
                    TIMER_REMOUNT => {
                        let _ = KillTimer(Some(h), TIMER_REMOUNT);
                        if a.panel.0.is_null() || !IsWindow(Some(a.panel)).as_bool() {
                            try_mount();
                        }
                    }
                    TIMER_STATS => {
                        let (ws, private) = mem_mb();
                        let vis = IsWindow(Some(a.panel)).as_bool() && IsWindowVisible(a.panel).as_bool();
                        log(format!("统计：工作集={ws:.1} MB 私有={private:.1} MB 绘制={} 次 窗口可见={vis}", a.paint_count));
                        log_z(a.panel);
                        if a.hide_icons {
                            if let Some(lv) = find_listview() {
                                if IsWindowVisible(lv).as_bool() {
                                    log("原生图标层被重新显示了，再次隐藏");
                                    set_icons_visible(false);
                                }
                            }
                        }
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_DISPLAYCHANGE => {
                log(format!("WM_DISPLAYCHANGE {}x{}", lp.0 & 0xffff, (lp.0 >> 16) & 0xffff));
                LRESULT(0)
            }
            WM_DPICHANGED => {
                log(format!("host WM_DPICHANGED dpi={}", wp.0 & 0xffff));
                LRESULT(0)
            }
            _ => DefWindowProcW(h, msg, wp, lp),
        }
    }
}

unsafe extern "system" fn panel_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let a = app();
        let screen = || {
            let mut p = POINT::default();
            let _ = GetCursorPos(&mut p);
            p
        };
        match msg {
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(h, &mut ps);
                a.paint_count += 1;
                if a.mode == Mode::ChildPlain {
                    if let Err(e) = paint_plain(hdc) {
                        log(format!("绘制失败：{e}"));
                    }
                }
                let _ = EndPaint(h, &ps);
                LRESULT(0)
            }
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            WM_WINDOWPOSCHANGING if a.mode == Mode::PopupOwned => {
                // 分区必须始终贴着桌面：任何抬升 z 序的请求都改写成压到底。
                let pos = &mut *(lp.0 as *mut WINDOWPOS);
                if !pos.flags.contains(SWP_NOZORDER) {
                    pos.hwndInsertAfter = HWND_BOTTOM;
                }
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                log("左键按下");
                log_z(h);
                SetCapture(h);
                a.drag = Some(screen());
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                if let Some(start) = a.drag {
                    let p = screen();
                    let (nx, ny) = (a.pos.0 + p.x - start.x, a.pos.1 + p.y - start.y);
                    let _ = SetWindowPos(h, None, nx, ny, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
                    a.pos = (nx, ny);
                    a.drag = Some(p);
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                a.drag = None;
                let _ = ReleaseCapture();
                log(format!("左键释放，位置 {:?}", a.pos));
                log_z(h);
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                log("右键释放");
                LRESULT(0)
            }
            WM_DPICHANGED_AFTERPARENT => {
                log(format!("panel WM_DPICHANGED_AFTERPARENT dpi={}", GetDpiForWindow(h)));
                LRESULT(0)
            }
            WM_DPICHANGED => {
                log(format!("panel WM_DPICHANGED dpi={}", wp.0 & 0xffff));
                LRESULT(0)
            }
            WM_DESTROY => {
                if !a.shutting_down {
                    // 父窗口属于资源管理器。它退出时本窗口会被连带销毁，这里安排重新挂载。
                    log("面板被销毁（父窗口消失），安排重新挂载");
                    SetTimer(Some(a.host), TIMER_REMOUNT, 500, None);
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(h, msg, wp, lp),
        }
    }
}

fn arg_value(args: &[String], key: &str) -> Option<String> {
    args.iter().position(|a| a == key).and_then(|i| args.get(i + 1)).cloned()
}

fn run() -> Result<()> {
    let started = Instant::now();
    let args: Vec<String> = std::env::args().collect();
    let mode = match arg_value(&args, "--mode").as_deref() {
        Some("child-plain") => Mode::ChildPlain,
        Some("child-dcomp") => Mode::ChildDcomp,
        Some("popup-owned") => Mode::PopupOwned,
        Some("progman-layered") => Mode::ProgmanLayered,
        Some("progman-dcomp") => Mode::ProgmanDcomp,
        _ => Mode::ChildLayered,
    };
    let num = |k: &str, d: i32| arg_value(&args, k).and_then(|v| v.parse().ok()).unwrap_or(d);
    let seconds = num("--seconds", 60).clamp(1, 3600);
    let log_path = arg_value(&args, "--log").unwrap_or_else(|| format!("{}.log", mode.name()));
    let log_file = OpenOptions::new().create(true).write(true).truncate(true).open(&log_path).map_err(|_| Error::from(E_FAIL))?;

    unsafe {
        let d2d: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let state = Box::new(App {
            mode,
            started,
            log: log_file,
            host: HWND::default(),
            panel: HWND::default(),
            gfx: Gfx::None,
            pos: (num("--x", 900), num("--y", 240)),
            size: (num("--w", 1000), num("--h", 480)),
            drag: None,
            shutting_down: false,
            mount_count: 0,
            paint_count: 0,
            d2d,
            dwrite,
            taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
            software: args.iter().any(|a| a == "--software"),
            warp: args.iter().any(|a| a == "--warp"),
            region: args.iter().any(|a| a == "--region"),
            hide_icons: args.iter().any(|a| a == "--hide-icons"),
        });
        APP.store(Box::into_raw(state), Ordering::Relaxed);

        let hinst: HINSTANCE = GetModuleHandleW(None)?.into();
        let host_class = WNDCLASSW { lpfnWndProc: Some(host_proc), hInstance: hinst, lpszClassName: w!("FractalBranchSpikeHost"), ..Default::default() };
        let panel_class = WNDCLASSW {
            lpfnWndProc: Some(panel_proc),
            hInstance: hinst,
            lpszClassName: w!("FractalBranchSpikePanel"),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassW(&host_class);
        RegisterClassW(&panel_class);

        // 隐藏的顶层窗口：子窗口收不到 TaskbarCreated 广播，也需要它在面板重建时保持存活。
        let a = app();
        a.host = CreateWindowExW(WS_EX_TOOLWINDOW, w!("FractalBranchSpikeHost"), w!("FractalBranchSpikeHost"), WS_POPUP, 0, 0, 0, 0, None, None, Some(hinst), None)?;
        log(format!("启动：模式={} 限时={seconds} 秒", mode.name()));

        // 隐藏图标前必须先有守护，否则崩溃后图标回不来。
        if a.hide_icons {
            let exe = std::env::current_exe().map_err(|_| Error::from(E_FAIL))?;
            let guard_log = format!("{log_path}.guard");
            std::process::Command::new(exe)
                .args(["--guard-for", &std::process::id().to_string(), "--log", &guard_log])
                .spawn()
                .map_err(|_| Error::from(E_FAIL))?;
        }

        try_mount();
        log(format!("进程启动到首次挂载结束 {} ms", started.elapsed().as_millis()));

        SetTimer(Some(a.host), TIMER_EXIT, seconds as u32 * 1000, None);
        SetTimer(Some(a.host), TIMER_STATS, 5000, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        log("正常退出");
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(pid) = arg_value(&args, "--guard-for").and_then(|v| v.parse().ok()) {
        run_guard(pid, &arg_value(&args, "--log").unwrap_or_else(|| "guard.log".to_string()));
        return;
    }
    if let Err(e) = run() {
        let msg = HSTRING::from(format!("desktop-mount 启动失败：{e}"));
        unsafe {
            MessageBoxW(None, &msg, w!("分形树枝验证"), MB_ICONERROR);
        }
    }
}
