//! src/ops/job.rs
//! 文件操作任务系统：复制 / 移动 / 删除（回收站）在独立工作线程执行，
//! 进度经共享状态暴露给 UI 轮询，支持协作式取消。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};

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
    /// 撤消 / 重做一条历史记录（`redo` = true 表示重做）。
    History {
        redo: bool,
        entry: crate::ops::undo::Kind,
    },
    /// 还原回收站条目（键 = 条目的唯一 id，也兼容原始路径）。
    RestoreRecycle { keys: Vec<PathBuf> },
    /// 从回收站剪切后粘贴：先还原，再把落回原位置的条目移进目标目录。
    RestoreMove { keys: Vec<PathBuf>, dest_dir: PathBuf },
}

impl JobKind {
    fn total(&self) -> usize {
        match self {
            JobKind::Copy { sources, .. } | JobKind::Move { sources, .. } => sources.len(),
            JobKind::Delete { paths, .. } => paths.len(),
            JobKind::RestoreRecycle { keys } => keys.len(),
            JobKind::RestoreMove { keys, .. } => keys.len(),
            JobKind::History { .. } => 1,
        }
    }
}

/// 成功完成的任务类型：UI 据此决定刷新/选中策略，不依赖提示文案。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum JobOutcome {
    /// 失败、取消或其它情况。
    #[default]
    Other,
    /// 已移动（含 Shell 成功后）。
    Moved,
    /// 已删除（回收站或彻底删除）。
    Deleted,
}

/// 任务完成后的摘要（供 UI 刷新受影响目录）。
#[derive(Clone, Debug, Default)]
#[allow(dead_code)] // failed 计数预留给更细的失败汇报
pub struct JobSummary {
    pub message: String,
    /// 需要刷新列表的目录（源父目录 + 目标目录）。
    pub affected_dirs: Vec<PathBuf>,
    pub failed: usize,
    pub outcome: JobOutcome,
    /// 这次操作成功后要记进撤消栈的记录（撤消 / 重做任务本身为 None）。
    pub recorded: Option<crate::ops::undo::Kind>,
    /// 撤消 / 重做任务自身的结果：(是否重做, 那条记录)。成功与否都由 UI
    /// 决定它在两个栈之间怎么搬。
    pub history: Option<(bool, crate::ops::undo::Kind)>,
    /// 这次操作不可撤消（彻底删除），撤消历史必须跟着作废。
    pub clear_undo: bool,
    /// 撤消 / 重做任务是被**用户取消**的（不是失败）：记录要退回原栈，
    /// 别的失败则把记录扔掉（见 `UndoStack::drop_undo`）。
    pub cancelled: bool,
    /// 任务完成后要选中 / 滚进视野的条目（目前只有「从回收站剪切粘贴」
    /// 用得上：那些路径是还原之后才知道的，UI 提交任务时拿不到）。
    pub select: Vec<PathBuf>,
}

/// 用户取消了操作（Shell 进度框点了「取消」）。
pub const CANCELLED: &str = "操作已取消";

/// 当前 unix 秒（回收站条目时间戳用同一把尺子）。
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 状态栏文案的中英选择（en = 当前界面为英文）。
fn t(en: bool, zh: &str, english: &str) -> String {
    if en { english.to_string() } else { zh.to_string() }
}

/// 带 `{}` 占位符的文案：按顺序替换。
fn tf(en: bool, zh: &str, english: &str, args: &[String]) -> String {
    let mut s = t(en, zh, english);
    for a in args {
        s = s.replace("{}", a);
    }
    s
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
    /// 界面是否为英文：完成摘要与进度文案按此选词。
    en: Arc<AtomicBool>,
}

impl JobManager {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel::<JobKind>();
        let status = Arc::new(Mutex::new(JobStatus::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let en = Arc::new(AtomicBool::new(false));
        let worker_status = status.clone();
        let worker_cancel = cancel.clone();
        let worker_en = en.clone();

        std::thread::Builder::new()
            .name("rove-jobs".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    worker_cancel.store(false, Ordering::SeqCst);
                    let summary = run_job(&job, &worker_status, &worker_cancel, &worker_en);
                    if let Ok(mut s) = worker_status.lock() {
                        s.running = false;
                        s.finished = Some(summary);
                    }
                }
            })
            .expect("spawn rove-jobs worker");

