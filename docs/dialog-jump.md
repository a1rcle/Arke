# 文件对话框自动跳转（Dialog Quick Switch）说明

Arke 内置了系统通用文件对话框（`#32770` 打开／保存窗口）目录快速跳转功能。开启后，无需手动层层点击路径，即可将第三方软件弹出的文件对话框自动或一键切换到 Arke 当前浏览的目录。

* **开关位置**：**设置 › 启动 › 文件对话框自动跳转到 Arke 目录**（`Settings › Startup › Jump file dialogs to Arke's folder`，配置键 `dialog_jump`，默认关闭，切换后立即生效）。
* **测试屏蔽变量**：设置环境变量 `ARKE_NO_DIALOG_JUMP` 可禁用本模块（用于自动化测试环境隔离）。

## 1. 双模触发方式

| 模式 | 触发条件 | 行为说明 |
| --- | --- | --- |
| **切回自动跳转（主模式）** | 焦点从 `ArkeWindow` 直接切换至外部 `#32770` 文件对话框（鼠标点击对话框任意区域或 `Alt+Tab`） | 实时读取该 `ArkeWindow` 活动标签页中活动窗格的目录并自动跳转；从浏览器、IDE 等非 Arke 窗口切入时保持静默。 |
| **`Ctrl + G` 手动跳转（辅助模式）** | 外部 `#32770` 文件对话框处于前台时，按下纯 `Ctrl + G`（不含 `Shift` / `Alt` / `Win`） | 原地拉取最近活跃的 Arke 窗口目录（若该窗口已关闭则回退到 Z-order 最靠前的 `ArkeWindow`）并跳转；非文件对话框中的 `Ctrl + G` 原样透传。 |

## 2. 核心架构与实现模块

### 2.1 进程内实时路径查询（[`crates/app/src/api.rs`](../crates/app/src/api.rs)）

复用 `Workspace::publish_api_state` 每帧增量维护的 `WINDOWS` 状态快照，无需跨进程 COM 查询：

* `active_folder_for_hwnd(hwnd)`：根据窗口句柄实时返回活动标签页中活动窗格的路径，天然支持多标签页与左右分屏双栏，避免“激活窗口后再切目录导致路径陈旧”的问题。
* `active_folder_of_frontmost()`：按 Windows Z-order 查找最靠前且正在显示文件夹的 `ArkeWindow`，供 `Ctrl + G` 手动模式在原 MRU 窗口已关闭时兜底。
* **路径类型过滤**：仅当窗格位置为 `Location::Folder(path)`（本地目录、盘符根目录、UNC 共享 `\\server\share`）时触发；位于主页、回收站、搜索结果、SFTP/WebDAV 或设置页时返回 `None` 并静默跳过。

### 2.2 双线程钩子与注入引擎（[`crates/app/src/win_dialog_jump.rs`](../crates/app/src/win_dialog_jump.rs)）

`Watcher` 负责完整生命周期（开启时启动、关闭设置或应用退出 `on_app_quit` 时发送 `WM_QUIT` 并关闭通道，安全卸载双钩子）：

1. **`dialog-jump-hook` 线程（轻量消息循环，绝不阻塞）**
   * 安装 `SetWinEventHook(EVENT_SYSTEM_FOREGROUND, ..., WINEVENT_OUTOFCONTEXT)`（**不传 `WINEVENT_SKIPOWNPROCESS`**，以便同时观测自身 `ArkeWindow` 与外部 `#32770`）。
   * 安装 `SetWindowsHookExW(WH_KEYBOARD_LL, ...)` 监听纯 `Ctrl + G`（通过纯函数 `decide_hotkey` 处理连发与松开键吞吐，99.99% 日常按键零 Win32 窗口查询开销）。
   * 维护纯状态机 `FocusTracker`：
     * **严格转移链路（Focus Transition Guard）**：仅 `ArkeWindow -> #32770` 触发自动跳转。
     * **弹出层容忍**：`ArkeWindow.Popup`（菜单、提示框等）不覆盖 `prev_foreground_hwnd`，避免在 Arke 内打开右键菜单后切回对话框时打断链路。
     * **防抖与去重**：同一 `#32770` 窗口 `< 300ms` 内不重复触发；切入 `#32770` 后立即更新 `prev_foreground_hwnd`，防止对话框内部点击反复触发。
