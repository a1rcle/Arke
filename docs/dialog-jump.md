# Arke 对话框自动跳转（Dialog Quick Switch / 路径注入）技术方案

> **文档用途**：面向后续开发 Agent 的可执行技术设计规范。  
> **设计参考**：`D:\ZenSearch\docs\features\path_injection.md` 及其实现（`injector.rs` / `explorer.rs` / `hotkey.rs`）。  
> **目标仓库**：`D:\Arke-main`（Rust Edition 2024、`windows = "0.62"`、GPUI、`parking_lot`、`flume`、`log`）。  
> **核心模块**：新增 `crates/app/src/win_dialog_jump.rs`，复用 `crates/app/src/api.rs` 状态快照与 `crates/app/src/settings.rs` 设置体系。

---

## 一、 背景与核心差异（ZenSearch → Arke 架构适配）

ZenSearch 作为独立外挂启动器，依赖 COM（`IShellWindows`）跨进程查询 Windows 资源管理器（`CabinetWClass`）的路径；而 **Arke 本身就是文件管理器**，具备天然的进程内状态优势，同时也需要遵守 Arke 自身严格的工程规范与边界约束：

| 维度 | ZenSearch 原实现 | Arke 植入方案（本规范） |
| :--- | :--- | :--- |
| **路径来源（主）** | `CabinetWClass` / `ExploreWClass`（经 `IShellWindows` COM 查询） | **`ArkeWindow`**（经 `api.rs` 进程内 `WINDOWS` 快照实时读取，零 COM、零延迟、原生支持多标签页与分屏双栏） |
| **路径时效性** | 激活资源管理器时静态快照（激活后再切目录可能陈旧） | **跳转瞬间实时查询** `api::active_folder_for_hwnd(hwnd)`，始终命中最新标签/分屏目录 |
| **路径来源（可选扩展）** | 仅系统 Explorer | 预留 `SourceKind::Explorer` 扩展点（默认关闭或作为 Phase 2 独立选项），避免与 ZenSearch/Listary 冲突 |
| **自有对话框防护** | 无特殊过滤 | **强制排除 Arke 自身 `win_picker.rs`（`--pick-file`）生成的 `#32770`**，防止劫持设置页“浏览”按钮 |
| **非文件弹窗防护** | 回退匹配首个任意 `Edit` 控件 | **必须先验明正身为文件对话框**（存在 `SHELLDLL_DefView` / `DUIViewWndClassName` 或标准 `ComboBoxEx32` 结构），杜绝误操作普通 `#32770` 弹窗 |
| **多语言地址栏解析** | 硬编码剥离 `"Address: "` / `"地址: "` | **语言无关解析**：通用识别前缀后的盘符路径（`[A-Za-z]:\`）或 UNC 路径（`\\server\share`），兼容 Arke 全部 7 种语言与任意 Windows 系统语言 |
| **依赖与基础库** | `windows 0.58` / `std::sync::Mutex` / `crossbeam-channel` / `tracing` | **`windows 0.62`** / **`parking_lot::Mutex`** / **`flume`** / **`log`**（严格遵守 `Cargo.toml` 规范，**零新增外部依赖**） |
| **Lint 约束** | 标准 Rust 2021 | **Edition 2024 + Workspace 严格 Lint**：`unsafe_op_in_unsafe_fn = "deny"`、`undocumented_unsafe_blocks = "deny"`、`missing_safety_doc = "deny"`、`unwrap_used = "warn"`、`missing_docs = "warn"` |

---

## 二、 交互规范与双模触发

完整继承 ZenSearch 的双模协同体验：

1. **切回自动跳转（主模式）**：
   - 用户在 Arke 主窗口（`ArkeWindow`）浏览某个本地文件夹、盘符根目录或网络共享（UNC `\\server\share`）后，直接切回（鼠标点击或 `Alt+Tab`）其他应用打开的系统通用文件对话框（打开/保存，`#32770`）。
   - 无论用户点击对话框的哪个区域（文件列表空白处 `DirectUIHWND`、左侧导航树 `SysTreeView32`、顶部面包屑地址栏 `ToolbarWindow32` 或底部文件名输入框），对话框均平滑跳转至该目录，且保留原有待保存文件名。
2. **`Ctrl + G` 手动跳转（辅助模式）**：
   - 在分屏并排（Arke 与文件对话框同屏可见）等场景下，无需切回 Arke 再切回来，直接在 `#32770` 文件对话框内按下 `Ctrl + G` 即可原地拉取最近活跃的 Arke 目录并跳转。
   - **按键拦截约束**：仅当前台窗口是符合条件的外部 `#32770` 文件对话框且按下纯 `Ctrl + G`（不含 `Alt` / `Shift` / `Win`）时才消费该按键；其他任何应用中的 `Ctrl + G`（如 IDE 跳转行、浏览器查找下一个）必须原样透传。

