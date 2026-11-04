//! 主程序：窗口、交互、托盘、持久化。所有状态只在 UI 线程上访问。

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::Instant;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::*;
use windows::Win32::System::ProcessStatus::*;
use windows::Win32::System::Registry::*;
use windows::Win32::System::SystemServices::*;
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcess};
use windows::Win32::UI::Controls::Dialogs::*;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;
use windows_core::implement;

use crate::desktop;
use crate::gfx::{Gfx, Surface};
use crate::icons::{self, Loader};
use crate::layout::{self, Layout, Partition, Roots, SortKey, Tab, View};
use crate::shell::{self, MenuResult};
use crate::util::{self, log, wide};
use crate::view::{self, Hit, Meta, MetaMap, PanelState, Rect, Res, path_key};
use crate::watch::{self, Change};
use crate::xz;

const WM_APP_ICONS: u32 = WM_APP + 1;
const WM_APP_FS: u32 = WM_APP + 2;
const WM_APP_TRAY: u32 = WM_APP + 3;
const WM_APP_EDIT_DONE: u32 = WM_APP + 4;
const WM_APP_TOGGLE: u32 = WM_APP + 5;

const TIMER_SAVE: usize = 1;
const TIMER_REMOUNT: usize = 2;
const TIMER_RESCAN: usize = 3;
const TIMER_ICONS_CHECK: usize = 4;
const TIMER_STATS: usize = 5;
const TIMER_DISPLAY: usize = 6;
const TIMER_TRIM: usize = 7;

const DRAG_THRESHOLD: i32 = 6;

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("FractalBranch");

// 菜单项 ID。
const M_VIEW_ICONS: u32 = 101;
const M_VIEW_LIST: u32 = 102;
const M_SORT_MANUAL: u32 = 110;
const M_SORT_NAME: u32 = 111;
const M_SORT_MODIFIED: u32 = 112;
const M_SORT_SIZE: u32 = 113;
const M_SORT_KIND: u32 = 114;
const M_SORT_DESC: u32 = 115;
const M_ICON_S: u32 = 120;
const M_ICON_M: u32 = 121;
const M_ICON_L: u32 = 122;
const M_TAB_NEW: u32 = 130;
const M_TAB_RENAME: u32 = 131;
const M_TAB_DELETE: u32 = 132;
const M_LOCK: u32 = 140;
const M_PART_NEW: u32 = 141;
const M_PART_DELETE: u32 = 142;
const M_TOGGLE: u32 = 150;
const M_ORGANIZE: u32 = 151;
const M_IMPORT_XZ: u32 = 152;
const M_EXPORT: u32 = 153;
const M_IMPORT: u32 = 154;
const M_AUTOSTART: u32 = 155;
const M_EXIT: u32 = 156;
const M_OPEN_DATA: u32 = 157;
const M_SUB_NEW: u32 = 158;
const M_REFRESH: u32 = 159;
const M_ALPHA_BASE: u32 = 170; // 170..=173
const ALPHAS: [(f32, &str); 4] = [(0.30, "浅（30%）"), (0.45, "中（45%）"), (0.60, "深（60%）"), (0.75, "很深（75%）")];

#[derive(Clone, PartialEq)]
enum Kind {
    Desktop,
    Part(String),
}

#[derive(Clone, Copy, PartialEq)]
enum Action {
    None,
    /// 按下了但还没超过拖动阈值。
    Press { hit: Hit, x: i32, y: i32 },
    Move { sx: i32, sy: i32, ox: f32, oy: f32 },
    Resize { mask: u8, sx: i32, sy: i32, ox: f32, oy: f32, ow: f32, oh: f32 },
    Band { x0: f32, y0: f32 },
}

struct Panel {
    hwnd: HWND,
    kind: Kind,
    surf: Option<Surface>,
    w: f32,
    h: f32,
    hover: Hit,
    selected: HashSet<String>,
    anchor: Option<usize>,
    scroll: f32,
    drop_hot: bool,
    band: Option<Rect>,
    action: Action,
    tracking: bool,
    cells: Vec<Rect>,
    /// 嵌套分区的上级窗口；顶层分区和底层桌面是 None。
    parent: Option<HWND>,
}

enum EditTarget {
    File(String),
    TabName(String),
}

struct Edit {
    hwnd: HWND,
    font: HFONT,
    target: EditTarget,
}

struct App {
    started: Instant,
    host: HWND,
    roots: Roots,
    layout: Layout,
    layout_path: PathBuf,
    meta: MetaMap,
    entries: Vec<layout::Entry>,
    gfx: Gfx,
    res: Res,
    loader: Loader,
    panels: Vec<Panel>,
    scale: f32,
    work: (f32, f32),
    origin: (i32, i32),
    hidden: bool,
    shutting_down: bool,
    first_paint_logged: bool,
    icons_checks: u32,
    taskbar_created: u32,
    changes: Receiver<Change>,
    new_targets: HashMap<String, String>,
    dragging: Vec<String>,
    edit: Option<Edit>,
    tray_icon: HICON,
    /// 刚通过桌面菜单“新建”了文件：它一出现就进入改名，和原生桌面一样。
    rename_new_until: Option<Instant>,
}

static APP: AtomicPtr<App> = AtomicPtr::new(std::ptr::null_mut());

/// 只在 UI 线程调用。可重入的窗口过程里也用它，所以不长期持有返回的引用。
fn app() -> &'static mut App {
    unsafe { &mut *APP.load(Ordering::Relaxed) }
}

fn app_ready() -> bool {
    !APP.load(Ordering::Relaxed).is_null()
}

// ---- 启动 ----

pub fn system_scale() -> f32 {
    unsafe {
        let mon = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let (mut x, mut y) = (96u32, 96u32);
        if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut x, &mut y).is_err() || x == 0 {
            return 1.0;
        }
        x as f32 / 96.0
    }
}

pub fn work_area(scale: f32) -> (f32, f32) {
    unsafe {
        let mut r = RECT::default();
        let ok = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut r as *mut _ as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)).is_ok();
        if ok && r.right > r.left {
            (r.right as f32 / scale, r.bottom as f32 / scale)
        } else {
            (GetSystemMetrics(SM_CXSCREEN) as f32 / scale, GetSystemMetrics(SM_CYSCREEN) as f32 / scale)
        }
    }
}

/// 读布局；坏了就依次试备份；都没有就从小智导入；再没有就新建。不会写小智的文件。
pub fn load_layout(path: &Path, roots: &Roots) -> Layout {
    if path.exists() {
        match layout::load(path) {
            Ok(l) => return l,
            Err(e) => {
                log(format!("布局文件损坏：{e}"));
                let _ = std::fs::rename(path, path.with_extension("corrupt.json"));
            }
        }
        for i in 1..=5 {
            let b = path.with_file_name(format!("layout.backup{i}.json"));
            if let Ok(l) = layout::load(&b) {
                log(format!("已从备份 {i} 恢复布局"));
                return l;
            }
        }
    }
    if let Some(dir) = util::xz_data_dir() {
        match xz::import(&dir, roots) {
            Ok((l, r)) => {
                log(format!("已从小智导入：{} 个分区，{} 个标签，{} 项；跳过 AI 工具 {} 项，缺失 {} 项", r.partitions, r.tabs, r.items, r.skipped_tools, r.missing.len()));
                return l;
            }
            Err(e) => log(format!("从小智导入失败：{e}")),
        }
    }
    Layout::default()
}

pub fn refresh_meta(entries: &[layout::Entry]) -> MetaMap {
    entries.iter().map(|e| (path_key(&e.path), Meta { modified: e.modified, size: e.size, is_dir: e.is_dir })).collect()
}

pub fn message(text: &str) {
    let t = wide(text);
    unsafe {
        MessageBoxW(None, PCWSTR(t.as_ptr()), w!("分形树枝"), MB_ICONINFORMATION | MB_SETFOREGROUND);
    }
}

pub fn confirm(text: &str) -> bool {
    let t = wide(text);
    unsafe { MessageBoxW(None, PCWSTR(t.as_ptr()), w!("分形树枝"), MB_YESNO | MB_ICONQUESTION | MB_SETFOREGROUND) == IDYES }
}