2. **`dialog-jump-worker` 线程（单线程串行执行器）**
   * 通过 `flume` 通道接收 `InjectJob` 并合并积压任务（始终执行最新目标），杜绝并发注入竞态与低级键盘钩子超时卸载。
   * 完整注入流水线（`inject_path_to_dialog`）：
     1. **幂等检查**：通过语言无关的 `parse_toolbar_address` 读取 `ToolbarWindow32` / `Breadcrumb Parent` 当前目录（兼容各语言 Windows 前缀如 `Address: `、`地址: `、`位址: `、`アドレス: `、`Adresse: ` 及 UNC 路径），已在目标目录时直接跳过。
     2. **前置文件名备份**：在等待鼠标释放前读取 `Edit` 内容，通过 `extract_preservable_filename` 暂存纯文件名（过滤含 `\` 或 `:` 的路径片段）。
     3. **输入同步**：切回模式调用 `wait_for_mouse_release` 轮询 `VK_LBUTTON` 物理释放（上限 400ms）+ 35ms 结算缓冲，并二次校验目标对话框仍存活且位于前台。
     4. **焦点强锁与回环校验（最多 3 次）**：通过 RAII 守卫 `ThreadInputAttachment`（`AttachThreadInput`）将焦点强锁到文件名 `Edit` 并全选（`EM_SETSEL`），写入补齐尾反斜杠的目录路径（`WM_SETTEXT`）并发送 `VK_RETURN` 驱动对话框内置跳转状态机，90ms 后比对地址栏确认完成。
     5. **文件名无损复原**：跳转后通过 `restore_dialog_filename` 还原原始文件名；若原本为空但输入框残留路径片段则自动清空。

### 2.3 安全守卫与容错边界

| 场景 | 保护策略 |
| --- | --- |
| **Arke 自有设置浏览对话框** | `is_arke_owned_dialog` 检查对话框 PID 与 `GW_OWNER`（排除 [`win_picker.rs`](../crates/app/src/win_picker.rs) `arke.exe --pick-file` 弹出的 `#32770`），绝不劫持设置页“浏览”按钮。 |
| **非文件类 `#32770` 弹窗** | `find_dialog_filename_edit` 强制要求存在 `SHELLDLL_DefView`、`DUIViewWndClassName` 或 `ComboBoxEx32` 结构，并排除搜索框（`SearchEditBox*`）与隐藏地址栏框（ID `41477`），不回退匹配普通弹窗的任意 `Edit`。 |
| **管理员权限程序对话框（UIPI）** | 检测到 `WM_SETTEXT` 返回 `ERROR_ACCESS_DENIED` 时立即中止重试并记录 debug 日志，不提权 Arke、不卡顿。 |
| **源窗口已关闭 / 虚拟页面** | `IsWindow` 校验失败时自动清理无效 MRU 句柄；非文件夹页面返回 `None` 保持静默。 |

### 2.4 设置与多语言接入

* [`crates/app/src/settings.rs`](../crates/app/src/settings.rs)：新增 `Settings::dialog_jump`（默认 `false`）。
* [`crates/app/src/main.rs`](../crates/app/src/main.rs)：新增 `follow_dialog_jump_setting`，响应设置动态启停并在 `on_app_quit` 卸载钩子（不强依赖后台常驻 `keep_running`）。
* [`crates/app/src/workspace_settings.rs`](../crates/app/src/workspace_settings.rs)：在 **设置 › 启动** 分组添加 `dialog-jump` 开关卡片。
* [`crates/app/locales/app.yml`](../crates/app/locales/app.yml)：提供 `settings.dialog_jump` 与 `settings.dialog_jump_desc` 的全部 7 种语言翻译（`en` / `zh-CN` / `zh-TW` / `de` / `es` / `pt-BR` / `ja`）。

## 3. 测试与验证

```powershell
# 静态检查（零警告）
cargo clippy --workspace --all-targets -- -D warnings

# 单元测试（状态机、热键判定、多语言地址栏解析、路径归一化、API 路径筛选）
cargo test -p arke-app win_dialog_jump
cargo test -p arke-app api::
cargo test -p arke-app settings::

# 实机 #32770 对话框交互集成测试（会拉起系统对话框并模拟鼠标点击）
cargo test -p arke-app live_dialog_jump_open_and_save -- --ignored --nocapture
```
