//! 布局模型：分区、标签、图标归属。纯数据，不依赖界面，保存为可读 JSON。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

pub const LAYOUT_VERSION: u32 = 2;
pub const VAR_DESKTOP: &str = "%DESKTOP%";
pub const VAR_PUBLIC: &str = "%PUBLIC_DESKTOP%";
pub const SHELL_PREFIX: &str = "shell:";
pub const DEFAULT_BG_ALPHA: f32 = 0.45;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum View {
    Icons,
    List,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortKey {
    Manual,
    Name,
    Modified,
    Size,
    Kind,
}

/// 一键整理时，什么样的文件归入这个标签。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Rule {
    Document,
    Image,
    Archive,
    Other,
    Folder,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Item {
    pub path: String,
    /// 只有系统图标（如“此电脑”）才需要存显示名，普通文件由路径推出。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Item {
    pub fn new(path: impl Into<String>) -> Self {
        Self { path: path.into(), name: None }
    }

    pub fn is_shell(&self) -> bool {
        self.path.starts_with(SHELL_PREFIX)
    }

    pub fn display_name(&self) -> String {
        if let Some(n) = &self.name {
            return n.clone();
        }
        let file = self.path.rsplit(['\\', '/']).next().unwrap_or(&self.path);
        let lower = file.to_ascii_lowercase();
        // 和资源管理器一致：快捷方式不显示扩展名。
        for ext in [".lnk", ".url", ".pif"] {
            if lower.ends_with(ext) && file.len() > ext.len() {
                return file[..file.len() - ext.len()].to_string();
            }
        }
        file.to_string()
    }
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Tab {
    pub id: String,
    pub name: String,
    pub view: View,
    pub sort: SortKey,
    #[serde(default)]
    pub desc: bool,
    /// 以下尺寸单位都是 DIP（96 DPI 下的像素）。
    pub icon: f32,
    pub cell_w: f32,
    pub cell_h: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<Rule>,
    #[serde(default)]
    pub items: Vec<Item>,
    /// 嵌在这个标签里的子分区。坐标相对于标签内容区的左上角，可以一层层继续嵌套。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Partition>,
}

impl Tab {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: new_id(),
            name: name.into(),
            view: View::Icons,
            sort: SortKey::Manual,
            desc: false,
            icon: 36.0,
            cell_w: 75.0,
            cell_h: 82.0,
            rule: None,
            items: Vec::new(),
            children: Vec::new(),
        }
    }
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Partition {
    pub id: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    #[serde(default)]
    pub locked: bool,
    #[serde(default)]
    pub collapsed: bool,
    #[serde(default)]
    pub active: usize,
    pub tabs: Vec<Tab>,
}

impl Partition {
    pub fn new(name: impl Into<String>, x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { id: new_id(), x, y, w, h, locked: false, collapsed: false, active: 0, tabs: vec![Tab::new(name)] }
    }

    pub fn active_tab(&self) -> usize {
        self.active.min(self.tabs.len().saturating_sub(1))
    }
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Style {
    /// 分区背景的不透明度，0 到 1。
    pub bg_alpha: f32,
    pub font_size: f32,
}

impl Default for Style {
    fn default() -> Self {
        Self { bg_alpha: DEFAULT_BG_ALPHA, font_size: 12.0 }
    }
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Layout {
    pub version: u32,
    #[serde(default)]
    pub style: Style,
    /// 底层桌面：没有归入任何分区的图标都在这里。
    pub desktop: Tab,
    #[serde(default)]
    pub partitions: Vec<Partition>,
}

impl Default for Layout {
    fn default() -> Self {
        let mut desktop = Tab::new("桌面");
        desktop.cell_h = 88.5;
        Self { version: LAYOUT_VERSION, style: Style::default(), desktop, partitions: Vec::new() }
    }
}

pub fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0);
    format!("{:x}-{:x}", t, COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// 桌面上一个真实存在的条目。
#[derive(Clone, Debug)]
pub struct Entry {
    pub path: String,
    pub is_dir: bool,
    pub hidden: bool,
    pub modified: u64,
    pub size: u64,
}

/// 布局和磁盘的对账结果。
#[derive(Default, Debug)]
pub struct Reconciled {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

fn key(path: &str) -> String {
    path.to_lowercase()
}

fn walk(tab: &Tab, f: &mut dyn FnMut(&Tab)) {
    f(tab);
    for p in &tab.children {
        for t in &p.tabs {
            walk(t, f);
        }
    }
}

fn walk_mut(tab: &mut Tab, f: &mut dyn FnMut(&mut Tab)) {
    f(tab);
    for p in &mut tab.children {
        for t in &mut p.tabs {
            walk_mut(t, f);
        }
    }
}

fn find_tab<'a>(tab: &'a mut Tab, id: &str) -> Option<&'a mut Tab> {
    if tab.id == id {
        return Some(tab);
    }
    for p in &mut tab.children {
        for t in &mut p.tabs {
            if let Some(found) = find_tab(t, id) {
                return Some(found);
            }
        }
    }
    None
}

fn find_in<'a>(list: &'a [Partition], id: &str) -> Option<&'a Partition> {
    for p in list {
        if p.id == id {
            return Some(p);
        }
        for t in &p.tabs {
            if let Some(found) = find_in(&t.children, id) {
                return Some(found);
            }
        }
    }
    None
}