pub fn run(crashes: u32, autostart: bool) -> Result<()> {
    let started = Instant::now();
    unsafe {
        // 同一时间只跑一份。句柄故意不关，随进程结束释放。
        let _mutex = CreateMutexW(None, false, w!("Local\\FractalBranch.Instance"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return Ok(());
        }
        OleInitialize(None)?;
    }
    if desktop::supported() == Some(false) {
        log("桌面结构不受支持，退出，不接管桌面");
        if !autostart {
            message("这台电脑的桌面结构还不受支持，分形树枝没有接管桌面。\n目前只在 Windows 11 24H2 及更新版本上验证过。");
        }
        return Ok(());
    }
    if desktop::xz_running() {
        log("小智桌面正在运行，退出");
        if !autostart {
            message("小智桌面正在运行，两者同时运行会互相遮挡。\n请先退出小智桌面（托盘图标右键），再启动分形树枝。");
        }
        return Ok(());
    }

    let roots = util::roots();
    let dir = util::data_dir();
    let layout_path = dir.join("layout.json");
    let first_run = !layout_path.exists();
    let mut layout = load_layout(&layout_path, &roots);
    let entries = layout::scan(&roots).unwrap_or_default();
    if !entries.is_empty() {
        layout.reconcile(&entries, &HashMap::new());
    }
    let meta = refresh_meta(&entries);
    if first_run {
        let _ = layout::save(&layout_path, &layout);
    }

    let scale = system_scale();
    let gfx = Gfx::new()?;
    let res = Res::new(gfx.dwrite.clone(), scale, roots.clone(), &layout.style)?;

    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None)?.into();
        let host_class = WNDCLASSW { lpfnWndProc: Some(host_proc), hInstance: hinst, lpszClassName: w!("FractalBranchHost"), ..Default::default() };
        let panel_class = WNDCLASSW {
            style: CS_DBLCLKS,
            lpfnWndProc: Some(panel_proc),
            hInstance: hinst,
            lpszClassName: w!("FractalBranchPanel"),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassW(&host_class);
        RegisterClassW(&panel_class);
        // 隐藏的顶层窗口：托盘、菜单、广播消息都走它。分区窗口随资源管理器重启会被销毁，它不会。
        let host = CreateWindowExW(WS_EX_TOOLWINDOW, w!("FractalBranchHost"), w!("分形树枝"), WS_POPUP, 0, 0, 0, 0, None, None, Some(hinst), None)?;

        let loader = icons::spawn(dir.join("icons.cache"), Some((host.0 as isize, WM_APP_ICONS)));
        let (tx, changes) = channel();
        watch::spawn(roots.desktop.clone(), tx.clone(), host.0 as isize, WM_APP_FS);
        watch::spawn(roots.public.clone(), tx, host.0 as isize, WM_APP_FS);

        let state = Box::new(App {
            started,
            host,
            roots,
            layout,
            layout_path,
            meta,
            entries,
            gfx,
            res,
            loader,
            panels: Vec::new(),
            scale,
            work: work_area(scale),
            origin: (0, 0),
            hidden: false,
            shutting_down: false,
            first_paint_logged: false,
            icons_checks: 0,
            taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
            changes,
            new_targets: HashMap::new(),
            dragging: Vec::new(),
            edit: None,
            tray_icon: make_tray_icon(),
            rename_new_until: None,
        });
        APP.store(Box::into_raw(state), Ordering::Relaxed);

        // 隐藏原生图标之前先把守护拉起来，之后无论怎么退出图标都回得来。
        desktop::spawn_guard(crashes);
        add_tray();
        if !mount() {
            SetTimer(Some(host), TIMER_REMOUNT, 500, None);
        }
        SetTimer(Some(host), TIMER_STATS, 8000, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        shutdown();
    }
    Ok(())
}

fn shutdown() {
    let a = app();
    a.shutting_down = true;
    save_now();
    remove_tray();
    // 窗口交给进程退出时回收，这里只负责把图标还回去。
    desktop::set_icons_visible(true);
}

// ---- 窗口挂载 ----

fn progman_origin(progman: HWND) -> (i32, i32) {
    let mut r = RECT::default();
    unsafe {
        let _ = GetWindowRect(progman, &mut r);
    }
    (r.left, r.top)
}

fn destroy_panels() {
    let a = app();
    let panels: Vec<Panel> = a.panels.drain(..).collect();
    for p in panels {
        unsafe {
            let _ = RevokeDragDrop(p.hwnd);
            if IsWindow(Some(p.hwnd)).as_bool() {
                let _ = DestroyWindow(p.hwnd);
            }
        }
    }
}

fn create_panel(kind: Kind, parent: Option<HWND>) -> Option<usize> {
    let a = app();
    let (progman, _) = desktop::find_desktop()?;
    let hinst: HINSTANCE = unsafe { GetModuleHandleW(None).ok()? }.into();
    let visible = parent.is_some() || !a.hidden || kind == Kind::Desktop;
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_NOREDIRECTIONBITMAP,
            w!("FractalBranchPanel"),
            w!("分形树枝分区"),
            WS_CHILD | WS_CLIPSIBLINGS | if visible { WS_VISIBLE } else { WINDOW_STYLE(0) },
            0,
            0,
            1,
            1,
            Some(parent.unwrap_or(progman)),
            None,
            Some(hinst),
            None,
        )
    };
    let hwnd = match hwnd {
        Ok(h) => h,
        Err(e) => {
            log(format!("创建分区窗口失败：{e}"));
            return None;
        }
    };
    let surf = match a.gfx.surface(hwnd, 1, 1) {
        Ok(s) => s,
        Err(e) => {
            log(format!("创建绘制表面失败：{e}"));
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return None;
        }
    };
    unsafe {
        // 新窗口要在 DefView 和壁纸层之上。按创建顺序压顶：先底层桌面，后各分区。
        let _ = SetWindowPos(hwnd, Some(HWND_TOP), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        let target: IDropTarget = Dropper::new(hwnd).into();
        if let Err(e) = RegisterDragDrop(hwnd, &target) {
            log(format!("注册拖放失败：{e}"));
        }
    }
    a.panels.push(Panel {
        hwnd,
        kind,
        surf: Some(surf),
        w: 1.0,
        h: 1.0,
        hover: Hit::None,
        selected: HashSet::new(),
        anchor: None,
        scroll: 0.0,
        drop_hot: false,
        band: None,
        action: Action::None,
        tracking: false,
        cells: Vec::new(),
        parent,
    });
    Some(a.panels.len() - 1)
}

/// 重建全部窗口并隐藏原生图标。桌面还没就绪时返回 false。
fn mount() -> bool {
    let a = app();
    let Some((progman, _)) = desktop::find_desktop() else { return false };
    destroy_panels();
    a.origin = progman_origin(progman);
    // 先藏原生图标再放窗口：全屏窗口压上去时，资源管理器会把还可见的图标层整个重绘一遍。
    // 空桌面只会露出几十毫秒，比启动慢几百毫秒划算。
    desktop::set_icons_visible(false);
    if create_panel(Kind::Desktop, None).is_none() {
        desktop::set_icons_visible(true);
        return false;
    }
    sync_panels();
    a.icons_checks = 15;
    unsafe {
        SetTimer(Some(a.host), TIMER_ICONS_CHECK, 1000, None);
    }
    log(format!("挂载完成：{} 个窗口，用时 {} ms", a.panels.len(), a.started.elapsed().as_millis()));
    true
}

/// 现在应该有窗口的分区：父在前、子在后。收起的分区和没选中的标签，里面的子分区不显示。
fn desired_panels(l: &Layout) -> Vec<(String, Option<String>)> {
    fn add(p: &Partition, parent: Option<&str>, out: &mut Vec<(String, Option<String>)>) {
        out.push((p.id.clone(), parent.map(str::to_string)));
        if !p.collapsed {
            for c in &p.tabs[p.active_tab()].children {
                add(c, Some(&p.id), out);
            }
        }
    }
    let mut out = Vec::new();
    for p in &l.partitions {
        add(p, None, &mut out);
    }
    out
}

/// 让窗口的数量、位置、大小和布局一致。任何改变布局的操作之后都调用它。
fn sync_panels() {
    let a = app();
    let desired = desired_panels(&a.layout);
    let wanted: HashMap<&str, Option<&str>> = desired.iter().map(|(id, p)| (id.as_str(), p.as_deref())).collect();
    let parent_of = |panel: &Panel, panels: &[Panel]| -> Option<String> {
        let parent = panel.parent?;
        panels.iter().find(|q| q.hwnd == parent).and_then(|q| if let Kind::Part(id) = &q.kind { Some(id.clone()) } else { None })
    };
    // 从后往前销毁：子窗口排在父窗口后面，不会先被父窗口连带销毁。
    let mut i = a.panels.len();
    while i > 0 {
        i -= 1;
        let keep = match &a.panels[i].kind {
            Kind::Desktop => true,
            Kind::Part(id) => match wanted.get(id.as_str()) {
                Some(parent) => parent_of(&a.panels[i], &a.panels).as_deref() == *parent,
                None => false,
            },
        };
        if !keep {
            let p = a.panels.remove(i);
            unsafe {
                let _ = RevokeDragDrop(p.hwnd);
                let _ = DestroyWindow(p.hwnd);
            }
        }
    }
    for (id, parent) in &desired {
        let kind = Kind::Part(id.clone());
        if a.panels.iter().any(|p| p.kind == kind) {
            continue;
        }
        let parent_hwnd = match parent {
            Some(pid) => match a.panels.iter().find(|p| p.kind == Kind::Part(pid.clone())) {
                Some(q) => Some(q.hwnd),
                None => continue,
            },
            None => None,
        };
        create_panel(kind, parent_hwnd);
    }
    for i in 0..a.panels.len() {
        place_panel(i);
    }
    update_desktop_cells();
}

fn px(a: &App, v: f32) -> i32 {
    (v * a.scale).round() as i32
}

/// 分区窗口相对父窗口的物理像素位置。
fn panel_pos(i: usize) -> Option<(i32, i32)> {
    let a = app();
    let Kind::Part(id) = &a.panels[i].kind else { return None };
    let part = a.layout.partition(id)?;
    match a.panels[i].parent {
        // 子分区坐标相对上级的内容区，还要减去上级的滚动量。
        Some(parent) => {
            let up = a.panels.iter().find(|q| q.hwnd == parent)?;
            let Kind::Part(up_id) = &up.kind else { return None };
            let top = view::content_top(a.layout.partition(up_id)?);
            Some((px(a, part.x), px(a, top + part.y - up.scroll)))
        }
        None => Some((px(a, part.x) - a.origin.0, px(a, part.y) - a.origin.1)),
    }
}

fn place_panel(i: usize) {
    let a = app();
    let Kind::Part(id) = a.panels[i].kind.clone() else { return };
    let Some(part) = a.layout.partition(&id) else { return };
    let h = if part.collapsed { view::HEADER_H } else { part.h };
    let w = part.w;
    let Some((xpx, ypx)) = panel_pos(i) else { return };
    let panel = &mut a.panels[i];
    panel.w = w;
    panel.h = h;
    let (wpx, hpx) = ((w * a.scale).round() as i32, (h * a.scale).round() as i32);
    unsafe {
        let _ = SetWindowPos(panel.hwnd, None, xpx, ypx, wpx, hpx, SWP_NOZORDER | SWP_NOACTIVATE);
    }
    if let Some(s) = panel.surf.as_mut() {
        if let Err(e) = a.gfx.resize(s, wpx as u32, hpx as u32) {
            log(format!("调整表面大小失败：{e}"));
        }
    }
    invalidate(panel.hwnd);
}

