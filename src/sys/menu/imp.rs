//! Windows 实现：**父进程侧**的原生菜单调度。
//!
//! 菜单本身在子进程里构建和显示（见 `child.rs`），这里只管四件事：
//!   1. 把「要弹什么」送过去（`--menu-host` 子进程 + stdin 里的 bincode 请求）；
//!   2. 把「用户点了哪一项」收回来（子进程 stdout 上一行一条事件）；
//!   3. 看门狗：门槛（2.5 秒起，按实测耗时自动放宽）还没弹出来就**杀掉子进程**、
//!      改弹只含 Rove 自己项的菜单；
//!   4. 连着卡两次才记住「本次运行不要再请系统菜单了」（一次卡只降级这一下）。
//!
//! 为什么非要子进程：第三方外壳扩展（网盘 / 同步 / 杀毒）会在
//! `QueryContextMenu` 或子菜单展开里永远不返回。线程杀不掉，只能丢弃，丢着
//! 丢着进程就废了（新线程一行代码都跑不到，右键彻底没反应）。进程能杀干净，
//! 主程序也不会被扩展代码污染。
//!
//! id 约定（与 Shell 一致）：`1..=0x6FFF` 归 Shell 命令，`0x7000+` 归应用命令。

use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::wire;
use super::{dark, MenuAction, MenuNode, Options, Sink, Target};

/// `CMINVOKECOMMANDINFOEX` 的 `fMask` 位：`lpVerbW` 才是宽字符动词。
const CMIC_MASK_UNICODE: u32 = 0x4000;

use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;

/// 菜单要跟随深色（由 UI 线程按当前主题写入）。
static DARK: AtomicBool = AtomicBool::new(false);
/// `tray-icon` 那个隐藏窗口的句柄（类名固定 `tray_icon_app`）。托盘菜单的配色
/// 只认它自己的窗口，缓存下来免得每次 `FindWindowW`。
static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);
/// 是否已经有一张菜单在等（用户正开着，或者子进程卡在构建里）。
///
/// 第二轮右键不能排队：否则用户点外面关掉菜单后，排着的那份会迟一步跳出来。
static BUSY: AtomicBool = AtomicBool::new(false);
/// 这一轮是从什么时候开始的（毫秒，进程启动起算）。看门狗用它算门槛。
static BUSY_SINCE_MS: AtomicU64 = AtomicU64::new(0);
/// 子进程是否已经回报「菜单弹到屏幕上了」。
///
/// 只有**弹出之前**才计时：门槛内没弹出来就是第三方扩展卡住了，杀掉换精简菜单；
/// 已经弹出来的菜单只等用户（但卡死不动时另有 `IsHungAppWindow` 兜着）。
static SHOWN: AtomicBool = AtomicBool::new(false);
/// 本次运行系统菜单已经拖死够多次：从此只弹 Rove 自己的项。
///
/// 进程级、只在内存里，Rove 退出即清零 —— 下次启动重新试一次系统菜单。
/// 不做任何跨启动的记忆（那是自锁，用户明确否决过）。
static SHELL_DEAD: AtomicBool = AtomicBool::new(false);
/// 连着卡了几次（成功弹出来一次就清零）。
///
/// 一次卡就判死太狠了：真机日志里 `O:\Video` 两分钟前还好好的，转头就超过
/// 1 秒 —— 网盘/网络盘的扩展在 `QueryContextMenu` 里做网络 I/O，本来就时快
/// 时慢。所以给两次机会。
static SHELL_STRIKES: AtomicU32 = AtomicU32::new(0);
/// 连续卡这么多次，本次运行就不再请系统菜单了。
const MAX_SHELL_STRIKES: u32 = 2;
/// 菜单弹出耗时的平滑值（毫秒；0 = 还没有样本）。
///
/// 光给固定门槛不够：慢机器 / 慢扩展本来就要更久。这里按实测的正常弹出耗时
/// 放宽门槛（3 倍余量），快机器上永远不会被放宽。
static BUILD_EMA_MS: AtomicU64 = AtomicU64::new(0);
/// 最近一次「菜单有动静」的时刻（毫秒）。给外部的抖动取证用：原生菜单本来
/// 就要把前台交给菜单进程、关掉再还回来，这段窗口里的失焦/获焦不是窗口在抖。
static LAST_ACTIVE_MS: AtomicU64 = AtomicU64::new(0);
/// 菜单请求的代数：老的读线程回来时不许动新请求的状态。
static MENU_GEN: AtomicU64 = AtomicU64::new(1);
/// 菜单要多久还没弹出来就算卡住（下限）。
///
/// 真机日志：`O:` 盘上的系统菜单同一目标一会儿能用一会儿超过 1 秒，属于
/// 网盘扩展在 `QueryContextMenu` 里做网络 I/O。1 秒太紧，2.5 秒才够它读完。
const MENU_SHOW_TIMEOUT_MS: u64 = 2_500;
/// 自动放宽的上限：再慢也不让用户干等超过这个数。
const MENU_SHOW_MAX_MS: u64 = 6_000;
/// 子进程连「我起来了」都没报的硬上限：正常几十毫秒，超过这就是它自己
/// 起不来（被拦截 / 磁盘卡死），跟外壳扩展无关。
const MENU_START_TIMEOUT_MS: u64 = 5_000;

