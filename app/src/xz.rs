//! 从小智桌面导入分类结构。只读，不写小智的任何文件。
//!
//! 小智的 area.dat（分区）和 vgroup.dat（组和图标）格式相同：
//! Base64(14 字节头) + Base64(JSON 与 1024 字节循环密钥流异或的结果)。个别文件是明文 JSON。

use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

use crate::layout::{Item, Layout, Partition, Roots, Rule, SHELL_PREFIX, SortKey, Tab, View};

const KEY: &[u8; 1024] = include_bytes!("xz_key.bin");

/// 小智的 AI 面板工具项，磁盘上没有对应文件，不迁移。
const TYPE_AI_TOOL: i64 = 6;

#[derive(Debug, Default)]
pub struct Report {
    pub partitions: usize,
    pub tabs: usize,
    pub items: usize,
    pub skipped_tools: usize,
    /// 小智记录里有、磁盘上已经找不到的文件。
    pub missing: Vec<String>,
}

fn b64_decode(input: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let (mut buf, mut bits) = (0u32, 0u32);
    for &c in input {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\r' | b'\n' => continue,
            _ => return Err(format!("非法 Base64 字符 {c}")),
        };
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Ok(out)
}

/// 还原一个小智数据文件的 JSON 文本。
pub fn decode(raw: &[u8]) -> Result<String, String> {
    if raw.first() == Some(&b'{') {
        return String::from_utf8(raw.to_vec()).map_err(|e| e.to_string());
    }
    let sep = raw.iter().position(|&b| b == b'=').ok_or("缺少文件头")?;
    let mut body = b64_decode(&raw[sep + 1..])?;
    for (i, b) in body.iter_mut().enumerate() {
        *b ^= KEY[i % KEY.len()];
    }
    String::from_utf8(body).map_err(|_| "解密结果不是 UTF-8，可能是小智版本变了".to_string())
}

