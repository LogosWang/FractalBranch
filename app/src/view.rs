//! 分区和底层桌面的几何计算、绘制和点击判定。单位都是 DIP。
//! 只依赖 ID2D1RenderTarget，所以同一份代码既画到屏幕，也能画到离屏图片用于检查。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::*;
use windows_numerics::Vector2;

use crate::icons::{Job, Loaded, Loader};
use crate::layout::{Item, Partition, Roots, SHELL_PREFIX, SortKey, Style, Tab, View};

pub const HEADER_H: f32 = 36.0;
pub const TAB_H: f32 = 30.0;
pub const PAD: f32 = 6.0;
pub const EDGE: f32 = 6.0;
pub const MIN_W: f32 = 120.0;
pub const MIN_H: f32 = HEADER_H + 40.0;
const LIST_ICON: f32 = 16.0;

#[derive(Clone, Copy, Debug, Default)]
pub struct Meta {
    pub modified: u64,
    pub size: u64,
    pub is_dir: bool,
}

pub type MetaMap = HashMap<String, Meta>;

pub fn path_key(path: &str) -> String {
    path.to_lowercase()
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Hit {
    None,
    Title,
    Chevron,
    Lock,
    Add,
    More,
    Tab(usize),
    /// 下标指向 tab.items。
    Item(usize),
    Blank,
    /// 位掩码：1 左，2 上，4 右，8 下。
    Edge(u8),
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Rect {
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
}

impl Rect {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.l && x < self.r && y >= self.t && y < self.b
    }

    pub fn intersects(&self, o: &Rect) -> bool {
        self.l < o.r && o.l < self.r && self.t < o.b && o.t < self.b
    }

    fn d2d(&self) -> D2D_RECT_F {
        D2D_RECT_F { left: self.l, top: self.t, right: self.r, bottom: self.b }
    }
}

// ---- 几何 ----

/// 显示顺序：返回 tab.items 的下标。
pub fn display_order(tab: &Tab, meta: &MetaMap) -> Vec<usize> {
    let mut order: Vec<usize> = (0..tab.items.len()).collect();
    let m = |i: usize| meta.get(&path_key(&tab.items[i].path)).copied().unwrap_or_default();
    match tab.sort {
        SortKey::Manual => return order,
        SortKey::Name => order.sort_by_cached_key(|&i| tab.items[i].display_name().to_lowercase()),
        SortKey::Modified => order.sort_by_key(|&i| m(i).modified),
        SortKey::Size => order.sort_by_key(|&i| m(i).size),
        SortKey::Kind => order.sort_by_cached_key(|&i| {
            let p = tab.items[i].path.to_lowercase();
            (!m(i).is_dir, p.rsplit('.').next().unwrap_or("").to_string(), p)
        }),
    }
    if tab.desc {
        order.reverse();
    }
    order
}

pub fn content_top(p: &Partition) -> f32 {
    HEADER_H + if p.tabs.len() > 1 { TAB_H } else { 0.0 }
}

/// 图标模式下一个格子实际占用的高度。格子其余部分是行距，点击落空。
fn icon_box_h(tab: &Tab, style: &Style) -> f32 {
    (6.0 + tab.icon + 4.0 + label_h(style) + 2.0).min(tab.cell_h)
}

fn label_h(style: &Style) -> f32 {
    (style.font_size * 1.36 * 2.0).ceil()
}

/// 内容区里每个显示位置的矩形，坐标相对内容区左上角（未减滚动量）。
/// 被子分区占住的位置会跳过，所以子分区可以摆在任何地方而不压住图标。
pub fn flow_cells(tab: &Tab, style: &Style, n: usize, w: f32) -> Vec<Rect> {
    let blocked: Vec<Rect> = tab.children.iter().map(partition_rect).collect();
    let free = |r: &Rect| !blocked.iter().any(|b| b.intersects(r));
    let ch = tab.cell_h.max(1.0);
    let mut out = Vec::with_capacity(n);
    let mut slot = 0usize;
    match tab.view {
        View::Icons => {
            let cw = tab.cell_w.max(1.0);
            let cols = (((w - PAD * 2.0) / cw) as usize).max(1);
            let box_h = icon_box_h(tab, style);
            while out.len() < n {
                let x = PAD + (slot % cols) as f32 * cw;
                let y = PAD + (slot / cols) as f32 * ch;
                let r = Rect { l: x, t: y, r: x + cw, b: y + box_h };
                if free(&r) {
                    out.push(r);
                }
                slot += 1;
            }
        }
        View::List => {
            while out.len() < n {
                let y = 2.0 + slot as f32 * ch;
                let r = Rect { l: PAD, t: y, r: w - PAD, b: y + ch };
                if free(&r) {
                    out.push(r);
                }
                slot += 1;
            }
        }
    }
    out
}

/// 内容总高度：图标和子分区都要能滚动到。
pub fn content_height(tab: &Tab, cells: &[Rect]) -> f32 {
    let mut h = 0.0f32;
    for c in cells {
        h = h.max(c.b);
    }
    for child in &tab.children {
        h = h.max(partition_rect(child).b);
    }
    h + PAD
}

pub fn max_scroll(p: &Partition, style: &Style, w: f32, h: f32) -> f32 {
    let tab = &p.tabs[p.active_tab()];
    let cells = flow_cells(tab, style, tab.items.len(), w);
    (content_height(tab, &cells) - (h - content_top(p))).max(0.0)
}

/// 分区在桌面上实际占用的矩形（收起时只占标题栏）。
pub fn partition_rect(p: &Partition) -> Rect {
    Rect { l: p.x, t: p.y, r: p.x + p.w, b: p.y + if p.collapsed { HEADER_H } else { p.h } }
}

/// 分区实际显示的位置。屏幕暂时变小（唤醒、远程桌面、显卡驱动重载）时，
/// 跑出屏幕的分区只在显示上挪回来，保存的位置不动，屏幕恢复后自然回到原处。
pub fn shown_rect(p: &Partition, work: (f32, f32)) -> Rect {
    let x = p.x.min((work.0 - 60.0).max(0.0)).max(0.0);
    let y = p.y.min((work.1 - HEADER_H).max(0.0)).max(0.0);
    Rect { l: x, t: y, r: x + p.w, b: y + if p.collapsed { HEADER_H } else { p.h } }
}

/// 底层桌面的排布：先上后下、先左后右，让开分区占用的位置。
pub fn desktop_cells(n: usize, tab: &Tab, style: &Style, work: (f32, f32), partitions: &[Partition]) -> Vec<Rect> {
    let rects: Vec<Rect> = partitions.iter().map(|p| shown_rect(p, work)).collect();
    let (cw, ch) = (tab.cell_w.max(1.0), tab.cell_h.max(1.0));
    let box_h = icon_box_h(tab, style);
    let mut out = Vec::with_capacity(n);
    let (mut x, mut y) = (0.0f32, 4.0f32);
    let mut guard = 0usize;
    while out.len() < n {
        let cell = Rect { l: x, t: y, r: x + cw, b: y + box_h };
        if !rects.iter().any(|r| r.intersects(&cell)) || guard > 100_000 {
            out.push(cell);
        }
        guard += 1;
        y += ch;
        if y + box_h > work.1 {
            y = 4.0;
            x += cw;
            // 屏幕放满后叠在最后一列，至少还能点到。
            if x + cw > work.0 {
                x = (work.0 - cw).max(0.0);
                guard = 100_001;
            }
        }
    }
    out
}

// ---- 资源 ----

pub struct Res {
    dwrite: IDWriteFactory,
    pub scale: f32,
    pub roots: Roots,
    label: IDWriteTextFormat,
    line: IDWriteTextFormat,
    title: IDWriteTextFormat,
    glyph: IDWriteTextFormat,
    layouts: HashMap<(String, u32, u8), IDWriteTextLayout>,
    bitmaps: HashMap<String, ID2D1Bitmap>,
    raw: HashMap<String, Loaded>,
    requested: HashSet<String>,
}

/// 透明窗口用不了 ClearType，只能灰度抗锯齿。调高对比度、用对称抗锯齿，字形边缘更干净。
pub fn apply_text_params(rt: &ID2D1RenderTarget, dw: &IDWriteFactory) {
    unsafe {
        rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        if let Ok(p) = dw.CreateCustomRenderingParams(TEXT_GAMMA, TEXT_CONTRAST, 0.0, DWRITE_PIXEL_GEOMETRY_FLAT, DWRITE_RENDERING_MODE_NATURAL_SYMMETRIC) {
            rt.SetTextRenderingParams(&p);
        }
    }
}

const TEXT_GAMMA: f32 = 1.8;
const TEXT_CONTRAST: f32 = 1.0;

fn text_format(dw: &IDWriteFactory, family: PCWSTR, weight: DWRITE_FONT_WEIGHT, size: f32) -> Result<IDWriteTextFormat> {
    unsafe { dw.CreateTextFormat(family, None, weight, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, size, w!("zh-CN")) }
}

fn set_ellipsis(dw: &IDWriteFactory, f: &IDWriteTextFormat) -> Result<()> {
    unsafe {
        let sign = dw.CreateEllipsisTrimmingSign(f)?;
        let t = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, ..Default::default() };
        f.SetTrimming(&t, &sign)
    }
}

