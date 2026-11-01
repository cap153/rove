//! 双轨制跳转搜索：当前目录穿透（置顶） + 全局索引兜底。
//!
//! 轨道 0：历史记录（记录最多 100 个跳转过的目录，按次数和时间淘汰）。
//! 轨道 1：直接列举当前目录的子文件夹，`is_local=true` 排序时优先。
//! 轨道 2：全局索引搜索全部文件夹，过滤掉已在轨道 0/1 命中的项。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use nucleo_matcher::Matcher;

/// 一条跳转命中结果。
pub struct JumpHit {
    pub name: String,
    pub path: PathBuf,
    pub score: u16,
    /// 名称高亮片段：(文本, 是否高亮)。
    pub name_parts: Vec<(String, bool)>,
    /// 是否来自当前目录（轨道 1）。排序时优先置顶。
    pub is_local: bool,
    /// 是否来自历史记录。
    pub is_history: bool,
    /// 最后一次访问该历史记录的时间戳（若不在历史中则为 0）
    pub last_time: u64,
}

/// 历史记录条目。
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct HistoryItem {
    pub path: PathBuf,
    pub last_time: u64,
    pub count: u32,
}

static HISTORY: OnceLock<Mutex<Vec<HistoryItem>>> = OnceLock::new();

fn history_store() -> &'static Mutex<Vec<HistoryItem>> {
    HISTORY.get_or_init(|| {
        let path = std::env::temp_dir().join("rove_jump_history.json");
        let items: Vec<HistoryItem> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Mutex::new(items)
    })
}

/// 记录一次成功的跳转。
pub fn record_history(path: &Path) {
    let mut store = history_store().lock().unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    if let Some(item) = store.iter_mut().find(|i| i.path == path) {
        item.last_time = now;
        item.count += 1;
    } else {
        store.push(HistoryItem {
            path: path.to_path_buf(),
            last_time: now,
            count: 1,
        });
    }

    // 淘汰逻辑：超过 100 个时，次数升序、次数相同则时间升序（最旧的在前面），删掉第一个
    if store.len() > 100 {
        store.sort_by(|a, b| (a.count, a.last_time).cmp(&(b.count, b.last_time)));
        store.remove(0);
    }

    let path = std::env::temp_dir().join("rove_jump_history.json");
    if let Ok(json) = serde_json::to_string(&*store) {
        let _ = std::fs::write(path, json);
    }
}

/// 路径去重 key：Windows 上 `C:\Foo` 与 `c:\foo` 视为同一目录。
fn path_key(p: &Path) -> String {
    p.to_string_lossy().to_lowercase()
}

/// 双轨检索。
pub fn search(query: &str, current_dir: Option<&Path>) -> Vec<JumpHit> {
    let query_lower = query.to_lowercase();

    // 构建历史记录的时间索引，用于快速判断某个命中项是否为历史记录
    let mut history_map: HashMap<String, u64> = HashMap::new();
    {
        let store = history_store().lock().unwrap();
        for item in store.iter() {
            history_map.insert(path_key(&item.path), item.last_time);
        }
    }

    // 空查询：直接返回最新的 5 个历史记录
    if query_lower.is_empty() {
        let store = history_store().lock().unwrap();
        let mut items = store.clone();
        items.sort_by(|a, b| b.last_time.cmp(&a.last_time));
        return items
            .into_iter()
            .take(5)
            .map(|item| {
                let name = item
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| item.path.to_string_lossy().into_owned());
                JumpHit {
                    name: name.clone(),
                    path: item.path,
                    score: 0,
                    name_parts: vec![(name, false)],
                    is_local: false,
                    is_history: true,
                    last_time: item.last_time,
                }
            })
            .collect();
    }

    let mut hits = Vec::new();
    let mut seen = HashSet::new();
    let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
    let mut bufs = (Vec::new(), Vec::new(), Vec::new());

    // 轨道 0：历史记录
    {
        let store = history_store().lock().unwrap();
        for item in store.iter() {
            let name = item
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| item.path.to_string_lossy().into_owned());
            let fields = crate::search::pinyin_fields(&name);
            if let Some(score) =
                crate::search::score(&name, &fields, &query_lower, &mut matcher, &mut bufs)
            {
                if seen.insert(path_key(&item.path)) {
                    let parts = crate::search::highlight_parts(
                        &name,
                        &fields,
                        &query_lower,
                        &mut matcher,
                        &mut bufs,
                    );
                    let is_local = current_dir.map_or(false, |dir| item.path.parent() == Some(dir));
                    hits.push(JumpHit {
                        name,
                        path: item.path.clone(),
                        score,
                        name_parts: parts,
                        is_local,
                        is_history: true,
                        last_time: item.last_time,
                    });
                }
            }
        }
    }

    // 轨道 1：当前目录直接列举
    if let Some(dir) = current_dir {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if let Ok(ft) = entry.file_type() {
                    if !ft.is_dir() {
                        continue;
                    }
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let path = entry.path();
                    let fields = crate::search::pinyin_fields(&name);
                    if let Some(score) =
                        crate::search::score(&name, &fields, &query_lower, &mut matcher, &mut bufs)
                    {
                        let key = path_key(&path);
                        if !seen.insert(key.clone()) {
                            continue;
                        }
                        let parts = crate::search::highlight_parts(
                            &name,
                            &fields,
                            &query_lower,
                            &mut matcher,
                            &mut bufs,
                        );
                        let is_history = history_map.contains_key(&key);
                        let last_time = *history_map.get(&key).unwrap_or(&0);
                        hits.push(JumpHit {
                            name,
                            path,
                            score,
                            name_parts: parts,
                            is_local: true,
                            is_history,
                            last_time,
                        });
                    }
                }
            }
        }
    }

    // 轨道 2：全局索引兜底
    if let Some(idx) = crate::search::index::try_shared() {
        if let Ok(guard) = idx.read() {
            let res = guard.search(&query_lower, 50);
            for (entry, score) in res.hits {
                if entry.is_dir {
                    let key = path_key(&entry.path);
                    if !seen.contains(&key) {
                        let parts = crate::search::highlight_parts(
                            &entry.name,
                            &entry.fields,
                            &query_lower,
                            &mut matcher,
                            &mut bufs,
                        );
                        let is_history = history_map.contains_key(&key);
                        let last_time = *history_map.get(&key).unwrap_or(&0);
                        hits.push(JumpHit {
                            name: entry.name.clone(),
                            path: entry.path.clone(),
                            score,
                            name_parts: parts,
                            is_local: false,
                            is_history,
                            last_time,
                        });
                    }
                }
            }
        }
    }

    // 排序权重：子目录优先 > 历史记录优先 > 历史时间越新越优先 > 匹配度越高越优先
    hits.sort_by(|a, b| {
        let a_key = (a.is_local, a.is_history, a.last_time, a.score);
        let b_key = (b.is_local, b.is_history, b.last_time, b.score);
        b_key.cmp(&a_key)
    });
    hits.truncate(15);
    hits
}
