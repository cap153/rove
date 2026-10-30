//! 文件操作任务系统：复制 / 移动 / 删除（回收站）在独立工作线程执行，
//! 进度经共享状态暴露给 UI 轮询，支持协作式取消。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// 一次文件操作任务。
#[derive(Clone, Debug)]
pub enum JobKind {
    /// 复制到目标目录（保留源）。
    Copy {
        sources: Vec<PathBuf>,
        dest_dir: PathBuf,
    },
    /// 移动到目标目录（删除源）。
    Move {
        sources: Vec<PathBuf>,
        dest_dir: PathBuf,
    },
    /// 删除（to_trash = 进回收站）。
    Delete {
        paths: Vec<PathBuf>,
        to_trash: bool,
    },
}

impl JobKind {
    fn total(&self) -> usize {
        match self {
            JobKind::Copy { sources, .. } | JobKind::Move { sources, .. } => sources.len(),
            JobKind::Delete { paths, .. } => paths.len(),
        }
    }
}

/// 任务完成后的摘要（供 UI 刷新受影响目录）。
#[derive(Clone, Debug, Default)]
#[allow(dead_code)] // failed 计数预留给更细的失败汇报
pub struct JobSummary {
    pub message: String,
    /// 需要刷新列表的目录（源父目录 + 目标目录）。
    pub affected_dirs: Vec<PathBuf>,
    pub failed: usize,
}

/// 共享任务状态（UI 线程轮询）。
#[derive(Default)]
pub struct JobStatus {
    pub running: bool,
    pub done: usize,
    pub total: usize,
    pub current: String,
    /// 刚完成的任务摘要（消费一次后由 UI 清除）。
    pub finished: Option<JobSummary>,
}

/// 任务队列句柄。
pub struct JobManager {
    // cancel / is_running 预留给任务取消与查询 UI。
    tx: Sender<JobKind>,
    status: Arc<Mutex<JobStatus>>,
    cancel: Arc<AtomicBool>,
}

impl JobManager {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel::<JobKind>();
        let status = Arc::new(Mutex::new(JobStatus::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_status = status.clone();
        let worker_cancel = cancel.clone();

        std::thread::Builder::new()
            .name("rove-jobs".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    worker_cancel.store(false, Ordering::SeqCst);
                    let summary = run_job(&job, &worker_status, &worker_cancel);
                    if let Ok(mut s) = worker_status.lock() {
                        s.running = false;
                        s.finished = Some(summary);
                    }
                }
            })
            .expect("spawn rove-jobs worker");

        Self { tx, status, cancel }
    }

    /// 提交任务；返回 false 表示已有任务在进行。
    pub fn submit(&self, job: JobKind) -> bool {
        let mut status = match self.status.lock() {
            Ok(s) => s,
            Err(_) => return false,
        };
        if status.running {
            return false;
        }
        status.running = true;
        status.done = 0;
        status.total = job.total();
        status.current = String::new();
        status.finished = None;
        drop(status);
        self.tx.send(job).is_ok()
    }

    pub fn status(&self) -> Arc<Mutex<JobStatus>> {
        self.status.clone()
    }

    #[allow(dead_code)]
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    #[allow(dead_code)]
    pub fn is_running(&self) -> bool {
        self.status.lock().map(|s| s.running).unwrap_or(false)
    }
}

fn run_job(job: &JobKind, status: &Arc<Mutex<JobStatus>>, cancel: &Arc<AtomicBool>) -> JobSummary {
    match job {
        JobKind::Copy { sources, dest_dir } => run_transfer(sources, dest_dir, false, status, cancel),
        JobKind::Move { sources, dest_dir } => run_transfer(sources, dest_dir, true, status, cancel),
        JobKind::Delete { paths, to_trash } => run_delete(paths, *to_trash, status, cancel),
    }
}