---

## 三、 核心架构与状态机设计

### 1. 模块分层与线程模型

> **关键约束**：Windows 会在 `WH_KEYBOARD_LL` 回调耗时超过系统阈值（通常 300ms~1000ms）时静默卸载钩子，因此**绝对禁止**在 Hook 线程执行 `sleep`、鼠标释放等待或跨进程 `SendMessageTimeoutW`。

系统采用**双后台线程 + 进程内只读快照**架构：

```text
[ GPUI UI 线程 ]
   Workspace::publish_api_state() ──(每帧增量更新)──> api::WINDOWS (parking_lot::Mutex)
   SettingsStore 变更监听 (follow_dialog_jump_setting)
        │ 开启 -> Watcher::start()
        │ 关闭 -> drop(Watcher) -> PostThreadMessageW(WM_QUIT)
        ▼
[ 线程 1: "dialog-jump-hook" (Win32 消息循环线程) ]
   ├── SetWinEventHook(EVENT_SYSTEM_FOREGROUND, ..., WINEVENT_OUTOFCONTEXT)
   │      ⚠ 注意：严禁传 WINEVENT_SKIPOWNPROCESS，否则收不到 ArkeWindow 前台事件！
   ├── SetWindowsHookExW(WH_KEYBOARD_LL, ...)  (仅处理 Ctrl+G)
   └── 更新 FocusTracker (纯内存、无阻塞) ──(flume::Sender<InjectJob>)──┐
                                                                        ▼
[ 线程 2: "dialog-jump-worker" (单线程串行执行器) ]
   接收 InjectJob { dialog_hwnd, target_path, source }
   ├── 1. 过滤检查：是否为外部文件对话框（排除非文件弹窗、排除 Arke --pick-file）
   ├── 2. 幂等检查：读取 ToolbarWindow32，已在目标目录则直接跳过
   ├── 3. 文件名备份：在等待鼠标释放前暂存 Edit 原始文件名
   ├── 4. 输入同步：wait_for_mouse_release() + 35ms 结算窗口 + 前台/存活二次校验
   ├── 5. 强锁注入：AttachThreadInput + SetFocus + WM_SETTEXT + VK_RETURN（最多 3 次回环校验）
   └── 6. 无损复原：还原原始文件名或清空残留路径
```

**串行 Worker 的优势**：
- 杜绝快速连续切换导致的并发注入竞态；
- 若队列中已有新的 `InjectJob`，worker 可采用 `drain` 取最新一条（coalesce），自动丢弃过时的中间跳转请求；
- `Watcher::drop` 时关闭 channel 发送端并向 Hook 线程发送 `WM_QUIT`，两个线程均可干净退出、零泄漏。

### 2. 核心设计原则与状态转移（`FocusTracker`）

