//! 每个文件夹自己的「视图 + 排序」记忆（设置 → 通用 → 独立视图和排序）。
//!
//! 单独的 `views.json`，不和 `config.json` 混在一起：
//! - 这份数据被频繁小改（每次切视图 / 改排序），塞进 config.json 会让那个
//!   给人手改的文件每次跟着重写、还会越写越大；
//! - 它只是"记忆"，删了就等于所有目录回到默认视图，不会影响任何设置。
//!
//! 上限 500 个目录，按「最后一次改设置」的时间淘汰最旧的（只浏览不算，
//! 否则常翻的目录会把真正改过设置的老目录挤掉）。

use std::collections::HashMap;
use std::path::Path;

/// 最多记多少个路径。
pub const MAX_ENTRIES: usize = 500;

/// 「此电脑」没有真实路径，用伪键单独记。
pub const KEY_THIS_PC: &str = "::thispc";
/// 「回收站」同理。
pub const KEY_RECYCLE: &str = "::recycle";

/// 一个路径的视图偏好。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewPref {
    /// details / list / icons-small / icons-medium / icons-large / icons-xl
    pub view: String,
    /// name / size / modified / type / path / drive
    pub sort_key: String,
    pub sort_ascending: bool,
}

impl ViewPref {
    pub fn new(view: &str, sort_key: &str, sort_ascending: bool) -> Self {
        Self {
            view: view.to_string(),
            sort_key: sort_key.to_string(),
            sort_ascending,
        }
    }
}

/// 磁盘上的形状：紧凑写，不 pretty。
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct DiskFile {
    /// 递增序号：下次分配从 `seq + 1` 开始。
    #[serde(default)]
    seq: u64,
    /// 路径键 → [视图, 排序键, 升序, 最后修改序号]
    #[serde(default)]
    items: HashMap<String, DiskItem>,
}

/// 每项都带 `default`：`views.json` 是能手改的文件，少一个字段不该让整份
/// 记忆作废（缺视图就按 details、缺排序就按名称）。
#[derive(serde::Serialize, serde::Deserialize)]
struct DiskItem {
    #[serde(default)]
    v: String,
    #[serde(default)]
    k: String,
    #[serde(default = "default_true")]
    a: bool,
    #[serde(default)]
    n: u64,
}

fn default_true() -> bool {
    true
}

pub struct ViewMemory {
    items: HashMap<String, (ViewPref, u64)>,
    seq: u64,
    dirty: bool,
}

/// 路径规范化成记忆键：统一小写、去掉末尾分隔符（`D:\A` 和 `d:\a\` 是同一个）。
///
/// Windows 路径本身不区分大小写，同名不同大小写却记成两份毫无意义。
pub fn path_key(path: &Path) -> String {
    let s = path.to_string_lossy();
    let trimmed = s.trim_end_matches(['\\', '/']);
    let trimmed = if trimmed.is_empty() { s.as_ref() } else { trimmed };
    trimmed.to_lowercase()
}

/// 认识的视图模式。别的字符串一律按默认处理：`views.json` 是能手改的文件，
/// 也可能来自旧/新版本，一个不认识的模式会让列表落到"哪个分支都不匹配"的
/// 中间态（看似图标视图、格宽却是小图标），排查起来很费劲。
pub fn normalize_view(view: &str) -> &str {
    match view {
        "details" | "list" | "icons-small" | "icons-medium" | "icons-large" | "icons-xl" => view,
        _ => "details",
    }
}

fn memory_path() -> Option<std::path::PathBuf> {
    let dir = crate::config::Config::config_path()?.parent()?.to_path_buf();
    Some(dir.join("views.json"))
}

impl ViewMemory {
    pub fn load() -> Self {
        let Some(path) = memory_path() else {
            return Self::empty();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::empty();
        };
        let disk: DiskFile = match serde_json::from_str(&text) {
            Ok(d) => d,
            Err(e) => {
                // 坏了就当没有：这只是一份视图记忆，不能因为它起不来。
                crate::logal!("!! [views] views.json 解析失败，按空处理：{e}");
                return Self::empty();
            }
        };
        let mut items = HashMap::new();
        for (key, item) in disk.items {
            items.insert(
                key,
                (
                    ViewPref {
                        view: item.v,
                        sort_key: item.k,
                        sort_ascending: item.a,
                    },
                    item.n,
                ),
            );
        }
        let mut mem = Self { items, seq: disk.seq, dirty: false };
        // 手改过的文件可能超限：先按序号削一遍。
        mem.trim();
        crate::logln!("[views] 载入 {} 个目录的视图记忆", mem.len());
        mem
    }