        Self { tx, status, cancel, en }
    }

    /// 语言切换时调用：之后完成的任务摘要按该语言生成。
    pub fn set_english(&self, value: bool) {
        self.en.store(value, Ordering::Relaxed);
    }

    pub fn is_english(&self) -> bool {
        self.en.load(Ordering::Relaxed)
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

fn run_job(
    job: &JobKind,
    status: &Arc<Mutex<JobStatus>>,
    cancel: &Arc<AtomicBool>,
    en: &Arc<AtomicBool>,
) -> JobSummary {
    let en = en.load(Ordering::Relaxed);
    match job {
        JobKind::Copy { sources, dest_dir } => {
            run_transfer(sources, dest_dir, false, status, cancel, en)
        }
        JobKind::Move { sources, dest_dir } => {
            run_transfer(sources, dest_dir, true, status, cancel, en)
        }
        JobKind::Delete { paths, to_trash } => run_delete(paths, *to_trash, status, cancel, en),
        JobKind::History { redo, entry } => run_history(entry, *redo, en),
        JobKind::RestoreRecycle { keys } => run_restore_recycle(keys, en),
        JobKind::RestoreMove { keys, dest_dir } => {
            run_restore_move(keys, dest_dir, status, cancel, en)
        }
    }
}

#[allow(unused_variables)]
fn run_transfer(
    sources: &[PathBuf],
    dest_dir: &Path,
    is_move: bool,
    status: &Arc<Mutex<JobStatus>>,
    cancel: &Arc<AtomicBool>,
    en: bool,
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
        // （原生 Shell 操作自带官方传输进度条、同名覆盖确认、UAC 提权）。
        // 撤消栈是 Rove 自己记的（见 ops::undo），不依赖 Shell 有没有记录。
        // 复制 / 移动前先记下目标目录里已有什么：事后差出来的才是这次新落的
        // 条目（同名冲突时系统会改名，只有差集能准确告诉我们是哪几条）。
        let before = name_set(dest_dir);
        let res = if is_move {
            crate::sys::shell::shell_move(sources, dest_dir, 0)
        } else {
            crate::sys::shell::shell_copy(sources, dest_dir, 0)
        };
        return match res {
            Ok(aborted) => {
                let created = before
                    .as_ref()
                    .map(|before| new_entries(dest_dir, before))
                    .unwrap_or_default();
                let recorded = if aborted {
                    None
                } else {
                    let entry = if is_move {
                        crate::ops::undo::Kind::Move {
                            pairs: crate::ops::undo::pair_created(sources, &created),
                        }
                    } else {
                        crate::ops::undo::Kind::Copy {
                            sources: sources.to_vec(),
                            dest: dest_dir.to_path_buf(),
                            created,
                        }
                    };
                    // 一条都没落地就别记（全被跳过 / 同名全冲突时列表还是空的）。
                    (entry.path_count() > 0).then_some(entry)
                };
                JobSummary {
                    message: if aborted {
                        t(en, "操作已取消", "Operation cancelled")
                    } else if is_move {
                        tf(en, "已移动 {} 个项目", "Moved {} items", &[total.to_string()])
                    } else {
                        tf(en, "已复制 {} 个项目", "Copied {} items", &[total.to_string()])
                    },
                    affected_dirs: affected,
                    failed: 0,
                    outcome: if aborted {
                        JobOutcome::Other
                    } else if is_move {
                        JobOutcome::Moved
                    } else {
                        JobOutcome::Other
                    },
                    recorded,
                    ..Default::default()
                }
            }
            Err(e) => JobSummary {
                message: tf(en, "操作失败：{}", "Operation failed: {}", &[e.to_string()]),
                affected_dirs: affected,
                failed: total,
                outcome: JobOutcome::Other,
                ..Default::default()
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
                    message: t(en, "已取消", "Cancelled"),
                    affected_dirs: affected,
                    failed,
                    outcome: JobOutcome::Other,
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
                    tf(en, "已移动 {} 个项目", "Moved {} items", &[total.to_string()])
                } else {
                    tf(en, "已复制 {} 个项目", "Copied {} items", &[total.to_string()])
                }
            }
            (f, Some(err)) => tf(
                en,
                "{}/{} 个项目失败（{}）",
                "{}/{} items failed ({})",
                &[f.to_string(), total.to_string(), err],
            ),
            (f, None) => tf(
                en,
                "{}/{} 个项目失败",
                "{}/{} items failed",
                &[f.to_string(), total.to_string()],
            ),
        };
        JobSummary {
            message,
            affected_dirs: affected,
            failed,
            outcome: if failed == 0 && is_move {
                JobOutcome::Moved
            } else {
                JobOutcome::Other
            },
        }
    }
}

