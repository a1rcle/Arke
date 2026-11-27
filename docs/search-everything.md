# Everything 检索引擎接入说明

Arke 已将生产环境的文件名检索后端从自建本地索引 / `arke-indexd` 替换为 **Everything SDK（`Everything64.dll`）**，保留 Arke 原有的搜索界面、筛选语法、正文内容搜索与基于使用习惯（Frecency）的排序能力。

## 1. 搜索范围与分流规则

| 搜索范围 | 后端引擎 | 说明 |
| --- | --- | --- |
| **当前文件夹**（`Scope::Here`） | Arke 内存过滤 | 纯内存模糊筛选当前已加载列表，无磁盘 I/O。 |
| **含子文件夹**（`Scope::Subfolders`） | Everything / 原遍历自动分流 | 当目录位于本地磁盘且不含 `content:` 时，由 Everything 通过 `path:"<dir>\"` 限定范围检索；含 `content:` 正文检索、网络/UNC 路径或 Everything 未就绪时，自动回退到 [`arke_core::SearchService`](../crates/core/src/search.rs) 遍历。 |
| **整台电脑**（`Scope::Computer`） | Everything | 全部由 Everything 检索；支持在 `Settings › Search` 中跳过构建目录（`search_skip_build_dirs`）及排除指定驱动器/文件夹。 |

## 2. 核心架构与模块

### 2.1 运行时动态加载驱动（`crates/app/src/win_everything.rs`）

* **零硬链接依赖**：使用 Win32 `LoadLibraryW` / `GetProcAddress` 在运行时按需加载 `Everything64.dll`（优先检查可执行文件同级目录、`tools/`、仓库 `third_party/everything/`，最后回退系统搜索路径）。缺失 DLL 时优雅返回 `EverythingState::DllMissing`，不会导致主程序启动崩溃或阻塞单元测试。
* **线程安全串行化**：Everything SDK 内部共享全局查询缓冲区，所有 IPC 查询与状态调用统一由 `parking_lot::Mutex` 串行化保护。
* **服务探测与自动静默拉起**：
  * `is_everything_running` 同时检测 Everything 1.4（`EVERYTHING_TASKBAR_NOTIFICATION`）与 1.5（`EVERYTHING_TASKBAR_NOTIFICATION_1.5`）窗口类，并用非阻塞 IPC 查询兜底。
  * 未运行时，`probe_everything_executable` 依次探测注册表（`HKCU/HKLM\Software\voidtools\Everything`、`App Paths\Everything.exe`）、`PATH`、常见安装目录及可执行文件同级目录，并以 `Everything.exe -startup` 静默启动后台服务，轮询等待 `Everything_IsDBLoaded` 就绪。
* **完整元数据单次返回**：请求标志包含 `FILE_NAME | PATH | SIZE | DATE_MODIFIED | ATTRIBUTES`，一次 IPC 即可拿到文件大小、修改时间（`FILETIME` 转 `SystemTime`）、目录标志与隐藏属性，无需对每条结果额外执行 `fs::metadata`。

### 2.2 Arke 语法 → Everything 语法转译（`crates/core/src/everything_query.rs`）

复用 [`arke_core::index_query::Query::parse`](../crates/core/src/index_query.rs) 解析结果，保证搜索框筛选器芯片（类型 / 大小 / 日期）继续生效：

| Arke 语法 / 设置 | Everything 转译结果 | 备注 |
| --- | --- | --- |
| 子文件夹 `root` | `path:"D:\Work\"` | 强制补齐尾部 `\` 防止误匹配兄弟前缀目录；客户端再次校验严格父子关系。 |
| `file:` / `folder:` | `file:` / `folder:` | 直接映射。 |
| `ext:pdf;docx` / `*.png` | `ext:pdf;docx` | 直接映射。 |
| `type:image;folder` | `<ext:png;jpg;...\|folder:>` | 按 [`arke_core::filetype`](../crates/core/src/filetype.rs) 扩展名集合展开，保证分类语义一致。 |
| `size:>10mb` / `size:1mb..10mb` | `size:>=10485761` / `size:1048576..10485760` | 精确字节区间映射。 |
| `dm:today` / `dm:7d` / `dm:2026-11` | `dm:YYYY-MM-DD` / `dm:>=YYYY-MM-DD` | 转为本地 ISO-8601 日期区间，并结合客户端 `Query::accepts` 精确校验小时级筛选（如 `dm:24h`）。 |
| `'report` | `report` | 去除 Arke 的精确匹配前导单引号标记。 |
| `search_skip_build_dirs` | `!\node_modules\ !\.git\ ...` | 复用 [`arke_core::index::BUILD_DIRS`](../crates/core/src/index.rs) 列表。 |
| `search_excluded` / `search_skipped_drives` | `!path:"C:\Excluded\"` | 在「整台电脑」查询中自动排除。 |

### 2.3 排序与候选上限（`crates/app/src/global_search.rs`）

* **排序沿用 Arke 原有机制**：不引入额外的第三方历史文件或硬编码 `.lnk` 排除规则。Everything 检索返回的条目通过 `IndexHit::from_external(..., 0)` 构造后，统一交由 [`SearchHistory`](../crates/app/src/search_history.rs) + [`arke_core::rank::merge`](../crates/core/src/rank.rs) 按 **匹配等级（精确 > 前缀 > 包含 > 模糊）+ 访问频率/时效（Frecency）+ 路径权重** 精排。
* **纯筛选器查询**：无关键词时采用 `EVERYTHING_SORT_DATE_MODIFIED_DESCENDING`，保持“最新修改在前”的体验；有关键词时采用 `EVERYTHING_SORT_RUN_COUNT_DESCENDING` 提取候选池再精排。
* **候选池上限保护（`everything_candidate_cap`）**：通过 `limit.saturating_mul(3).min(5_000).max(limit)` 计算候选数量，确保全局搜索（`limit = 1_000` → 拉取 `3_000` 候选精排）与子文件夹搜索（`limit = 5_000` → 拉取 `5_000` 候选）均不越界、不会触发 `Ord::clamp` 的 `min > max` panic。

### 2.4 原有索引与设置页调整

* **生产模式停用自建索引**：`Roots::LocalDrives` 下不再启动本地 `IndexService`、`NetIndex` 与 `arke-indexd` 后台服务。`GlobalSearch::local_service()` 返回 `None`，文件夹大小计算（`explorer_sizes.rs`、`properties.rs`）自动平滑回退为目录遍历。
* **测试模式兼容**：`#[cfg(test)]` 中的 `Roots::Fixed` 仍走内存 `FakeFs` 索引，保持原有 UI 自动化测试稳定。
* **设置页与搜索面板**：`Settings › Search` 与搜索下拉面板展示 Everything 实时连接状态（已连接 / 启动中 / 缺少 DLL / 未安装 / 未运行），提供一键 **重新连接** 与 **重建数据库（`Everything_RebuildDB`）** 操作。

## 3. 构建与依赖部署

* 仓库内置 [third_party/everything/Everything64.dll](../third_party/everything/Everything64.dll)（MIT 许可证，来自 voidtools Everything SDK）。
* [`crates/app/build.rs`](../crates/app/build.rs) 中的 `copy_everything_dll` 会在构建时自动将 `Everything64.dll` 复制到 `target/<profile>/`（与 `arke.exe` 同级）及测试二进制输出目录，兼容 Cargo 1.100 构建目录布局。