/// 滚动上级之后，里面的子分区窗口要跟着动。
fn place_children(h: HWND) {
    let a = app();
    for i in 0..a.panels.len() {
        if a.panels[i].parent == Some(h) {
            if let Some((x, y)) = panel_pos(i) {
                unsafe {
                    let _ = SetWindowPos(a.panels[i].hwnd, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
                }
            }
        }
    }
}

/// 重新排布底层桌面。窗口铺满屏幕，接住空白处的点击；但绘制表面只要盖住图标就够，
/// 这样 4K 屏也不用多占一整屏的缓冲。只有框选时才临时扩到全屏。
fn update_desktop_cells() {
    let a = app();
    let Some(i) = a.panels.iter().position(|p| p.kind == Kind::Desktop) else { return };
    let n = a.layout.desktop.items.len();
    let cells = view::desktop_cells(n, &a.layout.desktop, &a.layout.style, a.work, &a.layout.partitions);
    let (mut right, mut bottom) = (1.0f32, 1.0f32);
    for c in &cells {
        right = right.max(c.r);
        bottom = bottom.max(c.b);
    }
    let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    let panel = &mut a.panels[i];
    panel.w = sw as f32 / a.scale;
    panel.h = sh as f32 / a.scale;
    panel.cells = cells;
    let (wpx, hpx) = if panel.band.is_some() { (sw, sh) } else { ((right * a.scale).ceil() as i32, (bottom * a.scale).ceil() as i32) };
    unsafe {
        let _ = SetWindowPos(panel.hwnd, None, -a.origin.0, -a.origin.1, sw, sh, SWP_NOZORDER | SWP_NOACTIVATE);
    }
    if let Some(s) = panel.surf.as_mut() {
        if let Err(e) = a.gfx.resize(s, wpx as u32, hpx as u32) {
            log(format!("调整桌面表面失败：{e}"));
        }
    }
    invalidate(panel.hwnd);
}

fn invalidate(h: HWND) {
    unsafe {
        let _ = InvalidateRect(Some(h), None, false);
    }
}

fn invalidate_all() {
    for p in &app().panels {
        invalidate(p.hwnd);
    }
}

fn paint(h: HWND) {
    let a = app();
    let Some(i) = a.panels.iter().position(|p| p.hwnd == h) else { return };
    let App { panels, layout, gfx, res, loader, meta, scale, hidden, .. } = a;
    let panel = &mut panels[i];
    let dpi = *scale * 96.0;
    let state = PanelState { hover: panel.hover, selected: &panel.selected, scroll: panel.scroll, drop_hot: panel.drop_hot, band: panel.band };
    let Some(surf) = panel.surf.as_mut() else { return };
    let (w, ph) = (panel.w, panel.h);
    let r = gfx.frame(surf, dpi, |rt| {
        let drawn = match &panel.kind {
            // 隐藏时桌面层仍然在，只是什么也不画：还要靠它接住双击，把图标叫回来。
            Kind::Desktop if *hidden => Ok(()),
            Kind::Desktop => view::draw_desktop(rt, res, &layout.desktop, &panel.cells, meta, &state, loader),
            Kind::Part(id) => match layout.partition(id) {
                Some(part) => view::draw_partition(rt, res, &layout.style, part, meta, &state, loader, w, ph),
                None => Ok(()),
            },
        };
        if let Err(e) = drawn {
            log(format!("绘制失败：{e}"));
        }
    });
    if let Err(e) = r {
        log(format!("提交画面失败：{e}"));
    }
    // 停止绘制一会儿之后放掉渲染缓存。每次绘制都把定时器往后推。
    unsafe {
        SetTimer(Some(a.host), TIMER_TRIM, 2000, None);
    }
    if !a.first_paint_logged {
        a.first_paint_logged = true;
        log(format!("首帧完成：进程启动后 {} ms", a.started.elapsed().as_millis()));
    }
}

// ---- 持久化和文件变化 ----

fn mark_dirty() {
    unsafe {
        SetTimer(Some(app().host), TIMER_SAVE, 1500, None);
    }
}

fn save_now() {
    let a = app();
    layout::rotate_backups(&a.layout_path, 5);
    if let Err(e) = layout::save(&a.layout_path, &a.layout) {
        log(format!("保存布局失败：{e}"));
    }
}

fn rescan() {
    let a = app();
    let entries = match layout::scan(&a.roots) {
        Ok(e) => e,
        Err(e) => {
            log(format!("扫描桌面失败，本次不对账：{e}"));
            return;
        }
    };
    // 桌面忽然全空，多半是目录暂时不可用（比如网盘重定向）。不能据此清空分类。
    if entries.is_empty() && a.layout.item_count() > 8 {
        log("扫描结果为空，本次不对账");
        return;
    }
    let r = a.layout.reconcile(&entries, &a.new_targets);
    a.new_targets.retain(|k, _| !entries.iter().any(|e| path_key(&e.path) == *k));
    a.meta = refresh_meta(&entries);
    a.entries = entries;
    if !r.added.is_empty() || !r.removed.is_empty() {
        for p in &r.removed {
            log(format!("文件已不在桌面，从布局移除：{p}"));
        }
        mark_dirty();
        update_desktop_cells();
    }
    invalidate_all();
    if a.rename_new_until.is_some_and(|t| Instant::now() < t) && r.added.len() == 1 {
        a.rename_new_until = None;
        if let Some(i) = a.panels.iter().position(|p| p.kind == Kind::Desktop) {
            start_rename_item(i, &r.added[0]);
        }
    }
}

fn drain_changes() {
    let a = app();
    let mut dirty = false;
    while let Ok(c) = a.changes.try_recv() {
        match c {
            Change::Dirty => dirty = true,
            Change::Renamed { old, new } => {
                let (o, n) = (a.roots.to_var(&old), a.roots.to_var(&new));
                if a.layout.rename_item(&o, &n) {
                    mark_dirty();
                }
                dirty = true;
            }
        }
    }
    if dirty {
        // 解压、批量复制时变化一连串到来，攒一下再扫描。
        unsafe {
            SetTimer(Some(a.host), TIMER_RESCAN, 250, None);
        }
    }
}

fn set_hidden(hidden: bool) {
    let a = app();
    a.hidden = hidden;
    for p in &a.panels {
        if matches!(p.kind, Kind::Part(_)) && p.parent.is_none() {
            unsafe {
                let _ = ShowWindowAsync(p.hwnd, if hidden { SW_HIDE } else { SW_SHOWNA });
            }
        } else if p.kind == Kind::Desktop {
            invalidate(p.hwnd);
        }
    }
}

fn apply_display_change() {
    let a = app();
    let scale = system_scale();
    if (scale - a.scale).abs() > 0.001 {
        log(format!("缩放变化：{} -> {}", a.scale, scale));
        a.scale = scale;
        a.res.set_scale(scale);
    }
    a.work = work_area(scale);
    if let Some((progman, _)) = desktop::find_desktop() {
        a.origin = progman_origin(progman);
    }
    // 分辨率变小时，把跑出屏幕的分区拉回来。
    let (sw, sh) = a.work;
    let mut moved = false;
    for p in &mut a.layout.partitions {
        let (nx, ny) = (p.x.min((sw - 60.0).max(0.0)).max(0.0), p.y.min((sh - view::HEADER_H).max(0.0)).max(0.0));
        if (nx, ny) != (p.x, p.y) {
            p.x = nx;
            p.y = ny;
            moved = true;
        }
    }
    if moved {
        mark_dirty();
    }
    sync_panels();
}

// ---- 窗口过程 ----

fn panel_index(h: HWND) -> Option<usize> {
    app().panels.iter().position(|p| p.hwnd == h)
}

fn lparam_xy(lp: LPARAM) -> (i32, i32) {
    ((lp.0 & 0xffff) as i16 as i32, ((lp.0 >> 16) & 0xffff) as i16 as i32)
}

fn cursor() -> POINT {
    let mut p = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    p
}

fn key_down(vk: VIRTUAL_KEY) -> bool {
    unsafe { GetKeyState(vk.0 as i32) < 0 }
}

fn hit_test(i: usize, x: f32, y: f32) -> Hit {
    let a = app();
    let panel = &a.panels[i];
    match &panel.kind {
        Kind::Desktop if a.hidden => Hit::Blank,
        Kind::Desktop => view::hit_desktop(&a.layout.desktop, &panel.cells, &a.meta, x, y),
        Kind::Part(id) => match a.layout.partition(id) {
            Some(p) => view::hit_partition(&mut a.res, &a.layout.style, p, &a.meta, x, y, panel.w, panel.h, panel.scroll),
            None => Hit::None,
        },
    }
}

fn panel_tab(i: usize) -> Option<&'static mut Tab> {
    let a = app();
    match &a.panels[i].kind {
        Kind::Desktop => Some(&mut a.layout.desktop),
        Kind::Part(id) => {
            let id = id.clone();
            let p = a.layout.partition_mut(&id)?;
            let at = p.active_tab();
            p.tabs.get_mut(at)
        }
    }
}

fn panel_partition(i: usize) -> Option<&'static mut Partition> {
    let a = app();
    let Kind::Part(id) = &a.panels[i].kind else { return None };
    let id = id.clone();
    a.layout.partition_mut(&id)
}

fn selected_paths(i: usize) -> Vec<String> {
    let a = app();
    let Some(tab) = panel_tab(i) else { return Vec::new() };
    let sel = &a.panels[i].selected;
    tab.items.iter().filter(|it| sel.contains(&path_key(&it.path))).map(|it| it.path.clone()).collect()
}

fn shell_names(paths: &[String]) -> Vec<String> {
    let a = app();
    paths.iter().map(|p| shell::parse_name(&a.roots.expand(p))).collect()
}

fn select_item(i: usize, index: usize) {
    let Some(tab) = panel_tab(i) else { return };
    let a = app();
    let key = path_key(&tab.items[index].path);
    let order = view::display_order(tab, &a.meta);
    let panel = &mut a.panels[i];
    if key_down(VK_SHIFT) {
        if let Some(anchor) = panel.anchor {
            let (p0, p1) = (order.iter().position(|&o| o == anchor), order.iter().position(|&o| o == index));
            if let (Some(p0), Some(p1)) = (p0, p1) {
                if !key_down(VK_CONTROL) {
                    panel.selected.clear();
                }
                for &o in &order[p0.min(p1)..=p0.max(p1)] {
                    panel.selected.insert(path_key(&tab.items[o].path));
                }
                return;
            }
        }
    }
    if key_down(VK_CONTROL) {
        if !panel.selected.remove(&key) {
            panel.selected.insert(key);
        }
    } else if !panel.selected.contains(&key) {
        panel.selected.clear();
        panel.selected.insert(key);
    }
    panel.anchor = Some(index);
}

fn clear_other_selections(keep: usize) {
    for (j, p) in app().panels.iter_mut().enumerate() {
        if j != keep && !p.selected.is_empty() {
            p.selected.clear();
            invalidate(p.hwnd);
        }
    }
}

fn open_selected(i: usize) {
    let a = app();
    for p in selected_paths(i) {
        shell::open(&a.roots.expand(&p));
    }
}

