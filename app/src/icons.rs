//! 图标加载。Shell 取图可能很慢（PDF 缩略图、第三方扩展），全部放在后台线程；
//! 结果追加写入磁盘缓存，下次启动不必再问 Shell。

use std::collections::HashMap;
use std::io::{BufWriter, Read, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::Duration;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows::core::*;

use crate::util::{log, wide};

pub struct Job {
    pub key: String,
    /// 给 Shell 解析的名字：真实路径，或 `::{CLSID}`。
    pub parse: String,
    pub px: u32,
    /// 文件修改时间，用来判断缓存是否过期。系统图标为 0。
    pub stamp: u64,
}

pub struct Loaded {
    pub key: String,
    pub w: u32,
    pub h: u32,
    pub pixels: Arc<Vec<u8>>,
}

pub struct Loader {
    tx: Sender<Job>,
    pub rx: Receiver<Loaded>,
    pub pending_wake: Arc<AtomicBool>,
}

impl Loader {
    pub fn request(&self, job: Job) {
        let _ = self.tx.send(job);
    }
}

/// `notify` 是收到结果时要投递消息的窗口和消息号；None 表示调用方自己轮询。
pub fn spawn(cache: PathBuf, notify: Option<(isize, u32)>) -> Loader {
    let (tx, jobs) = channel::<Job>();
    let (out, rx) = channel::<Loaded>();
    let pending_wake = Arc::new(AtomicBool::new(false));
    let wake = pending_wake.clone();
    let _ = std::thread::Builder::new().name("icons".into()).spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
        worker(cache, jobs, out, notify, wake);
    });
    Loader { tx, rx, pending_wake }
}

type Cached = (u64, u32, u32, Arc<Vec<u8>>);

/// 单次取图最多等这么久。第三方缩略图扩展、指向断开网络盘的快捷方式都可能卡很久。
const FETCH_TIMEOUT: Duration = Duration::from_secs(4);
/// 同时被卡住的线程上限。超过后不再尝试，等它们自己返回。
const MAX_STUCK: usize = 8;

type Fetched = std::result::Result<(u32, u32, Vec<u8>), String>;

struct Request {
    key: String,
    parse: String,
    px: u32,
    stamp: u64,
}

/// 真正调用 Shell 的线程。主工作线程等得不耐烦时会丢开它，另起一个；
/// 被丢开的线程取到结果后自己写缓存、自己交付，然后退出。
struct Fetcher {
    tx: Sender<Request>,
    rx: Receiver<Fetched>,
}

#[derive(Clone)]
struct Delivery {
    path: PathBuf,
    out: Sender<Loaded>,
    notify: Option<(isize, u32)>,
    wake: Arc<AtomicBool>,
    stuck: Arc<AtomicUsize>,
}

impl Delivery {
    fn deliver(&self, key: String, entry: &Cached) -> bool {
        let sent = self.out.send(Loaded { key, w: entry.1, h: entry.2, pixels: entry.3.clone() }).is_ok();
        if let Some((hwnd, msg)) = self.notify {
            // 结果成批到达时只叫醒一次，界面线程一次取完。
            if !self.wake.swap(true, Ordering::AcqRel) {
                unsafe {
                    let _ = PostMessageW(Some(HWND(hwnd as *mut _)), msg, WPARAM(0), LPARAM(0));
                }
            }
        }
        sent
    }
}

fn spawn_fetcher(d: Delivery) -> Option<Fetcher> {
    let (tx, requests) = channel::<Request>();
    let (reply, rx) = channel::<Fetched>();
    std::thread::Builder::new()
        .name("icon-fetch".into())
        .spawn(move || unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
            for r in requests {
                let result = fetch(&r.parse, r.px).map_err(|e| e.to_string());
                if let Err(unwanted) = reply.send(result) {
                    // 主工作线程已经放弃等待，结果由这里收尾。
                    if let Ok((w, h, data)) = unwanted.0 {
                        let c: Cached = (r.stamp, w, h, Arc::new(data));
                        let _ = append(&d.path, &r.key, &c);
                        d.deliver(r.key, &c);
                    }
                    d.stuck.fetch_sub(1, Ordering::AcqRel);
                    return;
                }
            }
        })
        .ok()?;
    Some(Fetcher { tx, rx })
}