impl Res {
    pub fn new(dwrite: IDWriteFactory, scale: f32, roots: Roots, style: &Style) -> Result<Self> {
        unsafe {
            let ui = w!("Microsoft YaHei UI");
            let label = text_format(&dwrite, ui, DWRITE_FONT_WEIGHT_NORMAL, style.font_size)?;
            label.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            label.SetWordWrapping(DWRITE_WORD_WRAPPING_EMERGENCY_BREAK)?;
            set_ellipsis(&dwrite, &label)?;

            let line = text_format(&dwrite, ui, DWRITE_FONT_WEIGHT_NORMAL, style.font_size)?;
            line.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            line.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            set_ellipsis(&dwrite, &line)?;

            // 微软雅黑没有半粗字重，用了会被合成加粗，糊成一团。标题靠字号和位置区分。
            let title = text_format(&dwrite, ui, DWRITE_FONT_WEIGHT_NORMAL, style.font_size + 1.0)?;
            title.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            title.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            set_ellipsis(&dwrite, &title)?;

            let glyph = text_format(&dwrite, w!("Segoe MDL2 Assets"), DWRITE_FONT_WEIGHT_NORMAL, 12.0)?;
            glyph.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            glyph.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

            Ok(Self {
                dwrite,
                scale,
                roots,
                label,
                line,
                title,
                glyph,
                layouts: HashMap::new(),
                bitmaps: HashMap::new(),
                raw: HashMap::new(),
                requested: HashSet::new(),
            })
        }
    }