/// 正在跑的那张菜单。
struct Active {
    gen: u64,
    child: Child,
    /// 子进程报 `ready` 的时刻（`MENU_SHOW_TIMEOUT_MS` 从这里算）。
    started_ms: u64,
    /// 派出去的时刻：子进程一直不报 `ready` 时用这条硬上限。
    spawned_ms: u64,
    shown: bool,
    /// 这次请求本来就是「只含 Rove 自己项」（没有系统项可退）。
    app_only: bool,
    /// 已经为这张菜单退过一次精简版：不许再退第二次。
    retried: bool,
    sink: Sink,
    /// 精简版要用的菜单项（由 UI 线程一并算好，退的时候不用再回头问它）。
    fallback: Vec<MenuNode>,
    hide_keywords: Vec<String>,
    extended: bool,
    owner: isize,
}

static ACTIVE: Mutex<Option<Active>> = Mutex::new(None);

/// 菜单已经关掉、但子进程还在跑的子进程。
///
/// 「发送到 / 复制到 / 删除到回收站」这些是 Shell 在**菜单进程里**起的后台
/// 任务：菜单一关就把进程杀掉，等于把用户的复制砍断。所以关掉之后不杀，
/// 交给它自己收工（见 `child.rs` 的 `linger_for_shell_work`）。
static LINGER: Mutex<Vec<Child>> = Mutex::new(Vec::new());
/// 同时最多留几个干活的子进程（正常只有一个；留点余量防手贱连点）。
const MAX_LINGER: usize = 4;

/// 记一次「菜单有动静」（派出去 / 起来了 / 弹出来 / 关掉了）。
fn touch() {
    LAST_ACTIVE_MS.store(now_ms(), Ordering::Release);
}

/// 最近 `ms` 毫秒内菜单有没有动静。
///
/// 焦点抖动取证用：`WindowEvent::Focused` 在菜单抢前台 / 还前台的瞬间必然
/// 成对出现，那是用户在右键，不是窗口在抖。
pub(crate) fn recently_active(ms: u64) -> bool {
    let last = LAST_ACTIVE_MS.load(Ordering::Acquire);
    last != 0 && now_ms().saturating_sub(last) <= ms
}

/// 进程启动起的毫秒数。
fn now_ms() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

/// 弹出原生菜单。
///
/// `nodes` 是带系统项时该显示的应用项，`fallback` 是「只含 Rove 自己项」时
/// 该显示的应用项 —— 一旦卡死要立刻退到后者，不能再回头问 UI 线程。
///
/// 返回 false 表示子进程根本没起来（真正的「菜单不可用」）。
pub(crate) fn open(
    target: Target,
    nodes: Vec<MenuNode>,
    fallback: Vec<MenuNode>,
    options: Options,
    owner: isize,
    sink: Sink,
) -> bool {
    if BUSY.swap(true, Ordering::AcqRel) {
        // 上一张还在：要么用户正开着，要么子进程卡在构建里（那由 `tick()` 收拾）。
        crate::logv!("[menu] 上一张菜单还没结束，忽略这次右键");
        return true;
    }
    BUSY_SINCE_MS.store(now_ms(), Ordering::Release);
    touch();
    SHOWN.store(false, Ordering::Release);
    // 系统菜单已经拖死过：本次运行只弹 Rove 自己的项。
    let (target, nodes) = if SHELL_DEAD.load(Ordering::Acquire)
        && !matches!(target, Target::AppOnly)
    {
        crate::logv!("[menu] 本次运行已判死系统菜单，直接弹 Rove 自己的项");
        (Target::AppOnly, fallback.clone())
    } else {
        (target, nodes)
    };
    let app_only = matches!(target, Target::AppOnly);
    if start(target, nodes, &options, owner, sink, fallback, app_only, false) {
        return true;
    }
    BUSY.store(false, Ordering::Release);
    false
}

/// 派一个菜单子进程（请求由读线程去写，见 `reader`）。
///
/// 写请求**不能**放在 UI 线程：选中的文件多到几万个时，请求会有几百 KB，
/// 超过管道缓冲区，`write_all` 就要等子进程消费 —— 那是个能卡住 UI 线程的
/// 阻塞点。交给 `rove-menu-ipc` 线程去做，UI 线程只负责 `CreateProcess`。
fn launch() -> std::io::Result<(Child, ChildStdin, ChildStdout)> {
    let exe = std::env::current_exe()?;
    let mut child = std::process::Command::new(exe)
        .arg("--menu-host")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    match (stdin, stdout) {
        (Some(stdin), Some(stdout)) => Ok((child, stdin, stdout)),
        _ => {
            let _ = child.kill();
            Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "菜单子进程的管道没建起来",
            ))
        }
    }
}