fn on_left_down(i: usize, h: HWND, x: i32, y: i32) {
    let a = app();
    unsafe {
        let _ = SetFocus(Some(h));
    }
    let scale = a.scale;
    let (dx, dy) = (x as f32 / scale, y as f32 / scale);
    let hit = hit_test(i, dx, dy);
    clear_other_selections(i);
    // 点到的分区提到同级最上面，它的各级上级也一样。
    if matches!(a.panels[i].kind, Kind::Part(_)) {
        let mut cur = Some(h);
        while let Some(w) = cur {
            unsafe {
                let _ = SetWindowPos(w, Some(HWND_TOP), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
            cur = a.panels.iter().find(|p| p.hwnd == w).and_then(|p| p.parent);
        }
    }
    let sp = cursor();
    match hit {
        Hit::Chevron => {
            if let Some(p) = panel_partition(i) {
                p.collapsed = !p.collapsed;
                mark_dirty();
                sync_panels();
            }
        }
        Hit::Lock => {
            if let Some(p) = panel_partition(i) {
                p.locked = !p.locked;
                mark_dirty();
                invalidate(h);
            }
        }
        Hit::Add => add_menu(i, sp),
        Hit::More => {
            partition_menu(i, sp);
        }
        Hit::Tab(t) => {
            if let Some(p) = panel_partition(i) {
                if p.active != t {
                    p.active = t;
                    mark_dirty();
                    let panel = &mut a.panels[i];
                    panel.scroll = 0.0;
                    panel.selected.clear();
                    // 每个标签有自己的子分区，切换后窗口要跟着变。
                    sync_panels();
                    invalidate(h);
                }
            }
        }
        Hit::Item(index) => {
            select_item(i, index);
            a.panels[i].action = Action::Press { hit, x: sp.x, y: sp.y };
            unsafe { SetCapture(h) };
            invalidate(h);
        }
        Hit::Title => {
            a.panels[i].action = Action::Press { hit, x: sp.x, y: sp.y };
            unsafe { SetCapture(h) };
        }
        Hit::Edge(mask) => {
            if let Some(p) = panel_partition(i) {
                a.panels[i].action = Action::Resize { mask, sx: sp.x, sy: sp.y, ox: p.x, oy: p.y, ow: p.w, oh: p.h };
                unsafe { SetCapture(h) };
            }
        }
        Hit::Blank => {
            let panel = &mut a.panels[i];
            panel.selected.clear();
            panel.action = Action::Band { x0: dx, y0: dy };
            let is_desktop = panel.kind == Kind::Desktop;
            unsafe { SetCapture(h) };
            if is_desktop && !a.hidden {
                // 框选要画到表面之外的地方，先把表面扩到全屏。
                a.panels[i].band = Some(Rect { l: dx, t: dy, r: dx, b: dy });
                update_desktop_cells();
            }
            invalidate(h);
        }
        Hit::None => {}
    }
}

fn begin_item_drag(i: usize, h: HWND) {
    let a = app();
    let paths = selected_paths(i);
    a.panels[i].action = Action::None;
    unsafe {
        let _ = ReleaseCapture();
    }
    if paths.is_empty() {
        return;
    }
    a.dragging = paths.clone();
    let names = shell_names(&paths);
    let result = shell::drag(h, &names);
    a.dragging.clear();
    if let Err(e) = result {
        log(format!("拖放失败：{e}"));
        return;
    }
    if a.panels.len() > i {
        a.panels[i].selected.clear();
    }
    invalidate_all();
}

fn on_mouse_move(i: usize, h: HWND, x: i32, y: i32) {
    let a = app();
    let scale = a.scale;
    let (dx, dy) = (x as f32 / scale, y as f32 / scale);
    let sp = cursor();
    match a.panels[i].action {
        Action::Press { hit, x: px0, y: py0 } => {
            if (sp.x - px0).abs() < DRAG_THRESHOLD && (sp.y - py0).abs() < DRAG_THRESHOLD {
                return;
            }
            match hit {
                Hit::Item(_) => begin_item_drag(i, h),
                Hit::Title => {
                    if let Some(p) = panel_partition(i) {
                        a.panels[i].action = if p.locked { Action::None } else { Action::Move { sx: px0, sy: py0, ox: p.x, oy: p.y } };
                    }
                }
                _ => a.panels[i].action = Action::None,
            }
        }
        Action::Move { sx, sy, ox, oy } => {
            if let Some(p) = panel_partition(i) {
                let (mut nx, mut ny) = (ox + (sp.x - sx) as f32 / scale, oy + (sp.y - sy) as f32 / scale);
                // 子分区不能拖出上级，否则会被裁掉，再也抓不回来。
                if let Some(parent) = a.panels[i].parent {
                    if let Some(up) = a.panels.iter().find(|q| q.hwnd == parent) {
                        nx = nx.clamp(0.0, (up.w - 40.0).max(0.0));
                        ny = ny.max(0.0);
                    }
                }
                p.x = nx;
                p.y = ny;
                if let Some((x, y)) = panel_pos(i) {
                    unsafe {
                        let _ = SetWindowPos(h, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
                    }
                }
            }
        }
        Action::Resize { mask, sx, sy, ox, oy, ow, oh } => {
            if let Some(p) = panel_partition(i) {
                let (ddx, ddy) = ((sp.x - sx) as f32 / scale, (sp.y - sy) as f32 / scale);
                let (mut nx, mut ny, mut nw, mut nh) = (ox, oy, ow, oh);
                if mask & 4 != 0 {
                    nw = (ow + ddx).max(view::MIN_W);
                }
                if mask & 8 != 0 {
                    nh = (oh + ddy).max(view::MIN_H);
                }
                if mask & 1 != 0 {
                    nw = (ow - ddx).max(view::MIN_W);
                    nx = ox + ow - nw;
                }
                if mask & 2 != 0 {
                    nh = (oh - ddy).max(view::MIN_H);
                    ny = oy + oh - nh;
                }
                p.x = nx;
                p.y = ny;
                p.w = nw;
                p.h = nh;
                place_panel(i);
            }
        }
        Action::Band { x0, y0 } => {
            let band = Rect { l: x0.min(dx), t: y0.min(dy), r: x0.max(dx), b: y0.max(dy) };
            if a.panels[i].kind == Kind::Desktop {
                let panel = &a.panels[i];
                let tab = &a.layout.desktop;
                let hits = view::items_in_band_desktop(tab, &panel.cells, &a.meta, &band);
                let selected = hits.iter().map(|&n| path_key(&tab.items[n].path)).collect();
                let panel = &mut a.panels[i];
                panel.selected = selected;
                panel.band = Some(band);
                invalidate(h);
            } else if let Some(p) = panel_partition(i) {
                let panel = &a.panels[i];
                let hits = view::items_in_band(p, &a.layout.style, &a.meta, &band, panel.w, panel.scroll);
                let tab = &p.tabs[p.active_tab()];
                let selected = hits.iter().map(|&n| path_key(&tab.items[n].path)).collect();
                let panel = &mut a.panels[i];
                panel.selected = selected;
                panel.band = Some(band);
                invalidate(h);
            }
        }
        Action::None => {
            let hit = hit_test(i, dx, dy);
            let panel = &mut a.panels[i];
            if !panel.tracking {
                let mut t = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: h, dwHoverTime: 0 };
                unsafe {
                    let _ = TrackMouseEvent(&mut t);
                }
                panel.tracking = true;
            }
            if panel.hover != hit {
                panel.hover = hit;
                invalidate(h);
            }
        }
    }
}

fn on_left_up(i: usize, h: HWND) {
    let a = app();
    let action = a.panels[i].action;
    a.panels[i].action = Action::None;
    unsafe {
        let _ = ReleaseCapture();
    }
    match action {
        Action::Move { .. } | Action::Resize { .. } => {
            mark_dirty();
            // 分区挪开或占住的格子变了，底层桌面和上级分区的图标要重新排。
            update_desktop_cells();
            if let Some(parent) = a.panels[i].parent {
                invalidate(parent);
            }
        }
        Action::Band { .. } => {
            a.panels[i].band = None;
            if a.panels[i].kind == Kind::Desktop {
                update_desktop_cells();
            }
            invalidate(h);
        }
        Action::Press { hit: Hit::Item(index), .. } => {
            // 没拖动的单击：多选中点一项，收窄为这一项。
            if !key_down(VK_CONTROL) && !key_down(VK_SHIFT) {
                if let Some(tab) = panel_tab(i) {
                    if let Some(it) = tab.items.get(index) {
                        let panel = &mut a.panels[i];
                        panel.selected.clear();
                        panel.selected.insert(path_key(&it.path));
                        invalidate(h);
                    }
                }
            }
        }
        _ => {}
    }
}

fn on_double_click(i: usize, x: i32, y: i32) {
    let a = app();
    let hit = hit_test(i, x as f32 / a.scale, y as f32 / a.scale);
    match hit {
        Hit::Item(_) => open_selected(i),
        Hit::Blank if a.panels[i].kind == Kind::Desktop => set_hidden(!a.hidden),
        Hit::Title => {
            if let Some(p) = panel_partition(i) {
                p.collapsed = !p.collapsed;
                mark_dirty();
                sync_panels();
            }
        }
        _ => {}
    }
}

fn on_right_up(i: usize, h: HWND, x: i32, y: i32) {
    let a = app();
    let hit = hit_test(i, x as f32 / a.scale, y as f32 / a.scale);
    let sp = cursor();
    match hit {
        Hit::Item(index) => {
            if let Some(tab) = panel_tab(i) {
                let key = path_key(&tab.items[index].path);
                let panel = &mut a.panels[i];
                if !panel.selected.contains(&key) {
                    panel.selected.clear();
                    panel.selected.insert(key);
                }
            }
            clear_other_selections(i);
            invalidate(h);
            let paths = selected_paths(i);
            let names = shell_names(&paths);
            match shell::context_menu(a.host, &names, sp.x, sp.y) {
                Ok(MenuResult::Rename) => start_rename_item(i, &paths[0]),
                Ok(_) => {}
                Err(e) => log(format!("右键菜单失败：{e}")),
            }
        }
        Hit::Blank if a.panels[i].kind == Kind::Desktop => desktop_menu(sp),
        Hit::Title | Hit::Blank | Hit::Tab(_) => partition_menu(i, sp),
        _ => {}
    }
}

fn on_key(i: usize, vk: u16) {
    let a = app();
    let ctrl = key_down(VK_CONTROL);
    match VIRTUAL_KEY(vk) {
        VK_F5 => rescan(),
        VK_RETURN => open_selected(i),
        VK_F2 => {
            if let Some(p) = selected_paths(i).first() {
                start_rename_item(i, p);
            }
        }
        VK_DELETE => {
            let names = shell_names(&selected_paths(i));
            if !names.is_empty() {
                if let Err(e) = shell::invoke_verb(a.host, &names, "delete") {
                    log(format!("删除失败：{e}"));
                }
            }
        }
        VK_ESCAPE => {
            a.panels[i].selected.clear();
            invalidate(a.panels[i].hwnd);
        }
        k if ctrl && (k == VIRTUAL_KEY(b'C' as u16) || k == VIRTUAL_KEY(b'X' as u16)) => {
            let names = shell_names(&selected_paths(i));
            if !names.is_empty() {
                let verb = if k == VIRTUAL_KEY(b'C' as u16) { "copy" } else { "cut" };
                if let Err(e) = shell::invoke_verb(a.host, &names, verb) {
                    log(format!("{verb} 失败：{e}"));
                }
            }
        }
        k if ctrl && k == VIRTUAL_KEY(b'A' as u16) => {
            if let Some(tab) = panel_tab(i) {
                let all = tab.items.iter().map(|it| path_key(&it.path)).collect();
                a.panels[i].selected = all;
                invalidate(a.panels[i].hwnd);
            }
        }
        _ => {}
    }
}

unsafe extern "system" fn panel_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if !app_ready() {
            return DefWindowProcW(h, msg, wp, lp);
        }
        if msg == WM_DESTROY {
            let a = app();
            // 不是我们自己销毁的，说明父窗口（资源管理器）没了，等它回来再挂载。
            if !a.shutting_down && panel_index(h).is_some() {
                log("分区窗口被连带销毁，等待资源管理器恢复");
                SetTimer(Some(a.host), TIMER_REMOUNT, 500, None);
            }
            return LRESULT(0);
        }
        let Some(i) = panel_index(h) else { return DefWindowProcW(h, msg, wp, lp) };
        match msg {
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let _ = BeginPaint(h, &mut ps);
                let _ = EndPaint(h, &ps);
                paint(h);
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let (x, y) = lparam_xy(lp);
                on_left_down(i, h, x, y);
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                on_left_up(i, h);
                LRESULT(0)
            }
            WM_LBUTTONDBLCLK => {
                let (x, y) = lparam_xy(lp);
                on_double_click(i, x, y);
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                let (x, y) = lparam_xy(lp);
                on_right_up(i, h, x, y);
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let (x, y) = lparam_xy(lp);
                on_mouse_move(i, h, x, y);
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                let panel = &mut app().panels[i];
                panel.tracking = false;
                if panel.hover != Hit::None {
                    panel.hover = Hit::None;
                    invalidate(h);
                }
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                let delta = ((wp.0 >> 16) & 0xffff) as i16 as f32;
                let a = app();
                let mut scrolled = false;
                if let Some(p) = panel_partition(i) {
                    let tab = &p.tabs[p.active_tab()];
                    let panel = &mut a.panels[i];
                    let max = view::max_scroll(p, &a.layout.style, panel.w, panel.h);
                    let step = if tab.view == View::List { tab.cell_h * 3.0 } else { tab.cell_h };
                    let next = (panel.scroll - delta / 120.0 * step).clamp(0.0, max);
                    scrolled = next != panel.scroll;
                    panel.scroll = next;
                    invalidate(h);
                }
                if scrolled {
                    place_children(h);
                } else if let Some(parent) = a.panels[i].parent {
                    // 子分区滚到头了（或不用滚），把滚轮交给上级。
                    SendMessageW(parent, msg, Some(wp), Some(lp));
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                on_key(i, wp.0 as u16);
                LRESULT(0)
            }
            WM_SETCURSOR => {
                let a = app();
                let c = cursor();
                let mut pt = c;
                let _ = ScreenToClient(h, &mut pt);
                let id = match hit_test(i, pt.x as f32 / a.scale, pt.y as f32 / a.scale) {
                    Hit::Edge(1) | Hit::Edge(4) => IDC_SIZEWE,
                    Hit::Edge(2) | Hit::Edge(8) => IDC_SIZENS,
                    Hit::Edge(3) | Hit::Edge(12) => IDC_SIZENWSE,
                    Hit::Edge(6) | Hit::Edge(9) => IDC_SIZENESW,
                    _ => IDC_ARROW,
                };
                if let Ok(cur) = LoadCursorW(None, id) {
                    SetCursor(Some(cur));
                }
                LRESULT(1)
            }
            _ => DefWindowProcW(h, msg, wp, lp),
        }
    }
}