fn find_in_mut<'a>(list: &'a mut [Partition], id: &str) -> Option<&'a mut Partition> {
    for p in list {
        if p.id == id {
            return Some(p);
        }
        for t in &mut p.tabs {
            if let Some(found) = find_in_mut(&mut t.children, id) {
                return Some(found);
            }
        }
    }
    None
}

/// 取出一个分区。返回它和上级标签的 id（顶层分区没有上级）。
fn take_from(list: &mut Vec<Partition>, id: &str, parent: Option<&str>) -> Option<(Partition, Option<String>)> {
    if let Some(i) = list.iter().position(|p| p.id == id) {
        return Some((list.remove(i), parent.map(str::to_string)));
    }
    for p in list.iter_mut() {
        for t in &mut p.tabs {
            let tab_id = t.id.clone();
            if let Some(found) = take_from(&mut t.children, id, Some(&tab_id)) {
                return Some(found);
            }
        }
    }
    None
}

fn flatten_items(tab: Tab, out: &mut Vec<Item>) {
    out.extend(tab.items);
    for p in tab.children {
        for t in p.tabs {
            flatten_items(t, out);
        }
    }
}

impl Layout {
    pub fn for_each_tab(&self, f: &mut dyn FnMut(&Tab)) {
        walk(&self.desktop, f);
        for p in &self.partitions {
            for t in &p.tabs {
                walk(t, f);
            }
        }
    }

    pub fn for_each_tab_mut(&mut self, f: &mut dyn FnMut(&mut Tab)) {
        walk_mut(&mut self.desktop, f);
        for p in &mut self.partitions {
            for t in &mut p.tabs {
                walk_mut(t, f);
            }
        }
    }

    pub fn tab_by_id_mut(&mut self, id: &str) -> Option<&mut Tab> {
        if let Some(t) = find_tab(&mut self.desktop, id) {
            return Some(t);
        }
        for p in &mut self.partitions {
            for t in &mut p.tabs {
                if let Some(found) = find_tab(t, id) {
                    return Some(found);
                }
            }
        }
        None
    }

    pub fn partition(&self, id: &str) -> Option<&Partition> {
        find_in(&self.partitions, id)
    }

    pub fn partition_mut(&mut self, id: &str) -> Option<&mut Partition> {
        find_in_mut(&mut self.partitions, id)
    }

    pub fn item_count(&self) -> usize {
        let mut n = 0;
        self.for_each_tab(&mut |t| n += t.items.len());
        n
    }

