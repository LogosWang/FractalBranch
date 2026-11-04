//! 监视两个桌面目录。线程阻塞在系统调用里，没有变化时不占 CPU。

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use windows::Win32::Foundation::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows::core::*;

use crate::util::{log, wide};

pub enum Change {
    /// 目录内容有变化，界面线程应当重新扫描。
    Dirty,
    Renamed { old: String, new: String },
}

pub fn spawn(dir: PathBuf, tx: Sender<Change>, hwnd: isize, msg: u32) {
    let _ = std::thread::Builder::new().name("watch".into()).spawn(move || {
        if let Err(e) = run(&dir, &tx, hwnd, msg) {
            log(format!("监视 {} 失败：{e}", dir.display()));
        }
    });
}

fn run(dir: &PathBuf, tx: &Sender<Change>, hwnd: isize, msg: u32) -> Result<()> {
    let name = wide(&dir.to_string_lossy());
    let handle = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            FILE_LIST_DIRECTORY.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )?
    };
    // 按 u32 对齐：系统要求缓冲区 DWORD 对齐。
    let mut buf = vec![0u32; 16 * 1024];
    loop {
        let mut got = 0u32;
        unsafe {
            ReadDirectoryChangesW(
                handle,
                buf.as_mut_ptr() as *mut _,
                (buf.len() * 4) as u32,
                false,
                FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME | FILE_NOTIFY_CHANGE_ATTRIBUTES | FILE_NOTIFY_CHANGE_LAST_WRITE,
                Some(&mut got),
                None,
                None,
            )?;
        }
        let root = dir.to_string_lossy();
        let events = parse_events(buf.as_ptr() as *const u8, got as usize);
        let mut old_name: Option<String> = None;
        for (action, file) in events {
            match action {
                FILE_ACTION_RENAMED_OLD_NAME => old_name = Some(format!("{root}\\{file}")),
                FILE_ACTION_RENAMED_NEW_NAME => {
                    if let Some(old) = old_name.take() {
                        let _ = tx.send(Change::Renamed { old, new: format!("{root}\\{file}") });
                    }
                }
                _ => {}
            }
        }
        // 缓冲区溢出时 got 是 0，也要叫界面线程重新扫描。
        let _ = tx.send(Change::Dirty);
        unsafe {
            let _ = PostMessageW(Some(HWND(hwnd as *mut _)), msg, WPARAM(0), LPARAM(0));
        }
    }
}

fn parse_events(ptr: *const u8, len: usize) -> Vec<(FILE_ACTION, String)> {
    let mut out = Vec::new();
    if len == 0 {
        return out;
    }
    let mut off = 0usize;
    loop {
        if off + size_of::<FILE_NOTIFY_INFORMATION>() > len {
            break;
        }
        unsafe {
            let info = &*(ptr.add(off) as *const FILE_NOTIFY_INFORMATION);
            let n = info.FileNameLength as usize / 2;
            let name = std::slice::from_raw_parts(info.FileName.as_ptr(), n);
            out.push((info.Action, String::from_utf16_lossy(name)));
            if info.NextEntryOffset == 0 {
                break;
            }
            off += info.NextEntryOffset as usize;
        }
    }
    out
}