#[allow(unused_variables)]
fn run_delete(
    paths: &[PathBuf],
    to_trash: bool,
    status: &Arc<Mutex<JobStatus>>,
    cancel: &Arc<AtomicBool>,
    en: bool,
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
        // 记下删除开始的时刻：回填回收站条目 id 时用它排除以前删的同名条目。
        let started = unix_now() - 3;
        let res = crate::sys::shell::shell_delete(paths, to_trash, 0);
        return match res {
            Ok(aborted) => JobSummary {
                // 彻底删除不进撤消栈：它本来就没有反向动作。
                recorded: (to_trash && !aborted && !paths.is_empty()).then(|| {
                    // 回填这次删除产生的回收站条目 id：「撤消删除」靠它精确找回
                    // 这一条，同一路径删过多次也不会认错。
                    let ids = crate::fs::drives::resolve_trash_ids(paths, started);
                    crate::ops::undo::Kind::Trash {
                        paths: paths.to_vec(),
                        ids,
                    }
                }),
                message: if aborted {
                    t(en, "删除已取消", "Delete cancelled")
                } else if to_trash {
                    tf(
                        en,
                        "已删除 {} 个项目到回收站",
                        "Moved {} items to the Recycle Bin",
                        &[total.to_string()],
                    )
                } else {
                    tf(
                        en,
                        "已彻底删除 {} 个项目",
                        "Permanently deleted {} items",
                        &[total.to_string()],
                    )
                },
                affected_dirs: affected,
                failed: 0,
                outcome: if aborted {
                    JobOutcome::Other
                } else {
                    JobOutcome::Deleted
                },
                // 彻底删除没有反向动作：撤消历史必须一起作废，否则下一次
                // Ctrl+Z 会往前跳过一条，去撤消更早的那次操作。
                clear_undo: !to_trash && !aborted,
                ..Default::default()
            },
            Err(e) => JobSummary {
                message: tf(en, "删除失败：{}", "Delete failed: {}", &[e.to_string()]),
                affected_dirs: affected,
                failed: total,
                outcome: JobOutcome::Other,
                ..Default::default()
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
                    outcome: JobOutcome::Other,
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
                    tf(
                        en,
                        "已删除 {} 个项目到回收站",
                        "Moved {} items to the Recycle Bin",
                        &[total.to_string()],
                    )
                } else {
                    tf(
                        en,
                        "已彻底删除 {} 个项目",
                        "Permanently deleted {} items",
                        &[total.to_string()],
                    )
                }
            }
            (f, Some(err)) => tf(
                en,
                "{}/{} 个项目删除失败（{}）",
                "{}/{} items failed to delete ({})",
                &[f.to_string(), total.to_string(), err],
            ),
            (f, None) => tf(
                en,
                "{}/{} 个项目删除失败",
                "{}/{} items failed to delete",
                &[f.to_string(), total.to_string()],
            ),
        };
        JobSummary {
            message,
            affected_dirs: affected,
            failed,
            outcome: if failed == 0 {
                JobOutcome::Deleted
            } else {
                JobOutcome::Other
            },
        }
    }
}

// ---------------------------------------------------------------------------
// 撤消 / 重做
// ---------------------------------------------------------------------------

/// 目录里现有条目的名字（只取名字：只是用来做前后差集）。
fn name_set(dir: &Path) -> Option<std::collections::HashSet<std::ffi::OsString>> {
    let read = std::fs::read_dir(dir).ok()?;
    Some(read.flatten().map(|e| e.file_name()).collect())
}

/// 操作之后目录里多出来的条目（相对 `before` 快照）。
fn new_entries(dir: &Path, before: &std::collections::HashSet<std::ffi::OsString>) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    read.flatten()
        .filter(|e| !before.contains(&e.file_name()))
        .map(|e| dir.join(e.file_name()))
        .collect()
}

