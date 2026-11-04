//! 桌面窗口树：定位、隐藏和恢复原生图标层，以及崩溃守护。

use std::time::Instant;

use windows::Win32::Foundation::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

use crate::util::log;

/// 定位桌面。24H2 起 DefView 直接在 Progman 下，更早的系统在某个 WorkerW 下。
pub fn find_desktop() -> Option<(HWND, HWND)> {
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

/// 分区窗口挂在 Progman 下，只在 DefView 也直属 Progman 的桌面结构（24H2 起）上验证过。
/// 旧结构里 DefView 在 WorkerW 下，分区可能被盖住：图标藏了却什么也看不到。不能冒这个险。
/// 桌面还没就绪时返回 None。
pub fn supported() -> Option<bool> {
    let (progman, view) = find_desktop()?;
    Some(unsafe { GetAncestor(view, GA_PARENT) } == progman)
}

pub fn find_listview() -> Option<HWND> {
    let (_, view) = find_desktop()?;
    unsafe { FindWindowExW(Some(view), None, w!("SysListView32"), None).ok() }
}

pub fn icons_visible() -> Option<bool> {
    find_listview().map(|lv| unsafe { IsWindowVisible(lv).as_bool() })
}

/// 用异步调用：资源管理器卡住时不会把我们一起卡住。
pub fn set_icons_visible(visible: bool) -> bool {
    match find_listview() {
        Some(lv) => unsafe { ShowWindowAsync(lv, if visible { SW_SHOW } else { SW_HIDE }).as_bool() || true },
        None => false,
    }
}

/// 小智的全屏窗口还在时，我们的分区会和它互相遮挡。
pub fn xz_running() -> bool {
    let Some((_, view)) = find_desktop() else { return false };
    unsafe { FindWindowExW(Some(view), None, w!("XZDesktopWnd"), None).is_ok() }
}

pub fn spawn_guard(crashes: u32) {
    let Ok(exe) = std::env::current_exe() else { return };
    let r = std::process::Command::new(exe)
        .args(["--guard", &std::process::id().to_string(), "--crashes", &crashes.to_string()])
        .spawn();
    if let Err(e) = r {
        log(format!("启动守护失败：{e}"));
    }
}

/// 连续崩溃不超过这个次数才自动重启，否则恢复原生桌面后停手。
const MAX_CRASHES: u32 = 3;
/// 进程活过这么久，就不算连续崩溃。
const STABLE_SECS: u64 = 60;
/// 任务管理器“结束任务”的退出码，用户主动结束，不重启。
const EXIT_END_TASK: u32 = 1;

/// 守护模式：等主进程退出。无论怎么退出都恢复原生图标；异常退出时重启主进程。
pub fn run_guard(pid: u32, crashes: u32) {
    let Ok(handle) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        log(format!("守护：无法打开进程 {pid}"));
        set_icons_visible(true);
        return;
    };
    let started = Instant::now();
    let mut code = 0u32;
    unsafe {
        WaitForSingleObject(handle, INFINITE);
        let _ = GetExitCodeProcess(handle, &mut code);
        let _ = CloseHandle(handle);
    }
    set_icons_visible(true);
    if code == 0 || code == EXIT_END_TASK {
        return;
    }
    let lived = started.elapsed().as_secs();
    let count = if lived >= STABLE_SECS { 1 } else { crashes + 1 };
    log(format!("主进程异常退出，退出码 {code:#x}，存活 {lived} 秒，连续第 {count} 次"));
    if count > MAX_CRASHES {
        log("连续崩溃次数过多，不再重启，已恢复原生桌面");
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).args(["--crashes", &count.to_string()]).spawn();
    }
}