#[allow(unused_variables)]
fn run_transfer(
    sources: &[PathBuf],
    dest_dir: &Path,
    is_move: bool,
    status: &Arc<Mutex<JobStatus>>,
    cancel: &Arc<AtomicBool>,
) -> JobSummary {
    let total = sources.len();
    let mut affected = vec![dest_dir.to_path_buf()];
    for s in sources {
        if let Some(parent) = s.parent() {
            if !affected.iter().any(|d| d == parent) {
                affected.push(parent.to_path_buf());
            }
        }
    }

    #[cfg(windows)]
    {
        // 传入 0 作无模态宿主 HWND，系统自行弹出居中对话框
        // （原生 Shell 操作自带官方传输进度条、同名覆盖确认、UAC 提权、Ctrl+Z 撤销栈）
        let res = if is_move {
            crate::sys::shell::shell_move(sources, dest_dir, 0)
        } else {
            crate::sys::shell::shell_copy(sources, dest_dir, 0)
        };
        return match res {
            Ok(aborted) => JobSummary {
                message: if aborted {
                    "操作已取消".into()
                } else if is_move {
                    format!("已移动 {total} 个项目")
                } else {
                    format!("已复制 {total} 个项目")
                },
                affected_dirs: affected,
                failed: 0,
            },
            Err(e) => JobSummary {
                message: format!("操作失败：{e}"),
                affected_dirs: affected,
                failed: total,
            },
        };
    }

    #[cfg(not(windows))]
    {
        // 非 Windows 平台：保留原有逐文件拷贝/移动回退实现
        let mut failed = 0usize;
        let mut last_error = None;

        for (i, src) in sources.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                return JobSummary {
                    message: "已取消".into(),
                    affected_dirs: affected,
                    failed,
                };
            }
            let name = src
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| src.to_string_lossy().into_owned());
            if let Ok(mut s) = status.lock() {
                s.done = i;
                s.current = name.clone();
            }

            let dest = unique_dest(dest_dir, &name, src.is_dir());
            let result = if is_move {
                move_path(src, &dest)
            } else {
                copy_recursive(src, &dest)
            };
            if let Err(e) = result {
                failed += 1;
                last_error = Some(format!("{}: {e}", name));
            }
        }

        let message = match (failed, last_error) {
            (0, _) => {
                if is_move {
                    format!("已移动 {total} 个项目")
                } else {
                    format!("已复制 {total} 个项目")
                }
            }
            (f, Some(err)) => format!("{f}/{total} 个项目失败（{err}）"),
            (f, None) => format!("{f}/{total} 个项目失败"),
        };
        JobSummary {
            message,
            affected_dirs: affected,
            failed,
        }
    }
}

#[allow(unused_variables)]
fn run_delete(
    paths: &[PathBuf],
    to_trash: bool,
    status: &Arc<Mutex<JobStatus>>,
    cancel: &Arc<AtomicBool>,
) -> JobSummary {
    let total = paths.len();
    let mut affected = Vec::new();
    for p in paths {
        if let Some(parent) = p.parent() {
            if !affected.iter().any(|d| d == parent) {
                affected.push(parent.to_path_buf());
            }
        }
    }

    #[cfg(windows)]
    {
        let res = crate::sys::shell::shell_delete(paths, to_trash, 0);
        return match res {
            Ok(aborted) => JobSummary {
                message: if aborted {
                    "删除已取消".into()
                } else if to_trash {
                    format!("已删除 {total} 个项目到回收站")
                } else {
                    format!("已彻底删除 {total} 个项目")
                },
                affected_dirs: affected,
                failed: 0,
            },
            Err(e) => JobSummary {
                message: format!("删除失败：{e}"),
                affected_dirs: affected,
                failed: total,
            },
        };
    }

    #[cfg(not(windows))]
    {
        // 非 Windows 平台：保留原有逐文件删除回退实现
        let mut failed = 0usize;
        let mut last_error = None;

        for (i, path) in paths.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                return JobSummary {
                    message: "已取消".into(),
                    affected_dirs: affected,
                    failed,
                };
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned());
            if let Ok(mut s) = status.lock() {
                s.done = i;
                s.current = name.clone();
            }

            let result = if to_trash {
                trash::delete(path).map_err(|e| e.to_string())
            } else if path.is_dir() {
                std::fs::remove_dir_all(path).map_err(|e| e.to_string())
            } else {
                std::fs::remove_file(path).map_err(|e| e.to_string())
            };
            if let Err(e) = result {
                failed += 1;
                last_error = Some(format!("{name}: {e}"));
            }
        }

        let message = match (failed, last_error) {
            (0, _) => {
                if to_trash {
                    format!("已删除 {total} 个项目到回收站")
                } else {
                    format!("已彻底删除 {total} 个项目")
                }
            }
            (f, Some(err)) => format!("{f}/{total} 个项目删除失败（{err}）"),
            (f, None) => format!("{f}/{total} 个项目删除失败"),
        };
        JobSummary {
            message,
            affected_dirs: affected,
            failed,
        }
    }
}