unsafe extern "system" fn host_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if !app_ready() {
            return DefWindowProcW(h, msg, wp, lp);
        }
        let mut handled = LRESULT(0);
        if shell::handle_menu_message(msg, wp, lp, &mut handled) {
            return handled;
        }
        let a = app();
        if msg == a.taskbar_created {
            log("资源管理器已重启");
            add_tray();
            SetTimer(Some(h), TIMER_REMOUNT, 300, None);
            return LRESULT(0);
        }
        match msg {
            WM_APP_ICONS => {
                a.loader.pending_wake.store(false, Ordering::Release);
                while let Ok(l) = a.loader.rx.try_recv() {
                    a.res.accept(l);
                }
                invalidate_all();
                LRESULT(0)
            }
            WM_APP_FS => {
                drain_changes();
                LRESULT(0)
            }
            WM_APP_TRAY => {
                let event = (lp.0 & 0xffff) as u32;
                if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                    tray_menu();
                } else if event == WM_LBUTTONDBLCLK {
                    set_hidden(!a.hidden);
                }
                LRESULT(0)
            }
            WM_APP_TOGGLE => {
                set_hidden(!a.hidden);
                LRESULT(0)
            }
            WM_APP_EDIT_DONE => {
                finish_edit(wp.0 == 1);
                LRESULT(0)
            }
            WM_TIMER => {
                match wp.0 {
                    TIMER_SAVE => {
                        let _ = KillTimer(Some(h), TIMER_SAVE);
                        save_now();
                    }
                    TIMER_RESCAN => {
                        let _ = KillTimer(Some(h), TIMER_RESCAN);
                        rescan();
                    }
                    TIMER_REMOUNT => {
                        let alive = !a.panels.is_empty() && a.panels.iter().all(|p| IsWindow(Some(p.hwnd)).as_bool());
                        if alive {
                            let _ = KillTimer(Some(h), TIMER_REMOUNT);
                        } else if mount() {
                            let _ = KillTimer(Some(h), TIMER_REMOUNT);
                        }
                    }
                    TIMER_ICONS_CHECK => {
                        // 资源管理器刚起来时可能晚一点才把图标层显示出来，挂载后盯一小会儿。
                        if desktop::icons_visible() == Some(true) {
                            desktop::set_icons_visible(false);
                        }
                        a.icons_checks = a.icons_checks.saturating_sub(1);
                        if a.icons_checks == 0 {
                            let _ = KillTimer(Some(h), TIMER_ICONS_CHECK);
                        }
                    }
                    TIMER_STATS => {
                        let _ = KillTimer(Some(h), TIMER_STATS);
                        log_memory();
                    }
                    TIMER_TRIM => {
                        let _ = KillTimer(Some(h), TIMER_TRIM);
                        a.gfx.trim();
                    }
                    TIMER_DISPLAY => {
                        let _ = KillTimer(Some(h), TIMER_DISPLAY);
                        apply_display_change();
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            // 第 0 阶段实测：切换缩放时只有 WM_DISPLAYCHANGE，没有 WM_DPICHANGED。
            WM_DISPLAYCHANGE | WM_DPICHANGED | WM_SETTINGCHANGE => {
                SetTimer(Some(h), TIMER_DISPLAY, 300, None);
                DefWindowProcW(h, msg, wp, lp)
            }
            WM_QUERYENDSESSION => LRESULT(1),
            WM_ENDSESSION => {
                if wp.0 != 0 {
                    save_now();
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(h, msg, wp, lp),
        }
    }
}

fn log_memory() {
    unsafe {
        let mut c = PROCESS_MEMORY_COUNTERS_EX::default();
        let _ = K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS, size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32);
        log(format!("内存：私有 {:.1} MB，工作集 {:.1} MB", c.PrivateUsage as f64 / 1048576.0, c.WorkingSetSize as f64 / 1048576.0));
    }
}

// ---- 菜单 ----

fn append(menu: HMENU, id: u32, text: &str, checked: bool) {
    let t = wide(text);
    let flags = MF_STRING | if checked { MF_CHECKED } else { MF_UNCHECKED };
    unsafe {
        let _ = AppendMenuW(menu, flags, id as usize, PCWSTR(t.as_ptr()));
    }
}

fn separator(menu: HMENU) {
    unsafe {
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    }
}

fn submenu(menu: HMENU, text: &str) -> HMENU {
    let t = wide(text);
    unsafe {
        let sub = CreatePopupMenu().unwrap_or_default();
        let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, PCWSTR(t.as_ptr()));
        sub
    }
}