/// 启动一次菜单请求并接管它的输出。返回 false = 子进程没起来。
#[allow(clippy::too_many_arguments)]
fn start(
    target: Target,
    nodes: Vec<MenuNode>,
    options: &Options,
    owner: isize,
    sink: Sink,
    fallback: Vec<MenuNode>,
    app_only: bool,
    retried: bool,
) -> bool {
    let request = wire::Request {
        target,
        nodes,
        hide_keywords: options.hide_keywords.clone(),
        extended: options.extended,
        dark: DARK.load(Ordering::Acquire),
        owner,
    };
    let payload = bincode::serialize(&request).unwrap_or_default();
    let gen = MENU_GEN.fetch_add(1, Ordering::AcqRel) + 1;
    let (child, stdin, stdout) = match launch() {
        Ok(parts) => parts,
        Err(e) => {
            crate::logal!("!! [menu] 菜单子进程启动失败: {e}");
            return false;
        }
    };
    // 前台锁只认「父进程放行」：不放行的话子进程抢不到前台，菜单就收不到键盘。
    let child_pid = child.id();
    if let Err(e) = unsafe { AllowSetForegroundWindow(child_pid) } {
        crate::logv!("[menu] AllowSetForegroundWindow({child_pid}) 失败 {:?}", e.code());
    }
    let now = now_ms();
    *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Active {
        gen,
        started_ms: now,
        spawned_ms: now,
        child,
        shown: false,
        app_only,
        retried,
        sink: sink.clone(),
        fallback,
        hide_keywords: request.hide_keywords,
        extended: request.extended,
        owner,
    });
    let spawned = std::thread::Builder::new()
        .name("rove-menu-ipc".into())
        .spawn(move || {
            // 请求写给子进程，写完就关（drop）：它读到 EOF 才知道请求结束了。
            {
                use std::io::Write;
                let mut stdin = stdin;
                let _ = stdin.write_all(&payload);
            }
            reader(stdout, gen, sink);
        });
    if spawned.is_err() {
        crate::logal!("!! [menu] 菜单读线程创建失败");
        let active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(mut active) = active {
            let _ = active.child.kill();
        }
        return false;
    }
    true
}

/// 读子进程的事件行，翻译成 UI 线程认识的动作。
fn reader(stdout: ChildStdout, gen: u64, sink: Sink) {
    use std::io::BufRead;
    let mut shown = false;
    let mut chosen = false;
    for line in std::io::BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        if line == wire::EV_READY {
            mark_ready(gen);
            continue;
        }
        if line == wire::EV_SHOWN {
            shown = true;
            SHOWN.store(true, Ordering::Release);
            mark_shown(gen);
            continue;
        }
        let ends = ends_menu(&line);
        chosen |= ends;
        if ends {
            // 菜单已经有结果：立刻放行，下一次右键马上能用。
            finish_menu(gen);
        }
        if let Some(action) = parse_event(&line) {
            sink(action);
        }
        // 这里**不**补发 `Closed`：子进程关菜单时自己会把前台还给 Rove
        // （只在它确实还持有时才还），足够让键盘回到主窗口。反过来由父进程
        // 无条件抢前台，会把「用某某打开 / 在终端中打开」刚拉起来的窗口按下去。
        // 只有「菜单被我们杀掉、没机会还前台」那几条路径才补 `Closed`。
    }
    // 子进程结束了。被看门狗接管的（代数对不上）这里一律不碰。
    let pending = {
        let mut guard = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(active) if active.gen == gen => guard.take(),
            _ => None,
        }
    };
    let Some(mut active) = pending else {
        // 菜单早就结束了（子进程在跑 Shell 的后台任务）：这里只是等它收工。
        crate::logv!("[menu] 子进程结束");
        return;
    };
    let _ = active.child.wait();
    if !shown {
        // 还没弹出来就没了：构建阶段崩了（扩展写坏了内存之类）。
        crate::logal!("!! [menu] 菜单子进程没弹出来就退出了，这次改用 Rove 自己的菜单项");
        note_shell_stall();
        retry_app_only(active);
        return;
    }
    crate::logv!("[menu] 菜单结束 chosen={chosen}");
    SHOWN.store(false, Ordering::Release);
    BUSY.store(false, Ordering::Release);
    sink(MenuAction::Closed);
}

/// 子进程的一行事件 → 要回投给 UI 线程的动作。
fn parse_event(line: &str) -> Option<MenuAction> {
    if let Some(key) = line.strip_prefix(wire::EV_APP) {
        return Some(MenuAction::App(key.to_string()));
    }
    if let Some(path) = line.strip_prefix(wire::EV_NAVIGATE) {
        return Some(MenuAction::Navigate(wire::unescape_path(path)));
    }
    if line == wire::EV_RENAME {
        return Some(MenuAction::Rename);
    }
    if line == wire::EV_BEGIN {
        return Some(MenuAction::ShellOpBegin);
    }
    if line == wire::EV_END {
        return Some(MenuAction::ShellOpEnd);
    }
    if line == wire::EV_REFRESH {
        return Some(MenuAction::Refresh);
    }
    None
}

/// 这一行是不是「菜单已经关掉了」（之后只剩 Shell 任务的收尾事件）。
fn ends_menu(line: &str) -> bool {
    line == wire::EV_DISMISSED
        || line == wire::EV_RENAME
        || line == wire::EV_BEGIN
        || line.starts_with(wire::EV_APP)
        || line.starts_with(wire::EV_NAVIGATE)
}

