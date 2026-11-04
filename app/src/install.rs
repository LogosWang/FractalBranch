//! 一键安装和卸载。安装包就是程序本身：文件名带 Setup 时双击走安装流程。
//! 只写当前用户自己的目录和注册表，不需要管理员权限。

use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Registry::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

use crate::app::{confirm, message, set_autostart};
use crate::desktop;
use crate::util::{self, log, wide};

const UNINSTALL_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\FractalBranch");
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn install_dir() -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).ok_or_else(|| Error::from(E_FAIL))?;
    Ok(base.join("Programs").join("FractalBranch"))
}

fn shortcut_path() -> Option<PathBuf> {
    Some(util::known_folder(&FOLDERID_Programs)?.join("分形树枝.lnk"))
}

/// 文件名带 Setup 或“安装”的就是安装包。浏览器加上的“(1)”之类不影响判断。
pub fn is_setup_exe() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let stem = exe.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
    stem.contains("setup") || stem.contains("安装")
}

/// 让正在运行的实例正常退出（它会保存布局并还原原生图标）。卡死时才强杀。
fn stop_running() {
    unsafe {
        let Ok(host) = FindWindowW(w!("FractalBranchHost"), None) else { return };
        let _ = PostMessageW(Some(host), WM_CLOSE, WPARAM(0), LPARAM(0));
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if FindWindowW(w!("FractalBranchHost"), None).is_err() {
                // 窗口没了之后进程还要收尾，守护进程也要退出，留一点时间。
                std::thread::sleep(Duration::from_millis(400));
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    log("旧实例没有响应，强制结束");
    let filter = format!("PID ne {}", std::process::id());
    let _ = std::process::Command::new("taskkill")
        .args(["/f", "/im", "FractalBranch.exe", "/fi", &filter])
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    std::thread::sleep(Duration::from_millis(400));
    desktop::set_icons_visible(true);
}

fn set_string(key: HKEY, name: &str, value: &str) {
    let (n, v) = (wide(name), wide(value));
    unsafe {
        let bytes = std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 2);
        let _ = RegSetValueExW(key, PCWSTR(n.as_ptr()), None, REG_SZ, Some(bytes));
    }
}

fn set_dword(key: HKEY, name: &str, value: u32) {
    let n = wide(name);
    unsafe {
        let _ = RegSetValueExW(key, PCWSTR(n.as_ptr()), None, REG_DWORD, Some(&value.to_le_bytes()));
    }
}

/// 在“设置 → 应用”里登记，用户能从系统界面卸载。
fn register_uninstall(exe: &std::path::Path, dir: &std::path::Path, size: u64) -> Result<()> {
    unsafe {
        let mut key = HKEY::default();
        RegCreateKeyExW(HKEY_CURRENT_USER, UNINSTALL_KEY, None, None, REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, None, &mut key, None).ok()?;
        let exe_s = exe.display().to_string();
        set_string(key, "DisplayName", "分形树枝");
        set_string(key, "DisplayVersion", env!("CARGO_PKG_VERSION"));
        set_string(key, "Publisher", "FractalBranch");
        set_string(key, "InstallLocation", &dir.display().to_string());
        set_string(key, "DisplayIcon", &exe_s);
        set_string(key, "UninstallString", &format!("\"{exe_s}\" --uninstall"));
        set_string(key, "QuietUninstallString", &format!("\"{exe_s}\" --uninstall --quiet"));
        set_dword(key, "NoModify", 1);
        set_dword(key, "NoRepair", 1);
        set_dword(key, "EstimatedSize", (size / 1024) as u32);
        let _ = RegCloseKey(key);
    }
    Ok(())
}

fn create_shortcut(exe: &std::path::Path) -> Result<()> {
    let Some(path) = shortcut_path() else { return Err(Error::from(E_FAIL)) };
    let (target, link, desc) = (wide(&exe.display().to_string()), wide(&path.display().to_string()), wide("桌面分类整理和分区"));
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let sl: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        sl.SetPath(PCWSTR(target.as_ptr()))?;
        sl.SetDescription(PCWSTR(desc.as_ptr()))?;
        sl.SetIconLocation(PCWSTR(target.as_ptr()), 0)?;
        let file: IPersistFile = sl.cast()?;
        file.Save(PCWSTR(link.as_ptr()), true)
    }
}

