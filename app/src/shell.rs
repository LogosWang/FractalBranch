//! 和 Windows Shell 打交道：打开、右键菜单、拖放、重命名。

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Ole::*;
use windows::Win32::UI::Shell::Common::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

use crate::layout::SHELL_PREFIX;
use crate::util::{log, wide};

const FIRST_CMD: u32 = 1;
const LAST_CMD: u32 = 0x7FFF;

/// 布局路径（已展开变量）转成 Shell 能解析的名字。
pub fn parse_name(expanded: &str) -> String {
    match expanded.strip_prefix(SHELL_PREFIX) {
        Some(clsid) => format!("::{clsid}"),
        None => expanded.to_string(),
    }
}

pub fn open(expanded: &str) {
    let target = match expanded.strip_prefix(SHELL_PREFIX) {
        Some(clsid) => format!("shell:::{clsid}"),
        None => expanded.to_string(),
    };
    let w = wide(&target);
    let dir = Path::new(expanded).parent().map(|p| wide(&p.to_string_lossy()));
    unsafe {
        let r = ShellExecuteW(
            None,
            None,
            PCWSTR(w.as_ptr()),
            None,
            dir.as_ref().map(|d| PCWSTR(d.as_ptr())).unwrap_or(PCWSTR::null()),
            SW_SHOWNORMAL,
        );
        if r.0 as usize <= 32 {
            log(format!("打开失败 {target}：{}", r.0 as usize));
        }
    }
}

struct Pidl(*mut ITEMIDLIST);

impl Drop for Pidl {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0 as *const _)) };
    }
}

fn item_array(names: &[String]) -> Result<IShellItemArray> {
    let mut pidls = Vec::with_capacity(names.len());
    for n in names {
        let w = wide(n);
        let mut p: *mut ITEMIDLIST = std::ptr::null_mut();
        unsafe { SHParseDisplayName(PCWSTR(w.as_ptr()), None, &mut p, 0, None)? };
        pidls.push(Pidl(p));
    }
    let raw: Vec<*const ITEMIDLIST> = pidls.iter().map(|p| p.0 as *const _).collect();
    unsafe { SHCreateShellItemArrayFromIDLists(&raw) }
}

pub fn data_object(names: &[String]) -> Result<IDataObject> {
    unsafe { item_array(names)?.BindToHandler(None, &BHID_DataObject) }
}

/// 启动 OLE 拖放，模态运行到松手为止。返回实际发生的效果。
pub fn drag(hwnd: HWND, names: &[String]) -> Result<DROPEFFECT> {
    let data = data_object(names)?;
    unsafe {
        let effects = DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK;
        SHDoDragDrop(Some(hwnd), &data, None, effects)
    }
}

pub fn drop_target_for(parse: &str) -> Result<IDropTarget> {
    let w = wide(parse);
    unsafe {
        let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None)?;
        item.BindToHandler(None, &BHID_SFUIObject)
    }
}