/// 菜单有结果了：立刻放行（下一次右键马上能用），子进程转去「可能还在干活」的名单。
fn finish_menu(gen: u64) {
    let taken = {
        let mut guard = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(active) if active.gen == gen => guard.take(),
            _ => None,
        }
    };
    let Some(active) = taken else { return };
    touch();
    SHOWN.store(false, Ordering::Release);
    BUSY.store(false, Ordering::Release);
    let mut linger = LINGER.lock().unwrap_or_else(|e| e.into_inner());
    linger.push(active.child);
    while linger.len() > MAX_LINGER {
        if let Some(oldest) = linger.first_mut() {
            let _ = oldest.kill();
        }
        linger.remove(0);
    }
}

/// 这一轮实际允许等多久：固定下限 + 本机实测放宽。
fn show_timeout_ms() -> u64 {
    let ema = BUILD_EMA_MS.load(Ordering::Acquire);
    // 实测平均的 3 倍留余量；没样本时就是下限本身。
    let relaxed = ema.saturating_mul(3);
    MENU_SHOW_TIMEOUT_MS.max(relaxed).min(MENU_SHOW_MAX_MS)
}

/// 记一次「菜单弹出耗时」样本（滑动平均，不需要精确）。
fn record_build(ms: u64) {
    let mut cur = BUILD_EMA_MS.load(Ordering::Acquire);
    loop {
        let next = if cur == 0 { ms } else { (cur * 3 + ms) / 4 };
        match BUILD_EMA_MS.compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return,
            Err(actual) => cur = actual,
        }
    }
}

/// 系统菜单又卡了一次。返回 true = 本次运行不再请系统菜单。
fn note_shell_stall() -> bool {
    let n = SHELL_STRIKES.fetch_add(1, Ordering::AcqRel) + 1;
    if n >= MAX_SHELL_STRIKES {
        SHELL_DEAD.store(true, Ordering::Release);
        true
    } else {
        crate::logv!("[menu] 系统菜单第 {n} 次卡住（连着卡 {MAX_SHELL_STRIKES} 次才判死）");
        false
    }
}

/// 子进程报「我起来了」：弹出门槛从这一刻开始算。
fn mark_ready(gen: u64) {
    let mut guard = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(active) = guard.as_mut() {
        if active.gen == gen {
            active.started_ms = now_ms();
            touch();
        }
    }
}

fn mark_shown(gen: u64) {
    let now = now_ms();
    let mut guard = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(active) = guard.as_mut() {
        if active.gen == gen {
            active.shown = true;
            touch();
            let built = now.saturating_sub(active.started_ms);
            let app_only = active.app_only;
            drop(guard);
            record_build(built);
            crate::logv!("[menu] {built}ms 弹出（现门槛 {}ms）", show_timeout_ms());
            if !app_only {
                // 系统菜单弹出来了：连击清零，下次照样请它。
                SHELL_STRIKES.store(0, Ordering::Release);
            }
        }
    }
}

/// 判死之后改弹只含 Rove 自己项的菜单（这张右键立刻就有反应）。
fn retry_app_only(active: Active) {
    let mut active = active;
    let _ = active.child.kill();
    let _ = active.child.wait();
    SHOWN.store(false, Ordering::Release);
    if active.app_only || active.retried {
        // 精简菜单也弹不出来：这次右键只能放弃，别再无限重试。
        crate::logal!("!! [menu] 精简菜单也没弹出来，本次右键放弃");
        BUSY.store(false, Ordering::Release);
        (active.sink)(MenuAction::Closed);
        return;
    }
    let options = Options {
        hide_keywords: active.hide_keywords.clone(),
        extended: active.extended,
    };
    crate::logv!("[menu] 改弹 Rove 自己的菜单项");
    if !start(
        Target::AppOnly,
        active.fallback.clone(),
        &options,
        active.owner,
        active.sink.clone(),
        active.fallback.clone(),
        true,
        true,
    ) {
        BUSY.store(false, Ordering::Release);
    }
}

/// UI 线程周期调用（40ms）：不点右键也能发现「菜单卡住没弹出来」。
pub(crate) fn tick() {
    reap_linger();
    if !BUSY.load(Ordering::Acquire) {
        return;
    }
    // 菜单已经弹出来了，但子进程的线程被扩展堵在不返回的调用里：菜单留在
    // 屏幕上、点它也点不掉，整台机器上这块区域就废了。Windows 自己会判定
    // 「5 秒没泵消息」的窗口无响应 —— 借这个判断把它收掉。
    //
    // 必须放在「已弹出就返回」**之前**：这一段只在菜单已经弹出来时才有意义。
    kill_if_hung();
    if SHOWN.load(Ordering::Acquire) {
        return;
    }

    let now = now_ms();
    let pending = {
        let mut guard = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(active) if !active.shown => {
                let built = now.saturating_sub(active.started_ms);
                let alive = now.saturating_sub(active.spawned_ms);
                if built >= show_timeout_ms() || alive >= MENU_START_TIMEOUT_MS {
                    guard.take()
                } else {
                    None
                }
            }
            _ => None,
        }
    };
    let Some(mut active) = pending else { return };
    let age = now.saturating_sub(active.started_ms);
    // 卡住的扩展在子进程里：直接杀掉（进程能杀，线程杀不掉）。
    let _ = active.child.kill();
    if active.app_only {
        crate::logal!("!! [menu] 精简菜单 {age}ms 也没弹出来（门槛 {}ms）", show_timeout_ms());
    } else if note_shell_stall() {
        crate::logal!(
            "[menu] 系统菜单 {age}ms 没弹出来（门槛 {}ms，多半是第三方外壳扩展卡死），\
             杀掉子进程；连着卡 {MAX_SHELL_STRIKES} 次，本次运行改用 Rove 自己的菜单项",
            show_timeout_ms()
        );
    } else {
        crate::logal!(
            "[menu] 系统菜单 {age}ms 没弹出来（门槛 {}ms），杀掉子进程；\
             这次只降级这一下，下次右键还试系统菜单",
            show_timeout_ms()
        );
    }
    retry_app_only(active);
}