fn worker(path: PathBuf, jobs: Receiver<Job>, out: Sender<Loaded>, notify: Option<(isize, u32)>, wake: Arc<AtomicBool>) {
    let d = Delivery { path: path.clone(), out, notify, wake, stuck: Arc::new(AtomicUsize::new(0)) };
    let mut cache: Option<HashMap<String, Cached>> = None;
    let mut used: Vec<(String, Cached)> = Vec::new();
    let mut file_entries = 0usize;
    let mut fetcher: Option<Fetcher> = None;
    loop {
        // 忙完之后把缓存从内存里放掉：图标已经交给界面线程，这里没必要再留一份。
        let job = match if cache.is_some() { jobs.recv_timeout(Duration::from_secs(3)) } else { jobs.recv().map_err(|_| RecvTimeoutError::Disconnected) } {
            Ok(j) => j,
            Err(RecvTimeoutError::Timeout) => {
                if file_entries > used.len() + used.len() / 4 + 8 {
                    if let Err(e) = rewrite(&path, &used) {
                        log(format!("重写图标缓存失败：{e}"));
                    }
                }
                used = Vec::new();
                cache = None;
                fetcher = None;
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let map = cache.get_or_insert_with(|| {
            let (m, n) = read_cache(&path);
            file_entries = n;
            m
        });
        let hit = map.get(&job.key).filter(|c| c.0 == job.stamp).cloned();
        let entry = match hit {
            Some(c) => c,
            None => {
                if d.stuck.load(Ordering::Acquire) >= MAX_STUCK {
                    log(format!("卡住的取图线程太多，跳过 {}", job.parse));
                    continue;
                }
                if fetcher.is_none() {
                    fetcher = spawn_fetcher(d.clone());
                }
                let Some(f) = fetcher.as_ref() else { continue };
                if f.tx.send(Request { key: job.key.clone(), parse: job.parse.clone(), px: job.px, stamp: job.stamp }).is_err() {
                    fetcher = None;
                    continue;
                }
                match f.rx.recv_timeout(FETCH_TIMEOUT) {
                    Ok(Ok((w, h, data))) => {
                        let c: Cached = (job.stamp, w, h, Arc::new(data));
                        if let Err(e) = append(&path, &job.key, &c) {
                            log(format!("写图标缓存失败：{e}"));
                        }
                        file_entries += 1;
                        c
                    }
                    Ok(Err(e)) => {
                        log(format!("取图标失败 {}：{e}", job.parse));
                        continue;
                    }
                    Err(_) => {
                        log(format!("取图标超时，转入后台继续等：{}", job.parse));
                        d.stuck.fetch_add(1, Ordering::AcqRel);
                        fetcher = None;
                        continue;
                    }
                }
            }
        };
        used.push((job.key.clone(), entry.clone()));
        if !d.deliver(job.key, &entry) {
            return;
        }
    }
}

const MAX_ICON_PX: u32 = 1024;

fn read_cache(path: &Path) -> (HashMap<String, Cached>, usize) {
    let mut map = HashMap::new();
    let mut count = 0;
    let Ok(mut bytes) = std::fs::File::open(path).and_then(|mut f| {
        let mut v = Vec::new();
        f.read_to_end(&mut v)?;
        Ok(v)
    }) else {
        return (map, 0);
    };
    let mut at = 0usize;
    let take = |at: &mut usize, n: usize, b: &[u8]| -> Option<std::ops::Range<usize>> {
        let r = *at..at.checked_add(n)?;
        if r.end > b.len() {
            return None;
        }
        *at = r.end;
        Some(r)
    };
    // 文件尾部可能因断电而不完整，读到不完整的记录就停。
    loop {
        let Some(r) = take(&mut at, 4, &bytes) else { break };
        let klen = u32::from_le_bytes(bytes[r].try_into().unwrap()) as usize;
        let Some(r) = take(&mut at, klen, &bytes) else { break };
        let Ok(key) = String::from_utf8(bytes[r].to_vec()) else { break };
        let Some(r) = take(&mut at, 16, &bytes) else { break };
        let head = &bytes[r];
        let stamp = u64::from_le_bytes(head[0..8].try_into().unwrap());
        let w = u32::from_le_bytes(head[8..12].try_into().unwrap());
        let h = u32::from_le_bytes(head[12..16].try_into().unwrap());
        if w == 0 || h == 0 || w > MAX_ICON_PX || h > MAX_ICON_PX {
            break;
        }
        let Some(r) = take(&mut at, (w * h * 4) as usize, &bytes) else { break };
        map.insert(key, (stamp, w, h, Arc::new(bytes[r].to_vec())));
        count += 1;
    }
    bytes.clear();
    (map, count)
}

fn write_entry(f: &mut impl Write, key: &str, c: &Cached) -> std::io::Result<()> {
    f.write_all(&(key.len() as u32).to_le_bytes())?;
    f.write_all(key.as_bytes())?;
    f.write_all(&c.0.to_le_bytes())?;
    f.write_all(&c.1.to_le_bytes())?;
    f.write_all(&c.2.to_le_bytes())?;
    f.write_all(&c.3)
}

fn append(path: &Path, key: &str, c: &Cached) -> std::io::Result<()> {
    // 先拼好再一次写入：卡住的线程可能同时在追加，不能交错。
    let mut buf = Vec::with_capacity(key.len() + c.3.len() + 20);
    write_entry(&mut buf, key, c)?;
    std::fs::OpenOptions::new().create(true).append(true).open(path)?.write_all(&buf)
}

fn rewrite(path: &Path, used: &[(String, Cached)]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = BufWriter::new(std::fs::File::create(&tmp)?);
    let mut seen = std::collections::HashSet::new();
    for (k, c) in used.iter().rev() {
        if seen.insert(k) {
            write_entry(&mut f, k, c)?;
        }
    }
    f.flush()?;
    drop(f);
    std::fs::rename(&tmp, path)
}

/// 从 Shell 取图。缩略图和图标都走这一个接口，结果是预乘 alpha 的 BGRA。
fn fetch(parse: &str, px: u32) -> Result<(u32, u32, Vec<u8>)> {
    unsafe {
        let name = wide(parse);
        let factory: IShellItemImageFactory = SHCreateItemFromParsingName(PCWSTR(name.as_ptr()), None)?;
        let hbm = factory.GetImage(SIZE { cx: px as i32, cy: px as i32 }, SIIGBF_RESIZETOFIT)?;
        let out = read_bitmap(hbm);
        let _ = DeleteObject(hbm.into());
        out
    }
}

unsafe fn read_bitmap(hbm: HBITMAP) -> Result<(u32, u32, Vec<u8>)> {
    unsafe {
        let mut bm = BITMAP::default();
        if GetObjectW(hbm.into(), size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _)) == 0 {
            return Err(Error::from(E_FAIL));
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
        if w <= 0 || h <= 0 || w as u32 > MAX_ICON_PX || h as u32 > MAX_ICON_PX {
            return Err(Error::from(E_FAIL));
        }
        let mut info = BITMAPINFO {
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
        let mut data = vec![0u8; (w * h * 4) as usize];
        let dc = CreateCompatibleDC(None);
        let got = GetDIBits(dc, hbm, 0, h as u32, Some(data.as_mut_ptr() as *mut _), &mut info, DIB_RGB_COLORS);
        let _ = DeleteDC(dc);
        if got == 0 {
            return Err(Error::from(E_FAIL));
        }
        // 没有 alpha 通道的缩略图 alpha 全是 0，要当成不透明。
        if data.chunks_exact(4).all(|p| p[3] == 0) {
            data.chunks_exact_mut(4).for_each(|p| p[3] = 255);
        }
        Ok((w as u32, h as u32, data))
    }
}
