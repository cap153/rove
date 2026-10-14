//! 文本对比（Diff）：底层用 `similar` 库，输出统一视图行模型。
//!
//! 参考 pulse `text_diff.cpp` 的分块策略：连续删除 + 连续新增配对为“修改”，
//! 相同行折叠计数，便于渲染层按块着色。

use similar::{ChangeTag, TextDiff};

/// 一行对比结果
#[derive(Debug, Clone)]
pub struct DiffRow {
    /// same | add | del | mod
    pub kind: &'static str,
    /// 旧文件行号（1 起，无则 0）
    pub old_line: usize,
    /// 新文件行号（1 起，无则 0）
    pub new_line: usize,
    pub text: String,
}

/// 对比结果
#[derive(Debug, Default)]
pub struct DiffResult {
    pub rows: Vec<DiffRow>,
    pub added: usize,
    pub removed: usize,
    pub unchanged: usize,
}

/// 对比两段文本
pub fn compare(old: &str, new: &str) -> DiffResult {
    let diff = TextDiff::from_lines(old, new);
    let mut result = DiffResult::default();
    let mut old_line = 0usize;
    let mut new_line = 0usize;

    // 先把 ops 收集出来，便于把连续的 Del + Ins 配对成 Mod
    let ops: Vec<(ChangeTag, String)> = diff
        .iter_all_changes()
        .map(|change| (change.tag(), change.to_string_lossy().to_string()))
        .collect();

    let mut index = 0usize;
    while index < ops.len() {
        let (tag, text) = &ops[index];
        match tag {
            ChangeTag::Equal => {
                old_line += 1;
                new_line += 1;
                result.unchanged += 1;
                result.rows.push(DiffRow {
                    kind: "same",
                    old_line,
                    new_line,
                    text: text.clone(),
                });
                index += 1;
            }
            ChangeTag::Delete => {
                // 收集连续删除
                let mut deleted = Vec::new();
                while index < ops.len() && ops[index].0 == ChangeTag::Delete {
                    old_line += 1;
                    deleted.push(ops[index].1.clone());
                    index += 1;
                }
                // 紧随其后的新增配对为“修改”
                if index < ops.len() && ops[index].0 == ChangeTag::Insert {
                    let mut inserted = Vec::new();
                    while index < ops.len() && ops[index].0 == ChangeTag::Insert {
                        new_line += 1;
                        inserted.push(ops[index].1.clone());
                        index += 1;
                    }
                    result.removed += deleted.len();
                    result.added += inserted.len();
                    let pairs = deleted.len().max(inserted.len());
                    for position in 0..pairs {
                        let old_text = deleted.get(position).cloned().unwrap_or_default();
                        let new_text = inserted.get(position).cloned().unwrap_or_default();
                        result.rows.push(DiffRow {
                            kind: if old_text.is_empty() {
                                "add"
                            } else if new_text.is_empty() {
                                "del"
                            } else {
                                "mod"
                            },
                            old_line: if old_text.is_empty() { 0 } else { old_line - deleted.len() + position + 1 },
                            new_line: if new_text.is_empty() { 0 } else { new_line - inserted.len() + position + 1 },
                            text: if old_text.is_empty() {
                                new_text
                            } else {
                                old_text
                            },
                        });
                    }
                } else {
                    result.removed += deleted.len();
                    for (position, text) in deleted.iter().enumerate() {
                        result.rows.push(DiffRow {
                            kind: "del",
                            old_line: old_line - deleted.len() + position + 1,
                            new_line: 0,
                            text: text.clone(),
                        });
                    }
                }
            }
            ChangeTag::Insert => {
                new_line += 1;
                result.added += 1;
                result.rows.push(DiffRow {
                    kind: "add",
                    old_line: 0,
                    new_line,
                    text: text.clone(),
                });
                index += 1;
            }
        }
    }

    result
}

impl DiffResult {
    /// 汇总文案
    pub fn summary(&self) -> String {
        format!(
            "+{}  -{}  未变 {}",
            self.added, self.removed, self.unchanged
        )
    }
}