    /// 让布局和磁盘一致：消失的文件移除，新文件进入 `new_targets` 指定的标签，否则进底层桌面。
    pub fn reconcile(&mut self, entries: &[Entry], new_targets: &HashMap<String, String>) -> Reconciled {
        let mut out = Reconciled::default();
        let present: HashSet<String> = entries.iter().map(|e| key(&e.path)).collect();
        let mut seen = HashSet::new();
        self.for_each_tab_mut(&mut |tab| {
            tab.items.retain(|it| {
                if it.is_shell() {
                    return seen.insert(key(&it.path));
                }
                let k = key(&it.path);
                if !present.contains(&k) {
                    out.removed.push(it.path.clone());
                    return false;
                }
                // 同一个文件只能属于一个标签。
                seen.insert(k)
            });
        });
        for e in entries {
            let k = key(&e.path);
            if seen.contains(&k) || e.hidden {
                continue;
            }
            seen.insert(k.clone());
            let item = Item::new(e.path.clone());
            let placed = new_targets.get(&k).and_then(|id| self.tab_by_id_mut(id)).map(|t| t.items.push(item.clone())).is_some();
            if !placed {
                self.desktop.items.push(item);
            }
            out.added.push(e.path.clone());
        }
        out
    }

    /// 把一批条目移到目标标签。`before` 是目标标签里的插入位置，None 表示放到末尾。
    pub fn move_items(&mut self, paths: &[String], tab_id: &str, before: Option<usize>) -> usize {
        let wanted: HashSet<String> = paths.iter().map(|p| key(p)).collect();
        // 先算好插入位置：同标签内移动时，前面被取走的条目会让下标前移。
        let mut insert_at = before;
        if let (Some(at), Some(t)) = (before, self.tab_by_id_mut(tab_id)) {
            let shift = t.items.iter().take(at).filter(|i| wanted.contains(&key(&i.path))).count();
            insert_at = Some(at - shift);
        }
        let mut taken: HashMap<String, Item> = HashMap::new();
        self.for_each_tab_mut(&mut |tab| {
            tab.items.retain(|it| {
                let k = key(&it.path);
                if wanted.contains(&k) {
                    taken.insert(k, it.clone());
                    false
                } else {
                    true
                }
            });
        });
        let moved: Vec<Item> = paths.iter().filter_map(|p| taken.remove(&key(p))).collect();
        let n = moved.len();
        if let Some(tab) = self.tab_by_id_mut(tab_id) {
            let at = insert_at.unwrap_or(tab.items.len()).min(tab.items.len());
            tab.items.splice(at..at, moved);
        } else {
            self.desktop.items.extend(moved);
        }
        n
    }

    /// 重命名后保持原来的归属和位置。
    pub fn rename_item(&mut self, old: &str, new: &str) -> bool {
        let k = key(old);
        let mut done = false;
        self.for_each_tab_mut(&mut |tab| {
            if let Some(it) = tab.items.iter_mut().find(|i| key(&i.path) == k) {
                it.path = new.to_string();
                done = true;
            }
        });
        done
    }

    fn give_back(&mut self, items: Vec<Item>, parent_tab: Option<String>) {
        match parent_tab.and_then(|id| self.tab_by_id_mut(&id).map(|t| t as *mut Tab)) {
            // tab_by_id_mut 的借用到这里已经结束，指针只在本行内解引用。
            Some(t) => unsafe { (*t).items.extend(items) },
            None => self.desktop.items.extend(items),
        }
    }

    /// 删除分区时，里面的图标（包括各层子分区里的）回到上一级，文件本身不动。
    pub fn remove_partition(&mut self, id: &str) {
        let Some((p, parent)) = take_from(&mut self.partitions, id, None) else { return };
        let mut items = Vec::new();
        for t in p.tabs {
            flatten_items(t, &mut items);
        }
        self.give_back(items, parent);
    }

    /// 删除标签，规则同删除分区。分区至少要留一个标签。
    pub fn remove_tab(&mut self, partition_id: &str, index: usize) -> bool {
        let parent = self.parent_tab_of(partition_id);
        let Some(p) = self.partition_mut(partition_id) else { return false };
        if p.tabs.len() < 2 || index >= p.tabs.len() {
            return false;
        }
        let t = p.tabs.remove(index);
        p.active = p.active_tab().min(p.tabs.len() - 1);
        let mut items = Vec::new();
        flatten_items(t, &mut items);
        self.give_back(items, parent);
        true
    }

    /// 分区所在的上级标签。顶层分区返回 None。
    pub fn parent_tab_of(&self, partition_id: &str) -> Option<String> {
        fn search(list: &[Partition], id: &str, parent: Option<&str>) -> Option<Option<String>> {
            for p in list {
                if p.id == id {
                    return Some(parent.map(str::to_string));
                }
                for t in &p.tabs {
                    if let Some(found) = search(&t.children, id, Some(&t.id)) {
                        return Some(found);
                    }
                }
            }
            None
        }
        search(&self.partitions, partition_id, None).flatten()
    }