    fn empty() -> Self {
        Self {
            items: HashMap::new(),
            seq: 0,
            dirty: false,
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn get(&self, key: &str) -> Option<&ViewPref> {
        self.items.get(key).map(|(p, _)| p)
    }

    /// 写入一条完整偏好。返回是否真的变了（没变就不必写盘、也不必刷新界面）。
    pub fn set(&mut self, key: &str, pref: ViewPref) -> bool {
        self.seq += 1;
        let seq = self.seq;
        match self.items.get_mut(key) {
            Some(slot) => {
                if slot.0 == pref {
                    return false;
                }
                slot.0 = pref;
                slot.1 = seq;
            }
            None => {
                self.items.insert(key.to_string(), (pref, seq));
            }
        }
        self.trim();
        self.dirty = true;
        true
    }

    /// 只改视图，排序保持原样（没有记录时用给定的默认排序补上）。
    pub fn set_view(&mut self, key: &str, view: &str, base: ViewPref) -> bool {
        let mut pref = self.get(key).cloned().unwrap_or(base);
        if pref.view == view {
            return false;
        }
        pref.view = view.to_string();
        self.set(key, pref)
    }

    /// 只改排序，视图保持原样。
    pub fn set_sort(&mut self, key: &str, sort_key: &str, ascending: bool, base: ViewPref) -> bool {
        let mut pref = self.get(key).cloned().unwrap_or(base);
        if pref.sort_key == sort_key && pref.sort_ascending == ascending {
            return false;
        }
        pref.sort_key = sort_key.to_string();
        pref.sort_ascending = ascending;
        self.set(key, pref)
    }

    /// 按序号淘汰最久没改过设置的，直到回到上限内。
    ///
    /// 排序一次再删多余的。不能用"循环找最小值"：`views.json` 是能手改的文件，
    /// 万一里面有几万条，那样每次淘汰都要扫全表，O(n²) 会在启动时直接卡住。
    fn trim(&mut self) {
        if self.items.len() <= MAX_ENTRIES {
            return;
        }
        let mut order: Vec<(u64, String)> = self
            .items
            .iter()
            .map(|(k, (_, seq))| (*seq, k.clone()))
            .collect();
        order.sort_unstable();
        let excess = self.items.len() - MAX_ENTRIES;
        for (_, key) in order.into_iter().take(excess) {
            self.items.remove(&key);
        }
    }

    /// 脏了才写盘（调用方做防抖，见 `App::schedule_views_flush`）。
    pub fn flush(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let Some(path) = memory_path() else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let disk = DiskFile {
            seq: self.seq,
            items: self
                .items
                .iter()
                .map(|(k, (p, n))| {
                    (
                        k.clone(),
                        DiskItem {
                            v: p.view.clone(),
                            k: p.sort_key.clone(),
                            a: p.sort_ascending,
                            n: *n,
                        },
                    )
                })
                .collect(),
        };
        match serde_json::to_string(&disk) {
            Ok(text) => {
                if let Err(e) = std::fs::write(&path, text) {
                    // 写失败就把它重新标脏，下一次改动/退出时再试一次。
                    self.dirty = true;
                    crate::logal!("!! [views] 写入 views.json 失败：{e}");
                }
            }
            Err(e) => crate::logal!("!! [views] 序列化失败：{e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pref(view: &str, key: &str, asc: bool) -> ViewPref {
        ViewPref::new(view, key, asc)
    }

    #[test]
    fn path_key_is_case_and_separator_insensitive() {
        assert_eq!(path_key(Path::new(r"D:\Downloads")), r"d:\downloads");
        assert_eq!(path_key(Path::new(r"d:\downloads\")), r"d:\downloads");
        assert_eq!(path_key(Path::new(r"D:\Downloads")), path_key(Path::new(r"d:\downloads\")));
    }

    #[test]
    fn set_reports_change_only_when_value_differs() {
        let mut mem = ViewMemory::empty();
        assert!(mem.set(r"d:\a", pref("details", "name", true)));
        assert!(!mem.set(r"d:\a", pref("details", "name", true)), "同值不该算改动");
        assert!(mem.set(r"d:\a", pref("list", "name", true)));
        assert_eq!(mem.get(r"d:\a").unwrap().view, "list");
    }

    #[test]
    fn partial_updates_keep_the_other_field() {
        let mut mem = ViewMemory::empty();
        mem.set(r"d:\a", pref("icons-large", "size", false));
        // 只改排序：视图不动。
        mem.set_sort(r"d:\a", "modified", true, pref("details", "name", true));
        let got = mem.get(r"d:\a").unwrap();
        assert_eq!(got.view, "icons-large");
        assert_eq!(got.sort_key, "modified");
        assert!(got.sort_ascending);
        // 只改视图：排序不动。
        mem.set_view(r"d:\a", "list", pref("details", "name", true));
        let got = mem.get(r"d:\a").unwrap();
        assert_eq!(got.view, "list");
        assert_eq!(got.sort_key, "modified");
    }

    #[test]
    fn partial_update_on_missing_key_uses_base() {
        let mut mem = ViewMemory::empty();
        mem.set_view(r"d:\new", "icons-xl", pref("details", "drive", false));
        let got = mem.get(r"d:\new").unwrap();
        assert_eq!(got.view, "icons-xl");
        assert_eq!(got.sort_key, "drive");
        assert!(!got.sort_ascending);
    }

    /// 超上限时淘汰「最久没改设置」的那个，而不是乱砍。
    #[test]
    fn trim_drops_the_least_recently_changed() {
        let mut mem = ViewMemory::empty();
        for i in 0..MAX_ENTRIES {
            mem.set(&format!(r"d:\dir{i}"), pref("details", "name", true));
        }
        // 最老的是 dir0。
        mem.set(r"d:\dir0", pref("details", "name", false));
        // 再塞 5 个：应淘汰 dir1..dir5，dir0 因为是最近改过的要留下。
        for i in 0..5 {
            mem.set(&format!(r"d:\extra{i}"), pref("list", "name", true));
        }
        assert_eq!(mem.len(), MAX_ENTRIES);
        assert!(mem.get(r"d:\dir0").is_some(), "最近改过的不能被淘汰");
        assert!(mem.get(r"d:\dir1").is_none(), "最久没改的应该先走");
        assert_eq!(mem.get(r"d:\dir1").is_none(), true);
    }

    /// 手改坏的文件可能有几万条：淘汰必须是一次性排序，不能逐条扫全表。
    #[test]
    fn trim_handles_oversized_file() {
        let mut mem = ViewMemory::empty();
        let total = MAX_ENTRIES * 6;
        for i in 0..total {
            mem.items.insert(
                format!(r"d:\x{i}"),
                (pref("details", "name", true), i as u64),
            );
        }
        mem.trim();
        assert_eq!(mem.len(), MAX_ENTRIES);
        assert!(mem.get(r"d:\x0").is_none(), "序号最小的先走");
        assert!(mem.get(&format!(r"d:\x{}", total - 1)).is_some());
    }

    #[test]
    fn unknown_view_falls_back_to_details() {
        assert_eq!(normalize_view("icons-huge"), "details");
        assert_eq!(normalize_view(""), "details");
        assert_eq!(normalize_view("icons-xl"), "icons-xl");
        assert_eq!(normalize_view("list"), "list");
    }

    /// 手改过的 views.json 少字段不该让整份记忆作废：缺的字段走默认值。
    #[test]
    fn disk_item_tolerates_missing_fields() {
        let disk: DiskFile =
            serde_json::from_str(r#"{"seq":3,"items":{"d:\\a":{"v":"list"}}}"#).unwrap();
        let item = &disk.items[r"d:\a"];
        assert_eq!(item.v, "list");
        assert_eq!(item.k, "");
        assert!(item.a, "缺 asc 时按升序");
        assert_eq!(item.n, 0);
    }

    #[test]
    fn pseudo_keys_are_plain_keys() {
        let mut mem = ViewMemory::empty();
        mem.set(KEY_THIS_PC, pref("icons-medium", "drive", true));
        mem.set(KEY_RECYCLE, pref("details", "modified", false));
        assert_eq!(mem.get(KEY_THIS_PC).unwrap().view, "icons-medium");
        assert_eq!(mem.get(KEY_RECYCLE).unwrap().sort_key, "modified");
    }
}