/// 菜单弹出来之后卡死：杀掉子进程，别让一张点不动的菜单糊在屏幕上。
fn kill_if_hung() {
    let pid = {
        let guard = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(active) if active.shown => active.child.id(),
            _ => return,
        }
    };
    if !process_is_hung(pid) {
        return;
    }
    let taken = ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).take();
    let Some(mut active) = taken else { return };
    crate::logal!("!! [menu] 菜单弹出来后卡死（第三方扩展不返回），杀掉菜单子进程");
    note_shell_stall();
    let _ = active.child.kill();
    let _ = active.child.wait();
    SHOWN.store(false, Ordering::Release);
    BUSY.store(false, Ordering::Release);
    // 前台权跟着那个进程一起没了：让主窗口把焦点收回，不然键盘没人接。
    (active.sink)(MenuAction::Closed);
}

/// 这个进程里有没有「无响应」的窗口（Windows 的判定：5 秒没泵消息）。
fn process_is_hung(pid: u32) -> bool {
    use windows::Win32::Foundation::BOOL;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, IsHungAppWindow,
    };

    struct Scan {
        pid: u32,
        hung: bool,
    }
    unsafe extern "system" fn each_window(hwnd: HWND, lp: LPARAM) -> BOOL {
        let scan = unsafe { &mut *(lp.0 as *mut Scan) };
        let mut owner_pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner_pid)) };
        if owner_pid == scan.pid && unsafe { IsHungAppWindow(hwnd).as_bool() } {
            scan.hung = true;
            return BOOL(0);
        }
        BOOL(1)
    }

    let mut scan = Scan { pid, hung: false };
    unsafe { EnumWindows(Some(each_window), LPARAM(&mut scan as *mut Scan as isize)) }.ok();
    scan.hung
}

/// 收掉已经退出的「还在干活」的子进程。
fn reap_linger() {
    let mut linger = LINGER.lock().unwrap_or_else(|e| e.into_inner());
    linger.retain_mut(|child| match child.try_wait() {
        Ok(Some(_)) => false,
        _ => true,
    });
}

/// 关窗时收摊：菜单子进程不能留着（它可能正卡在扩展里）。
pub(crate) fn shutdown() {
    let active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(mut active) = active {
        let _ = active.child.kill();
        let _ = active.child.wait();
    }
    // 注意：`LINGER` 里的**不杀** —— 那里面可能有正在跑的复制 / 移动，
    // 杀掉等于把用户的操作砍断。它们收工后自己退出。
    SHOWN.store(false, Ordering::Release);
    BUSY.store(false, Ordering::Release);
}

/// 菜单挂着多久了（毫秒）。给看门狗 / 关闭流程留证据用。
pub(crate) fn busy_for_ms() -> Option<u64> {
    BUSY.load(Ordering::Acquire)
        .then(|| now_ms().saturating_sub(BUSY_SINCE_MS.load(Ordering::Acquire)))
}

/// 系统菜单这次运行还值不值得请？（拖死过一次就再也不请）
pub(crate) fn shell_unhealthy() -> bool {
    SHELL_DEAD.load(Ordering::Acquire)
}

/// 让原生菜单跟随主题（UI 线程调用）。
///
/// 应急退路：设置环境变量 `ROVE_NO_DARK_MENU=1` 后一律浅色菜单。
pub fn set_dark(dark: bool) {
    let dark = dark && std::env::var_os("ROVE_NO_DARK_MENU").is_none();
    // 第一次即使值等于默认的 `false` 也不能提前返回：进程默认是「跟随系统」，
    // 用户「Rove 亮色 + Windows 深色」时不设就一直是深色菜单。
    let first = !dark::mode_set();
    if DARK.swap(dark, Ordering::AcqRel) == dark && !first {
        return;
    }
    crate::logv!("[menu] set_dark dark={} first={}", dark, first);
    // 托盘菜单是父进程自己弹的，进程级偏好在这里设；菜单子进程每次启动
    // 都会按请求里的 `dark` 自己再设一遍（偏好不跨进程）。
    let _ = dark::set_preferred_mode(dark);
    apply_tray_theme();
}

/// 让托盘右键菜单跟随 Rove 的主题。
///
/// 托盘菜单是 `tray-icon` / `muda` 用普通 `TrackPopupMenu` 弹的，配色取决于两件事：
/// 进程级 `SetPreferredAppMode`，以及**拥有窗口**（`tray_icon_app` 隐藏窗口）的
/// 深色允许位。后者它自己从不设，所以要么继承进程状态，要么跟着 Windows 走。
pub fn apply_tray_theme() {
    let dark = DARK.load(Ordering::Acquire);
    let mut hwnd = TRAY_HWND.load(Ordering::Acquire);
    if hwnd == 0 {
        // 托盘图标还没建出来（`apply_theme` 早于 `tray::spawn`）：托管给托盘线程，
        // 它建完图标会自己调一次本函数。
        match find_own_tray_window() {
            Some(found) => {
                hwnd = found;
                TRAY_HWND.store(hwnd, Ordering::Release);
            }
            None => return,
        }
    }
    dark::apply_dark_flag(HWND(hwnd as _), dark);
}