    /// 按规则把底层桌面的文件归入带规则的标签。快捷方式和系统图标留在原地。
    pub fn organize(&mut self, entries: &[Entry]) -> usize {
        let dirs: HashSet<String> = entries.iter().filter(|e| e.is_dir).map(|e| key(&e.path)).collect();
        let mut targets: HashMap<Rule, String> = HashMap::new();
        self.for_each_tab(&mut |t| {
            if let Some(r) = t.rule {
                targets.entry(r).or_insert_with(|| t.id.clone());
            }
        });
        let mut moves: Vec<(String, String)> = Vec::new();
        for it in &self.desktop.items {
            if it.is_shell() {
                continue;
            }
            let is_dir = dirs.contains(&key(&it.path));
            let Some(rule) = classify(&it.path, is_dir) else { continue };
            if let Some(id) = targets.get(&rule) {
                moves.push((it.path.clone(), id.clone()));
            }
        }
        for (path, id) in &moves {
            self.move_items(std::slice::from_ref(path), id, None);
        }
        moves.len()
    }
}

impl std::hash::Hash for Rule {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (*self as u8).hash(state);
    }
}

/// 快捷方式返回 None：它们不参与一键整理。
pub fn classify(path: &str, is_dir: bool) -> Option<Rule> {
    if is_dir {
        return Some(Rule::Folder);
    }
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    const SHORTCUT: &[&str] = &["lnk", "url", "pif"];
    const DOC: &[&str] = &[
        "txt", "pdf", "doc", "docx", "ppt", "pptx", "xls", "xlsx", "csv", "wps", "xml", "rtf", "xps", "htm", "html", "mht", "mhtml", "odt", "ods", "odp",
        "xlsb", "xlsm", "pptm", "docm", "md", "mobi", "epub",
    ];
    const IMG: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "webp", "tif", "tiff", "ico", "svg", "heic", "raw"];
    const ARC: &[&str] = &["zip", "rar", "7z", "tar", "gz", "bz2", "xz", "iso", "cab"];
    let e = ext.as_str();
    if SHORTCUT.contains(&e) {
        None
    } else if DOC.contains(&e) {
        Some(Rule::Document)
    } else if IMG.contains(&e) {
        Some(Rule::Image)
    } else if ARC.contains(&e) {
        Some(Rule::Archive)
    } else {
        Some(Rule::Other)
    }
}

/// 路径里的桌面目录。测试时可以换成假目录。
#[derive(Clone, Debug)]
pub struct Roots {
    pub desktop: PathBuf,
    pub public: PathBuf,
}

impl Roots {
    fn strip<'a>(path: &'a str, root: &Path) -> Option<&'a str> {
        let r = root.to_string_lossy();
        let head = path.get(..r.len())?;
        if !head.eq_ignore_ascii_case(&r) {
            return None;
        }
        let tail = &path[r.len()..];
        if tail.is_empty() || tail.starts_with('\\') { Some(tail) } else { None }
    }

    /// 真实路径转成带变量的形式，换机、换用户名后布局仍然可用。
    pub fn to_var(&self, path: &str) -> String {
        if let Some(t) = Self::strip(path, &self.desktop) {
            format!("{VAR_DESKTOP}{t}")
        } else if let Some(t) = Self::strip(path, &self.public) {
            format!("{VAR_PUBLIC}{t}")
        } else {
            path.to_string()
        }
    }

    pub fn expand(&self, path: &str) -> String {
        if let Some(t) = path.strip_prefix(VAR_DESKTOP) {
            format!("{}{t}", self.desktop.display())
        } else if let Some(t) = path.strip_prefix(VAR_PUBLIC) {
            format!("{}{t}", self.public.display())
        } else {
            path.to_string()
        }
    }
}

pub fn is_on_desktop(var_path: &str) -> bool {
    var_path.starts_with(VAR_DESKTOP) || var_path.starts_with(VAR_PUBLIC)
}

