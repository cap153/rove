//! 暂存盘（Staging Tray）：跨目录多选暂存池，支持批次复制 / 移动到当前目录。
//!
//! 参考 pulse `drop_staging.cpp` 的收集箱交互：暂存只登记路径，提交时才执行传输，
//! 任一项失效不影响其余项，并给出汇总状态。

use std::path::{Path, PathBuf};

/// 暂存池
#[derive(Debug, Default)]
pub struct Staging {
    items: Vec<PathBuf>,
}

/// 批次传输结果
#[derive(Debug, Default)]
pub struct TransferReport {
    pub succeeded: usize,
    pub failed: Vec<(PathBuf, String)>,
}

impl Staging {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn items(&self) -> &[PathBuf] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 加入暂存（去重）
    pub fn add(&mut self, path: &Path) {
        let path = path.to_path_buf();
        if !self.items.contains(&path) {
            self.items.push(path);
        }
    }

    /// 移除指定项
    pub fn remove(&mut self, index: usize) {
        if index < self.items.len() {
            self.items.remove(index);
        }
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// 批次复制到目标目录
    pub fn copy_to(&mut self, dest_dir: &Path) -> TransferReport {
        let sources = self.items.clone();
        let mut report = TransferReport::default();
        for source in &sources {
            if !source.exists() {
                report
                    .failed
                    .push((source.clone(), "源不存在".to_string()));
                continue;
            }
            match crate::ops::copy(std::slice::from_ref(source), dest_dir) {
                Ok(_) => report.succeeded += 1,
                Err(e) => report.failed.push((source.clone(), e.to_string())),
            }
        }
        report
    }

    /// 批次移动到目标目录（成功后从暂存池移除）
    pub fn move_to(&mut self, dest_dir: &Path) -> TransferReport {
        let sources = self.items.clone();
        let mut report = TransferReport::default();
        for source in &sources {
            if !source.exists() {
                report
                    .failed
                    .push((source.clone(), "源不存在".to_string()));
                continue;
            }
            match crate::ops::move_to(std::slice::from_ref(source), dest_dir) {
                Ok(_) => {
                    report.succeeded += 1;
                    self.items.retain(|item| item != source);
                }
                Err(e) => report.failed.push((source.clone(), e.to_string())),
            }
        }
        report
    }
}

impl TransferReport {
    /// 汇总文案
    pub fn summary(&self) -> String {
        if self.failed.is_empty() {
            format!("完成 {} 项", self.succeeded)
        } else {
            format!(
                "完成 {} 项，失败 {} 项（{}）",
                self.succeeded,
                self.failed.len(),
                self.failed
                    .first()
                    .map(|(path, error)| format!("{}: {error}", path.display()))
                    .unwrap_or_default()
            )
        }
    }
}