/// 一条历史记录的反向 / 正向动作。
///
/// `redo = false` 是撤消（执行反向），`true` 是重做（再执行一次正向）。
/// 允许就地更新记录：复制重做后新条目的名字可能变了（同名冲突被系统改名），
/// 把最新的一份写回记录，下次撤消才不会去删一个不存在的路径。
fn apply_history(entry: &mut crate::ops::undo::Kind, redo: bool) -> Result<usize, String> {
    use crate::ops::undo::Kind;
    match entry {
        Kind::Trash { paths, ids } => {
            if redo {
                // 重做删除 = 再删一次。仍然进回收站，所以还能再撤消。
                // 只删**现在还在**的：撤消时被「跳过」的、或已经被别人挪走的
                // 那些路径不存在，整批发给 Shell 会直接报「找不到文件」。
                let existing: Vec<PathBuf> = paths.iter().filter(|p| p.exists()).cloned().collect();
                if existing.is_empty() {
                    return Err("这些项目已经不在原位置了（撤消时可能没有还原成功）".into());
                }
                let started = unix_now() - 3;
                if crate::sys::shell::shell_delete(&existing, true, 0)? {
                    return Err(CANCELLED.into());
                }
                // 新删出来的是**另一批**回收站条目：id 要重新回填，否则下一次
                // 撤消还拿着上一批的 id，回收站里早就没有了。
                // id 按 `paths` 对齐（找不到的就是 None），撤消那一侧靠长度判断。
                *ids = crate::fs::drives::resolve_trash_ids(paths, started);
                Ok(existing.len())
            } else {
                // 撤消删除 = 从回收站还原。优先用删除时记下的唯一 id（同名条目
                // 各有自己的 id）；老记录没有 id，退回原始路径。
                let keys: Vec<PathBuf> = if ids.len() == paths.len() {
                    paths
                        .iter()
                        .zip(ids.iter())
                        .map(|(path, id)| id.clone().unwrap_or_else(|| path.clone()))
                        .collect()
                } else {
                    paths.clone()
                };
                let report = crate::fs::drives::restore_recycle_entries(&keys);
                if report.restored.is_empty() {
                    // 用户在进度框里点了「取消」：什么都没动，记录必须留着，
                    // 否则一次取消就把这条撤消历史扔了（以前就是这样丢的）。
                    if report.cancelled {
                        return Err(CANCELLED.into());
                    }
                    // 分清「回收站里没有了」和「还原时被跳过」：前者是真的没了，
                    // 后者还能再试一次（在冲突框里选「替换」）。
                    if report.missing.len() >= keys.len() {
                        return Err("回收站里已经找不到这些项目（可能已被彻底删除）".into());
                    }
                    return Err("没有还原（原位置有同名项目时可以在提示框里选「替换」）".into());
                }
                Ok(report.restored.len())
            }
        }
        Kind::Create { dirs } => {
            if redo {
                for dir in dirs.iter_mut() {
                    // `create_dir_all` 对已存在的目录是「成功什么都不做」：
                    // 直接放行会把用户后来自己建的目录认成我们建的，下一次撤消
                    // 就把它删掉。这里必须先确认位置是空的。
                    if dir.path.exists() {
                        return Err(format!("「{}」已经存在了", dir.path.display()));
                    }
                    if dir.is_file {
                        std::fs::File::create(&dir.path)
                            .map_err(|e| format!("{}：{e}", dir.path.display()))?;
                    } else {
                        std::fs::create_dir_all(&dir.path)
                            .map_err(|e| format!("{}：{e}", dir.path.display()))?;
                    }
                    // 重建出来的是**另一个**对象：身份要重新采一遍，否则下一次
                    // 撤消会拿旧身份去核对，永远核对不上。
                    dir.id = crate::ops::undo::capture_dir_id(&dir.path);
                }
                Ok(dirs.len())
            } else {
                // 逐个核对身份再删：
                // 1. 已经不是当初那个对象（用户删了又建了同名的）→ 不删；
                // 2. 里面已经有东西了（用户往里放了文件）→ 不删。
                // 宁可少撤一条并说明原因，也不能把用户后来的东西删掉。
                let mut removable: Vec<PathBuf> = Vec::new();
                let mut refused_replaced = 0usize;
                let mut refused_dir_content = 0usize;
                let mut refused_file_content = 0usize;
                let mut refused_unreadable = 0usize;
                for dir in dirs.iter() {
                    match std::fs::symlink_metadata(&dir.path) {
                        // 本来就没了：不需要做什么，也不算失败。
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                        // 读不到 ≠ 不在（没权限 / 被独占 / 盘掉线）：如实报失败，
                        // 不能当成「已撤消」蒙混过去。
                        Err(_) => {
                            refused_unreadable += 1;
                            crate::logv!("[undo] 读不到新建项，跳过：{:?}", dir.path);
                            continue;
                        }
                        Ok(meta) => {
                            let now = crate::ops::undo::capture_dir_id(&dir.path);
                            let same = match (dir.id, now) {
                                (Some(expected), Some(actual)) => expected == actual,
                                // 采集不到身份（FAT / 网络路径）时退回「名字还在」，
                                // 但下面「必须为空」这条照样守着，不会误删有内容的目录。
                                (None, _) => true,
                                (Some(_), None) => false,
                            };
                            if !same {
                                refused_replaced += 1;
                                crate::logv!(
                                    "[undo] 新建的项已被换成别的对象，跳过：{:?}",
                                    dir.path
                                );
                                continue;
                            }
                            if dir.is_file {
                                // 我们建的是一个 0 字节的空文件：用户往里面写了
                                // 东西就不是「新建」那个对象了，不能删。
                                if meta.is_dir() {
                                    refused_replaced += 1;
                                    crate::logv!(
                                        "[undo] 原位置已经变成文件夹，跳过：{:?}",
                                        dir.path
                                    );
                                    continue;
                                }
                                if meta.len() != 0 {
                                    refused_file_content += 1;
                                    crate::logv!(
                                        "[undo] 新建的文件里已经有内容，跳过：{:?}",
                                        dir.path
                                    );
                                    continue;
                                }
                            } else {
                                if !meta.is_dir() {
                                    refused_replaced += 1;
                                    crate::logv!(
                                        "[undo] 原位置已经变成文件，跳过：{:?}",
                                        dir.path
                                    );
                                    continue;
                                }
                                let empty = std::fs::read_dir(&dir.path)
                                    .map(|mut it| it.next().is_none())
                                    .unwrap_or(false);
                                if !empty {
                                    refused_dir_content += 1;
                                    crate::logv!(
                                        "[undo] 文件夹里已经有内容，跳过：{:?}",
                                        dir.path
                                    );
                                    continue;
                                }
                            }
                            removable.push(dir.path.clone());
                        }
                    }
                }
                if removable.is_empty() {
                    if refused_replaced == 0
                        && refused_dir_content == 0
                        && refused_file_content == 0
                        && refused_unreadable == 0
                    {
                        // 已经被用户自己删掉了：撤消的目标已经达成。
                        return Ok(0);
                    }
                    return Err(if refused_unreadable > 0 {
                        "读不到新建项（可能没有权限或被占用），没有删除".into()
                    } else if refused_file_content > 0 {
                        "新建的文件里已经有内容，没有删除".into()
                    } else if refused_dir_content > 0 {
                        "文件夹里已经有内容，没有删除".into()
                    } else {
                        "这个新建项已经不是当初新建的那个了，没有删除".into()
                    });
                }
                if crate::sys::shell::shell_delete(&removable, true, 0)? {
                    return Err(CANCELLED.into());
                }
                Ok(removable.len())
            }
        }
        Kind::Rename { pairs } => {
            // `std::fs::rename` 在 Windows 上是 MoveFileEx(REPLACE_EXISTING)：
            // 目标已存在会被**直接覆盖**，所以每一步都得先确认目标名空着。
            //
            // 但光靠「顺序 + 目标名空着」不够：批量重命名经常出现 a→b、b→c
            // 这种链（编号顺移），撤消第一步「c→b」时 b 还在 —— 它自己也要
            // 改回 a。照着判「名字被占用」会把整批撤消打死，所以分三段：
            //   1. 源已经不在的跳过（上次撤到一半、或用户自己挪走了）；
            //   2. 目标名被**外部**文件占着（不是本批任何一步的源）→ 直接报错，
            //      一个都不改；
            //   3. 反复挑「目标名空闲」的改；整圈都互相占着就借临时名解环。
            // 撤消的一步是「从新名字改回旧名字」，顺序仍按记录顺序：
            // 链式改名 1→2、2→3 撤起来就是 2→1、3→2，前一步腾出的名字正好给
            // 后一步用。（把顺序也倒过来反而会先撞上还没腾出来的名字。）
            let mut steps: Vec<(PathBuf, PathBuf)> = if redo {
                pairs.iter().cloned().collect()
            } else {
                pairs
                    .iter()
                    .map(|(old, new)| (new.clone(), old.clone()))
                    .collect()
            };
            steps.retain(|(from, _)| from.exists());
            if steps.is_empty() {
                // 每一条都已经在目标位置上了：撤消的目的已经达成。
                return Ok(pairs.len());
            }
            let sources: Vec<PathBuf> = steps.iter().map(|(from, _)| from.clone()).collect();
            for (_, to) in steps.iter() {
                if to.exists() && !sources.iter().any(|s| s == to) {
                    return Err(format!("「{}」已经被占用了，没有改名回去", to.display()));
                }
            }
            let mut done = 0usize;
            let mut nth = 0usize;
            while !steps.is_empty() {
                if let Some(index) = steps.iter().position(|(_, to)| !to.exists()) {
                    let (from, to) = steps.remove(index);
                    std::fs::rename(&from, &to).map_err(|e| format!("{}：{e}", to.display()))?;
                    done += 1;
                    continue;
                }
                // 剩下的互相占着（a→b、b→a 这种环）：先挪到临时名。
                let (from, to) = steps.remove(0);
                let Some(parent) = from.parent() else {
                    return Err(format!("{}：上级目录不存在", from.display()));
                };
                let temp = loop {
                    let candidate =
                        parent.join(format!(".rove-undo-{}-{nth}.tmp", std::process::id()));
                    nth += 1;
                    if !candidate.exists() {
                        break candidate;
                    }
                };
                std::fs::rename(&from, &temp).map_err(|e| format!("{}：{e}", temp.display()))?;
                steps.push((temp, to));
            }
            Ok(done.max(1))
        }
        Kind::Move { pairs } => {
            // 撤消：把条目从「现在的位置」搬回原目录；重做：再搬回去。
            let mut groups: Vec<(PathBuf, Vec<PathBuf>)> = Vec::new();
            for (original, current) in pairs.iter() {
                let (from, dir) = if redo {
                    (original, current.parent())
                } else {
                    (current, original.parent())
                };
                let Some(dir) = dir else { continue };
                match groups.iter_mut().find(|(d, _)| d == dir) {
                    Some((_, items)) => items.push(from.clone()),
                    None => groups.push((dir.to_path_buf(), vec![from.clone()])),
                }
            }
            let mut moved = 0usize;
            for (dir, items) in groups {
                crate::sys::shell::shell_move(&items, &dir, 0)?;
                moved += items.len();
            }
            Ok(moved)
        }
        Kind::Copy {
            sources,
            dest,
            created,
        } => {
            if redo {
                let before = name_set(dest);
                if crate::sys::shell::shell_copy(sources, dest, 0)? {
                    return Err(CANCELLED.into());
                }
                match before {
                    Some(before) => *created = new_entries(dest, &before),
                    // 列举失败（目标目录突然不可读）就没法知道这次落了哪些条目：
                    // 清空比留着上一轮那批旧路径安全（旧路径现在可能属于别人）。
                    None => created.clear(),
                }
                Ok(created.len())
            } else {
                if created.is_empty() {
                    return Err("没有可删除的副本".into());
                }
                if crate::sys::shell::shell_delete(created, true, 0)? {
                    return Err(CANCELLED.into());
                }
                Ok(created.len())
            }
        }
    }
}