/// 从拖放数据里取出文件路径。没有文件时返回空。
pub fn dropped_paths(data: &IDataObject) -> Vec<PathBuf> {
    let fmt = FORMATETC {
        cfFormat: CF_HDROP_ID,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    let mut out = Vec::new();
    unsafe {
        let Ok(mut medium) = data.GetData(&fmt) else { return out };
        let drop = HDROP(medium.u.hGlobal.0);
        let count = DragQueryFileW(drop, u32::MAX, None);
        for i in 0..count {
            let len = DragQueryFileW(drop, i, None) as usize;
            let mut buf = vec![0u16; len + 1];
            DragQueryFileW(drop, i, Some(&mut buf));
            out.push(PathBuf::from(String::from_utf16_lossy(&buf[..len])));
        }
        ReleaseStgMedium(&mut medium);
    }
    out
}

const CF_HDROP_ID: u16 = 15;

thread_local! {
    /// 菜单显示期间，部分菜单项要靠 IContextMenu2/3 处理消息才能画出来。
    static ACTIVE_MENU: RefCell<Option<IContextMenu>> = const { RefCell::new(None) };
}

/// 把弹出菜单期间的消息转给 Shell。返回 true 表示已处理。
pub fn handle_menu_message(msg: u32, wp: WPARAM, lp: LPARAM, result: &mut LRESULT) -> bool {
    if !matches!(msg, WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR | WM_MENUSELECT) {
        return false;
    }
    ACTIVE_MENU.with(|m| {
        let m = m.borrow();
        let Some(menu) = m.as_ref() else { return false };
        unsafe {
            if let Ok(m3) = menu.cast::<IContextMenu3>() {
                let mut r = LRESULT(0);
                if m3.HandleMenuMsg2(msg, wp, lp, Some(&mut r)).is_ok() {
                    *result = r;
                    return true;
                }
            } else if let Ok(m2) = menu.cast::<IContextMenu2>() {
                return m2.HandleMenuMsg(msg, wp, lp).is_ok();
            }
        }
        false
    })
}

#[derive(PartialEq, Debug)]
pub enum MenuResult {
    None,
    Invoked,
    Rename,
}

/// 显示文件的系统右键菜单。第三方扩展也在这里加载，用完立即释放。
pub fn context_menu(owner: HWND, names: &[String], x: i32, y: i32) -> Result<MenuResult> {
    let array = item_array(names)?;
    let menu: IContextMenu = unsafe { array.BindToHandler(None, &BHID_SFUIObject)? };
    let result = unsafe { run_menu(owner, &menu, x, y) };
    ACTIVE_MENU.with(|m| *m.borrow_mut() = None);
    drop(menu);
    drop(array);
    unsafe { CoFreeUnusedLibraries() };
    result
}

unsafe fn run_menu(owner: HWND, menu: &IContextMenu, x: i32, y: i32) -> Result<MenuResult> {
    unsafe {
        let popup = CreatePopupMenu()?;
        let r = (|| {
            menu.QueryContextMenu(popup, 0, FIRST_CMD, LAST_CMD, CMF_NORMAL | CMF_CANRENAME).ok()?;
            ACTIVE_MENU.with(|m| *m.borrow_mut() = Some(menu.clone()));
            // 不把宿主窗口设为前台，菜单点别处时不会自动关闭。
            let _ = SetForegroundWindow(owner);
            let cmd = TrackPopupMenuEx(popup, (TPM_RETURNCMD | TPM_RIGHTBUTTON).0, x, y, owner, None).0 as u32;
            let _ = PostMessageW(Some(owner), WM_NULL, WPARAM(0), LPARAM(0));
            if cmd == 0 {
                return Ok(MenuResult::None);
            }
            let id = cmd - FIRST_CMD;
            if verb(menu, id).eq_ignore_ascii_case("rename") {
                return Ok(MenuResult::Rename);
            }
            invoke(menu, owner, PCSTR(id as usize as *const u8))?;
            Ok(MenuResult::Invoked)
        })();
        let _ = DestroyMenu(popup);
        r
    }
}

unsafe fn invoke(menu: &IContextMenu, owner: HWND, verb: PCSTR) -> Result<()> {
    let info = CMINVOKECOMMANDINFO {
        cbSize: size_of::<CMINVOKECOMMANDINFO>() as u32,
        hwnd: owner,
        lpVerb: verb,
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe { menu.InvokeCommand(&info) }
}

fn verb(menu: &IContextMenu, id: u32) -> String {
    let mut buf = [0u16; 64];
    unsafe {
        let r = menu.GetCommandString(id as usize, GCS_VERBW, None, PSTR(buf.as_mut_ptr() as *mut u8), buf.len() as u32);
        if r.is_err() {
            return String::new();
        }
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
    String::from_utf16_lossy(&buf[..n])
}

pub const BG_FIRST: u32 = 0x1000;
const BG_LAST: u32 = 0x7FFF;

/// 把系统的桌面背景菜单（新建、粘贴、显示设置等）并入 `popup`。
/// 调用方显示菜单后要调用 `end_background_menu`。
pub fn background_menu(owner: HWND, popup: HMENU, position: u32) -> Result<IContextMenu> {
    unsafe {
        let desktop = SHGetDesktopFolder()?;
        let menu: IContextMenu = desktop.CreateViewObject(owner)?;
        menu.QueryContextMenu(popup, position, BG_FIRST, BG_LAST, CMF_NORMAL).ok()?;
        ACTIVE_MENU.with(|m| *m.borrow_mut() = Some(menu.clone()));
        Ok(menu)
    }
}

pub fn invoke_background(menu: &IContextMenu, owner: HWND, cmd: u32) -> Result<()> {
    unsafe { invoke(menu, owner, PCSTR((cmd - BG_FIRST) as usize as *const u8)) }
}

pub fn end_background_menu() {
    ACTIVE_MENU.with(|m| *m.borrow_mut() = None);
    unsafe { CoFreeUnusedLibraries() };
}

/// 不弹菜单，直接执行一个 Shell 动词（delete、copy、cut、properties）。
pub fn invoke_verb(owner: HWND, names: &[String], name: &str) -> Result<()> {
    let array = item_array(names)?;
    let menu: IContextMenu = unsafe { array.BindToHandler(None, &BHID_SFUIObject)? };
    let c: Vec<u8> = name.bytes().chain(std::iter::once(0)).collect();
    unsafe {
        let popup = CreatePopupMenu()?;
        let r = menu.QueryContextMenu(popup, 0, FIRST_CMD, LAST_CMD, CMF_NORMAL).ok().and_then(|_| invoke(&menu, owner, PCSTR(c.as_ptr())));
        let _ = DestroyMenu(popup);
        r
    }
}

/// 重命名文件。Shell 负责处理重名提示和撤销记录。
pub fn rename(expanded: &str, new_name: &str) -> Result<()> {
    let w = wide(expanded);
    let n = wide(new_name);
    unsafe {
        let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None)?;
        let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
        op.RenameItem(&item, PCWSTR(n.as_ptr()), None)?;
        op.PerformOperations()
    }
}