    /// 接收后台线程送来的图标。D2D 位图到绘制时才创建，用不到的不占内存。
    pub fn accept(&mut self, l: Loaded) {
        self.raw.insert(l.key.clone(), l);
    }

    /// 缩放变化后，所有图标和文字布局都要按新尺寸重建。
    pub fn set_scale(&mut self, scale: f32) {
        self.scale = scale;
        self.bitmaps.clear();
        self.raw.clear();
        self.requested.clear();
        self.layouts.clear();
    }

    fn layout(&mut self, text: &str, width: f32, height: f32, mode: u8) -> Option<IDWriteTextLayout> {
        let key = (text.to_string(), (width * 4.0) as u32, mode);
        if let Some(l) = self.layouts.get(&key) {
            return Some(l.clone());
        }
        // 文件名增删后旧条目没有用了，涨得太多就整体清掉。
        if self.layouts.len() > 4000 {
            self.layouts.clear();
        }
        let fmt = match mode {
            0 => &self.label,
            1 => &self.line,
            _ => &self.title,
        };
        let wide: Vec<u16> = text.encode_utf16().collect();
        let l = unsafe { self.dwrite.CreateTextLayout(&wide, fmt, width, height).ok()? };
        self.layouts.insert(key, l.clone());
        Some(l)
    }

    fn text_width(&mut self, text: &str) -> f32 {
        // 标签栏用 line 格式测宽。
        let Some(l) = self.layout(text, 4000.0, 30.0, 1) else { return 40.0 };
        let mut m = DWRITE_TEXT_METRICS::default();
        if unsafe { l.GetMetrics(&mut m) }.is_err() {
            return 40.0;
        }
        m.width
    }

    fn icon(&mut self, rt: &ID2D1RenderTarget, item: &Item, size: f32, meta: &MetaMap, loader: &Loader) -> Option<ID2D1Bitmap> {
        let px = (size * self.scale).round() as u32;
        let stamp = meta.get(&path_key(&item.path)).map(|m| m.modified).unwrap_or(0);
        let key = format!("{}|{px}|{stamp}", path_key(&item.path));
        if let Some(b) = self.bitmaps.get(&key) {
            return Some(b.clone());
        }
        if let Some(l) = self.raw.remove(&key) {
            let b = make_bitmap(rt, &l.pixels, l.w, l.h).ok()?;
            self.bitmaps.insert(key, b.clone());
            return Some(b);
        }
        if self.requested.insert(key.clone()) {
            let parse = match item.path.strip_prefix(SHELL_PREFIX) {
                Some(clsid) => format!("::{clsid}"),
                None => self.roots.expand(&item.path),
            };
            loader.request(Job { key, parse, px: px.max(16), stamp });
        }
        None
    }
}