fn history_verb(redo: bool, en: bool) -> String {
    if redo {
        t(en, "重做", "Redo")
    } else {
        t(en, "撤消", "Undo")
    }
}

/// 撤消 / 重做在状态栏上的文案。
fn history_message(entry: &crate::ops::undo::Kind, redo: bool, count: usize, en: bool) -> String {
    use crate::ops::undo::Kind;
    let n = count.to_string();
    match (entry, redo) {
        (Kind::Trash { .. }, false) => tf(
            en,
            "已撤消删除：{} 项已还原到原位置",
            "Undid delete: {} items restored",
            &[n],
        ),
        (Kind::Trash { .. }, true) => tf(
            en,
            "已重做删除：{} 项已放回回收站",
            "Redid delete: {} items back in the Recycle Bin",
            &[n],
        ),
        (Kind::Create { .. }, false) => t(en, "已撤消新建", "Undid new folder").to_string(),
        (Kind::Create { .. }, true) => t(en, "已重新新建", "Recreated folder").to_string(),
        (Kind::Rename { .. }, false) => t(en, "已撤消重命名", "Undid rename").to_string(),
        (Kind::Rename { .. }, true) => t(en, "已重做重命名", "Redid rename").to_string(),
        (Kind::Move { .. }, false) => tf(
            en,
            "已撤消移动：{} 项已移回原处",
            "Undid move: {} items moved back",
            &[n],
        ),
        (Kind::Move { .. }, true) => tf(en, "已重做移动：{} 项", "Redid move: {} items", &[n]),
        (Kind::Copy { .. }, false) => tf(
            en,
            "已撤消复制：删除了 {} 个副本",
            "Undid copy: removed {} copies",
            &[n],
        ),
        (Kind::Copy { .. }, true) => tf(en, "已重做复制：{} 项", "Redid copy: {} items", &[n]),
    }
}