/// 列出两个桌面目录的内容。desktop.ini 这类系统文件不算桌面图标。
/// 目录读不出来时返回错误：调用方不能把“读不到”当成“文件都没了”。
pub fn scan(roots: &Roots) -> Result<Vec<Entry>, String> {
    use std::os::windows::fs::MetadataExt;
    const HIDDEN: u32 = 0x2;
    const SYSTEM: u32 = 0x4;
    let mut out = Vec::new();
    for dir in [&roots.desktop, &roots.public] {
        let rd = std::fs::read_dir(dir).map_err(|e| format!("{}：{e}", dir.display()))?;
        for e in rd.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            let attrs = meta.file_attributes();
            let name = e.file_name().to_string_lossy().to_string();
            if attrs & SYSTEM != 0 && attrs & HIDDEN != 0 {
                continue;
            }
            if name.eq_ignore_ascii_case("desktop.ini") {
                continue;
            }
            out.push(Entry {
                path: roots.to_var(&e.path().to_string_lossy()),
                is_dir: meta.is_dir(),
                hidden: attrs & HIDDEN != 0,
                modified: meta.last_write_time(),
                size: meta.len(),
            });
        }
    }
    Ok(out)
}

// ---- 读写 ----

pub fn load(path: &Path) -> Result<Layout, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("读取失败：{e}"))?;
    parse(&text)
}

pub fn parse(text: &str) -> Result<Layout, String> {
    let mut layout: Layout = serde_json::from_str(text).map_err(|e| format!("格式错误：{e}"))?;
    if layout.version > LAYOUT_VERSION {
        return Err(format!("布局版本 {} 比程序新", layout.version));
    }
    // 版本 1 的背景沿用小智的 30%，用户反馈可读性不够，调到新的默认值。
    if layout.version < 2 && layout.style.bg_alpha < DEFAULT_BG_ALPHA {
        layout.style.bg_alpha = DEFAULT_BG_ALPHA;
    }
    layout.version = LAYOUT_VERSION;
    Ok(layout)
}

pub fn to_json(layout: &Layout) -> String {
    serde_json::to_string_pretty(layout).unwrap_or_else(|_| "{}".to_string())
}

/// 先写临时文件再替换，断电也不会留下半个布局文件。
pub fn save(path: &Path, layout: &Layout) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, to_json(layout))?;
    std::fs::rename(&tmp, path)
}