/// 目标已存在时生成 "名称 - 副本.ext" / "名称 - 副本 (2).ext"。
pub fn unique_dest(dest_dir: &Path, name: &str, is_dir: bool) -> PathBuf {
    let direct = dest_dir.join(name);
    if !direct.exists() {
        return direct;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), Some(e.to_string())),
        _ => (name.to_string(), None),
    };
    let _ = is_dir;
    for candidate in [
        format!("{stem} - 副本.{}", ext.clone().unwrap_or_default()),
        format!("{stem} - 副本 (2).{}", ext.clone().unwrap_or_default()),
        format!("{stem} - 副本 (3).{}", ext.clone().unwrap_or_default()),
        format!("{stem} - 副本 (4).{}", ext.clone().unwrap_or_default()),
        format!("{stem} - 副本 (5).{}", ext.clone().unwrap_or_default()),
    ] {
        let path = dest_dir.join(&candidate);
        if !path.exists() {
            return path;
        }
    }
    // 极端情况：追加时间戳。
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    dest_dir.join(format!("{stem} - 副本 {stamp}.{}", ext.unwrap_or_default()))
}

/// 递归复制（文件或目录）。
// 仅在非 Windows 回退路径与单元测试中使用；Windows 运行时由原生 Shell 接管。
#[allow(dead_code)]
pub fn copy_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    let meta = std::fs::metadata(src).map_err(|e| e.to_string())?;
    if meta.is_dir() {
        std::fs::create_dir_all(dst).map_err(|e| e.to_string())?;
        let entries = std::fs::read_dir(src).map_err(|e| e.to_string())?;
        for entry in entries.flatten() {
            let child_src = entry.path();
            let child_dst = dst.join(entry.file_name());
            copy_recursive(&child_src, &child_dst)?;
        }
        Ok(())
    } else {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::copy(src, dst).map(|_| ()).map_err(|e| e.to_string())
    }
}

/// 移动：优先 rename，跨卷时退回复制 + 删除。
// 仅在非 Windows 回退路径与单元测试中使用；Windows 运行时由原生 Shell 接管。
#[allow(dead_code)]
pub fn move_path(src: &Path, dst: &Path) -> Result<(), String> {
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    copy_recursive(src, dst)?;
    let meta = std::fs::metadata(src).map_err(|e| e.to_string())?;
    if meta.is_dir() {
        std::fs::remove_dir_all(src).map_err(|e| e.to_string())
    } else {
        std::fs::remove_file(src).map_err(|e| e.to_string())
    }
}

/// 状态栏文本。
pub fn status_text(status: &JobStatus) -> String {
    if !status.running {
        return String::new();
    }
    format!(
        "正在处理 {}/{}：{}",
        status.done, status.total, status.current
    )
}

/// 测试辅助。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_dest_appends_suffix() {
        let dir = std::env::temp_dir().join("rove-test-unique-dest");
        let _ = std::fs::create_dir_all(&dir);
        let existing = dir.join("a.txt");
        std::fs::write(&existing, b"x").unwrap();
        let dest = unique_dest(&dir, "a.txt", false);
        assert!(dest != existing);
        assert!(dest.file_name().unwrap().to_string_lossy().contains("副本"));
        let fresh = unique_dest(&dir, "b.txt", false);
        assert_eq!(fresh.file_name().unwrap(), "b.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_recursive_copies_tree() {
        let base = std::env::temp_dir().join("rove-test-copy-recursive");
        let _ = std::fs::remove_dir_all(&base);
        let src = base.join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("top.txt"), b"top").unwrap();
        std::fs::write(src.join("sub/inner.txt"), b"inner").unwrap();

        let dst = base.join("dst");
        copy_recursive(&src, &dst).unwrap();
        assert_eq!(std::fs::read(dst.join("top.txt")).unwrap(), b"top");
        assert_eq!(std::fs::read(dst.join("sub/inner.txt")).unwrap(), b"inner");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn move_path_same_fs() {
        let base = std::env::temp_dir().join("rove-test-move");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let src = base.join("src.txt");
        std::fs::write(&src, b"data").unwrap();
        let dst = base.join("dst.txt");
        move_path(&src, &dst).unwrap();
        assert!(!src.exists());
        assert_eq!(std::fs::read(&dst).unwrap(), b"data");
        let _ = std::fs::remove_dir_all(&base);
    }
}