/// 找 `tray-icon` 的隐藏窗口，**必须是本进程的**。
///
/// 类名 `tray_icon_app` 是 `tray-icon` 库写死的，同一个桌面上别的程序
/// （很多托盘程序也用这个库）可能也有同名窗口。`FindWindowW` 会返回第一个
/// 命中的，可能不是我们的 —— 那就会去给别人的窗口改主题，自己的托盘菜单
/// 反而没变。所以枚举顶层窗口，按进程号过滤。
fn find_own_tray_window() -> Option<isize> {
    use windows::Win32::Foundation::BOOL;
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetWindowThreadProcessId, IsWindowVisible,
    };

    struct Ctx {
        class: Vec<u16>,
        pid: u32,
        found: isize,
    }
    let mut class: Vec<u16> = "tray_icon_app".encode_utf16().collect();
    class.push(0);
    let mut ctx = Ctx {
        class,
        pid: unsafe { GetCurrentProcessId() },
        found: 0,
    };
    unsafe extern "system" fn each(hwnd: HWND, lp: LPARAM) -> BOOL {
        let ctx = unsafe { &mut *(lp.0 as *mut Ctx) };
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid != ctx.pid {
            return BOOL(1);
        }
        let want = ctx.class.len() - 1;
        let mut buf = [0u16; 32];
        let n = unsafe { GetClassNameW(hwnd, &mut buf) } as usize;
        if n > 0 && n == want && buf[..want] == ctx.class[..want] {
            // 托盘窗口是隐藏的：不看可见性，有句柄就够。
            let _ = unsafe { IsWindowVisible(hwnd) };
            ctx.found = hwnd.0 as isize;
            return BOOL(0);
        }
        BOOL(1)
    }
    unsafe { EnumWindows(Some(each), LPARAM(&mut ctx as *mut Ctx as isize)) }.ok();
    (ctx.found != 0).then_some(ctx.found)
}