/// 每小时最多留一份备份，共保留 `keep` 份。
pub fn rotate_backups(path: &Path, keep: usize) {
    let name = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let dir = path.parent().unwrap_or(Path::new("."));
    let backup = |i: usize| dir.join(format!("{name}.backup{i}.json"));
    if let Ok(m) = std::fs::metadata(backup(1)).and_then(|m| m.modified()) {
        if m.elapsed().map(|e| e.as_secs() < 3600).unwrap_or(false) {
            return;
        }
    }
    if !path.exists() {
        return;
    }
    for i in (1..keep).rev() {
        let _ = std::fs::rename(backup(i), backup(i + 1));
    }
    let _ = std::fs::copy(path, backup(1));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(p: &str) -> Entry {
        Entry { path: p.to_string(), is_dir: false, hidden: false, modified: 0, size: 0 }
    }

    fn layout_with_partition() -> (Layout, String) {
        let mut l = Layout::default();
        let p = Partition::new("文档", 0.0, 0.0, 100.0, 100.0);
        let id = p.tabs[0].id.clone();
        l.partitions.push(p);
        (l, id)
    }

    #[test]
    fn reconcile_adds_new_to_desktop_and_removes_missing() {
        let (mut l, id) = layout_with_partition();
        l.tab_by_id_mut(&id).unwrap().items.push(Item::new("%DESKTOP%\\a.pdf"));
        l.desktop.items.push(Item::new("%DESKTOP%\\gone.txt"));
        let r = l.reconcile(&[entry("%DESKTOP%\\a.pdf"), entry("%DESKTOP%\\new.txt")], &HashMap::new());
        assert_eq!(r.added, vec!["%DESKTOP%\\new.txt"]);
        assert_eq!(r.removed, vec!["%DESKTOP%\\gone.txt"]);
        assert_eq!(l.desktop.items[0].path, "%DESKTOP%\\new.txt");
        assert_eq!(l.tab_by_id_mut(&id).unwrap().items.len(), 1);
    }

    #[test]
    fn reconcile_routes_dropped_file_to_target_tab() {
        let (mut l, id) = layout_with_partition();
        let mut t = HashMap::new();
        t.insert("%desktop%\\drop.txt".to_string(), id.clone());
        l.reconcile(&[entry("%DESKTOP%\\drop.txt")], &t);
        assert_eq!(l.tab_by_id_mut(&id).unwrap().items[0].path, "%DESKTOP%\\drop.txt");
        assert!(l.desktop.items.is_empty());
    }

    #[test]
    fn reconcile_keeps_shell_items_and_skips_new_hidden_files() {
        let mut l = Layout::default();
        l.desktop.items.push(Item { path: "shell:{20D04FE0-3AEA-1069-A2D8-08002B30309D}".into(), name: Some("此电脑".into()) });
        let mut hidden = entry("%DESKTOP%\\~$x.docx");
        hidden.hidden = true;
        let r = l.reconcile(&[hidden], &HashMap::new());
        assert!(r.added.is_empty());
        assert_eq!(l.desktop.items.len(), 1);
    }

    #[test]
    fn reconcile_removes_duplicates_across_tabs() {
        let (mut l, id) = layout_with_partition();
        l.desktop.items.push(Item::new("%DESKTOP%\\a.pdf"));
        l.tab_by_id_mut(&id).unwrap().items.push(Item::new("%DESKTOP%\\A.PDF"));
        l.reconcile(&[entry("%DESKTOP%\\a.pdf")], &HashMap::new());
        assert_eq!(l.item_count(), 1);
    }

    #[test]
    fn move_items_keeps_order_when_moving_within_a_tab() {
        let mut l = Layout::default();
        for n in ["a", "b", "c", "d"] {
            l.desktop.items.push(Item::new(n));
        }
        let id = l.desktop.id.clone();
        // 把 a 拖到 c 之前（下标 2），取走 a 后 c 的下标变成 1。
        l.move_items(&["a".to_string()], &id, Some(2));
        let names: Vec<_> = l.desktop.items.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(names, ["b", "a", "c", "d"]);
    }

    #[test]
    fn remove_partition_returns_items_to_desktop() {
        let (mut l, id) = layout_with_partition();
        l.tab_by_id_mut(&id).unwrap().items.push(Item::new("x"));
        let pid = l.partitions[0].id.clone();
        l.remove_partition(&pid);
        assert!(l.partitions.is_empty());
        assert_eq!(l.desktop.items[0].path, "x");
    }

    #[test]
    fn organize_moves_by_rule_and_leaves_shortcuts() {
        let (mut l, id) = layout_with_partition();
        l.tab_by_id_mut(&id).unwrap().rule = Some(Rule::Document);
        l.desktop.items.push(Item::new("%DESKTOP%\\a.pdf"));
        l.desktop.items.push(Item::new("%DESKTOP%\\b.lnk"));
        l.desktop.items.push(Item::new("%DESKTOP%\\c.zip")); // 没有压缩规则的标签，留在原地。
        assert_eq!(l.organize(&[]), 1);
        assert_eq!(l.desktop.items.len(), 2);
        assert_eq!(l.tab_by_id_mut(&id).unwrap().items[0].path, "%DESKTOP%\\a.pdf");
    }

    #[test]
    fn path_vars_round_trip_case_insensitively() {
        let r = Roots { desktop: PathBuf::from(r"C:\Users\me\Desktop"), public: PathBuf::from(r"C:\Users\Public\Desktop") };
        assert_eq!(r.to_var(r"c:\users\ME\desktop\a.txt"), r"%DESKTOP%\a.txt");
        assert_eq!(r.to_var(r"C:\Users\Public\Desktop\b.lnk"), r"%PUBLIC_DESKTOP%\b.lnk");
        assert_eq!(r.to_var(r"C:\Users\me\DesktopOther\c.txt"), r"C:\Users\me\DesktopOther\c.txt");
        assert_eq!(r.expand(r"%DESKTOP%\a.txt"), r"C:\Users\me\Desktop\a.txt");
    }

    #[test]
    fn display_name_hides_shortcut_extensions_only() {
        assert_eq!(Item::new(r"%DESKTOP%\Word.lnk").display_name(), "Word");
        assert_eq!(Item::new(r"%DESKTOP%\a.pdf").display_name(), "a.pdf");
        assert_eq!(Item::new(r"%DESKTOP%\.lnk").display_name(), ".lnk");
    }

    fn nested() -> (Layout, String, String, String) {
        let mut l = Layout::default();
        let mut outer = Partition::new("外层", 0.0, 0.0, 400.0, 400.0);
        let mut inner = Partition::new("内层", 10.0, 10.0, 100.0, 100.0);
        inner.tabs[0].items.push(Item::new("%DESKTOP%\\deep.pdf"));
        let inner_id = inner.id.clone();
        let inner_tab = inner.tabs[0].id.clone();
        let outer_tab = outer.tabs[0].id.clone();
        outer.tabs[0].children.push(inner);
        l.partitions.push(outer);
        (l, inner_id, inner_tab, outer_tab)
    }

    #[test]
    fn reconcile_sees_items_in_sub_partitions() {
        let (mut l, _, inner_tab, _) = nested();
        let r = l.reconcile(&[entry("%DESKTOP%\\deep.pdf")], &HashMap::new());
        assert!(r.added.is_empty() && r.removed.is_empty());
        assert_eq!(l.tab_by_id_mut(&inner_tab).unwrap().items.len(), 1);
        assert!(l.desktop.items.is_empty());
    }

    #[test]
    fn move_items_reaches_sub_partitions() {
        let (mut l, _, inner_tab, _) = nested();
        l.desktop.items.push(Item::new("x"));
        assert_eq!(l.move_items(&["x".to_string()], &inner_tab, None), 1);
        assert_eq!(l.tab_by_id_mut(&inner_tab).unwrap().items.len(), 2);
    }

    #[test]
    fn removing_sub_partition_returns_items_to_parent_tab() {
        let (mut l, inner_id, _, outer_tab) = nested();
        assert_eq!(l.parent_tab_of(&inner_id).as_deref(), Some(outer_tab.as_str()));
        l.remove_partition(&inner_id);
        assert!(l.partition(&inner_id).is_none());
        assert_eq!(l.tab_by_id_mut(&outer_tab).unwrap().items[0].path, "%DESKTOP%\\deep.pdf");
        assert!(l.desktop.items.is_empty());
    }

    #[test]
    fn removing_outer_partition_flattens_everything_to_desktop() {
        let (mut l, _, _, _) = nested();
        let outer = l.partitions[0].id.clone();
        l.remove_partition(&outer);
        assert_eq!(l.desktop.items.len(), 1);
    }

    #[test]
    fn remove_tab_keeps_at_least_one_and_returns_items() {
        let (mut l, inner_id, _, outer_tab) = nested();
        assert!(!l.remove_tab(&inner_id, 0));
        let mut t = Tab::new("第二个");
        t.items.push(Item::new("y"));
        l.partition_mut(&inner_id).unwrap().tabs.push(t);
        assert!(l.remove_tab(&inner_id, 1));
        assert_eq!(l.tab_by_id_mut(&outer_tab).unwrap().items[0].path, "y");
    }

    #[test]
    fn nested_json_round_trip_and_old_files_still_load() {
        let (l, ..) = nested();
        assert_eq!(parse(&to_json(&l)).unwrap(), l);
        // 版本 1 的文件没有 children 字段，背景是 30%。
        let mut old = serde_json::to_value(Layout::default()).unwrap();
        old["version"] = 1.into();
        old["style"]["bg_alpha"] = 0.30.into();
        let parsed = parse(&old.to_string()).unwrap();
        assert_eq!(parsed.version, LAYOUT_VERSION);
        assert_eq!(parsed.style.bg_alpha, DEFAULT_BG_ALPHA);
    }

    #[test]
    fn json_round_trip() {
        let (mut l, id) = layout_with_partition();
        l.tab_by_id_mut(&id).unwrap().items.push(Item::new("%DESKTOP%\\中文.pdf"));
        assert_eq!(parse(&to_json(&l)).unwrap(), l);
    }

    #[test]
    fn rejects_newer_version() {
        let mut l = Layout::default();
        l.version = LAYOUT_VERSION + 1;
        assert!(parse(&to_json(&l)).is_err());
    }
}
