//! 系统通用文件对话框（`#32770`）探测、路径逆向提取与原生跳转驱动。
//!
//! 核心原理：
//! 1. 后台监听前台焦点窗口，探测打开/另存为通用对话框；
//! 2. 从对话框地址栏（`ToolbarWindow32`）逆向读取当前目录路径；
//! 3. 向对话框文件名框（`0x480` / `0x47C`）写入目标路径并发送 `IDOK`，驱动系统原生跳转。
//!
//! 非 Windows 平台提供安全空实现（stub）。

#[cfg(windows)]
mod imp {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
    use windows_sys::core::w;
    use windows_sys::Win32::Foundation::{HMODULE, HWND, LPARAM, RECT};
    use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
    use windows_sys::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
    use windows_sys::Win32::UI::Controls::EM_SETSEL;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        BM_CLICK, DispatchMessageW, EnumChildWindows, FindWindowExW, GetClassNameW, GetDlgCtrlID, GetDlgItem,
        GetForegroundWindow, GetMessageW, GetParent, GetSystemMetrics, GetWindowRect,
        GetWindowThreadProcessId, IsWindowVisible,
        PostMessageW, SendMessageTimeoutW, SendMessageW, SetForegroundWindow, SMTO_ABORTIFHUNG, SMTO_BLOCK,
        SM_CYSCREEN, TranslateMessage, BN_CLICKED, EVENT_OBJECT_DESTROY, EVENT_OBJECT_LOCATIONCHANGE,
        EVENT_SYSTEM_FOREGROUND, IDOK, WM_COMMAND, WM_GETTEXT, WM_GETTEXTLENGTH, WM_SETTEXT,
        WINEVENT_OUTOFCONTEXT,
    };

    use crate::logv;

    static CURRENT_DIALOG: AtomicIsize = AtomicIsize::new(0);

    /// 登记 Slint 搜索吸附窗的句柄，防止自我监听引发死循环（抢焦点 → 误判失焦 → 隐藏）。
    static JUMP_WINDOW_HWND: AtomicIsize = AtomicIsize::new(0);

    /// 登记吸附窗 HWND，供事件钩子识别自隐藏边界。
    pub fn register_jump_window(hwnd: isize) {
        JUMP_WINDOW_HWND.store(hwnd, Ordering::SeqCst);
    }

    /// 前台窗口是否属于本进程（吸附窗、主窗口等自身 UI）。
    ///
    /// 用进程归属判断而不是句柄比对：`show()` 之后 `window_handle()` 可能尚未就绪，
    /// 吸附窗 HWND 的登记会静默失败；此时若只靠句柄比对，吸附窗自己抢到前台会被
    /// 误判成「对话框失焦」→ 隐藏 + 清空输入，造成 hide/show 抖动、输入被吃掉。
    fn is_own_window(hwnd: HWND) -> bool {
        let mut pid: u32 = 0;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        pid == std::process::id()
    }

    fn get_class_name(hwnd: HWND) -> String {
        let mut buf = [0u16; 64];
        let len = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
        if len > 0 {
            String::from_utf16_lossy(&buf[..len as usize])
        } else {
            String::new()
        }
    }

    /// 跨进程同步发消息，目标进程卡死时最多等 `timeout` 毫秒（失败返回 None）。
    ///
    /// 读写其它进程的子控件只能用消息；而裸 `SendMessage` 在目标挂起时会无限阻塞，
    /// 本模块的探测又被主线程 500ms 轮询调用，必须加超时以免拖死 UI。
    fn send_sync(hwnd: HWND, msg: u32, wparam: usize, lparam: LPARAM, timeout: u32) -> Option<usize> {
        let mut result: usize = 0;
        let ok = unsafe {
            SendMessageTimeoutW(
                hwnd,
                msg,
                wparam,
                lparam,
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                timeout,
                &mut result as *mut usize,
            )
        };
        if ok != 0 {
            Some(result)
        } else {
            None
        }
    }

    /// 读取控件文本。
    ///
    /// 关键：文件名框是**其它进程**的子控件，`GetWindowText(Length)` 的文档化行为是对
    /// 跨进程子控件一律返回 0 / 空串（防止目标进程挂起时拖死调用方）。用它会导致永远
    /// 误判「文件名为空」，进而还原逻辑被整段跳过。跨进程读取必须发 WM_GETTEXT。
    fn get_window_text(hwnd: HWND) -> String {
        let Some(len) = send_sync(hwnd, WM_GETTEXTLENGTH, 0, 0, 100) else {
            return String::new();
        };
        if len == 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len + 1];
        let cap = buf.len();
        let Some(copied) = send_sync(hwnd, WM_GETTEXT, cap, buf.as_mut_ptr() as LPARAM, 100) else {
            return String::new();
        };
        buf.truncate(copied.min(cap));
        String::from_utf16_lossy(&buf)
    }

    /// 写入控件文本，返回是否写入成功（读回校验：僵尸句柄 / 只读控件会失败）。
    fn set_window_text(hwnd: HWND, text: &str) -> bool {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        if send_sync(hwnd, WM_SETTEXT, 0, wide.as_ptr() as LPARAM, 200).is_none() {
            return false;
        }
        let got = get_window_text(hwnd);
        // 严格比对（忽略结尾反斜杠）：避免 "C:\foo" 把 "C:\foobar" 误判为写入成功
        got.trim_end_matches('\\') == text.trim_end_matches('\\')
    }

    /// 探测用的文件名输入框。
    fn find_file_edit(dialog: HWND) -> HWND {
        edit_candidates(dialog).into_iter().next().unwrap_or(0 as HWND)
    }

    /// 跳转并发守卫：同一时刻只允许一个跳转线程改写文件名框，避免连点 / 回车+点击互相覆盖。
    static NAVIGATING: AtomicBool = AtomicBool::new(false);

    /// RAII 守卫：无论跳转线程正常结束还是 panic，都复位守卫位。
    struct NavGuard;
    impl Drop for NavGuard {
        fn drop(&mut self) {
            NAVIGATING.store(false, Ordering::SeqCst);
        }
    }

    /// 递归扫描收集到的候选控件（按可靠性从高到低）。
    #[derive(Default)]
    struct ScanCtx {
        /// 控件 ID == 0x0480 的 Edit（文件名编辑框，最稳定）
        id_edit: HWND,
        /// 位于控件 ID == 0x047C 的 ComboBox 内的 Edit
        id_combo_edit: HWND,
        /// 任意 ComboBox 内的 Edit
        combo_edit: HWND,
        /// 任意 Edit（最后兜底）
        any_edit: HWND,
        /// 确定 / 保存 / 打开按钮
        ok: HWND,
    }

    unsafe extern "system" fn scan_tree(hwnd: HWND, lparam: LPARAM) -> i32 {
        let ctx = &mut *(lparam as *mut ScanCtx);
        let class = get_class_name(hwnd);
        let id = GetDlgCtrlID(hwnd);

        if class == "Edit" {
            if ctx.any_edit.is_null() {
                ctx.any_edit = hwnd;
            }
            if id == 0x0480 && ctx.id_edit.is_null() {
                ctx.id_edit = hwnd;
            }
            // 父级为 ComboBox / ComboBoxEx32 时，极可能就是文件名框
            let parent = GetParent(hwnd);
            if !parent.is_null() {
                let pclass = get_class_name(parent);
                if pclass == "ComboBox" || pclass == "ComboBoxEx32" {
                    if ctx.combo_edit.is_null() {
                        ctx.combo_edit = hwnd;
                    }
                    if GetDlgCtrlID(parent) == 0x047C && ctx.id_combo_edit.is_null() {
                        ctx.id_combo_edit = hwnd;
                    }
                }
            }
        }

        if ctx.ok.is_null() {
            if id == 1 {
                ctx.ok = hwnd;
            } else if class == "Button" {
                // 仅在必要时才跨进程读文本（每次读都要发消息等目标线程处理）
                let text = get_window_text(hwnd);
                if text.contains("保存")
                    || text.contains("打开")
                    || text.contains("Save")
                    || text.contains("Open")
                {
                    ctx.ok = hwnd;
                }
            }
        }
        1
    }

    /// 控件是否可用（句柄有效 + 可见 + 启用）。
    ///
    /// 跳转后 DirectUI 子树会重建，常残留不可见的旧 Edit 与自动完成下拉框；
    /// 写进这类控件不会触发跳转，也是「第一次能跳、之后跳不动」的主因之一。
    fn is_usable(hwnd: HWND) -> bool {
        !hwnd.is_null()
            && unsafe { IsWindowVisible(hwnd) } != 0
            && unsafe { IsWindowEnabled(hwnd) } != 0
    }

    /// 按可靠性从高到低列出文件名输入框候选（已去重并过滤不可用控件）。
    ///
    /// 顺序：控件 ID 0x0480 / 0x047C → DirectUI 五层链路 → 经典直接子控件 → 组合框 / 任意 Edit 兜底。
    fn edit_candidates(dialog: HWND) -> Vec<HWND> {
        // 策略 1：递归扫描全树按控件 ID 定位（ID 最稳定，跳转后句柄会变但 ID 不变）。
        let mut scan = ScanCtx::default();
        unsafe {
            EnumChildWindows(dialog, Some(scan_tree), &mut scan as *mut ScanCtx as LPARAM);
        }

        // 策略 2：现代 Windows 10/11 IFileDialog 标准五层链路。
        // #32770 -> DUIViewWndClassName -> DirectUIHWND -> FloatNotifySink -> ComboBox -> Edit
        // 拆成两组：带 Edit 子控件的可编辑组合框（文件名框）优先；
        // 无 Edit 子控件的只读下拉（「保存类型」等）排到最后兜底。
        let mut dui_edits: Vec<HWND> = Vec::new();
        let mut dui_readonly: Vec<HWND> = Vec::new();
        unsafe {
            let dui = FindWindowExW(dialog, 0 as HWND, w!("DUIViewWndClassName"), std::ptr::null::<u16>());
            if !dui.is_null() {
                let direct_ui = FindWindowExW(dui, 0 as HWND, w!("DirectUIHWND"), std::ptr::null::<u16>());
                if !direct_ui.is_null() {
                    let mut sink = FindWindowExW(
                        direct_ui,
                        0 as HWND,
                        w!("FloatNotifySink"),
                        std::ptr::null::<u16>(),
                    );
                    while !sink.is_null() {
                        let combo =
                            FindWindowExW(sink, 0 as HWND, w!("ComboBox"), std::ptr::null::<u16>());
                        if !combo.is_null() {
                            let edit = FindWindowExW(combo, 0 as HWND, w!("Edit"), std::ptr::null::<u16>());
                            if edit.is_null() {
                                dui_readonly.push(combo);
                            } else {
                                dui_edits.push(edit);
                            }
                        }
                        sink = FindWindowExW(
                            direct_ui,
                            sink,
                            w!("FloatNotifySink"),
                            std::ptr::null::<u16>(),
                        );
                    }
                }
            }
        }

        // 策略 3：经典对话框直接子控件（ID 0x0480 / 0x047C）。
        let classic = unsafe {
            let e = GetDlgItem(dialog, 0x0480);
            if !e.is_null() {
                e
            } else {
                let c = GetDlgItem(dialog, 0x047C);
                if !c.is_null() {
                    let sub = FindWindowExW(c, 0 as HWND, w!("Edit"), std::ptr::null::<u16>());
                    if sub.is_null() {
                        c
                    } else {
                        sub
                    }
                } else {
                    0 as HWND
                }
            }
        };

        let mut out: Vec<HWND> = Vec::new();
        for cand in [scan.id_edit, scan.id_combo_edit]
            .into_iter()
            .chain(dui_edits)
            .chain([classic, scan.combo_edit, scan.any_edit])
            .chain(dui_readonly)
        {
            if is_usable(cand) && !out.contains(&cand) {
                out.push(cand);
            }
        }
        out
    }

    /// 确定 / 保存 / 打开按钮。
    fn find_ok_button(dialog: HWND) -> HWND {
        let mut scan = ScanCtx::default();
        unsafe {
            EnumChildWindows(dialog, Some(scan_tree), &mut scan as *mut ScanCtx as LPARAM);
        }
        if !scan.ok.is_null() {
            scan.ok
        } else {
            unsafe { GetDlgItem(dialog, 1) }
        }
    }



    pub fn probe_file_dialog(hwnd: HWND) -> bool {
        if hwnd.is_null() || get_class_name(hwnd) != "#32770" {
            return false;
        }
        // 先用 Shell 视图判断（两个 FindWindowExW，微秒级、更准），命中即返回；
        // 只有非 Shell 的 #32770（消息框、设置框等）才做全树递归 + 跨进程读按钮的重扫描。
        let has_shell = unsafe {
            let sv = FindWindowExW(hwnd, 0 as HWND, w!("SHELLDLL_DefView"), std::ptr::null::<u16>());
            let dv = FindWindowExW(hwnd, 0 as HWND, w!("DUIViewWndClassName"), std::ptr::null::<u16>());
            !sv.is_null() || !dv.is_null()
        };
        if has_shell {
            return true;
        }
        !find_file_edit(hwnd).is_null()
    }

    pub fn extract_current_folder(dialog: HWND) -> Option<PathBuf> {
        struct SearchCtx {
            found_path: Option<PathBuf>,
        }

        unsafe extern "system" fn enum_child(hwnd: HWND, lparam: LPARAM) -> i32 {
            let ctx = &mut *(lparam as *mut SearchCtx);
            let class = get_class_name(hwnd);

            if class == "ToolbarWindow32" {
                let text = get_window_text(hwnd);
                for prefix in ["地址: ", "Address: ", "地址:"] {
                    if let Some(rest) = text.strip_prefix(prefix) {
                        let trimmed = rest.trim();
                        // 地址栏文本本身已足够可信，不再做 exists() 校验
                        // （网络路径 exists() 会触发 SMB 超时，卡住探测线程）
                        let p = PathBuf::from(trimmed);
                        ctx.found_path = Some(p);
                        return 0;
                    }
                }
            }
            1
        }

        let mut ctx = SearchCtx { found_path: None };
        unsafe {
            EnumChildWindows(dialog, Some(enum_child), &mut ctx as *mut SearchCtx as LPARAM);
        }
        ctx.found_path
    }

    /// 触发对话框接受。
    ///
    /// 只用一次真实点击：`BM_CLICK` 本身就会向父对话框送达 `WM_COMMAND(BN_CLICKED, IDOK)`；
    /// 再手动补发一条 `WM_COMMAND(IDOK)` 等于重复触发，加载中的对话框偶尔会消费到第二条，
    /// 此时文件名框已清空，可能误触「保存」。
    fn trigger_accept(dialog: HWND, ok_btn: HWND) {
        unsafe {
            if !ok_btn.is_null() {
                PostMessageW(ok_btn, BM_CLICK, 0, 0);
            } else {
                // 找不到确认按钮时的兜底
                PostMessageW(
                    dialog,
                    WM_COMMAND,
                    (BN_CLICKED << 16 | IDOK as u32) as usize,
                    0,
                );
            }
        }
    }

    /// 还原原始文件名。
    ///
    /// 跳转后 DirectUI 子树会重建、旧句柄可能失效，因此需**重新查找**文件名框；
    /// 且候选里可能混入只读下拉（「保存类型」等），必须逐个试写直到真正写进去，
    /// 否则还原会静默失败（表现为文件名停在对话框初始值）。
    fn restore_filename(dialog: HWND, preferred: HWND, name: &str) {
        if name.is_empty() {
            return;
        }
        // 1) 优先复用刚成功注入的那个控件，避免再次选错
        if is_usable(preferred) && set_window_text(preferred, name) {
            unsafe {
                SendMessageW(preferred, EM_SETSEL, 0, -1);
            }
            return;
        }
        // 2) 回退：重新查找，逐个试写
        for edit in edit_candidates(dialog) {
            if set_window_text(edit, name) {
                unsafe {
                    SendMessageW(edit, EM_SETSEL, 0, -1);
                }
                return;
            }
        }
        logv!("[rove:dialog_jump] filename restore failed: no writable edit");
    }

    /// 驱动对话框跳转（Win32 消息流）。
    ///
    /// 跳转会让 DirectUI 子树重建（句柄失效、枚举顺序也会变），因此：
    /// 1) 注入前用「写入 + 读回」校验候选控件，排除僵尸 / 隐藏 Edit；
    /// 2) 触发后等待目录加载，若文件名框仍持有注入的路径说明没跳成功，换下一个候选；
    /// 3) 还原文件名时重新查找控件。
    pub fn navigate_dialog(dialog: HWND, target_dir: &Path) -> Result<(), String> {
        if !target_dir.exists() {
            return Err("目标目录不存在".into());
        }
        // 并发守卫：连点 / 回车 + 点击会让多个线程同时改写文件名框、各自触发确认并还原，
        // 目录与文件名都不可预测。已在进行中则直接放弃本次。
        if NAVIGATING.swap(true, Ordering::SeqCst) {
            return Err("已有跳转进行中".into());
        }

        // 末尾反斜杠：Windows 对话框识别到目录后直接跳转，而不是当成文件名保存
        let mut target_str = target_dir.to_string_lossy().into_owned();
        if !target_str.ends_with('\\') {
            target_str.push('\\');
        }

        // 候选枚举 / 按钮查找较耗时（递归枚举 + 跨进程读文本），且会与文件对话框同进程交互，
        // 放进后台线程，避免点击瞬间主线程卡顿。
        let dialog_as = dialog as usize;
        std::thread::spawn(move || {
            let _guard = NavGuard;
            let dialog = dialog_as as HWND;
            let candidates: Vec<usize> =
                edit_candidates(dialog).into_iter().map(|h| h as usize).take(3).collect();
            if candidates.is_empty() {
                logv!("[rove:dialog_jump] navigation failed: no filename edit found");
                return;
            }
            let ok_btn = find_ok_button(dialog);
            // (实际跳转成功的控件句柄, 原始文件名)
            let mut restore: Option<(HWND, String)> = None;
            // 首个候选的原始名：全部候选失败时回退用，避免吃掉用户已输入的文件名
            let first_edit = *candidates.first().unwrap() as HWND;
            let first_name = if is_usable(first_edit) {
                get_window_text(first_edit)
            } else {
                String::new()
            };

            for (idx, cand) in candidates.iter().enumerate() {
                let edit = *cand as HWND;
                if !is_usable(edit) {
                    continue;
                }

                // 1. 记下原本文件名（必须在注入动作之前）
                let original_name = get_window_text(edit);
                logv!(
                    "[rove:dialog_jump] navigating dialog={:#x}, edit_ctrl={:#x}, ok_btn={:#x}, filename={:?}",
                    dialog as usize,
                    edit as usize,
                    ok_btn as usize,
                    original_name
                );

                // 2. 注入带尾斜杠的目标目录路径 + 触发跳转
                if !set_window_text(edit, &target_str) {
                    logv!("[rove:dialog_jump] candidate #{} rejected: write failed", idx);
                    continue;
                }
                trigger_accept(dialog, ok_btn);

                // 3. 等待系统加载目标目录
                std::thread::sleep(std::time::Duration::from_millis(400));

                // 4. 校验：跳转成功后文件名框会被清空/重置；仍持有注入路径说明没跳成
                if get_window_text(edit).trim_end_matches('\\')
                    == target_str.trim_end_matches('\\')
                {
                    logv!("[rove:dialog_jump] candidate #{} did not navigate", idx);
                    set_window_text(edit, "");
                    continue;
                }
                restore = Some((edit, original_name));
                break;
            }

            // 5. 还原文件名；全部失败时回退首个候选的原始名
            if restore.is_none() && !first_name.is_empty() {
                restore_filename(dialog, first_edit, &first_name);
            } else if let Some((edit, name)) = restore.as_ref() {
                restore_filename(dialog, *edit, name);
            }
            unsafe {
                SetForegroundWindow(dialog);
            }
            logv!(
                "[rove:dialog_jump] navigation completed, filename restored: {}",
                restore.as_ref().map(|(_, n)| n.as_str()).unwrap_or("")
            );
        });

        Ok(())
    }

    /// 获取对话框的精确可视边界，并判定吸附位置（bottom 或 top）。
    fn get_dialog_placement(hwnd: HWND) -> (i32, i32, i32, &'static str) {
        let mut rect: RECT = unsafe { std::mem::zeroed() };
        let res = unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                &mut rect as *mut _ as *mut _,
                std::mem::size_of::<RECT>() as u32,
            )
        };
        if res != 0 {
            unsafe {
                GetWindowRect(hwnd, &mut rect);
            }
        }

        let screen_h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
        let w = rect.right - rect.left;

        // 预估下拉列表最大高度 350px：若下方放不下且上方空间充足，则吸附到对话框顶部。
        if rect.bottom + 350 > screen_h && rect.top > 350 {
            (rect.left, rect.top, w, "top")
        } else {
            (rect.left, rect.bottom, w, "bottom")
        }
    }

    /// 定时轮询前台窗口（由主线程 Slint 定时器驱动，见 app.rs 的 jump_poll_timer）。
    ///
    /// 必须在主线程调用：此前挂在钩子线程的 `SetTimer` 会因 WINEVENT_OUTOFCONTEXT
    /// 钩子持续投递 LOCATIONCHANGE 事件导致队列长期非空，WM_TIMER 作为低优先级消息被饿死。
    ///
    /// 解决的是「对话框刚打开时子控件（SHELLDLL_DefView / DirectUI）尚未创建，
    /// 该次 FOREGROUND 探测失败且之后不再触发」导致吸附条迟迟不出现的问题。
    pub fn poll_foreground() {
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.is_null() {
            return;
        }
        // 豁免自身窗口：用户点击吸附条后它就是前台窗口，绝不能据此判定对话框失焦
        if is_own_window(hwnd) {
            return;
        }
        if hwnd as isize == JUMP_WINDOW_HWND.load(Ordering::SeqCst) {
            return;
        }
        // 已吸附到该对话框且前台未变化：跳过重量级的控件树递归探测。
        // 移动跟随由 LOCATIONCHANGE 事件负责，失焦由 FOREGROUND 事件负责，二者均已正常工作。
        if hwnd as isize == CURRENT_DIALOG.load(Ordering::SeqCst) {
            return;
        }

        if probe_file_dialog(hwnd) {
            let prev = CURRENT_DIALOG.swap(hwnd as isize, Ordering::SeqCst);
            if prev != hwnd as isize {
                let folder = extract_current_folder(hwnd);
                let (x, y, w, placement) = get_dialog_placement(hwnd);
                logv!(
                    "[rove:dialog_jump] poll: attaching to dialog {:#x}, placement={}",
                    hwnd as usize,
                    placement
                );
                let hwnd_usize = hwnd as usize;
                let p = placement.to_string();
                let _ = slint::invoke_from_event_loop(move || {
                    crate::app::show_dialog_jump(hwnd_usize, folder, x, y, w, p);
                });
            }
        } else {
            let prev = CURRENT_DIALOG.swap(0, Ordering::SeqCst);
            if prev != 0 {
                logv!("[rove:dialog_jump] poll: dialog blurred, hiding search bar");
                let _ = slint::invoke_from_event_loop(|| {
                    crate::app::hide_dialog_jump();
                });
            }
        }
    }

    pub fn start_daemon() {
        std::thread::Builder::new()
            .name("rove-dialog-hook".into())
            .spawn(|| unsafe {
                logv!("[rove:dialog_jump] hook thread started");

                let hook = SetWinEventHook(
                    EVENT_SYSTEM_FOREGROUND,
                    EVENT_OBJECT_LOCATIONCHANGE,
                    0 as HMODULE,
                    Some(winevent_proc),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT,
                );

                let mut msg = std::mem::zeroed();
                while GetMessageW(&mut msg, 0 as HWND, 0, 0) > 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }

                if !hook.is_null() {
                    UnhookWinEvent(hook);
                }
            })
            .expect("spawn rove-dialog-hook thread");
    }

    unsafe extern "system" fn winevent_proc(
        _hook: HWINEVENTHOOK,
        event: u32,
        hwnd: HWND,
        id_object: i32,
        _id_child: i32,
        _event_thread: u32,
        _event_time: u32,
    ) {
        if id_object != 0 || hwnd.is_null() {
            return;
        }

        // 核心修复 1：豁免自身所有窗口（吸附窗 / 主窗口）。
        // 优先用进程归属判断——它不依赖吸附窗 HWND 是否已登记成功。
        if is_own_window(hwnd) {
            return;
        }
        let jump_hwnd = JUMP_WINDOW_HWND.load(Ordering::SeqCst) as HWND;
        if hwnd == jump_hwnd {
            return;
        }

        match event {
            EVENT_SYSTEM_FOREGROUND => {
                if probe_file_dialog(hwnd) {
                    // 核心修复 2：同窗口防抖。仅当这是一个新对话框或从外部切回时才呼出并抢焦点；
                    // 若只是吸附窗与该对话框之间互点，prev == hwnd，静默保持，不抢焦点。
                    let prev = CURRENT_DIALOG.swap(hwnd as isize, Ordering::SeqCst);
                    if prev != hwnd as isize {
                        let folder = extract_current_folder(hwnd);
                        let (x, y, w, placement) = get_dialog_placement(hwnd);
                        logv!(
                            "[rove:dialog_jump] auto-attaching to dialog {:#x}, placement={}",
                            hwnd as usize,
                            placement
                        );
                        let hwnd_usize = hwnd as usize;
                        let p = placement.to_string();
                        let _ = slint::invoke_from_event_loop(move || {
                            crate::app::show_dialog_jump(hwnd_usize, folder, x, y, w, p);
                        });
                    } else {
                        // 同一对话框反复获得焦点：仅跟随更新位置，避免重复抢焦点。
                        let (x, y, w, placement) = get_dialog_placement(hwnd);
                        let p = placement.to_string();
                        let _ = slint::invoke_from_event_loop(move || {
                            crate::app::update_dialog_jump_position(x, y, w, p);
                        });
                    }
                } else {
                    let prev = CURRENT_DIALOG.swap(0, Ordering::SeqCst);
                    if prev != 0 {
                        logv!("[rove:dialog_jump] dialog blurred, hiding search bar");
                        let _ = slint::invoke_from_event_loop(|| {
                            crate::app::hide_dialog_jump();
                        });
                    }
                }
            }
            EVENT_OBJECT_DESTROY => {
                let cur = CURRENT_DIALOG.load(Ordering::SeqCst);
                if cur == hwnd as isize {
                    CURRENT_DIALOG.store(0, Ordering::SeqCst);
                    logv!("[rove:dialog_jump] dialog closed, hiding search bar");
                    let _ = slint::invoke_from_event_loop(|| {
                        crate::app::hide_dialog_jump();
                    });
                }
            }
            EVENT_OBJECT_LOCATIONCHANGE => {
                let cur = CURRENT_DIALOG.load(Ordering::SeqCst);
                if cur == hwnd as isize {
                    let (x, y, w, placement) = get_dialog_placement(hwnd);
                    let p = placement.to_string();
                    let _ = slint::invoke_from_event_loop(move || {
                        crate::app::update_dialog_jump_position(x, y, w, p);
                    });
                }
            }
            _ => {}
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn start_daemon() {}
}

pub use imp::*;