/// 还原结果的统一文案。
fn restore_message(report: &crate::fs::drives::RestoreReport, en: bool) -> String {
    let n = report.restored.len();
    let left = report.left.len() + report.missing.len();
    if n == 0 && report.cancelled {
        return t(en, "已取消还原", "Restore cancelled");
    }
    if n == 0 {
        if report.left.is_empty() && !report.missing.is_empty() {
            // 回收站里根本没有这些条目：是被彻底删掉了，不是还原失败。
            return t(
                en,
                "回收站里已经找不到这些项目（可能已被彻底删除）",
                "Those items are no longer in the Recycle Bin (already deleted)",
            );
        }
        // 条目还在回收站里：多半是在「替换 / 跳过」里点了跳过，还能再试。
        return t(
            en,
            "没有还原（原位置有同名项目时，可以在提示框里选「替换」）",
            "Nothing was restored (choose Replace in the prompt if a file already exists)",
        );
    }
    if left > 0 {
        return tf(
            en,
            "已还原 {} 项，{} 项仍在回收站",
            "Restored {} items, {} still in the Recycle Bin",
            &[n.to_string(), left.to_string()],
        );
    }
    tf(
        en,
        "已还原 {} 项到原位置",
        "Restored {} items to their original location",
        &[n.to_string()],
    )
}