将状态机设计为**纯 Rust 可单元测试结构**（不直接调用 Win32，通过参数传入窗口分类与时间戳）：

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceKind {
    /// Arke 主窗口 (`ArkeWindow`)
    Arke,
    /// 可选扩展：Windows 原生资源管理器 (`CabinetWClass` / `ExploreWClass`)
    Explorer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowClassKind {
    /// Arke 主窗口 (`ArkeWindow`)
    ArkeMain,
    /// Arke 弹出层 (`ArkeWindow.Popup`：菜单、tooltip、命令面板浮层等)
    ArkePopup,
    /// Windows 原生文件资源管理器 (`CabinetWClass` / `ExploreWClass`)
    Explorer,
    /// Win32 标准对话框 (`#32770`)
    Dialog,
    /// 其他任意应用窗口
    Other,
}

#[derive(Debug, Default)]
pub(crate) struct FocusTracker {
    /// 最近一次激活的有效路径源窗口句柄
    pub(crate) last_source_hwnd: Option<isize>,
    pub(crate) last_source_kind: Option<SourceKind>,
    /// 仅用于 Explorer 源缓存（Arke 源在触发时从 api::WINDOWS 实时读取）
    pub(crate) last_explorer_path: Option<PathBuf>,
    /// 上一个非 Popup 前台窗口句柄（用于校验严格转移链路）
    pub(crate) prev_foreground_hwnd: Option<isize>,
    /// 防抖状态：最近一次触发注入的对端对话框与时间
    pub(crate) last_trigger: Option<(isize, Instant)>,
}
```

#### 状态转移规则（`on_foreground_change`）

1. **`WindowClassKind::ArkeMain`**：
   - 更新 `last_source_hwnd = Some(hwnd)`、`last_source_kind = Some(SourceKind::Arke)`、`prev_foreground_hwnd = Some(hwnd)`。
   - **不需要**在此刻提前快照路径（避免用户停留在 Arke 内继续切换目录/标签/分屏后路径过期）。
2. **`WindowClassKind::ArkePopup`**：
   - **保持现状，不覆写 `prev_foreground_hwnd`！**
   - 原因：用户在 Arke 中打开右键菜单或下拉弹层（`ArkeWindow.Popup`）后直接点击旁边的 `#32770` 对话框，逻辑上仍属于“从 Arke 切回对话框”，不应被 `ArkeWindow.Popup` 破坏转移链路。
3. **`WindowClassKind::Explorer`**（仅当启用可选 Explorer 源时，否则按 `Other` 处理）：
   - 更新 `last_source_hwnd`、`last_source_kind = Some(SourceKind::Explorer)`、`prev_foreground_hwnd`。
   - 若关闭 Explorer 源，走 `Other` 分支。
4. **`WindowClassKind::Dialog`（`#32770`）**：
   - **严格转移链路校验**：检查 `prev_foreground_hwnd.is_some() && prev_foreground_hwnd == last_source_hwnd`。
   - **防抖去重校验**：若 `last_trigger` 为同一 `dialog_hwnd` 且距今 `< 300ms`，跳过。
   - **无论是否触发，均立刻更新 `prev_foreground_hwnd = Some(hwnd)`**，确保在 `#32770` 内部反复点击/获焦不会二次触发。
   - 若校验通过，更新 `last_trigger = Some((hwnd, now))`，返回 `Some((source_hwnd, source_kind))` 交由路径解析器获取最新目录。
5. **`WindowClassKind::Other`**：
   - 仅更新 `prev_foreground_hwnd = Some(hwnd)`（不清除 `last_source_hwnd`，以便用户在分屏场景下依然可以使用 `Ctrl + G` 手动拉取最近一次有效的 Arke 目录；但自动切回链路已断开，下次从该其他应用切入 `#32770` 时保持静默）。

#### `Ctrl + G` 手动模式路径查找策略
- 优先使用 `last_source_hwnd`（需校验 `IsWindow` 仍存活）：
  - 若 `last_source_kind == Some(SourceKind::Arke)`，实时调用 `api::active_folder_for_hwnd(source_hwnd)`。
- 若 `last_source_hwnd` 为空或对应窗口已关闭：
  - **推荐策略（兼顾 Strict MRU 与分屏体验）**：在手动按下 `Ctrl + G` 时，用户表达了明确的显式意图；若存在打开中的 `ArkeWindow`，可调用 `api::active_folder_of_frontmost()` 取 Z-order 最靠前的 `ArkeWindow` 当前目录；若无任何打开的 Arke 窗口或当前不是文件夹，则静默返回 `false`。
  - *（注意：切回自动模式严格遵守 Strict MRU，绝不做 Z-order 兜底猜测。）*

---

## 四、 关键实现机制与边界守卫

### 1. 进程内实时路径解析（扩展 `crates/app/src/api.rs`）

Arke 的 `Workspace::publish_api_state`（位于 `crates/app/src/workspace.rs`）每帧都会向 `api::WINDOWS` 同步每个窗口的 `hwnd`、`active_tab`、`active_pane` 与 `Location`。

在 `crates/app/src/api.rs` 新增两个只读辅助函数（避免序列化 JSON 与遍历 `selection`）：

```rust
/// Returns the active filesystem folder of the Arke window `hwnd`, if its active pane shows a
/// local folder, drive root, or UNC share (`Location::Folder`). Remote URLs, Home, Recycle Bin,
/// Search results, and Settings pages return `None`.
pub(crate) fn active_folder_for_hwnd(hwnd: isize) -> Option<PathBuf> {
    let state = WINDOWS
        .lock()
        .values()
        .find(|p| p.state.hwnd == hwnd)
        .map(|p| Arc::clone(&p.state))?;
    active_folder_of(&state)
}

/// Returns `(hwnd, folder)` of the frontmost `ArkeWindow` in Windows Z-order that is currently
/// showing a filesystem folder (`Location::Folder`). Used only by explicit manual `Ctrl+G`.
pub(crate) fn active_folder_of_frontmost() -> Option<(isize, PathBuf)> {
    let order = z_order();
    let snap = snapshot(Some(&order));
    snap.windows
        .into_iter()
        .find_map(|w| active_folder_of(&w).map(|p| (w.hwnd, p)))
}

fn active_folder_of(window: &WindowState) -> Option<PathBuf> {
    let tab = window.tabs.get(window.active_tab)?;
    let pane = tab.panes.get(tab.active_pane)?;
    match &pane.location {
        Location::Folder(path) => Some(path.clone()),
        Location::Remote(_)
        | Location::Search { .. }
        | Location::Home
        | Location::RecycleBin
        | Location::Other => None,
    }
}
```

> **锁纪律（Clippy `significant_drop_tightening`）**：`active_folder_for_hwnd` 中拿到 `Arc<WindowState>` 后立即释放 `WINDOWS` 锁，不持有锁执行后续逻辑。

### 2. 对话框合法性双重守卫（防误触 & 防自劫持）

在执行任何消息发送前，必须通过以下两道守卫：

1. **排除 Arke 自有对话框（`is_arke_owned_dialog`）**：
   - `GetWindowThreadProcessId(dialog_hwnd, Some(&raw mut dlg_pid))`：若 `dlg_pid == std::process::id()`，跳过。
   - `GetWindow(dialog_hwnd, GW_OWNER)`：若存在非空 `owner`，检查 `owner` 的 PID 是否等于 `std::process::id()`，或 `owner` 的窗口类是否为 `"ArkeWindow"`。
   - **目的**：`crates/app/src/win_picker.rs` 通过子进程 `arke.exe --pick-file` 弹出模态 `IFileOpenDialog`，其 `owner` 正是 `ArkeWindow`。此守卫保证点击 Arke 设置里的“浏览（Browse）”按钮时绝不会触发自动跳转。
2. **验明正身为通用文件对话框（`is_common_file_dialog`）**：
   - 仅类名为 `#32770` 远远不够（属性页、旧版弹窗、确认框也是 `#32770`）。
   - 在 `EnumChildWindows(Some(dialog_hwnd), ...)` 中，至少满足以下之一才判定为文件对话框：
     - 存在 `SHELLDLL_DefView` 或 `DUIViewWndClassName` 子窗口（Vista+ 现代 `IFileDialog` 及经典 `GetOpenFileName` 均具备 `SHELLDLL_DefView`）；
     - **或者**存在 `ComboBoxEx32` 且包含标准文件对话框控件 ID（`0x47c / 1148`、`1152`）。
   - 同时查找目标文件名 `Edit` 控件：
     - 必须满足 `IsWindowVisible(hwnd).as_bool() && IsWindowEnabled(hwnd).as_bool()`；
     - 显式排除搜索框（类名含 `"SearchEditBox"`，或其直接父窗口类名为 `"SearchEditBoxWrapperClass"`）以及顶部地址栏隐藏编辑框（`GetDlgCtrlID(hwnd) == 41477`）；
     - 优先匹配 `class_name == "Edit"` 且 `ctrl_id` 为 `0x47c (1148)` / `1001` / `1152`；次选匹配父窗口类名为 `"ComboBox"` 或 `"ComboBoxEx32"` 的可见 `Edit`。**不再回退到任意未知 `Edit`**。

### 3. 多语言地址栏解析与幂等检查

读取 `ToolbarWindow32` / `Breadcrumb Parent` 的窗口文本后，采用语言无关的 `parse_toolbar_address(text: &str) -> Option<String>`：

1. 若文本包含 `": "`（Windows 几乎所有语言均为 `"<本地化地址标签>: <路径>"`，如 `"Address: D:\Work"`、`"地址: D:\Work"`、`"位址: D:\Work"`、`"アドレス: D:\Work"`、`"Adresse: D:\Work"`），取第一个 `": "` 之后的子串并 `trim()`；否则取原文本 `trim()`。
2. 校验提取结果是否表示有效文件系统路径：
   - 盘符路径：前 3 字符匹配 `[A-Za-z]:\`（或恰好 2 字符 `[A-Za-z]:`）；
   - 或 UNC 路径：以 `\\` 开头且后续含主机名。
   - 若为虚拟位置名称（如 `"Desktop"` / `"桌面"` / `"This PC"` / `"此电脑"` / `"Quick access"`），返回 `None`（不与物理路径误判相等，允许正常跳转）。
3. **路径标准化比较（`paths_equal`）**：
   - 统一去除尾部 `\` 后做 ASCII 大小写无关比较（`eq_ignore_ascii_case`，注意盘符根 `C:\` 去除后为 `C:`，与 `C:` 一致，不会变空）。

### 4. 输入同步、焦点强锁与回环校验

完整保留并加固 ZenSearch 验证通过的时序方案：

1. **前置文件名备份**：
   - 在 `wait_for_mouse_release()` **之前**读取 `edit_hwnd` 当前文本（`WM_GETTEXTLENGTH` + `WM_GETTEXT`，超时 `200ms`，`SMTO_ABORTIFHUNG | SMTO_NORMAL`）。
   - `extract_preservable_filename`：仅当文本非空、不含 `\` 且不含 `:` 时保留；否则置空字符串。
2. **等待鼠标释放与结算**：
   - 轮询 `GetAsyncKeyState(i32::from(VK_LBUTTON.0)) < 0`，每 `10ms` 一次，上限 `400ms`。
   - 释放后追加 `35ms` 缓冲，让 `DirectUIHWND` / `SysTreeView32` 完成 `WM_LBUTTONUP` 与内部选区/焦点迁移。
   - **安全二次确认**：等待结束后，重新检查 `IsWindow(Some(dialog_hwnd)).as_bool()` 且 `GetForegroundWindow() == dialog_hwnd`；若用户已切走或关闭对话框，立即中止！
3. **跨线程焦点强锁（RAII `ThreadInputAttachment`）**：
   ```rust
   struct ThreadInputAttachment {
       cur: u32,
       target: u32,
       attached: bool,
   }
   // Drop 时必定调用 AttachThreadInput(self.cur, self.target, false)
   ```
   - 附加到 `GetWindowThreadProcessId(dialog_hwnd, None)`，调用 `SetFocus(Some(edit_hwnd))`，立即解绑，并发送 `EM_SETSEL (0x00B1)` (`WPARAM(0)`, `LPARAM(-1)`)。
4. **路径规范化与最多 3 次自愈注入**：
   - `normalize_dir_path(path)`：转为字符串并确保末尾带 `\`（例如 `D:\Work` → `D:\Work\`，`D:\` 保持 `D:\`，`\\srv\share` → `\\srv\share\`），防止对话框将其当成文件名提交。
   - 循环 `attempt in 1..=3`：
     - 重新 `find_dialog_filename_edit(dialog_hwnd)`（容忍句柄变动）；
     - `focus_dialog_edit(dialog_hwnd, current_edit)`；
     - `SendMessageTimeoutW(current_edit, WM_SETTEXT, ..., 200ms)`：
       - 若返回 `0` 且 `GetLastError() == ERROR_ACCESS_DENIED`（UIPI 权限隔离，目标对话框为管理员权限进程），**立即中止循环**并 `log::debug!` 记录，避免无谓重试。
     - 再次 `focus_dialog_edit` 确保焦点在 `Edit` 上；
     - `PostMessageW(Some(current_edit), WM_KEYDOWN, WPARAM(usize::from(VK_RETURN.0)), LPARAM(0))`；
     - `PostMessageW(Some(current_edit), WM_KEYUP, WPARAM(usize::from(VK_RETURN.0)), LPARAM(0))`；
     - 等待 `90ms`，读取 `get_dialog_current_path(dialog_hwnd)`：
       - 若与目标路径相等 → 标记 `navigated = true` 并退出；
       - 若对话框不存在 `ToolbarWindow32`（旧式无面包屑对话框）→ 视为完成并退出。
5. **文件名无损复原**：
   - 重新获取 `final_edit` 并读取其当前文本；
   - 若 `original_filename` 非空，或 `current_edit_text` 包含 `\` / `:`（残留了刚注入的路径），则用 `SendMessageTimeoutW(WM_SETTEXT)` 写回 `original_filename`（原为空则清空）。

### 5. `Ctrl + G` 低级键盘钩子（参考 `win_e_hook.rs` 范式）

复用 `win_e_hook.rs` 经过验证的纯函数判定与按键吞吐模式，防止按住连发或松开键泄露给对话框：

```rust
const VK_G: u16 = 0x47;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HotkeyVerdict {
    Pass,
    Trigger,
    Swallow,
}

/// 纯函数判定器（便于单元测试）：仅拦截纯 Ctrl+G，忽略 Ctrl+Shift+G / Ctrl+Alt+G / Ctrl+Win+G。
pub(crate) fn decide_hotkey(
    vk: u16,
    down: bool,
    held: impl Fn(u16) -> bool,
    g_taken: &mut bool,
) -> HotkeyVerdict { ... }
```

- 在 `on_key` 回调中，先调用 `decide_hotkey`（纯整数和位运算）；仅当返回 `HotkeyVerdict::Trigger` 时才调用 `GetForegroundWindow()` + `GetClassNameW` 校验是否为 `#32770` 且非 Arke 自有对话框！
- 若前台不是 `#32770`，则将 `G_TAKEN` 复位为 `false` 并 `CallNextHookEx` 原样放行——保证 99.99% 的日常按键路径**零 Win32 窗口查询开销**。

---

## 五、 `windows = "0.62"` API 迁移速查（避免照搬 ZenSearch 0.58 报错）

ZenSearch 使用 `windows 0.58`，而 Arke 使用 **`windows 0.62`** 与 **Rust Edition 2024**。实现 Agent 务必注意以下签名差异：

| API | `windows 0.58`（ZenSearch） | `windows 0.62`（Arke 必须这样写） |
| :--- | :--- | :--- |
| `BOOL` | `windows::Win32::Foundation::BOOL` | `windows::core::BOOL` |
| `IsWindow` | `IsWindow(hwnd)` | `IsWindow(Some(hwnd))` |
| `SetFocus` | `SetFocus(edit_hwnd)` | `SetFocus(Some(edit_hwnd))` |
| `EnumChildWindows` | `EnumChildWindows(dlg, Some(cb), lparam)` | `EnumChildWindows(Some(dlg), Some(cb), lparam)` |
| `PostMessageW` | `PostMessageW(hwnd, msg, w, l)` | `PostMessageW(Some(hwnd), msg, w, l)` |
| `GetClassNameW` | `GetClassNameW(hwnd, &mut buf)` | 返回 `i32`，先用 `usize::try_from(len).ok()` 安全切片，禁止裸 `as usize` 触发 lint |
| `SetWinEventHook` 判空 | `hook.0 != std::ptr::null_mut()` | `hook.is_invalid()`（参考 `win_takeover.rs:1086`） |
| `GetParent` / `GetWindow` | `GetParent(hwnd)` | `GetWindow(hwnd, GW_OWNER)` 返回 `Result<HWND>` |
| 原始指针构造 | `&mut ctx as *mut _ as isize` | `(&raw mut ctx) as isize`（符合 Rust 2024 习惯，见 `api.rs:902`） |
| `unsafe extern "system" fn` | 函数体内直接调 unsafe | **必须**包裹 `unsafe { ... }` 并写 `// SAFETY: ...`（`unsafe_op_in_unsafe_fn = "deny"` & `undocumented_unsafe_blocks = "deny"`） |

**`crates/app/Cargo.toml` 依赖检查结论**：  
经核对，`Win32_Foundation`、`Win32_UI_Accessibility`、`Win32_UI_WindowsAndMessaging`、`Win32_UI_Input_KeyboardAndMouse`、`Win32_System_Threading`、`Win32_System_LibraryLoader`、`Win32_UI_Shell`、`Win32_System_Com`、`Win32_System_Variant` 均已启用。**本功能无需在 `Cargo.toml` 添加任何新 crate 或新 feature。**

---

## 六、 文件级改动清单（File-by-File Implementation Plan）

按以下顺序实施，保持每一处都契合 Arke 既有代码风格：

### 1. `crates/app/src/api.rs`
- 新增 `pub(crate) fn active_folder_for_hwnd(hwnd: isize) -> Option<PathBuf>`
- 新增 `pub(crate) fn active_folder_of_frontmost() -> Option<(isize, PathBuf)>`
- 新增私有辅助 `fn active_folder_of(window: &WindowState) -> Option<PathBuf>`
- 补充单元测试：验证 `Location::Folder` 返回路径，而 `Location::Remote` / `Home` / `RecycleBin` / `Search` / `Other` 均返回 `None`；验证多 Tab、分屏双栏取的是 `active_tab` -> `active_pane`。

### 2. 新增 `crates/app/src/win_dialog_jump.rs`
- 公开接口（仿 `win_takeover::Watcher` 与 `win_e_hook::Hook`）：
  - `pub(crate) struct Watcher`
  - `impl Watcher { pub(crate) fn start() -> Option<Self> }`
  - `impl Drop for Watcher`（关闭 worker channel + `PostThreadMessageW(hook_tid, WM_QUIT, ...)`）
- 内部结构与模块划分：
  - **纯逻辑层**：`FocusTracker`、`SourceKind`、`WindowClassKind`、`HotkeyVerdict`、`decide_hotkey`、`parse_toolbar_address`、`normalize_dir_path`、`paths_equal`、`extract_preservable_filename`
  - **Win32 探测层**：`window_class_name`、`classify_window`、`is_arke_owned_dialog`、`is_common_file_dialog`、`find_dialog_filename_edit`、`get_dialog_current_path`、`get_window_text`
  - **Win32 注入层**：`wait_for_mouse_release`、`ThreadInputAttachment`、`focus_dialog_edit`、`inject_path_to_dialog`
  - **钩子与线程层**：`run_hook_loop`（安装 `SetWinEventHook(WINEVENT_OUTOFCONTEXT)` + `SetWindowsHookExW(WH_KEYBOARD_LL)` 并运行 `GetMessageW`）、`run_worker_loop`（串行消费 `InjectJob`，支持队列合并只执行最新任务）
  - **单元测试**：`#[cfg(test)] mod tests` 覆盖全部纯逻辑与状态机边界。

### 3. `crates/app/src/settings.rs`
- 在 `struct Settings` 的系统集成区（`win_e_opens_arke` 附近，约 L645）新增：
  ```rust
  /// Automatically jump Windows Open/Save file dialogs (`#32770`) to the folder shown in Arke
  /// when switching back from Arke, or on Ctrl+G inside the dialog (`win_dialog_jump.rs`).
  pub(crate) dialog_jump: bool,
  ```
- 在 `Default for Settings` 中设置默认值 `dialog_jump: false`（与 `open_folders_in_arke`、`win_e_opens_arke` 保持一致的保守默认策略；若产品希望开箱即用可改为 `true`，但需同步更新 `settings.rs` 相关测试）。

### 4. `crates/app/src/main.rs`
- 模块声明区添加 `mod win_dialog_jump;`。
- 在 `app_main` 中（约 L632，`if bench.is_none()` 块内，**不要求 `resident` 或 `keep_running`**，因为只要有 Arke 窗口打开，切回 `#32770` 就有意义）调用：
  ```rust
  follow_dialog_jump_setting(&store, cx);
  ```
- 实现 `fn follow_dialog_jump_setting(store: &gpui::Entity<settings::SettingsStore>, cx: &mut App)`：
  - 支持 `ARKE_NO_DIALOG_JUMP` 环境变量禁用（供测试隔离）；
  - 参考 `follow_takeover_setting`（L852-887）：在 `store.loaded` 为 `true` 后，依据 `store.settings.dialog_jump` 启动或销毁 `Rc<RefCell<Option<win_dialog_jump::Watcher>>>`；
  - 在 `cx.on_app_quit` 中确保退出前显式释放 `Watcher`，避免进程退出时悬挂钩子。

### 5. `crates/app/src/workspace_settings.rs`
- 在 Settings 页面系统/启动分组（约 L1736，`win-e` 卡片之后）新增开关卡片：
  ```rust
  card(
      Icon::External, // 或 Icon::Folder / Icon::ArrowRight，按现存 assets/icons 选用
      t!("settings.dialog_jump"),
      Some(t!("settings.dialog_jump_desc")),
      switch("dialog-jump", s.dialog_jump, |st| {
          st.dialog_jump = !st.dialog_jump;
      }),
  ),
  ```

### 6. `crates/app/locales/app.yml`
- 在 `settings.win_e_desc` 之后新增 7 语言条目：
  - `settings.dialog_jump`（如 `en`: `"Jump file dialogs to Arke's folder"` / `zh-CN`: `"文件对话框自动跳转到 Arke 目录"` / `zh-TW` / `de` / `es` / `pt-BR` / `ja`）
  - `settings.dialog_jump_desc`（简明说明“从 Arke 切回打开/保存对话框时自动切换目录，或在对话框内按 Ctrl+G 跳转；管理员权限程序的对话框受系统安全限制无法跳转”）

---

## 七、 边界处理与容错矩阵

| 场景 | 触发条件 | 系统行为与保护策略 |
| :--- | :--- | :--- |
| **非 Arke 窗口切入** | 上一前台窗口为浏览器/IDE/记事本等 | **安全静默**：`prev_foreground_hwnd != last_source_hwnd`，绝不篡改对话框路径 |
| **Arke 弹出层短暂获焦** | 焦点路径为 `ArkeWindow -> ArkeWindow.Popup -> #32770` | **保持链路**：`ArkeWindow.Popup` 不覆盖 `prev_foreground_hwnd`，切回对话框仍可平滑跳转 |
| **Arke 停留在虚拟/远程页** | 活动 Pane 为 Home / 回收站 / 搜索结果 / SFTP / WebDAV / Settings | **安全静默**：`api::active_folder_for_hwnd` 返回 `None`，不注入非法路径 |
| **Arke 自身设置浏览弹窗** | `arke.exe --pick-file` 弹出的 `#32770`（Owner 为 `ArkeWindow`） | **强制排除**：`is_arke_owned_dialog` 命中，保持静默，不劫持设置页“浏览” |
| **非文件类 `#32770` 弹窗** | 属性框、MessageBox、旧版配置对话框 | **强制排除**：无 `SHELLDLL_DefView` / `DUIViewWndClassName` / 文件框结构，静默跳过 |
| **源窗口已关闭** | `last_source_hwnd` 对应窗口已销毁 | **安全静默**：`IsWindow` 返回 false，自动清空无效 MRU |
| **路径幂等** | 对话框已在目标目录 | **跳过注入**：经多语言 `parse_toolbar_address` 归一化比对一致，避免界面闪烁 |
| **高频重复焦点事件** | 同一 `#32770` `< 300ms` 内重复触发 | **防抖去重**：`last_trigger` 截断 + worker 队列只取最新任务 |
| **全域点击子控件夺焦** | 点击列表空白处 `DirectUIHWND`、导航树 `SysTreeView32` 或地址栏 | **时序同步与强锁**：等待 `VK_LBUTTON` 物理释放 + 35ms 缓冲 + 二次前台校验 + `AttachThreadInput` 强锁 `Edit` 焦点 |
| **等待期间用户切走/关窗** | 鼠标释放等待期间前台已不再是目标 `#32770` | **安全中止**：重新核对 `GetForegroundWindow() == dialog_hwnd`，不向后台窗口发键 |
| **路径语法歧义** | 路径无尾部 `\`（如 `D:\Work`） | **路径规范化**：强制追加 `\`（`D:\Work\`），确保对话框进入目录而非当作文件名 |
| **待保存文件名保护** | 保存对话框 `Edit` 中已有 `report.docx` | **备份与无损还原**：等待鼠标释放前即备份，跳转成功后复原；若原为空且残留注入路径则清空 |
| **目标为管理员权限程序** | 高完整性（Elevated）应用的 `#32770` | **UIPI 优雅降级**：检测到 `ERROR_ACCESS_DENIED` 立刻终止重试并 debug 日志记录，不卡顿不报错 |
| **非对话框按 `Ctrl + G`** | 在 VS Code、浏览器、Arke 主窗中按 `Ctrl + G` | **零干扰放行**：`CallNextHookEx` 原样透传 |

---

## 八、 可选 Phase 2 扩展：兼容原生 Explorer（`CabinetWClass`）

若后续需要同时支持“从原生 Windows 资源管理器切回文件对话框也自动跳转”：
1. 在 `Settings` 中新增子开关 `dialog_jump_explorer: bool`（默认 `false`，避免与 ZenSearch / Listary 双重注入冲突）。
2. 路径提取不可简单照搬 ZenSearch 仅遍历首个匹配 `HWND` 的方式，需兼容 **Windows 11 多标签 Explorer**：
   - 先通过 `FindWindowExW(cabinet_hwnd, None, w!("ShellTabWindowClass"), None)` 获取当前激活标签对应的 `ShellTabWindowClass` 子窗口句柄；
   - 在独立 STA 线程通过 `IShellWindows` 枚举 `IWebBrowser2`，经 `IServiceProvider::QueryService(&SID_STopLevelBrowser)` → `IShellBrowser` → `IOleWindow::GetWindow()` 精确匹配激活标签的 `ShellTabWindowClass` 句柄（参考 `win_takeover.rs` 中已有的 `IShellBrowser` / `IFolderView2` 读取逻辑），从而准确读取 Win11 当前活动 Tab 的目录。
   - **重要**：该 COM 查询必须在 worker 线程（独立 STA）执行，绝不可放在 `win_event_proc` 回调或主 UI 线程中执行。

---

## 九、 测试与验收清单

开发 Agent 完成编码后，必须依次通过以下验证：

### 1. 静态检查与代码规范（零警告）
```powershell
cargo clippy --workspace --all-targets
```
重点自检：
- 无 `std::sync::Mutex` / `RwLock` / `Condvar`（统一 `parking_lot`）；
- 所有 `unsafe` 块均有 `// SAFETY:` 注释，所有 `unsafe fn` 均有 `# Safety` 文档段；
- 非测试代码中无 `unwrap()` / `expect()` / `panic!()` / `dbg!()` / `println!()`；
- `pub(crate)` 与模块顶部均有清晰注释。

### 2. 单元测试（全自动，无 GUI 依赖）
```powershell
cargo test -p arke-app win_dialog_jump
cargo test -p arke-app api::
cargo test -p arke-app settings::
```
必须覆盖：
- `FocusTracker` 全部状态转移（`Arke -> Dialog`、`Other -> Dialog`、`Arke -> ArkePopup -> Dialog`、`Dialog -> Dialog`、`< 300ms` 防抖、`>= 300ms` 放行）；
- `decide_hotkey` 全部组合（`Ctrl+G` 触发/连发/抬起、`Ctrl+Shift+G` / `Ctrl+Alt+G` / `Win+G` 放行）；
- `parse_toolbar_address` 多语言用例（英/简中/繁中/日/德、盘符根、UNC、虚拟名称）；
- `normalize_dir_path` & `paths_equal` & `extract_preservable_filename`；
- `api::active_folder_for_hwnd` 对 `Folder` / `Remote` / `Home` / `RecycleBin` / `Search` / `Other` 与多 Tab / Split Pane 的正确筛选。

### 3. 真实 `#32770` 对话框集成测试（手工 / `#[ignore]` 实机验证）
在 `win_dialog_jump.rs` 中提供 `#[ignore = "spawns a real Windows file dialog and moves the mouse"]` 的实机测试（参照 `D:\ZenSearch\tests\test_path_injection.rs`）：
```powershell
cargo test -p arke-app live_dialog_jump -- --ignored --nocapture
```
验证用例：
1. 真实 `IFileOpenDialog`：点击 `DirectUIHWND` 空白区 → 跳转成功；
2. 真实 `IFileOpenDialog`：点击 `SysTreeView32` 导航树 → 跳转成功；
3. 真实 `IFileOpenDialog`：点击 `ToolbarWindow32` 顶部地址栏 → 跳转成功；
4. 真实 `IFileSaveDialog`：输入预设文件名 `QuarterlyReport.docx` 后跳转 → 目录切换且文件名完整保留；
5. 幂等校验：已在目标目录再次注入 → 立即返回 `true` 且不发送回车；
6. 排除校验：Owner 为当前进程的 `#32770`（模拟 `win_picker`）不被注入；普通无 `SHELLDLL_DefView` 的 `#32770` 不被注入。
