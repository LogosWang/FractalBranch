//! 零散工具：宽字符串、目录、日志。

use std::io::Write;
use std::path::PathBuf;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{FOLDERID_Desktop, FOLDERID_PublicDesktop, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows::core::GUID;

use crate::layout::Roots;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn data_dir() -> PathBuf {
    // 测试时可以指向临时目录，不碰真实布局。
    if let Some(dir) = std::env::var_os("FRACTALBRANCH_DATA") {
        let dir = PathBuf::from(dir);
        let _ = std::fs::create_dir_all(&dir);
        return dir;
    }
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let dir = base.join("FractalBranch");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn xz_data_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("XZDesktop");
    dir.join("vgroup.dat").exists().then_some(dir)
}

pub fn known_folder(id: &GUID) -> Option<PathBuf> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, Some(HANDLE::default())).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s.map(PathBuf::from)
    }
}

pub fn roots() -> Roots {
    Roots {
        desktop: known_folder(&FOLDERID_Desktop).unwrap_or_else(|| PathBuf::from(std::env::var_os("USERPROFILE").unwrap_or_default()).join("Desktop")),
        public: known_folder(&FOLDERID_PublicDesktop).unwrap_or_else(|| PathBuf::from(r"C:\Users\Public\Desktop")),
    }
}

/// 追加写日志；超过 1 MB 时丢弃旧内容。日志写失败不影响程序运行。
pub fn log(msg: impl AsRef<str>) {
    let path = data_dir().join("log.txt");
    if std::fs::metadata(&path).map(|m| m.len() > 1 << 20).unwrap_or(false) {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let _ = writeln!(f, "[{secs}] [{}] {}", std::process::id(), msg.as_ref());
    }
}