/// 「还原到原位置」任务。
fn run_restore_recycle(keys: &[PathBuf], en: bool) -> JobSummary {
    let report = crate::fs::drives::restore_recycle_entries(keys);
    let mut affected: Vec<PathBuf> = Vec::new();
    for path in &report.restored {
        if let Some(parent) = path.parent() {
            if !affected.iter().any(|d| d == parent) {
                affected.push(parent.to_path_buf());
            }
        }
    }
    JobSummary {
        message: restore_message(&report, en),
        affected_dirs: affected,
        failed: report.left.len() + report.missing.len(),
        outcome: JobOutcome::Other,
        cancelled: report.cancelled,
        ..Default::default()
    }
}

/// 「回收站剪切 → 粘贴到普通目录」任务：先还原，再移动。
fn run_restore_move(
    keys: &[PathBuf],
    dest_dir: &Path,
    status: &Arc<Mutex<JobStatus>>,
    cancel: &Arc<AtomicBool>,
    en: bool,
) -> JobSummary {
    let report = crate::fs::drives::restore_recycle_entries(keys);
    if report.restored.is_empty() {
        return JobSummary {
            message: restore_message(&report, en),
            failed: keys.len().max(1),
            outcome: JobOutcome::Other,
            cancelled: report.cancelled,
            ..Default::default()
        };
    }
    // 已经落回目标目录里的不用再动。
    let to_move: Vec<PathBuf> = report
        .restored
        .iter()
        .filter(|path| path.parent() != Some(dest_dir))
        .cloned()
        .collect();
    if to_move.is_empty() {
        return JobSummary {
            message: tf(
                en,
                "已从回收站还原 {} 个项目到当前目录",
                "Restored {} items from the Recycle Bin",
                &[report.restored.len().to_string()],
            ),
            affected_dirs: vec![dest_dir.to_path_buf()],
            outcome: JobOutcome::Moved,
            select: report.restored.clone(),
            ..Default::default()
        };
    }
    let mut moved = run_transfer(&to_move, dest_dir, true, status, cancel, en);
    // 落地的路径只有任务里才知道：交给 UI 去选中 / 滚进视野。
    if let Some(crate::ops::undo::Kind::Move { pairs }) = &moved.recorded {
        moved.select = pairs.iter().map(|(_, new)| new.clone()).collect();
    }
    JobSummary {
        message: format!("{}；{}", restore_message(&report, en), moved.message),
        ..moved
    }
}

/// 撤消 / 重做任务本体（跑在任务线程上，可以随便阻塞）。
fn run_history(entry: &crate::ops::undo::Kind, redo: bool, en: bool) -> JobSummary {
    let mut entry = entry.clone();
    let result = apply_history(&mut entry, redo);
    let affected = entry.affected_dirs();
    let count = match &result {
        Ok(count) => *count,
        // 失败时至少要刷一遍相关目录：可能已经改了一部分。
        Err(_) => 0,
    };
    let (message, failed, cancelled) = match result {
        Ok(_) => (history_message(&entry, redo, count, en), 0, false),
        Err(e) => (
            format!("{}：{}", history_verb(redo, en), e),
            1,
            e == CANCELLED,
        ),
    };
    crate::logv!("[undo] redo={} failed={} {}", redo, failed, message);
    JobSummary {
        message,
        affected_dirs: affected,
        failed,
        outcome: JobOutcome::Other,
        recorded: None,
        history: Some((redo, entry)),
        cancelled,
        ..Default::default()
    }
}

/// 目标已存在时生成 "名称 - 副本.ext" / "名称 - 副本 (2).ext"。
/// 仅非 Windows 的逐文件回退实现使用（Windows 走原生 Shell 复制/移动）。
#[cfg(not(windows))]
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
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
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
pub fn status_text(status: &JobStatus, en: bool) -> String {
    if !status.running {
        return String::new();
    }
    if en {
        format!(
            "Processing {}/{}: {}",
            status.done, status.total, status.current
        )
    } else {
        format!(
            "正在处理 {}/{}：{}",
            status.done, status.total, status.current
        )
    }
}

/// 测试辅助。
#[cfg(test)]
mod tests {
    use super::*;

