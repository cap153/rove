//! 双轨制跳转搜索：当前目录穿透（置顶） + 全局索引兜底。
//!
//! 轨道 1：直接列举当前目录的子文件夹，`is_local=true` 排序时优先置顶。
//! 轨道 2：全局索引搜索全部文件夹，过滤掉已在轨道 1 命中的项。
//! 两者均走 `crate::search` 的拼音/模糊匹配（nucleo），支持全拼、简拼、原文与容错。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use nucleo_matcher::Matcher;

/// 一条跳转命中结果。
pub struct JumpHit {
    pub name: String,
    pub path: PathBuf,
    pub score: u16,
    /// 名称高亮片段：(文本, 是否高亮)。
    pub name_parts: Vec<(String, bool)>,
    /// 是否来自当前目录（轨道 1）。排序时优先置顶，比数值相加更稳。
    pub is_local: bool,
}

/// 路径去重 key：Windows 上 `C:\Foo` 与 `c:\foo` 视为同一目录，避免轨道 1 / 轨道 2 重复。
fn path_key(p: &Path) -> String {
    p.to_string_lossy().to_lowercase()
}

/// 双轨检索：当前目录子文件夹（+10000 分置顶）与全局索引文件夹合并返回。
pub fn search(query: &str, current_dir: Option<&Path>) -> Vec<JumpHit> {
    let query_lower = query.to_lowercase();
    if query_lower.is_empty() {
        return Vec::new();
    }

    let mut hits = Vec::new();
    let mut seen = HashSet::new();
    let mut matcher = Matcher::new(nucleo_matcher::Config::DEFAULT);
    let mut bufs = (Vec::new(), Vec::new(), Vec::new());

    // 轨道 1：当前目录直接列举（无视权限限制，强制置顶）。
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
                        // 命中才标记去重，且用大小写无关的 key
                        if !seen.insert(path_key(&path)) {
                            continue;
                        }
                        let parts = crate::search::highlight_parts(
                            &name,
                            &fields,
                            &query_lower,
                            &mut matcher,
                            &mut bufs,
                        );
                        hits.push(JumpHit {
                            name,
                            path,
                            score,
                            name_parts: parts,
                            is_local: true,
                        });
                    }
                }
            }
        }
    }

    // 轨道 2：全局索引兜底（过滤掉已在轨道 1 命中的项）。
    if let Some(idx) = crate::search::index::try_shared() {
        if let Ok(guard) = idx.read() {
            let res = guard.search(&query_lower, 50);
            for (entry, score) in res.hits {
                if entry.is_dir && !seen.contains(&path_key(&entry.path)) {
                    let parts = crate::search::highlight_parts(
                        &entry.name,
                        &entry.fields,
                        &query_lower,
                        &mut matcher,
                        &mut bufs,
                    );
                    hits.push(JumpHit {
                        name: entry.name.clone(),
                        path: entry.path.clone(),
                        score,
                        name_parts: parts,
                        is_local: false,
                    });
                }
            }
        }
    }

    // 当前目录（is_local）优先，其次按分数；避免依赖 +10000 的数值相加（u16 可能饱和）。
    hits.sort_by(|a, b| (b.is_local, b.score).cmp(&(a.is_local, a.score)));
    hits.truncate(15);
    hits
}