fn read_json(path: &Path) -> Result<Value, String> {
    // 小智运行时可能占用文件，std::fs::read 用的是允许共享读写的默认打开方式。
    let raw = std::fs::read(path).map_err(|e| format!("读取 {} 失败：{e}", path.display()))?;
    let text = decode(&raw).map_err(|e| format!("解析 {} 失败：{e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("解析 {} 失败：{e}", path.display()))
}

fn num(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(Value::as_f64).unwrap_or(0.0)
}

fn int(v: &Value, k: &str) -> i64 {
    v.get(k).and_then(Value::as_i64).unwrap_or(0)
}

fn text<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn flag(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_bool).unwrap_or(false)
}

fn rule_of(type_flag: i64) -> Option<Rule> {
    match type_flag {
        1 => Some(Rule::Document),
        2 => Some(Rule::Image),
        3 => Some(Rule::Archive),
        4 => Some(Rule::Other),
        5 => Some(Rule::Folder),
        _ => None,
    }
}

/// 小智的组 → 我们的标签。尺寸在小智里是物理像素。
fn convert_group(g: &Value, scale: f32, roots: &Roots, report: &mut Report) -> Tab {
    let view = if int(g, "show_style") == 0 { View::List } else { View::Icons };
    let mut items: Vec<&Value> = g.get("item_list").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
    items.sort_by_key(|i| int(i, "index"));

    let mut out = Vec::with_capacity(items.len());
    for it in items {
        if int(it, "type") == TYPE_AI_TOOL {
            report.skipped_tools += 1;
            continue;
        }
        let path = text(it, "path");
        if path.starts_with('{') {
            // 系统图标（此电脑、回收站等），路径就是 CLSID。
            out.push(Item { path: format!("{SHELL_PREFIX}{path}"), name: Some(text(it, "display_name").to_string()) });
            continue;
        }
        if !Path::new(path).exists() {
            report.missing.push(path.to_string());
            continue;
        }
        out.push(Item::new(roots.to_var(path)));
    }
    report.items += out.len();

    let dim = |k: &str| num(g, k) as f32 / scale;
    let (cell_w, cell_h) = match view {
        View::Icons => (dim("ItemWidth") + dim("ItemPadding"), dim("ItemHeight") + dim("ItemVPadding")),
        View::List => (0.0, 28.0),
    };
    Tab {
        id: text(g, "guid").to_string(),
        name: text(g, "name").to_string(),
        view,
        // 小智的 sort_style：4 是手动顺序（图标用 index 排好了），其余本机只见到按时间的用法。
        sort: if int(g, "sort_style") == 4 { SortKey::Manual } else { SortKey::Modified },
        desc: flag(g, "sort_decrease"),
        icon: dim("icon_size_dx"),
        cell_w,
        cell_h,
        rule: rule_of(int(g, "type_flag")),
        items: out,
        children: Vec::new(),
    }
}

/// 读取小智目录，转成新布局。任何一步不合预期都报错，不产出半成品。
pub fn import(dir: &Path, roots: &Roots) -> Result<(Layout, Report), String> {
    let area = read_json(&dir.join("area.dat"))?;
    let vgroup = read_json(&dir.join("vgroup.dat"))?;
    let areas = area.get("list_area").and_then(Value::as_array).ok_or("area.dat 里没有 list_area")?;
    let groups = vgroup.get("group_list").and_then(Value::as_array).ok_or("vgroup.dat 里没有 group_list")?;
    let by_guid: HashMap<&str, &Value> = groups.iter().map(|g| (text(g, "guid"), g)).collect();

    let dpi = num(&area, "dpi");
    let scale = if dpi > 0.0 { (dpi / 96.0) as f32 } else { 1.0 };

    let mut report = Report::default();
    let mut layout = Layout::default();
    let mut found_desktop = false;

    for a in areas {
        let list = a.get("list_group").and_then(Value::as_array).ok_or("分区缺少 list_group")?;
        let mut members = Vec::new();
        for lg in list {
            let guid = text(lg, "group_guid");
            members.push(*by_guid.get(guid).ok_or_else(|| format!("分区引用了不存在的组 {guid}"))?);
        }
        // tool_type 不是 -1 的是 AI 面板，整个跳过。
        let is_tool = members.iter().any(|g| int(g, "tool_type") != -1);
        if is_tool {
            report.skipped_tools += members.iter().map(|g| g.get("item_list").and_then(Value::as_array).map_or(0, |v| v.len())).sum::<usize>();
            continue;
        }
        if flag(a, "is_desktop") {
            let g = members.first().ok_or("底层桌面没有组")?;
            layout.desktop = convert_group(g, scale, roots, &mut report);
            found_desktop = true;
            continue;
        }
        let tabs: Vec<Tab> = members.iter().map(|g| convert_group(g, scale, roots, &mut report)).collect();
        if tabs.is_empty() {
            continue;
        }
        let sel = text(a, "sel_group");
        let active = tabs.iter().position(|t| t.id == sel).unwrap_or(0);
        report.tabs += tabs.len();
        report.partitions += 1;
        layout.partitions.push(Partition {
            id: text(a, "guid").to_string(),
            x: num(a, "pos_x") as f32 / scale,
            y: num(a, "pos_y") as f32 / scale,
            w: num(a, "width") as f32 / scale,
            h: num(a, "height") as f32 / scale,
            locked: flag(a, "is_lock"),
            collapsed: !flag(a, "is_expland"),
            active,
            tabs,
        });
    }
    if !found_desktop {
        return Err("没有找到底层桌面分区".to_string());
    }
    Ok((layout, report))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64_matches_known_values() {
        assert_eq!(b64_decode(b"aGVsbG8=").unwrap(), b"hello");
        assert_eq!(b64_decode(b"aGVsbG8h").unwrap(), b"hello!");
        assert!(b64_decode(b"a$b").is_err());
    }

    #[test]
    fn decode_round_trips_through_the_cipher() {
        let plain = b"{\"list_area\":[]}";
        let cipher: Vec<u8> = plain.iter().enumerate().map(|(i, b)| b ^ KEY[i]).collect();
        // 手工 Base64，免得测试再引入一个依赖。
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut enc = Vec::new();
        for c in cipher.chunks(3) {
            let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
            enc.push(T[(n >> 18) as usize & 63]);
            enc.push(T[(n >> 12) as usize & 63]);
            enc.push(if c.len() > 1 { T[(n >> 6) as usize & 63] } else { b'=' });
            enc.push(if c.len() > 2 { T[n as usize & 63] } else { b'=' });
        }
        let mut file = b"HEADERHEADER=".to_vec();
        file.extend(enc);
        assert_eq!(decode(&file).unwrap().as_bytes(), plain);
    }

    #[test]
    fn decode_accepts_plain_json() {
        assert_eq!(decode(b"{\"a\":1}").unwrap(), "{\"a\":1}");
    }

    /// 用真实备份验证，没有备份的机器上直接跳过。
    #[test]
    fn imports_real_backup_if_present() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../private/xz-backup-20261104");
        if !dir.join("area.dat").exists() {
            return;
        }
        let home = std::env::var("USERPROFILE").unwrap();
        let roots = Roots { desktop: Path::new(&home).join("Desktop"), public: r"C:\Users\Public\Desktop".into() };
        let (l, r) = import(&dir, &roots).unwrap();
        assert_eq!(l.partitions.len(), 5);
        assert_eq!(r.tabs, 8);
        assert!(!l.partitions.iter().any(|p| p.tabs.iter().any(|t| t.name == "AI对话")));
        let doc = l.partitions.iter().find(|p| p.tabs.len() == 4).expect("文档分区应有 4 个标签");
        assert_eq!(doc.tabs[0].rule, Some(Rule::Document));
        assert_eq!(doc.tabs[0].view, View::List);
        assert_eq!(l.desktop.items.iter().filter(|i| i.is_shell()).count(), 3);
    }
}