    /// 还原失败的原因必须分开说：**回收站里没有了**（被彻底删掉）和
    /// **还原时被跳过**（原位置有同名文件）是两回事，前者再试也没用，
    /// 后者还能在冲突框里选「替换」重来。
    #[test]
    fn restore_message_separates_missing_from_skipped() {
        let mut missing = crate::fs::drives::RestoreReport::default();
        missing.missing.push(PathBuf::from("C:\\x\\a.txt"));
        let msg = restore_message(&missing, false);
        assert!(msg.contains("已经找不到"), "{msg}");

        let mut skipped = crate::fs::drives::RestoreReport::default();
        skipped.left.push(PathBuf::from("C:\\x\\a.txt"));
        let msg = restore_message(&skipped, false);
        assert!(msg.contains("替换"), "{msg}");
        assert!(!msg.contains("已经找不到"), "{msg}");

        let mut partial = crate::fs::drives::RestoreReport::default();
        partial.restored.push(PathBuf::from("C:\\x\\a.txt"));
        partial.left.push(PathBuf::from("C:\\x\\b.txt"));
        let msg = restore_message(&partial, false);
        assert!(msg.starts_with("已还原 1 项"), "{msg}");
        assert!(msg.contains("1 项仍在回收站"), "{msg}");

        let mut all = crate::fs::drives::RestoreReport::default();
        all.restored.push(PathBuf::from("C:\\x\\a.txt"));
        assert_eq!(restore_message(&all, false), "已还原 1 项到原位置");

        // 用户在还原进度框里点了取消：不能报成「同名项目 / 选替换」。
        let cancelled = crate::fs::drives::RestoreReport {
            cancelled: true,
            ..Default::default()
        };
        assert_eq!(restore_message(&cancelled, false), "已取消还原");
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rove-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 批量重命名的链（1→2、2→3）撤消时必须按能走得通的顺序改名：
    /// 倒着来第一步「3→2」会撞上还在原位的 2，按旧逻辑整批撤消永远失败。
    #[test]
    fn rename_chain_undoes_in_a_workable_order() {
        let dir = scratch("undo-chain");
        std::fs::write(dir.join("2.txt"), b"one").unwrap();
        std::fs::write(dir.join("3.txt"), b"two").unwrap();
        let mut entry = crate::ops::undo::Kind::Rename {
            pairs: vec![(dir.join("1.txt"), dir.join("2.txt")), (dir.join("2.txt"), dir.join("3.txt"))],
        };
        let done = apply_history(&mut entry, false).expect("链式改名必须能整体撤消");
        assert_eq!(done, 2);
        assert_eq!(std::fs::read(dir.join("1.txt")).unwrap(), b"one");
        assert_eq!(std::fs::read(dir.join("2.txt")).unwrap(), b"two");
        assert!(!dir.join("3.txt").exists());
        // 重做回去也一样要走得通。
        apply_history(&mut entry, true).expect("重做同样要能走通");
        assert_eq!(std::fs::read(dir.join("2.txt")).unwrap(), b"one");
        assert_eq!(std::fs::read(dir.join("3.txt")).unwrap(), b"two");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 互换名字（a↔b）是个环：靠临时名解环，不能互相覆盖。
    #[test]
    fn rename_swap_undoes_without_losing_content() {
        let dir = scratch("undo-swap");
        std::fs::write(dir.join("b.txt"), b"from-a").unwrap();
        std::fs::write(dir.join("a.txt"), b"from-b").unwrap();
        let mut entry = crate::ops::undo::Kind::Rename {
            pairs: vec![(dir.join("a.txt"), dir.join("b.txt")), (dir.join("b.txt"), dir.join("a.txt"))],
        };
        apply_history(&mut entry, false).expect("互换改名要能撤消");
        // 换回来：a.txt 拿回原来属于 a 的内容，b.txt 拿回原来属于 b 的。
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"from-a");
        assert_eq!(std::fs::read(dir.join("b.txt")).unwrap(), b"from-b");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 目标名被**外部**文件占着时一个都不能改：那是用户后来自己建的文件，
    /// 覆盖掉就是删别人的数据。
    #[test]
    fn rename_undo_refuses_when_the_old_name_is_taken_by_an_outsider() {
        let dir = scratch("undo-taken");
        std::fs::write(dir.join("new.txt"), b"ours").unwrap();
        std::fs::write(dir.join("old.txt"), b"theirs").unwrap();
        let mut entry = crate::ops::undo::Kind::Rename {
            pairs: vec![(dir.join("old.txt"), dir.join("new.txt"))],
        };
        assert!(apply_history(&mut entry, false).is_err());
        assert_eq!(std::fs::read(dir.join("old.txt")).unwrap(), b"theirs");
        assert_eq!(std::fs::read(dir.join("new.txt")).unwrap(), b"ours");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(not(windows))]
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