/// 把自身复制到安装目录。运行中的旧文件不能覆盖，但可以改名让路。
fn copy_self(dir: &std::path::Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let target = dir.join("FractalBranch.exe");
    let old = dir.join("FractalBranch.old.exe");
    let _ = std::fs::remove_file(&old);
    if target.exists() {
        std::fs::rename(&target, &old)?;
    }
    if let Err(e) = std::fs::copy(std::env::current_exe()?, &target) {
        // 复制失败时把旧版本放回去，不能把用户留在没有程序的状态。
        let _ = std::fs::rename(&old, &target);
        return Err(e);
    }
    let _ = std::fs::remove_file(&old);
    Ok(target)
}

/// 返回进程退出码。
pub fn install(quiet: bool) -> i32 {
    if desktop::supported() == Some(false) {
        if !quiet {
            message("这台电脑的桌面结构还不受支持，没有安装任何东西。\n\n分形树枝目前只在 Windows 11 24H2 及更新版本上验证过。");
        }
        log("安装中止：桌面结构不受支持");
        return 3;
    }
    let Ok(dir) = install_dir() else { return 2 };
    if !quiet {
        let text = format!(
            "安装分形树枝？\n\n安装位置：{}\n开机自动启动，不需要管理员权限。\n桌面上的文件不会被移动或修改。\n\n如果这台电脑有小智桌面的数据，会自动继承它的分类结构。",
            dir.display()
        );
        if !confirm(&text) {
            return 0;
        }
    }
    stop_running();
    let exe = match copy_self(&dir) {
        Ok(e) => e,
        Err(e) => {
            log(format!("安装失败：{e}"));
            if !quiet {
                message(&format!("安装失败：{e}"));
            }
            return 2;
        }
    };
    set_autostart(true, Some(&exe));
    let size = std::fs::metadata(&exe).map(|m| m.len()).unwrap_or(0);
    if let Err(e) = register_uninstall(&exe, &dir, size) {
        log(format!("登记卸载信息失败：{e}"));
    }
    if let Err(e) = create_shortcut(&exe) {
        log(format!("创建开始菜单快捷方式失败：{e}"));
    }
    log(format!("已安装 {} 到 {}", env!("CARGO_PKG_VERSION"), dir.display()));

    if desktop::xz_running() {
        if !quiet {
            message("安装完成。\n\n小智桌面正在运行，两者同时运行会互相遮挡。\n请先退出小智桌面，再从开始菜单打开“分形树枝”。\n\n也建议关掉小智的开机自启。");
        }
        return 0;
    }
    if let Err(e) = std::process::Command::new(&exe).spawn() {
        log(format!("启动失败：{e}"));
    }
    if !quiet {
        message("安装完成，已经启动。\n\n桌面空白处右键可以新建分区，任务栏托盘的小树图标里有导出、设置和退出。");
    }
    0
}

pub fn uninstall(quiet: bool) -> i32 {
    if !quiet && !confirm("卸载分形树枝？\n\n桌面上的文件不受影响，原生桌面图标会恢复显示。") {
        return 0;
    }
    let delete_data = !quiet && confirm("同时删除分类结构和图标缓存吗？\n\n选“否”会保留，以后重新安装还能接着用。");
    stop_running();
    desktop::set_icons_visible(true);
    set_autostart(false, None);
    unsafe {
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, UNINSTALL_KEY);
    }
    if let Some(link) = shortcut_path() {
        let _ = std::fs::remove_file(link);
    }
    if delete_data {
        let _ = std::fs::remove_dir_all(util::data_dir());
    }
    if !quiet {
        message("已卸载。");
    }
    // 放在最后：对话框开着时 exe 还在运行，删不掉。
    remove_install_dir_after_exit();
    0
}

/// 正在运行的 exe 删不掉自己，交给一个后台 cmd：等本进程退出后删除目录。
/// 杀毒软件或图标缓存可能短暂占用文件，所以失败了隔一秒再试，最多试 20 次。
fn remove_install_dir_after_exit() {
    let Ok(dir) = install_dir() else { return };
    let d = dir.display();
    let cmd = format!("/c for /l %i in (1,1,20) do @(ping -n 2 127.0.0.1 >nul & rmdir /s /q \"{d}\" 2>nul & if not exist \"{d}\" exit)");
    let r = std::process::Command::new("cmd").raw_arg(cmd).current_dir(std::env::temp_dir()).creation_flags(CREATE_NO_WINDOW).spawn();
    if let Err(e) = r {
        log(format!("清理安装目录失败：{e}"));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_set() {
        assert!(!env!("CARGO_PKG_VERSION").is_empty());
    }
}