fn track(menu: HMENU, pt: POINT) -> u32 {
    let a = app();
    unsafe {
        let _ = SetForegroundWindow(a.host);
        let cmd = TrackPopupMenuEx(menu, (TPM_RETURNCMD | TPM_RIGHTBUTTON).0, pt.x, pt.y, a.host, None).0 as u32;
        let _ = PostMessageW(Some(a.host), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        cmd
    }
}

fn partition_menu(i: usize, pt: POINT) {
    let a = app();
    let Some(tab) = panel_tab(i) else { return };
    let locked = panel_partition(i).map(|p| p.locked).unwrap_or(false);
    let tabs = panel_partition(i).map(|p| p.tabs.len()).unwrap_or(1);
    let nested = a.panels[i].parent.is_some();
    let menu = unsafe { CreatePopupMenu().unwrap_or_default() };

    let v = submenu(menu, "查看");
    append(v, M_VIEW_ICONS, "图标", tab.view == View::Icons);
    append(v, M_VIEW_LIST, "列表", tab.view == View::List);
    separator(v);
    append(v, M_ICON_S, "小图标", tab.icon < 30.0);
    append(v, M_ICON_M, "中图标", (30.0..44.0).contains(&tab.icon));
    append(v, M_ICON_L, "大图标", tab.icon >= 44.0);
    let s = submenu(menu, "排序");
    append(s, M_SORT_MANUAL, "手动", tab.sort == SortKey::Manual);
    append(s, M_SORT_NAME, "名称", tab.sort == SortKey::Name);
    append(s, M_SORT_MODIFIED, "修改时间", tab.sort == SortKey::Modified);
    append(s, M_SORT_SIZE, "大小", tab.sort == SortKey::Size);
    append(s, M_SORT_KIND, "类型", tab.sort == SortKey::Kind);
    separator(s);
    append(s, M_SORT_DESC, "降序", tab.desc);
    separator(menu);
    append(menu, M_SUB_NEW, "新建子分区", false);
    append(menu, M_TAB_NEW, "新建标签", false);
    separator(menu);
    append(menu, M_TAB_RENAME, "重命名", false);
    if tabs > 1 {
        append(menu, M_TAB_DELETE, "删除这个标签（图标回到上一级）", false);
    }
    append(menu, M_LOCK, "锁定", locked);
    append(menu, M_PART_DELETE, if nested { "删除子分区（图标回到上一级）" } else { "删除分区（图标回到桌面）" }, false);

    let cmd = track(menu, pt);
    let Some(tab) = panel_tab(i) else { return };
    let mut icon_size = |size: f32| {
        // 格子跟着图标一起缩放，保持现在的留白比例。
        let k = size / tab.icon.max(1.0);
        tab.icon = size;
        tab.cell_w = (tab.cell_w * k).clamp(52.0, 160.0);
        tab.cell_h = (tab.cell_h * k).clamp(60.0, 170.0);
    };
    match cmd {
        0 => return,
        M_VIEW_ICONS => {
            tab.view = View::Icons;
            // 从小智导入的列表标签图标只有 16，格子宽是 0，切回图标视图要给合理的尺寸。
            if tab.cell_w < 1.0 || tab.icon < 24.0 {
                tab.icon = 36.0;
                tab.cell_w = 75.0;
                tab.cell_h = 82.0;
            }
        }
        M_VIEW_LIST => {
            tab.view = View::List;
            tab.cell_h = 28.0;
        }
        M_ICON_S => icon_size(24.0),
        M_ICON_M => icon_size(36.0),
        M_ICON_L => icon_size(48.0),
        M_SORT_MANUAL => tab.sort = SortKey::Manual,
        M_SORT_NAME => tab.sort = SortKey::Name,
        M_SORT_MODIFIED => tab.sort = SortKey::Modified,
        M_SORT_SIZE => tab.sort = SortKey::Size,
        M_SORT_KIND => tab.sort = SortKey::Kind,
        M_SORT_DESC => tab.desc = !tab.desc,
        M_TAB_NEW => add_tab(i),
        M_SUB_NEW => {
            new_sub_partition(i, Some(pt));
            return;
        }
        M_TAB_RENAME => {
            let id = tab.id.clone();
            let name = tab.name.clone();
            start_tab_rename(i, id, name);
        }
        M_TAB_DELETE => delete_tab(i),
        M_LOCK => {
            if let Some(p) = panel_partition(i) {
                p.locked = !p.locked;
            }
        }
        M_PART_DELETE => {
            if let Some(p) = panel_partition(i) {
                let n: usize = p.tabs.iter().map(|t| t.items.len()).sum();
                let to = if nested { "上一级" } else { "桌面" };
                if n == 0 && p.tabs.iter().all(|t| t.children.is_empty()) || confirm(&format!("删除这个分区？里面的图标（包括子分区里的）会回到{to}，文件不受影响。")) {
                    let id = p.id.clone();
                    a.layout.remove_partition(&id);
                    mark_dirty();
                    sync_panels();
                    return;
                }
            }
        }
        _ => {}
    }
    mark_dirty();
    if let Some(panel) = a.panels.get_mut(i) {
        panel.scroll = 0.0;
    }
    // 改了视图或图标大小，子分区的相对位置不变，但滚动量清零了。
    sync_panels();
    invalidate_all();
}

fn add_menu(i: usize, pt: POINT) {
    let menu = unsafe { CreatePopupMenu().unwrap_or_default() };
    append(menu, M_SUB_NEW, "新建子分区", false);
    append(menu, M_TAB_NEW, "新建标签", false);
    match track(menu, pt) {
        M_SUB_NEW => new_sub_partition(i, None),
        M_TAB_NEW => add_tab(i),
        _ => {}
    }
}

/// 在分区内创建子分区。`at` 是屏幕坐标，None 时放在内容区左上角。
fn new_sub_partition(i: usize, at: Option<POINT>) {
    let a = app();
    let scroll = a.panels[i].scroll;
    let panel_w = a.panels[i].w;
    let hwnd = a.panels[i].hwnd;
    let Some(p) = panel_partition(i) else { return };
    let top = view::content_top(p);
    let (mut x, mut y) = (view::PAD, view::PAD);
    if let Some(pt) = at {
        let mut c = pt;
        unsafe {
            let _ = ScreenToClient(hwnd, &mut c);
        }
        x = c.x as f32 / a.scale;
        y = c.y as f32 / a.scale - top + scroll;
    }
    let w = 240.0f32.min((panel_w - view::PAD * 2.0).max(view::MIN_W));
    x = x.clamp(0.0, (panel_w - w).max(0.0));
    y = y.max(0.0);
    p.collapsed = false;
    let child = Partition::new("子分区", x, y, w, 170.0);
    let (id, tab_id, name) = (child.id.clone(), child.tabs[0].id.clone(), child.tabs[0].name.clone());
    let at_tab = p.active_tab();
    p.tabs[at_tab].children.push(child);
    mark_dirty();
    sync_panels();
    invalidate(hwnd);
    if let Some(j) = a.panels.iter().position(|q| q.kind == Kind::Part(id.clone())) {
        start_tab_rename(j, tab_id, name);
    }
}

/// 桌面空白处的右键菜单：我们自己的项加上系统的桌面菜单。
fn desktop_menu(pt: POINT) {
    let a = app();
    let tab = &a.layout.desktop;
    let menu = unsafe { CreatePopupMenu().unwrap_or_default() };
    append(menu, M_PART_NEW, "新建分区", false);
    append(menu, M_ORGANIZE, "一键整理桌面", false);
    let v = submenu(menu, "查看");
    append(v, M_ICON_S, "小图标", tab.icon < 30.0);
    append(v, M_ICON_M, "中图标", (30.0..44.0).contains(&tab.icon));
    append(v, M_ICON_L, "大图标", tab.icon >= 44.0);
    separator(v);
    append(v, M_TOGGLE, if a.hidden { "显示桌面图标" } else { "隐藏桌面图标" }, false);
    let s = submenu(menu, "排序方式");
    append(s, M_SORT_MANUAL, "手动", tab.sort == SortKey::Manual);
    append(s, M_SORT_NAME, "名称", tab.sort == SortKey::Name);
    append(s, M_SORT_MODIFIED, "修改时间", tab.sort == SortKey::Modified);
    append(s, M_SORT_SIZE, "大小", tab.sort == SortKey::Size);
    append(s, M_SORT_KIND, "类型", tab.sort == SortKey::Kind);
    separator(s);
    append(s, M_SORT_DESC, "降序", tab.desc);
    append(menu, M_REFRESH, "刷新", false);
    separator(menu);
    // 系统项放在后面：新建文件、粘贴、显示设置、个性化。
    let position = unsafe { GetMenuItemCount(Some(menu)) } as u32;
    let shell_menu = match shell::background_menu(a.host, menu, position) {
        Ok(m) => Some(m),
        Err(e) => {
            log(format!("系统桌面菜单加载失败：{e}"));
            None
        }
    };
    let cmd = track(menu, pt);
    if cmd >= shell::BG_FIRST {
        if let Some(m) = &shell_menu {
            a.rename_new_until = Some(Instant::now() + std::time::Duration::from_secs(5));
            if let Err(e) = shell::invoke_background(m, a.host, cmd) {
                log(format!("桌面菜单命令失败：{e}"));
            }
        }
    }
    shell::end_background_menu();
    drop(shell_menu);
    let tab = &mut a.layout.desktop;
    let mut icon_size = |size: f32| {
        let k = size / tab.icon.max(1.0);
        tab.icon = size;
        tab.cell_w = (tab.cell_w * k).clamp(52.0, 160.0);
        tab.cell_h = (tab.cell_h * k).clamp(60.0, 170.0);
    };
    match cmd {
        M_PART_NEW => {
            new_partition(Some(pt));
            return;
        }
        M_ORGANIZE => organize(),
        M_TOGGLE => {
            set_hidden(!a.hidden);
            return;
        }
        M_REFRESH => {
            rescan();
            return;
        }
        M_ICON_S => icon_size(24.0),
        M_ICON_M => icon_size(36.0),
        M_ICON_L => icon_size(48.0),
        M_SORT_MANUAL => tab.sort = SortKey::Manual,
        M_SORT_NAME => tab.sort = SortKey::Name,
        M_SORT_MODIFIED => tab.sort = SortKey::Modified,
        M_SORT_SIZE => tab.sort = SortKey::Size,
        M_SORT_KIND => tab.sort = SortKey::Kind,
        M_SORT_DESC => tab.desc = !tab.desc,
        _ => return,
    }
    mark_dirty();
    update_desktop_cells();
}

fn organize() {
    let a = app();
    let n = a.layout.organize(&a.entries);
    mark_dirty();
    update_desktop_cells();
    invalidate_all();
    message(&format!("已整理 {n} 个文件。快捷方式和系统图标留在原地。"));
}

fn add_tab(i: usize) {
    let Some(p) = panel_partition(i) else { return };
    let t = Tab::new("新标签");
    let (id, name) = (t.id.clone(), t.name.clone());
    p.tabs.push(t);
    p.active = p.tabs.len() - 1;
    mark_dirty();
    invalidate(app().panels[i].hwnd);
    start_tab_rename(i, id, name);
}

fn delete_tab(i: usize) {
    let a = app();
    let Some(p) = panel_partition(i) else { return };
    if p.tabs.len() < 2 {
        return;
    }
    let at = p.active_tab();
    let (name, empty) = (p.tabs[at].name.clone(), p.tabs[at].items.is_empty() && p.tabs[at].children.is_empty());
    if !empty && !confirm(&format!("删除标签“{name}”？里面的图标（包括子分区里的）会回到上一级，文件不受影响。")) {
        return;
    }
    let id = p.id.clone();
    if a.layout.remove_tab(&id, at) {
        mark_dirty();
        sync_panels();
        invalidate_all();
    }
}

/// `at` 是屏幕坐标，None 时放在屏幕中央附近。
fn new_partition(at: Option<POINT>) {
    let a = app();
    let n = a.layout.partitions.len() as f32;
    let (w, h) = (360.0f32, 280.0f32);
    let (x, y) = match at {
        Some(pt) => (pt.x as f32 / a.scale, pt.y as f32 / a.scale),
        None => (a.work.0 / 2.0 - w / 2.0 + n * 16.0, a.work.1 / 2.0 - h / 2.0 + n * 16.0),
    };
    let p = Partition::new("新建分区", x.clamp(0.0, (a.work.0 - w).max(0.0)), y.clamp(0.0, (a.work.1 - h).max(0.0)), w, h);
    let (id, name, pid) = (p.tabs[0].id.clone(), p.tabs[0].name.clone(), p.id.clone());
    a.layout.partitions.push(p);
    mark_dirty();
    sync_panels();
    if let Some(i) = a.panels.iter().position(|x| x.kind == Kind::Part(pid.clone())) {
        start_tab_rename(i, id, name);
    }
}

fn tray_menu() {
    let a = app();
    let menu = unsafe { CreatePopupMenu().unwrap_or_default() };
    append(menu, M_PART_NEW, "新建分区", false);
    append(menu, M_TOGGLE, if a.hidden { "显示桌面图标" } else { "隐藏桌面图标" }, false);
    append(menu, M_ORGANIZE, "一键整理桌面", false);
    let alpha = submenu(menu, "分区背景深浅");
    for (i, (value, label)) in ALPHAS.iter().enumerate() {
        append(alpha, M_ALPHA_BASE + i as u32, label, (a.layout.style.bg_alpha - value).abs() < 0.01);
    }
    separator(menu);
    append(menu, M_EXPORT, "导出分类结构…", false);
    append(menu, M_IMPORT, "导入分类结构…", false);
    if util::xz_data_dir().is_some() {
        append(menu, M_IMPORT_XZ, "从小智桌面重新导入", false);
    }
    append(menu, M_OPEN_DATA, "打开数据目录", false);
    separator(menu);
    append(menu, M_AUTOSTART, "开机自启", autostart_enabled());
    append(menu, M_EXIT, "退出", false);
    match track(menu, cursor()) {
        M_PART_NEW => new_partition(None),
        M_TOGGLE => set_hidden(!a.hidden),
        M_ORGANIZE => organize(),
        c if (M_ALPHA_BASE..M_ALPHA_BASE + ALPHAS.len() as u32).contains(&c) => {
            a.layout.style.bg_alpha = ALPHAS[(c - M_ALPHA_BASE) as usize].0;
            mark_dirty();
            invalidate_all();
        }
        M_EXPORT => export_layout(),
        M_IMPORT => import_layout(),
        M_IMPORT_XZ => reimport_xz(),
        M_OPEN_DATA => shell::open(&util::data_dir().to_string_lossy()),
        M_AUTOSTART => set_autostart(!autostart_enabled(), None),
        M_EXIT => unsafe {
            let _ = DestroyWindow(a.host);
        },
        _ => {}
    }
}

fn replace_layout(mut new: Layout) {
    let a = app();
    new.reconcile(&a.entries, &HashMap::new());
    a.layout = new;
    save_now();
    // 字体大小可能变了，文字资源重建。
    if let Ok(res) = Res::new(a.gfx.dwrite.clone(), a.scale, a.roots.clone(), &a.layout.style) {
        a.res = res;
    }
    for p in &mut a.panels {
        p.selected.clear();
        p.scroll = 0.0;
    }
    sync_panels();
}

fn reimport_xz() {
    let a = app();
    let Some(dir) = util::xz_data_dir() else { return };
    if !confirm("用小智桌面的分类结构替换当前布局？当前布局会先留一份备份。") {
        return;
    }
    match xz::import(&dir, &a.roots) {
        Ok((l, r)) => {
            layout::rotate_backups(&a.layout_path, 5);
            replace_layout(l);
            message(&format!("已导入 {} 个分区、{} 个标签、{} 个图标。", r.partitions, r.tabs, r.items));
        }
        Err(e) => message(&format!("导入失败，当前布局没有改动。\n{e}")),
    }
}

fn file_dialog(save: bool) -> Option<PathBuf> {
    let a = app();
    let mut buf = wide("FractalBranch-layout.json");
    buf.resize(1024, 0);
    let filter = wide("分类结构 (*.json)\0*.json\0");
    let def = wide("json");
    let mut ofn = OPENFILENAMEW {
        lStructSize: size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: a.host,
        lpstrFilter: PCWSTR(filter.as_ptr()),
        lpstrFile: PWSTR(buf.as_mut_ptr()),
        nMaxFile: buf.len() as u32,
        lpstrDefExt: PCWSTR(def.as_ptr()),
        Flags: if save { OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR } else { OFN_FILEMUSTEXIST | OFN_NOCHANGEDIR },
        ..Default::default()
    };
    let ok = unsafe { if save { GetSaveFileNameW(&mut ofn).as_bool() } else { GetOpenFileNameW(&mut ofn).as_bool() } };
    if !ok {
        return None;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
    Some(PathBuf::from(String::from_utf16_lossy(&buf[..n])))
}

fn export_layout() {
    let a = app();
    let Some(path) = file_dialog(true) else { return };
    match std::fs::write(&path, layout::to_json(&a.layout)) {
        Ok(()) => message(&format!("已导出到 {}", path.display())),
        Err(e) => message(&format!("导出失败：{e}")),
    }
}

fn import_layout() {
    let a = app();
    let Some(path) = file_dialog(false) else { return };
    match layout::load(&path) {
        Ok(l) => {
            layout::rotate_backups(&a.layout_path, 5);
            replace_layout(l);
        }
        Err(e) => message(&format!("导入失败，当前布局没有改动。\n{e}")),
    }
}

// ---- 托盘 ----

fn tray_data() -> NOTIFYICONDATAW {
    let a = app();
    let mut d = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: a.host,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_APP_TRAY,
        hIcon: a.tray_icon,
        ..Default::default()
    };
    for (slot, c) in d.szTip.iter_mut().zip("分形树枝".encode_utf16()) {
        *slot = c;
    }
    d
}

fn add_tray() {
    let d = tray_data();
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &d);
    }
}