/// 回收站里的「彻底删除」：在**回收站命名空间**上执行 Shell 的 `delete` 动词。
///
/// 为什么不能在 `C:\$Recycle.Bin\...` 的物理路径上调 `SHFileOperationW` /
/// `IFileOperation`：那样 Shell 把条目当普通文件处理，一声不响就抹掉了，
/// 资源管理器那句「确实要永久删除此文件吗？」根本不会出现。真正弹确认框的是
/// 回收站文件夹自己的删除处理器，所以要拿到 `FOLDERID_RecycleBinFolder` 的
/// `IShellFolder`，把命中的子项 PIDL 交给它取 `IContextMenu` 再触发 `delete`。
///
/// `paths` 里既可能是 `$R` 物理路径（`trash` 库给的 id），也可能是命名空间解析名。
/// 匹配先按 `$R` 文件名（每条唯一），退而按显示名（原名）。
///
/// 返回投递给 Shell 的条目数；用户点「否」时命令返回失败，所以结果仍以
/// 「条目还在不在」为准。
pub(crate) fn purge_recycle_items(paths: &[PathBuf], owner: isize) -> usize {
    use windows::core::{PCSTR, PCWSTR};
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::{
        IContextMenu, IEnumIDList, IShellFolder, IShellItem, SHCreateItemWithParent,
        SHGetKnownFolderItem, BHID_SFObject, FOLDERID_RecycleBinFolder,
        KF_FLAG_DEFAULT, SHCONTF_FOLDERS, SHCONTF_NONFOLDERS, SIGDN_DESKTOPABSOLUTEPARSING,
        SIGDN_NORMALDISPLAY,
    };
    if paths.is_empty() {
        return 0;
    }
    let folder_item: IShellItem =
        match unsafe { SHGetKnownFolderItem(&FOLDERID_RecycleBinFolder, KF_FLAG_DEFAULT, None) } {
            Ok(item) => item,
            Err(e) => {
                crate::logal!("!! menu: 打不开回收站 {:?}", e.code());
                return 0;
            }
        };
    let folder: IShellFolder = match unsafe { folder_item.BindToHandler(None, &BHID_SFObject) } {
        Ok(folder) => folder,
        Err(e) => {
            crate::logal!("!! menu: 回收站不是 IShellFolder {:?}", e.code());
            return 0;
        }
    };
    let host = HWND(owner as *mut core::ffi::c_void);
    let mut list: Option<IEnumIDList> = None;
    if unsafe { folder.EnumObjects(host, (SHCONTF_FOLDERS.0 | SHCONTF_NONFOLDERS.0) as u32, &mut list) }
        .is_err()
    {
        return 0;
    }
    let Some(list) = list else {
        return 0;
    };

    // (子 PIDL, 解析名, 显示名)
    //
    // `IEnumIDList::Next` 给的子项 PIDL 是 `CoTaskMemAlloc` 出来的，调用方必须
    // 自己还 —— 这里交给守卫，提前 return 也不会漏。
    let mut owned = ChildPidls(Vec::new());
    let mut children: Vec<(*mut ITEMIDLIST, String, String)> = Vec::new();
    loop {
        let mut buf: [*mut ITEMIDLIST; 1] = [std::ptr::null_mut()];
        let mut fetched = 0u32;
        if unsafe { list.Next(&mut buf, Some(&mut fetched)) }.is_err() || fetched == 0 {
            break;
        }
        let child = buf[0];
        if child.is_null() {
            break;
        }
        owned.0.push(child);
        let names = unsafe { SHCreateItemWithParent::<_, IShellItem>(None, &folder, child) }
            .ok()
            .and_then(|item| {
                let parsing = shell_item_name(&item, SIGDN_DESKTOPABSOLUTEPARSING)?;
                let display = shell_item_name(&item, SIGDN_NORMALDISPLAY)?;
                Some((parsing, display))
            });
        match names {
            Some((parsing, display)) => children.push((child, parsing, display)),
            None => children.push((child, String::new(), String::new())),
        }
    }

    let tail = |path: &Path| path.file_name().map(|n| n.to_string_lossy().to_string());
    let mut hit: Vec<*const ITEMIDLIST> = Vec::new();
    for path in paths {
        let Some(name) = tail(path) else { continue };
        // 先按 `$R` 文件名（唯一）匹配；同名副本再多也不会误伤。
        let found = children
            .iter()
            .find(|(_, parsing, _)| parsing.rsplit('\\').next() == Some(name.as_str()))
            .or_else(|| {
                children
                    .iter()
                    .find(|(_, _, display)| display.eq_ignore_ascii_case(&name))
            });
        if let Some((pidl, _, _)) = found {
            let pidl = *pidl as *const ITEMIDLIST;
            // 两个键命中同一条（显示名回退撞车）时只能交一次，否则 Shell 会
            // 把同一条删两遍、真正想要的那条反而留下。
            if !hit.contains(&pidl) {
                hit.push(pidl);
            }
        }
    }
    if hit.is_empty() {
        crate::logal!("!! menu: 回收站里没匹配到要删除的条目");
        return 0;
    }

    let menu: IContextMenu = match unsafe { folder.GetUIObjectOf(host, &hit, None) } {
        Ok(menu) => menu,
        Err(e) => {
            crate::logal!("!! menu: 回收站取不到 IContextMenu {:?}", e.code());
            return 0;
        }
    };
    let verb = "delete";
    let ansi: Vec<u8> = verb.bytes().chain(Some(0)).collect();
    let wide: Vec<u16> = verb.encode_utf16().chain(Some(0)).collect();
    let info = windows::Win32::UI::Shell::CMINVOKECOMMANDINFOEX {
        cbSize: std::mem::size_of::<windows::Win32::UI::Shell::CMINVOKECOMMANDINFOEX>() as u32,
        fMask: CMIC_MASK_UNICODE,
        hwnd: host,
        lpVerb: PCSTR(ansi.as_ptr()),
        nShow: 1, // SW_SHOWNORMAL
        lpVerbW: PCWSTR(wide.as_ptr()),
        ..Default::default()
    };
    let result = unsafe {
        menu.InvokeCommand(
            (&info as *const windows::Win32::UI::Shell::CMINVOKECOMMANDINFOEX)
                .cast::<windows::Win32::UI::Shell::CMINVOKECOMMANDINFO>(),
        )
    };
    crate::logv!(
        "[menu] purge_recycle n={} hr={:?}",
        hit.len(),
        result.as_ref().err().map(|e| e.code().0)
    );
    hit.len()
}

/// 一批由 Shell 分配的子项 PIDL，析构时统一还给 `CoTaskMemFree`。
struct ChildPidls(Vec<*mut windows::Win32::UI::Shell::Common::ITEMIDLIST>);

impl Drop for ChildPidls {
    fn drop(&mut self) {
        for pidl in self.0.drain(..) {
            if !pidl.is_null() {
                unsafe {
                    windows::Win32::System::Com::CoTaskMemFree(Some(
                        pidl as *const core::ffi::c_void,
                    ))
                };
            }
        }
    }
}

/// `IShellItem` 显示名（调用方负责释放 `CoTaskMem` 那块内存）。
fn shell_item_name(
    item: &windows::Win32::UI::Shell::IShellItem,
    kind: windows::Win32::UI::Shell::SIGDN,
) -> Option<String> {
    use windows::Win32::System::Com::CoTaskMemFree;
    let raw = unsafe { item.GetDisplayName(kind) }.ok()?;
    let text = unsafe { raw.to_string() }.ok();
    unsafe { CoTaskMemFree(Some(raw.0 as *const core::ffi::c_void)) };
    text
}

/// 未使用，但保留类型引用以免 `Arc` 导入被判为多余。
#[allow(dead_code)]
type SharedSink = Arc<dyn Fn(MenuAction) + Send + Sync>;

#[cfg(test)]
mod wire_tests {
    use super::wire::Request;
    use super::{ends_menu, parse_event, wire, MenuAction, MenuNode, Target};
    use std::path::PathBuf;