fn make_bitmap(rt: &ID2D1RenderTarget, px: &Arc<Vec<u8>>, w: u32, h: u32) -> Result<ID2D1Bitmap> {
    unsafe {
        if px.len() < (w * h * 4) as usize {
            return Err(Error::from(E_FAIL));
        }
        rt.CreateBitmap(
            D2D_SIZE_U { width: w, height: h },
            Some(px.as_ptr() as *const _),
            w * 4,
            &D2D1_BITMAP_PROPERTIES {
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                dpiX: 96.0,
                dpiY: 96.0,
            },
        )
    }
}

// ---- 绘制 ----

pub struct PanelState<'a> {
    pub hover: Hit,
    pub selected: &'a HashSet<String>,
    pub scroll: f32,
    pub drop_hot: bool,
    pub band: Option<Rect>,
}

fn color(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

struct Brushes {
    white: ID2D1SolidColorBrush,
    dim: ID2D1SolidColorBrush,
    shadow: ID2D1SolidColorBrush,
    hover: ID2D1SolidColorBrush,
    select: ID2D1SolidColorBrush,
    placeholder: ID2D1SolidColorBrush,
    edge_dark: ID2D1SolidColorBrush,
    edge_light: ID2D1SolidColorBrush,
    divider: ID2D1SolidColorBrush,
    drop: ID2D1SolidColorBrush,
    soft_shadow: ID2D1SolidColorBrush,
}

fn brushes(rt: &ID2D1RenderTarget) -> Result<Brushes> {
    unsafe {
        let b = |c: D2D1_COLOR_F| rt.CreateSolidColorBrush(&c, None);
        Ok(Brushes {
            white: b(color(1.0, 1.0, 1.0, 1.0))?,
            dim: b(color(1.0, 1.0, 1.0, 0.62))?,
            shadow: b(color(0.0, 0.0, 0.0, 0.85))?,
            hover: b(color(1.0, 1.0, 1.0, 0.13))?,
            select: b(color(0.45, 0.7, 1.0, 0.35))?,
            placeholder: b(color(1.0, 1.0, 1.0, 0.10))?,
            edge_dark: b(color(0.0, 0.0, 0.0, 0.60))?,
            edge_light: b(color(1.0, 1.0, 1.0, 0.42))?,
            divider: b(color(1.0, 1.0, 1.0, 0.18))?,
            drop: b(color(0.45, 0.75, 1.0, 1.0))?,
            soft_shadow: b(color(0.0, 0.0, 0.0, 0.40))?,
        })
    }
}

impl Brushes {
    fn soft_shadow_brush(&self) -> ID2D1SolidColorBrush {
        self.soft_shadow.clone()
    }
}

fn round(r: &Rect, radius: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT { rect: r.d2d(), radiusX: radius, radiusY: radius }
}

/// 文字阴影：不描整圈（会把笔画糊粗），只在右下方投一层锐利的影子。
/// `shadow` 为 2 时再加淡边，用于直接压在壁纸上的底层桌面。
fn draw_label(rt: &ID2D1RenderTarget, l: &IDWriteTextLayout, x: f32, y: f32, shadow: u8, scale: f32, br: &Brushes) {
    let px = 1.0 / scale.max(1.0);
    unsafe {
        if shadow >= 2 {
            // 一个物理像素宽的淡边，白底上也能勾出字形，又不会把笔画糊粗。
            for (dx, dy) in [(-px, 0.0), (0.0, -px), (px * 2.0, px * 2.0)] {
                rt.DrawTextLayout(Vector2 { X: x + dx, Y: y + dy }, l, &br.soft_shadow_brush(), D2D1_DRAW_TEXT_OPTIONS_NONE);
            }
        }
        if shadow >= 1 {
            rt.DrawTextLayout(Vector2 { X: x + px, Y: y + px }, l, &br.shadow, D2D1_DRAW_TEXT_OPTIONS_NONE);
        }
        rt.DrawTextLayout(Vector2 { X: x, Y: y }, l, &br.white, D2D1_DRAW_TEXT_OPTIONS_NONE);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_item(rt: &ID2D1RenderTarget, res: &mut Res, tab: &Tab, index: usize, r: &Rect, hovered: bool, selected: bool, shadow: u8, meta: &MetaMap, loader: &Loader, br: &Brushes) {
    let item = &tab.items[index];
    unsafe {
        if selected {
            rt.FillRoundedRectangle(&round(r, 4.0), &br.select);
        } else if hovered {
            rt.FillRoundedRectangle(&round(r, 4.0), &br.hover);
        }
    }
    let name = item.display_name();
    match tab.view {
        View::Icons => {
            let ix = r.l + (r.r - r.l - tab.icon) / 2.0;
            let iy = r.t + 6.0;
            draw_icon(rt, res, item, tab.icon, ix, iy, meta, loader, br);
            let lw = r.r - r.l - 4.0;
            let lh = r.b - iy - tab.icon - 4.0;
            if let Some(l) = res.layout(&name, lw, lh.max(1.0), 0) {
                draw_label(rt, &l, r.l + 2.0, iy + tab.icon + 4.0, shadow, res.scale, br);
            }
        }
        View::List => {
            let iy = r.t + (r.b - r.t - LIST_ICON) / 2.0;
            draw_icon(rt, res, item, LIST_ICON, r.l + 6.0, iy, meta, loader, br);
            let lx = r.l + 6.0 + LIST_ICON + 8.0;
            if let Some(l) = res.layout(&name, (r.r - lx - 4.0).max(1.0), r.b - r.t, 1) {
                draw_label(rt, &l, lx, r.t, shadow, res.scale, br);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_icon(rt: &ID2D1RenderTarget, res: &mut Res, item: &Item, size: f32, x: f32, y: f32, meta: &MetaMap, loader: &Loader, br: &Brushes) {
    unsafe {
        match res.icon(rt, item, size, meta, loader) {
            Some(b) => {
                // 缩略图可能不是正方形，按比例放进方框并居中。
                let s = b.GetPixelSize();
                let k = size / s.width.max(s.height).max(1) as f32;
                let (dw, dh) = (s.width as f32 * k, s.height as f32 * k);
                let dest = D2D_RECT_F { left: x + (size - dw) / 2.0, top: y + (size - dh) / 2.0, right: x + (size + dw) / 2.0, bottom: y + (size + dh) / 2.0 };
                rt.DrawBitmap(&b, Some(&dest), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None);
            }
            None => rt.FillRoundedRectangle(&round(&Rect { l: x, t: y, r: x + size, b: y + size }, 4.0), &br.placeholder),
        }
    }
}

fn draw_glyph(rt: &ID2D1RenderTarget, res: &Res, ch: u16, x: f32, y: f32, brush: &ID2D1SolidColorBrush) {
    unsafe {
        rt.DrawText(&[ch], &res.glyph, &D2D_RECT_F { left: x, top: y, right: x + 24.0, bottom: y + HEADER_H }, brush, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
    }
}

const G_UP: u16 = 0xE70E;
const G_DOWN: u16 = 0xE70D;
const G_LOCK: u16 = 0xE72E;
const G_UNLOCK: u16 = 0xE785;
const G_ADD: u16 = 0xE710;
const G_MORE: u16 = 0xE712;

fn tab_spans(res: &mut Res, p: &Partition) -> Vec<(f32, f32)> {
    let mut x = 10.0;
    p.tabs
        .iter()
        .map(|t| {
            let w = res.text_width(&t.name) + 16.0;
            let span = (x, x + w);
            x += w + 2.0;
            span
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub fn draw_partition(rt: &ID2D1RenderTarget, res: &mut Res, style: &Style, p: &Partition, meta: &MetaMap, st: &PanelState, loader: &Loader, w: f32, h: f32) -> Result<()> {
    let br = brushes(rt)?;
    let h = if p.collapsed { HEADER_H } else { h };
    unsafe {
        let bg = rt.CreateSolidColorBrush(&color(0.0, 0.0, 0.0, style.bg_alpha), None)?;
        rt.FillRoundedRectangle(&round(&Rect { l: 0.0, t: 0.0, r: w, b: h }, 6.0), &bg);
        // 边框两层：外暗内亮。壁纸不管亮暗，总有一层看得清。
        rt.DrawRoundedRectangle(&round(&Rect { l: 0.5, t: 0.5, r: w - 0.5, b: h - 0.5 }, 6.0), &br.edge_dark, 1.0, None);
        if st.drop_hot {
            rt.DrawRoundedRectangle(&round(&Rect { l: 1.5, t: 1.5, r: w - 1.5, b: h - 1.5 }, 5.0), &br.drop, 2.0, None);
        } else {
            rt.DrawRoundedRectangle(&round(&Rect { l: 1.5, t: 1.5, r: w - 1.5, b: h - 1.5 }, 5.0), &br.edge_light, 1.0, None);
        }
        // 标题栏
        let a = p.active_tab();
        let tab = &p.tabs[a];
        draw_glyph(rt, res, if p.collapsed { G_DOWN } else { G_UP }, 6.0, 0.0, &br.dim);
        draw_glyph(rt, res, if p.locked { G_LOCK } else { G_UNLOCK }, 30.0, 0.0, &br.dim);
        draw_glyph(rt, res, G_ADD, w - 56.0, 0.0, &br.dim);
        draw_glyph(rt, res, G_MORE, w - 30.0, 0.0, &br.dim);
        if let Some(l) = res.layout(&tab.name, (w - 118.0).max(1.0), HEADER_H, 2) {
            draw_label(rt, &l, 58.0, 0.0, 1, res.scale, &br);
        }
        if p.collapsed {
            return Ok(());
        }
        rt.FillRectangle(&D2D_RECT_F { left: 2.0, top: HEADER_H - 0.5, right: w - 2.0, bottom: HEADER_H + 0.5 }, &br.divider);
        // 标签栏
        if p.tabs.len() > 1 {
            let spans = tab_spans(res, p);
            for (i, (t, (x0, x1))) in p.tabs.iter().zip(spans).enumerate() {
                let r = Rect { l: x0, t: HEADER_H, r: x1, b: HEADER_H + TAB_H };
                if st.hover == Hit::Tab(i) && i != a {
                    rt.FillRoundedRectangle(&round(&r, 4.0), &br.hover);
                }
                if let Some(l) = res.layout(&t.name, (x1 - x0).max(1.0), TAB_H, 1) {
                    let brush = if i == a { &br.white } else { &br.dim };
                    rt.DrawTextLayout(Vector2 { X: x0 + 8.0, Y: HEADER_H }, &l, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
                }
                if i == a {
                    rt.FillRectangle(&D2D_RECT_F { left: x0 + 8.0, top: HEADER_H + TAB_H - 3.0, right: x1 - 8.0, bottom: HEADER_H + TAB_H - 1.0 }, &br.white);
                }
            }
        }
        // 内容区
        let top = content_top(p);
        let order = display_order(tab, meta);
        let cells = flow_cells(tab, style, order.len(), w);
        let dy = top - st.scroll;
        rt.PushAxisAlignedClip(&D2D_RECT_F { left: 2.0, top, right: w - 2.0, bottom: h - 2.0 }, D2D1_ANTIALIAS_MODE_ALIASED);
        for (pos, &i) in order.iter().enumerate() {
            let c = &cells[pos];
            let r = Rect { l: c.l, t: c.t + dy, r: c.r, b: c.b + dy };
            if r.b < top || r.t > h {
                continue;
            }
            let sel = st.selected.contains(&path_key(&tab.items[i].path));
            draw_item(rt, res, tab, i, &r, st.hover == Hit::Item(i), sel, 1, meta, loader, &br);
        }
        if let Some(b) = st.band {
            rt.FillRectangle(&b.d2d(), &br.hover);
            rt.DrawRectangle(&b.d2d(), &br.dim, 1.0, None);
        }
        rt.PopAxisAlignedClip();
        // 滚动条
        let total = content_height(tab, &cells);
        let view = h - top;
        if total > view && view > 0.0 {
            let bar = (view * view / total).max(24.0);
            let y = top + (view - bar) * (st.scroll / (total - view)).clamp(0.0, 1.0);
            rt.FillRoundedRectangle(&round(&Rect { l: w - 6.0, t: y, r: w - 3.0, b: y + bar }, 1.5), &br.dim);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn draw_desktop(rt: &ID2D1RenderTarget, res: &mut Res, tab: &Tab, cells: &[Rect], meta: &MetaMap, st: &PanelState, loader: &Loader) -> Result<()> {
    let br = brushes(rt)?;
    let order = display_order(tab, meta);
    for (pos, &i) in order.iter().enumerate() {
        let Some(r) = cells.get(pos) else { break };
        let sel = st.selected.contains(&path_key(&tab.items[i].path));
        draw_item(rt, res, tab, i, r, st.hover == Hit::Item(i), sel, 2, meta, loader, &br);
    }
    if let Some(b) = st.band {
        unsafe {
            rt.FillRectangle(&b.d2d(), &br.hover);
            rt.DrawRectangle(&b.d2d(), &br.dim, 1.0, None);
        }
    }
    Ok(())
}

// ---- 点击判定 ----

pub fn hit_partition(res: &mut Res, style: &Style, p: &Partition, meta: &MetaMap, x: f32, y: f32, w: f32, h: f32, scroll: f32) -> Hit {
    let h = if p.collapsed { HEADER_H } else { h };
    if !p.locked && !p.collapsed {
        let mut mask = 0u8;
        if x < EDGE {
            mask |= 1;
        }
        if y < EDGE {
            mask |= 2;
        }
        if x >= w - EDGE {
            mask |= 4;
        }
        if y >= h - EDGE {
            mask |= 8;
        }
        if mask != 0 {
            return Hit::Edge(mask);
        }
    }
    if y < HEADER_H {
        return if x < 30.0 {
            Hit::Chevron
        } else if x < 54.0 {
            Hit::Lock
        } else if x >= w - 30.0 {
            Hit::More
        } else if x >= w - 56.0 {
            Hit::Add
        } else {
            Hit::Title
        };
    }
    if p.collapsed {
        return Hit::Title;
    }
    if p.tabs.len() > 1 && y < HEADER_H + TAB_H {
        for (i, (x0, x1)) in tab_spans(res, p).into_iter().enumerate() {
            if x >= x0 && x < x1 {
                return Hit::Tab(i);
            }
        }
        return Hit::Title;
    }
    let tab = &p.tabs[p.active_tab()];
    let order = display_order(tab, meta);
    let (cx, cy) = (x, y - content_top(p) + scroll);
    for (pos, c) in flow_cells(tab, style, order.len(), w).iter().enumerate() {
        if c.contains(cx, cy) {
            return Hit::Item(order[pos]);
        }
    }
    Hit::Blank
}

/// 橡皮筋框到了哪些条目。
pub fn items_in_band(p: &Partition, style: &Style, meta: &MetaMap, band: &Rect, w: f32, scroll: f32) -> Vec<usize> {
    let tab = &p.tabs[p.active_tab()];
    let dy = content_top(p) - scroll;
    let order = display_order(tab, meta);
    flow_cells(tab, style, order.len(), w)
        .iter()
        .enumerate()
        .filter(|(_, c)| Rect { l: c.l, t: c.t + dy, r: c.r, b: c.b + dy }.intersects(band))
        .map(|(pos, _)| order[pos])
        .collect()
}

pub fn items_in_band_desktop(tab: &Tab, cells: &[Rect], meta: &MetaMap, band: &Rect) -> Vec<usize> {
    let order = display_order(tab, meta);
    cells.iter().zip(order).filter(|(c, _)| c.intersects(band)).map(|(_, i)| i).collect()
}

pub fn hit_desktop(tab: &Tab, cells: &[Rect], meta: &MetaMap, x: f32, y: f32) -> Hit {
    let order = display_order(tab, meta);
    for (pos, r) in cells.iter().enumerate() {
        if r.contains(x, y) {
            if let Some(&i) = order.get(pos) {
                return Hit::Item(i);
            }
        }
    }
    Hit::Blank
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab_with(n: usize) -> Tab {
        let mut t = Tab::new("t");
        t.cell_w = 100.0;
        t.cell_h = 100.0;
        t.icon = 36.0;
        t.items = (0..n).map(|i| Item::new(format!("%DESKTOP%\\{i}.txt"))).collect();
        t
    }

    #[test]
    fn desktop_cells_flow_down_then_right() {
        let t = tab_with(0);
        let c = desktop_cells(5, &t, &Style::default(), (300.0, 250.0), &[]);
        assert_eq!((c[0].l, c[1].l, c[2].l), (0.0, 0.0, 100.0));
        assert!(c[1].t > c[0].t);
        assert_eq!(c[2].t, c[0].t);
    }

    #[test]
    fn desktop_cells_avoid_partitions() {
        let t = tab_with(0);
        let p = Partition::new("p", 0.0, 0.0, 100.0, 100.0);
        let c = desktop_cells(1, &t, &Style::default(), (300.0, 500.0), &[p]);
        assert!(c[0].t >= 100.0);
    }

    #[test]
    fn desktop_cells_terminate_when_screen_is_full() {
        let t = tab_with(0);
        let p = Partition::new("p", 0.0, 0.0, 1000.0, 1000.0);
        assert_eq!(desktop_cells(10, &t, &Style::default(), (300.0, 300.0), &[p]).len(), 10);
    }

    #[test]
    fn shown_rect_pulls_offscreen_partitions_in_without_touching_the_layout() {
        let p = Partition::new("p", 1650.0, 600.0, 260.0, 300.0);
        // 屏幕暂时只剩 1024x768 / 1.25 这么大。
        let r = shown_rect(&p, (819.0, 576.0));
        assert!(r.l <= 819.0 - 60.0 && r.t <= 576.0 - HEADER_H);
        assert_eq!((p.x, p.y), (1650.0, 600.0));
        // 屏幕恢复后，显示位置就是保存的位置。
        let r = shown_rect(&p, (1920.0, 1032.0));
        assert_eq!((r.l, r.t), (1650.0, 600.0));
    }

    #[test]
    fn desktop_cells_avoid_where_partitions_are_shown() {
        let t = tab_with(0);
        // 保存的位置在屏幕下方之外，实际显示被挪到了屏幕底部，图标要让开显示的位置。
        let p = Partition::new("p", 0.0, 5000.0, 400.0, 100.0);
        let shown = shown_rect(&p, (400.0, 500.0));
        let c = desktop_cells(5, &t, &Style::default(), (400.0, 500.0), &[p]);
        assert!(c.iter().all(|r| !r.intersects(&shown)));
        // 第一列第五格会压到挪上来的分区，所以换到了第二列。
        assert_eq!(c[4].l, 100.0);
    }

    #[test]
    fn sort_by_modified_desc() {
        let mut t = tab_with(3);
        t.sort = SortKey::Modified;
        t.desc = true;
        let mut m = MetaMap::new();
        for (i, stamp) in [5u64, 9, 1].iter().enumerate() {
            m.insert(path_key(&t.items[i].path), Meta { modified: *stamp, ..Default::default() });
        }
        assert_eq!(display_order(&t, &m), vec![1, 0, 2]);
    }

    #[test]
    fn scroll_range_is_never_negative() {
        let style = Style::default();
        let mut p = Partition::new("p", 0.0, 0.0, 500.0, 500.0);
        p.tabs[0] = tab_with(1);
        assert_eq!(max_scroll(&p, &style, 500.0, 500.0), 0.0);
        p.tabs[0] = tab_with(100);
        assert!(max_scroll(&p, &style, 500.0, 300.0) > 0.0);
    }

    #[test]
    fn icons_flow_around_sub_partitions() {
        let style = Style::default();
        let mut t = tab_with(6);
        t.children.push(Partition::new("子", 0.0, 0.0, 210.0, 110.0));
        let cells = flow_cells(&t, &style, 6, 410.0);
        assert_eq!(cells.len(), 6);
        let child = partition_rect(&t.children[0]);
        assert!(cells.iter().all(|c| !c.intersects(&child)));
    }

    #[test]
    fn sub_partitions_extend_the_scroll_range() {
        let style = Style::default();
        let mut p = Partition::new("p", 0.0, 0.0, 400.0, 200.0);
        p.tabs[0].children.push(Partition::new("子", 0.0, 500.0, 100.0, 100.0));
        assert!(max_scroll(&p, &style, 400.0, 200.0) > 400.0);
    }

    #[test]
    fn collapsed_sub_partition_only_blocks_its_header() {
        let style = Style::default();
        let mut t = tab_with(4);
        let mut c = Partition::new("子", 0.0, 0.0, 400.0, 400.0);
        c.collapsed = true;
        t.children.push(c);
        let cells = flow_cells(&t, &style, 4, 410.0);
        assert!(cells[0].t < 200.0);
    }

    #[test]
    fn list_rows_skip_sub_partitions() {
        let style = Style::default();
        let mut t = tab_with(3);
        t.view = View::List;
        t.cell_h = 28.0;
        t.children.push(Partition::new("子", 20.0, 0.0, 100.0, 60.0));
        let cells = flow_cells(&t, &style, 3, 300.0);
        assert!(cells.iter().all(|c| c.t >= 60.0));
    }
}