fn remove_tray() {
    let d = tray_data();
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &d);
    }
}

/// 托盘图标：用代码画一棵小分形树，免得单独带图标文件。
fn make_tray_icon() -> HICON {
    // 优先用编进 exe 的图标，没有时才现画。
    unsafe {
        if let Ok(hinst) = GetModuleHandleW(None) {
            let (cx, cy) = (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON));
            if let Ok(h) = LoadImageW(Some(hinst.into()), PCWSTR(1 as *const u16), IMAGE_ICON, cx, cy, LR_DEFAULTCOLOR) {
                return HICON(h.0);
            }
        }
    }
    const S: usize = 32;
    let mut px = vec![0u32; S * S];
    fn line(px: &mut [u32], x0: f32, y0: f32, x1: f32, y1: f32, thick: f32) {
        let steps = ((x1 - x0).abs().max((y1 - y0).abs()) * 2.0) as i32 + 1;
        for s in 0..=steps {
            let t = s as f32 / steps as f32;
            let (cx, cy) = (x0 + (x1 - x0) * t, y0 + (y1 - y0) * t);
            let r = thick as i32 + 1;
            for dy in -r..=r {
                for dx in -r..=r {
                    let (x, y) = (cx as i32 + dx, cy as i32 + dy);
                    if (0..S as i32).contains(&x) && (0..S as i32).contains(&y) && ((dx * dx + dy * dy) as f32) <= thick * thick {
                        px[y as usize * S + x as usize] = 0xFF6FD08C;
                    }
                }
            }
        }
    }
    fn branch(px: &mut [u32], x: f32, y: f32, angle: f32, len: f32, depth: u32) {
        let (nx, ny) = (x + angle.cos() * len, y + angle.sin() * len);
        line(px, x, y, nx, ny, 0.6 + depth as f32 * 0.35);
        if depth > 0 {
            branch(px, nx, ny, angle - 0.5, len * 0.68, depth - 1);
            branch(px, nx, ny, angle + 0.5, len * 0.68, depth - 1);
        }
    }
    branch(&mut px, 16.0, 31.0, -std::f32::consts::FRAC_PI_2, 10.0, 4);
    unsafe {
        let color = CreateBitmap(S as i32, S as i32, 1, 32, Some(px.as_ptr() as *const _));
        let mask = CreateBitmap(S as i32, S as i32, 1, 1, None);
        let info = ICONINFO { fIcon: TRUE, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info).unwrap_or_default();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

// ---- 开机自启 ----

fn autostart_enabled() -> bool {
    let mut size = 0u32;
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None, None, Some(&mut size)).is_ok() }
}

/// 写 HKCU 的 Run 键，不需要管理员权限，卸载时删掉这一项即可。
/// `exe` 为 None 时用当前进程的路径。
pub fn set_autostart(enable: bool, exe: Option<&Path>) {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, KEY_SET_VALUE, &mut key).is_err() {
            log("打开自启注册表失败");
            return;
        }
        if enable {
            if let Some(exe) = exe.map(Path::to_path_buf).or_else(|| std::env::current_exe().ok()) {
                let cmd = wide(&format!("\"{}\" --autostart", exe.display()));
                let bytes = std::slice::from_raw_parts(cmd.as_ptr() as *const u8, cmd.len() * 2);
                let _ = RegSetValueExW(key, RUN_VALUE, None, REG_SZ, Some(bytes));
            }
        } else {
            let _ = RegDeleteValueW(key, RUN_VALUE);
        }
        let _ = RegCloseKey(key);
    }
}

// ---- 就地改名 ----

fn start_rename_item(i: usize, path: &str) {
    let a = app();
    let Some(tab) = panel_tab(i) else { return };
    let Some(it) = tab.items.iter().find(|it| it.path == path) else { return };
    if it.is_shell() {
        return;
    }
    let order = view::display_order(tab, &a.meta);
    let index = tab.items.iter().position(|x| x.path == path).unwrap_or(0);
    let Some(pos) = order.iter().position(|&o| o == index) else { return };
    let panel = &a.panels[i];
    let r = match &panel.kind {
        Kind::Desktop => panel.cells.get(pos).copied(),
        Kind::Part(_) => panel_partition(i).and_then(|p| {
            let dy = view::content_top(p) - panel.scroll;
            view::flow_cells(tab, &a.layout.style, tab.items.len(), panel.w).get(pos).map(|c| Rect { l: c.l, t: c.t + dy, r: c.r, b: c.b + dy })
        }),
    };
    let Some(r) = r else { return };
    let file = it.path.rsplit('\\').next().unwrap_or(&it.path).to_string();
    let mut origin = POINT { x: px(a, r.l), y: px(a, r.b) - px(a, 26.0) };
    unsafe {
        let _ = ClientToScreen(panel.hwnd, &mut origin);
    }
    show_edit(origin, px(a, (r.r - r.l).max(180.0)), px(a, 26.0), &file, EditTarget::File(path.to_string()));
}

fn start_tab_rename(i: usize, id: String, name: String) {
    let a = app();
    let mut origin = POINT { x: px(a, 54.0), y: px(a, 5.0) };
    unsafe {
        let _ = ClientToScreen(a.panels[i].hwnd, &mut origin);
    }
    show_edit(origin, px(a, (a.panels[i].w - 110.0).max(120.0)), px(a, 26.0), &name, EditTarget::TabName(id));
}

fn show_edit(pt: POINT, w: i32, h: i32, text: &str, target: EditTarget) {
    finish_edit(false);
    let a = app();
    let t = wide(text);
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
        let style = WS_POPUP | WS_BORDER | WS_VISIBLE | WINDOW_STYLE(ES_AUTOHSCROLL as u32);
        let Ok(edit) = CreateWindowExW(WS_EX_TOPMOST | WS_EX_TOOLWINDOW, w!("EDIT"), PCWSTR(t.as_ptr()), style, pt.x, pt.y, w, h, Some(a.host), None, Some(hinst), None) else {
            return;
        };
        let font = CreateFontW(-(h * 2 / 3), 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, 0, w!("Microsoft YaHei UI"));
        SendMessageW(edit, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
        let _ = SetWindowSubclass(edit, Some(edit_proc), 1, 0);
        let _ = SetForegroundWindow(edit);
        let _ = SetFocus(Some(edit));
        // 默认只选中主文件名，和资源管理器一样，改名时不会误删扩展名。
        let stem = text.rfind('.').filter(|&p| p > 0 && matches!(target, EditTarget::File(_))).map_or(text.encode_utf16().count(), |p| text[..p].encode_utf16().count());
        SendMessageW(edit, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(stem as isize)));
        app().edit = Some(Edit { hwnd: edit, font, target });
    }
}