    fn sample_request() -> Request {
        Request {
            target: Target::Items(vec![
                PathBuf::from(r"O:\评测素材\甲.txt"),
                PathBuf::from(r"O:\评测素材\乙 空格.txt"),
            ]),
            nodes: vec![
                MenuNode::Separator,
                MenuNode::Item {
                    key: "builtin.trash".into(),
                    label: "删除".into(),
                    enabled: true,
                    checked: false,
                },
                MenuNode::Submenu {
                    label: "排序方式".into(),
                    enabled: false,
                    children: vec![MenuNode::Item {
                        key: "sort.name".into(),
                        label: "名称".into(),
                        enabled: true,
                        checked: true,
                    }],
                },
            ],
            hide_keywords: vec!["!删除(D)".into(), "撤消".into()],
            extended: true,
            dark: true,
            owner: 0x1234,
        }
    }

    /// 请求要跨进程过一遍 bincode：中文 / 空格 / 嵌套子菜单都不能变形。
    #[test]
    fn request_survives_bincode() {
        let request = sample_request();
        let bytes = bincode::serialize(&request).expect("serialize");
        let back: Request = bincode::deserialize(&bytes).expect("deserialize");

        match &back.target {
            Target::Items(paths) => {
                assert_eq!(paths.len(), 2);
                assert_eq!(paths[0], PathBuf::from(r"O:\评测素材\甲.txt"));
                assert_eq!(paths[1], PathBuf::from(r"O:\评测素材\乙 空格.txt"));
            }
            other => panic!("目标变形了: {other:?}"),
        }
        assert!(back.extended && back.dark);
        assert_eq!(back.owner, 0x1234);
        assert_eq!(back.hide_keywords, request.hide_keywords);
        assert_eq!(back.nodes.len(), 3);
        match &back.nodes[2] {
            MenuNode::Submenu {
                label,
                enabled,
                children,
            } => {
                assert_eq!(label, "排序方式");
                assert!(!enabled, "禁用的子菜单不能变成可用的");
                assert_eq!(children.len(), 1);
                match &children[0] {
                    MenuNode::Item { key, checked, .. } => {
                        assert_eq!(key, "sort.name");
                        assert!(*checked);
                    }
                    other => panic!("子项变形了: {other:?}"),
                }
            }
            other => panic!("子菜单变形了: {other:?}"),
        }
    }

    /// 空菜单项（纯应用菜单）也要能过一遍。
    #[test]
    fn app_only_request_survives_bincode() {
        let request = Request {
            target: Target::AppOnly,
            nodes: Vec::new(),
            hide_keywords: Vec::new(),
            extended: false,
            dark: false,
            owner: 0,
        };
        let bytes = bincode::serialize(&request).expect("serialize");
        let back: Request = bincode::deserialize(&bytes).expect("deserialize");
        assert!(matches!(back.target, Target::AppOnly));
        assert!(back.nodes.is_empty());
    }

    /// 文件名里带换行 / 百分号时，`nav` 这一行不能被拆断。
    #[test]
    fn nav_path_survives_line_protocol() {
        for raw in [
            r"O:\评测素材",
            r"O:\a%0Ab",
            "O:\\带\n换行\\的目录",
            "O:\\带\r回车",
        ] {
            let path = std::path::PathBuf::from(raw);
            let line = format!("{}{}", wire::EV_NAVIGATE, wire::escape_path(&path));
            assert!(!line.contains('\n') && !line.contains('\r'), "{line}");
            assert!(ends_menu(&line));
            match parse_event(&line) {
                Some(MenuAction::Navigate(back)) => assert_eq!(back, path, "原样还原 {raw:?}"),
                other => panic!("不是导航动作: {other:?}"),
            }
        }
    }

    #[test]
    fn events_map_to_actions() {
        assert!(matches!(
            parse_event("app\tbuiltin.trash"),
            Some(MenuAction::App(key)) if key == "builtin.trash"
        ));
        assert!(matches!(
            parse_event("nav\tO:\\评测"),
            Some(MenuAction::Navigate(path)) if path == PathBuf::from(r"O:\评测")
        ));
        assert!(matches!(parse_event("rename"), Some(MenuAction::Rename)));
        assert!(matches!(
            parse_event("begin"),
            Some(MenuAction::ShellOpBegin)
        ));
        assert!(matches!(parse_event("end"), Some(MenuAction::ShellOpEnd)));
        assert!(matches!(parse_event("refresh"), Some(MenuAction::Refresh)));
        // 这两种只是状态标记，不是用户动作。
        assert!(parse_event("ready").is_none());
        assert!(parse_event("shown").is_none());
        assert!(parse_event("dismissed").is_none());
    }

    /// 「菜单已经关掉了」要覆盖用户可能做过的每一种选择，否则 BUSY 会漏清。
    #[test]
    fn every_outcome_ends_the_menu() {
        for line in [
            "dismissed".to_string(),
            "rename".to_string(),
            format!("{}builtin.copy", wire::EV_APP),
            format!("{}O:\\x", wire::EV_NAVIGATE),
            // 点了 Shell 项：菜单当场就关了，之后只剩「开始 / 结束」的收尾。
            "begin".to_string(),
        ] {
            assert!(ends_menu(&line), "{line} 必须算菜单结束");
        }
        // Shell 任务的收尾事件不算：那时菜单早就关掉了。
        assert!(!ends_menu("end"));
        assert!(!ends_menu("refresh"));
        assert!(!ends_menu("shown"));
    }
}
