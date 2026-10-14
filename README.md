# Rove

高性能、轻量级的 Windows 文件管理器（Rust + Slint）。

## 架构

```
┌──────────────────┐   打开路径   ┌────────────────────────────────┐
│ rove.exe C:\x    │ ───────────► │ 守护进程（首次启动常驻）         │
└──────────────────┘  客户端即退  │  ├─ 命名管道 \\.\pipe\rove.daemon.v1
                                 │  ├─ 系统托盘线程（图标 + 菜单）   │
                                 │  └─ Slint 事件循环（主线程）      │
                                 └────────────────────────────────┘
```

* **单实例 / IPC**：`src/ipc.rs`。已有实例时，新进程作为客户端发送
  `open\t<path>` / `show` / `settings` / `quit` 后立即退出。
* **UI 生命周期**：`src/app.rs`。`run_event_loop_until_quit()` 常驻事件循环；
  关闭所有标签页（设置项“关闭所有标签后销毁 UI”）时 `hide()` 并释放窗口句柄，
  后台仅保留管道监听与托盘；收到唤醒请求后重建窗口，直接读取内存数据渲染。
* **系统托盘**：`src/tray.rs`，独立线程的隐藏窗口 + `Shell_NotifyIconW`，
  提供“打开主窗口 / 设置 / 彻底退出”。
* **浏览**：`src/browse.rs`（目录枚举、大小/时间格式化，无日期库依赖）。

## 构建

```bash
cargo build --release --target x86_64-pc-windows-msvc   # 目标平台
cargo check --target x86_64-pc-windows-gnu               # 交叉编译类型检查
```

运行：

```bash
rove.exe             # 启动守护进程并显示窗口
rove.exe C:\Windows  # 唤醒已运行实例并打开该路径
rove.exe --settings  # 唤醒并打开设置
rove.exe --quit      # 彻底退出
```

配置文件：`%APPDATA%\Rove\settings.toml`

## 当前进度（Phase 4 + 界面重构已完成）

- [x] **索引引擎** `src/index/`：`$MFT` 直读（run list 解码、USA fixup、`$FILE_NAME(0x30)`/`$DATA(0x80)` 解析、
      namespace 优先级选名）+ USN Journal 事件循环（`BytesToWaitFor=1` 阻塞等待，新建/删除/重命名增量应用）；
      未提权时自动启用 `SeManageVolumePrivilege`，仍失败则回退目录枚举索引
- [x] **拼音检索与高亮** `src/matcher.rs`（移植自 `stools`）：`nucleo_matcher` 模糊匹配 + 多音字异读 +
      全拼/首字母/字面三级回退 + `match_ranges` 反查表，Slint 端按 span 分段渲染高亮（命中段加粗 + 主题色）
- [x] **界面重构** `ui/main.slint`（对齐 Pulse 功能裁剪版）：
  - 左侧导航边栏：快速访问（桌面/下载/回收站含项数）、此电脑与驱动器列表（卷标 + 已用/总容量进度条）、
    网络位置（折叠组）；**底部常驻暂存托盘**（明确不含星标/标签体系）
  - 顶部：面包屑地址栏（可点击逐级导航）+ 前进/后退/向上/刷新 + 搜索框（作用域切换 + 建议下拉）
  - 动作工具条：新建/剪切/复制/重命名/删除/粘贴/排序下拉/分栏/加入暂存
  - 文件列表：名称（搜索高亮）/ 路径（搜索模式）/ 修改时间 / 类型徽章（IMG、ZIP、CSV、CODE 等彩色胶囊）/ 大小
- [x] **设置改为标签页**：`⚙ 设置` 标签全局唯一，两栏卡片视图，四个子页：
  通用（外观/主题、销毁 UI、默认文件管理器接管勾选）、搜索与索引（MFT 状态、压缩软件探测）、
  本地右键菜单（`actions.toml` 图形化管理）、关于与诊断
- [x] **搜索端到端**：当前目录名称（即时 + 高亮）、当前目录内容、全盘索引三种作用域；
  状态栏 `搜索 "xx" · N 项`，搜索模式激活路径列，右键"打开文件所在位置"
- [x] 暂存盘 `src/staging.rs`、QuickLook 预览 `src/preview.rs`（图片/文本/CSV/Excel/信息卡）、
  文本对比 `src/diff.rs` + `ui/diff.slint`

### 验证

| 检查项 | 结果 |
| :--- | :--- |
| `cargo test --bin rove` | 7/7 通过（拼音高亮、多音字、ASCII 恒等映射等） |
| `cargo build --target x86_64-pc-windows-gnu` | 0 警告 0 错误 |
| WSL 实机运行 | 新界面窗口正常、设置标签可通过 IPC `--settings` 打开、`--quit` 干净退出 |
| 全盘搜索端到端 | 探针验证：索引线程搜索 → 结果通道 → 主线程轮询 → 模型更新，291 项命中 |
| MFT 提权 | WSL 非提权下 `ERROR_ACCESS_DENIED` → 已启用 `SeManageVolumePrivilege` 尝试 + 目录枚举回退 |

后续阶段见 `plan/base.md`（Phase 5 注册表接管）。

> 注：工作区中的 `../pulse`（C++20 文件管理器）是规范第五章的参考实现，Phase 4 的
> MFT 直读、USN 监听、拼音搜索、暂存盘、Diff 将参照其对应模块移植。