unsafe extern "system" fn edit_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _: usize, _: usize) -> LRESULT {
    unsafe {
        let host = app().host;
        match msg {
            WM_KEYDOWN if wp.0 as u16 == VK_RETURN.0 => {
                let _ = PostMessageW(Some(host), WM_APP_EDIT_DONE, WPARAM(1), LPARAM(0));
                LRESULT(0)
            }
            WM_KEYDOWN if wp.0 as u16 == VK_ESCAPE.0 => {
                let _ = PostMessageW(Some(host), WM_APP_EDIT_DONE, WPARAM(0), LPARAM(0));
                LRESULT(0)
            }
            // 单行编辑框收到回车和 Esc 字符会响一声。
            WM_CHAR if wp.0 == 13 || wp.0 == 27 => LRESULT(0),
            WM_KILLFOCUS => {
                let _ = PostMessageW(Some(host), WM_APP_EDIT_DONE, WPARAM(1), LPARAM(0));
                DefSubclassProc(h, msg, wp, lp)
            }
            _ => DefSubclassProc(h, msg, wp, lp),
        }
    }
}

fn finish_edit(commit: bool) {
    let a = app();
    let Some(edit) = a.edit.take() else { return };
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetWindowTextW(edit.hwnd, &mut buf) };
    let text = String::from_utf16_lossy(&buf[..n.max(0) as usize]).trim().to_string();
    unsafe {
        let _ = DestroyWindow(edit.hwnd);
        let _ = DeleteObject(edit.font.into());
    }
    if !commit || text.is_empty() {
        return;
    }
    match edit.target {
        EditTarget::TabName(id) => {
            if let Some(t) = a.layout.tab_by_id_mut(&id) {
                t.name = text;
                mark_dirty();
                invalidate_all();
            }
        }
        EditTarget::File(path) => {
            let old = path.rsplit('\\').next().unwrap_or("");
            if old != text {
                // 改名成功后由目录监视把新名字同步进布局，位置不变。
                if let Err(e) = shell::rename(&a.roots.expand(&path), &text) {
                    log(format!("重命名失败：{e}"));
                }
            }
        }
    }
}

// ---- 拖放 ----

/// 分区窗口的拖放目标。
/// - 桌面上已有的文件（包括从分区里拖出来的）：只改归属，文件不动。
/// - 外部来的文件：转给桌面文件夹处理（复制或移动由 Shell 决定），落地后归入当前标签。
/// - 落在文件夹图标上：转给那个文件夹。
#[implement(IDropTarget)]
struct Dropper {
    hwnd: isize,
    data: RefCell<Option<IDataObject>>,
    paths: RefCell<Vec<PathBuf>>,
    assign_only: Cell<bool>,
    desktop_target: RefCell<Option<IDropTarget>>,
    folder: RefCell<Option<(String, IDropTarget)>>,
    helper: Option<IDropTargetHelper>,
}

impl Dropper {
    fn new(hwnd: HWND) -> Self {
        let helper = unsafe { CoCreateInstance(&CLSID_DRAG_DROP_HELPER, None, CLSCTX_INPROC_SERVER).ok() };
        Self {
            hwnd: hwnd.0 as isize,
            data: RefCell::new(None),
            paths: RefCell::new(Vec::new()),
            assign_only: Cell::new(false),
            desktop_target: RefCell::new(None),
            folder: RefCell::new(None),
            helper,
        }
    }

    fn h(&self) -> HWND {
        HWND(self.hwnd as *mut _)
    }

    fn set_hot(&self, hot: bool) {
        if let Some(i) = panel_index(self.h()) {
            let p = &mut app().panels[i];
            if p.drop_hot != hot {
                p.drop_hot = hot;
                invalidate(p.hwnd);
            }
        }
    }

    /// 鼠标当前是否在一个文件夹图标上（而且不是被拖的文件自己）。
    fn folder_under(&self, pt: &POINTL) -> Option<String> {
        let i = panel_index(self.h())?;
        let a = app();
        let mut p = POINT { x: pt.x, y: pt.y };
        unsafe {
            let _ = ScreenToClient(self.h(), &mut p);
        }
        let Hit::Item(index) = hit_test(i, p.x as f32 / a.scale, p.y as f32 / a.scale) else { return None };
        let tab = panel_tab(i)?;
        let item = tab.items.get(index)?;
        let key = path_key(&item.path);
        if !a.meta.get(&key).is_some_and(|m| m.is_dir) {
            return None;
        }
        let dragged = a.roots.expand(&item.path);
        if self.paths.borrow().iter().any(|q| q.to_string_lossy().eq_ignore_ascii_case(&dragged)) {
            return None;
        }
        Some(dragged)
    }

    fn update_folder(&self, wanted: Option<String>, key: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) {
        let same = match (&*self.folder.borrow(), &wanted) {
            (Some((a, _)), Some(b)) => a == b,
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        if let Some((_, t)) = self.folder.borrow_mut().take() {
            unsafe {
                let _ = t.DragLeave();
            }
        }
        if let Some(path) = wanted {
            if let (Ok(t), Some(data)) = (shell::drop_target_for(&path), self.data.borrow().as_ref()) {
                unsafe {
                    if t.DragEnter(data, key, *pt, effect).is_ok() {
                        *self.folder.borrow_mut() = Some((path, t));
                    }
                }
            }
        }
    }
}

#[allow(non_snake_case)]
impl IDropTarget_Impl for Dropper_Impl {
    fn DragEnter(&self, data: Ref<IDataObject>, key: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        let Some(obj) = data.as_ref() else { return Err(Error::from(E_INVALIDARG)) };
        let a = app();
        *self.data.borrow_mut() = Some(obj.clone());
        let paths = shell::dropped_paths(obj);
        // 桌面上已有的文件只需改归属。
        let assign = !paths.is_empty() && paths.iter().all(|p| layout::is_on_desktop(&a.roots.to_var(&p.to_string_lossy())));
        self.assign_only.set(assign);
        *self.paths.borrow_mut() = paths;
        unsafe {
            if assign {
                *effect = DROPEFFECT_MOVE;
            } else {
                let name = a.roots.desktop.to_string_lossy().to_string();
                if let Ok(t) = shell::drop_target_for(&name) {
                    if t.DragEnter(obj, key, *pt, effect).is_ok() {
                        *self.desktop_target.borrow_mut() = Some(t);
                    }
                }
                if self.desktop_target.borrow().is_none() {
                    *effect = DROPEFFECT_NONE;
                }
            }
            if let Some(h) = &self.helper {
                let mut p = POINT { x: pt.x, y: pt.y };
                let _ = h.DragEnter(self.h(), obj, &mut p, *effect);
            }
        }
        self.set_hot(true);
        Ok(())
    }

    fn DragOver(&self, key: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        unsafe {
            let wanted = self.folder_under(pt);
            self.update_folder(wanted, key, pt, effect);
            if let Some((_, t)) = &*self.folder.borrow() {
                t.DragOver(key, *pt, effect)?;
            } else if self.assign_only.get() {
                *effect = DROPEFFECT_MOVE;
            } else if let Some(t) = &*self.desktop_target.borrow() {
                t.DragOver(key, *pt, effect)?;
            } else {
                *effect = DROPEFFECT_NONE;
            }
            if let Some(h) = &self.helper {
                let mut p = POINT { x: pt.x, y: pt.y };
                let _ = h.DragOver(&mut p, *effect);
            }
        }
        Ok(())
    }

    fn DragLeave(&self) -> Result<()> {
        unsafe {
            if let Some((_, t)) = self.folder.borrow_mut().take() {
                let _ = t.DragLeave();
            }
            if let Some(t) = self.desktop_target.borrow_mut().take() {
                let _ = t.DragLeave();
            }
            if let Some(h) = &self.helper {
                let _ = h.DragLeave();
            }
        }
        *self.data.borrow_mut() = None;
        self.set_hot(false);
        Ok(())
    }

    fn Drop(&self, data: Ref<IDataObject>, key: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        let Some(obj) = data.as_ref() else { return Err(Error::from(E_INVALIDARG)) };
        let a = app();
        self.set_hot(false);
        unsafe {
            if let Some(h) = &self.helper {
                let mut p = POINT { x: pt.x, y: pt.y };
                let _ = h.Drop(obj, &mut p, *effect);
            }
        }
        let folder = self.folder.borrow_mut().take();
        let desktop_target = self.desktop_target.borrow_mut().take();
        let paths: Vec<PathBuf> = self.paths.borrow_mut().drain(..).collect();
        *self.data.borrow_mut() = None;
        let Some(i) = panel_index(self.h()) else { return Ok(()) };
        if let Some((_, t)) = folder {
            if let Some(t2) = desktop_target {
                unsafe {
                    let _ = t2.DragLeave();
                }
            }
            return unsafe { t.Drop(obj, key, *pt, effect) };
        }
        let Some(tab_id) = panel_tab(i).map(|t| t.id.clone()) else { return Ok(()) };
        if self.assign_only.get() {
            let vars: Vec<String> = paths.iter().map(|p| a.roots.to_var(&p.to_string_lossy())).collect();
            let mut p = POINT { x: pt.x, y: pt.y };
            unsafe {
                let _ = ScreenToClient(self.h(), &mut p);
            }
            let before = match hit_test(i, p.x as f32 / a.scale, p.y as f32 / a.scale) {
                Hit::Item(n) => Some(n),
                _ => None,
            };
            a.layout.move_items(&vars, &tab_id, before);
            mark_dirty();
            for panel in &mut a.panels {
                panel.selected.clear();
            }
            sync_panels();
            // 关键：文件没有被转移，必须告诉拖放源“什么也没发生”。
            // 如果回报 MOVE，资源管理器这类拖放源会把它当成移动完成，进而删除源文件。
            unsafe { *effect = DROPEFFECT_NONE };
            return Ok(());
        }
        // 外部文件：记下落地后归属的标签，再交给桌面文件夹。
        for p in &paths {
            if let Some(name) = p.file_name() {
                a.new_targets.insert(path_key(&format!("{}\\{}", layout::VAR_DESKTOP, name.to_string_lossy())), tab_id.clone());
            }
        }
        match desktop_target {
            Some(t) => unsafe { t.Drop(obj, key, *pt, effect) },
            None => {
                unsafe { *effect = DROPEFFECT_NONE };
                Ok(())
            }
        }
    }
}

const CLSID_DRAG_DROP_HELPER: GUID = GUID::from_u128(0x4657278a_411b_11d2_839a_00c04fd918d0);
