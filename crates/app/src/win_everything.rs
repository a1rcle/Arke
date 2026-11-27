//! Runtime-loaded Everything SDK client (`Everything64.dll`) and daemon launcher.
//!
//! Unlike static import linking, `Everything64.dll` is loaded on demand with `LoadLibraryW` so
//! Arke never fails to start (or run unit tests) when the DLL is absent. All SDK calls share
//! Everything's global query buffer and are therefore serialized behind a single
//! `parking_lot::Mutex`.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use windows::Win32::Foundation::{FILETIME, HMODULE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, RegCloseKey, RegOpenKeyExW,
    RegQueryValueExW,
};
use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
use windows::core::{PCSTR, PCWSTR, s, w};

/// Everything SDK error code: Everything search client is not running.
const EVERYTHING_ERROR_IPC: u32 = 2;

/// Everything request flags (see `Everything.h`).
const EVERYTHING_REQUEST_FILE_NAME: u32 = 0x0000_0001;
const EVERYTHING_REQUEST_PATH: u32 = 0x0000_0002;
const EVERYTHING_REQUEST_SIZE: u32 = 0x0000_0010;
const EVERYTHING_REQUEST_DATE_MODIFIED: u32 = 0x0000_0040;
const EVERYTHING_REQUEST_ATTRIBUTES: u32 = 0x0000_0100;

/// Everything sort modes.
pub(crate) const EVERYTHING_SORT_DATE_MODIFIED_DESCENDING: u32 = 14;
pub(crate) const EVERYTHING_SORT_RUN_COUNT_DESCENDING: u32 = 20;

/// Win32 `FILE_ATTRIBUTE_HIDDEN`.
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0000_0002;
/// Win32 `INVALID_FILE_ATTRIBUTES` (returned when attributes are unavailable).
const INVALID_FILE_ATTRIBUTES: u32 = 0xFFFF_FFFF;

/// Difference between Windows FILETIME epoch (1601-01-01) and Unix epoch (1970-01-01) in 100ns
/// ticks.
const WINDOWS_TO_UNIX_TICKS: u64 = 116_444_736_000_000_000;

/// High-level availability state of the Everything search backend.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum EverythingState {
    /// Not probed yet or disabled.
    #[default]
    Idle,
    /// Starting `Everything.exe -startup` or waiting for its database to finish loading.
    Starting,
    /// Connected to a running Everything instance whose database is loaded.
    Ready,
    /// `Everything64.dll` could not be found or loaded.
    DllMissing,
    /// `Everything.exe` is not running and could not be found on the machine.
    NotInstalled,
    /// `Everything.exe` was found, but failed to start or its IPC did not become ready in time.
    NotRunning,
}

/// One raw item returned by Everything.
#[derive(Clone, Debug)]
pub(crate) struct EverythingItem {
    pub(crate) path: PathBuf,
    pub(crate) is_dir: bool,
    pub(crate) size: u64,
    pub(crate) modified: Option<SystemTime>,
    pub(crate) hidden: bool,
}

/// Output of an Everything query.
#[derive(Clone, Debug, Default)]
pub(crate) struct EverythingOutput {
    pub(crate) items: Vec<EverythingItem>,
}

/// Error returned when a query could not be executed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueryError {
    DllMissing,
    IpcNotRunning,
    DbLoading,
    Sdk(u32),
}

type FnSetSearchW = unsafe extern "system" fn(*const u16);
type FnSetBool = unsafe extern "system" fn(i32);
type FnSetU32 = unsafe extern "system" fn(u32);
type FnQueryW = unsafe extern "system" fn(i32) -> i32;
type FnGetU32 = unsafe extern "system" fn() -> u32;
type FnGetBool = unsafe extern "system" fn() -> i32;
type FnIsKind = unsafe extern "system" fn(u32) -> i32;
type FnGetFullPathW = unsafe extern "system" fn(u32, *mut u16, u32) -> u32;
type FnGetSize = unsafe extern "system" fn(u32, *mut i64) -> i32;
type FnGetFileTime = unsafe extern "system" fn(u32, *mut FILETIME) -> i32;
type FnGetAttributes = unsafe extern "system" fn(u32) -> u32;
type FnReset = unsafe extern "system" fn();

/// Dynamically resolved function table for `Everything64.dll`.
struct EverythingApi {
    _module: HMODULE,
    set_search_w: FnSetSearchW,
    set_match_path: FnSetBool,
    set_match_case: FnSetBool,
    set_match_whole_word: FnSetBool,
    set_regex: FnSetBool,
    set_max: FnSetU32,
    set_sort: FnSetU32,
    set_request_flags: FnSetU32,
    query_w: FnQueryW,
    get_num_results: FnGetU32,
    get_last_error: FnGetU32,
    is_folder_result: FnIsKind,
    get_result_full_path_name_w: FnGetFullPathW,
    get_result_size: Option<FnGetSize>,
    get_result_date_modified: Option<FnGetFileTime>,
    get_result_attributes: Option<FnGetAttributes>,
    is_db_loaded: Option<FnGetBool>,
    rebuild_db: Option<FnGetBool>,
    reset: Option<FnReset>,
}

// SAFETY: `HMODULE` and function pointers are immutable process-wide addresses; all calls into
// Everything's global state are serialized through `QUERY_LOCK`.
unsafe impl Send for EverythingApi {}
// SAFETY: Same as `Send` above.
unsafe impl Sync for EverythingApi {}

static API: OnceLock<Option<EverythingApi>> = OnceLock::new();
static QUERY_LOCK: Mutex<()> = Mutex::new(());

fn to_wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Resolves a symbol from `module` into a typed `extern "system"` function pointer.
///
/// # Safety
/// `T` must match the exact ABI and signature of the exported symbol `name` in `Everything64.dll`.
unsafe fn load_sym<T: Copy>(module: HMODULE, name: PCSTR) -> Option<T> {
    // SAFETY: `module` is a valid loaded DLL handle and `name` is NUL-terminated.
    let proc = unsafe { GetProcAddress(module, name) }?;
    // SAFETY: Caller guarantees `T` matches the exported C symbol's calling convention and types.
    Some(unsafe { std::mem::transmute_copy::<unsafe extern "system" fn() -> isize, T>(&proc) })
}

fn load_dll_handle() -> Option<HMODULE> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("Everything64.dll"));
        candidates.push(dir.join("tools").join("Everything64.dll"));
        // Dev runs from `target/<profile>/deps/arke-*.exe` or workspace root.
        if let Some(parent) = dir.parent() {
            candidates.push(parent.join("Everything64.dll"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(
            cwd.join("third_party")
                .join("everything")
                .join("Everything64.dll"),
        );
        candidates.push(
            cwd.join("..")
                .join("..")
                .join("third_party")
                .join("everything")
                .join("Everything64.dll"),
        );
    }

    for path in candidates {
        if !path.is_file() {
            continue;
        }
        let wide = to_wide(path.as_os_str());
        // SAFETY: `wide` is NUL-terminated; the returned handle stays loaded for process lifetime.
        if let Ok(module) = unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) } {
            log::info!("Everything64.dll loaded from {}", path.display());
            return Some(module);
        }
    }

    // Fallback: standard Windows DLL search path.
    // SAFETY: Static UTF-16 string literal; handle stays loaded for process lifetime.
    if let Ok(module) = unsafe { LoadLibraryW(w!("Everything64.dll")) } {
        log::info!("Everything64.dll loaded from system search path");
        return Some(module);
    }

    log::warn!("Everything64.dll not found");
    None
}

fn api() -> Option<&'static EverythingApi> {
    API.get_or_init(|| {
        let module = load_dll_handle()?;
        // SAFETY: Signatures match `Everything-SDK/include/Everything.h` (`__stdcall`).
        unsafe {
            Some(EverythingApi {
                _module: module,
                set_search_w: load_sym(module, s!("Everything_SetSearchW"))?,
                set_match_path: load_sym(module, s!("Everything_SetMatchPath"))?,
                set_match_case: load_sym(module, s!("Everything_SetMatchCase"))?,
                set_match_whole_word: load_sym(module, s!("Everything_SetMatchWholeWord"))?,
                set_regex: load_sym(module, s!("Everything_SetRegex"))?,
                set_max: load_sym(module, s!("Everything_SetMax"))?,
                set_sort: load_sym(module, s!("Everything_SetSort"))?,
                set_request_flags: load_sym(module, s!("Everything_SetRequestFlags"))?,
                query_w: load_sym(module, s!("Everything_QueryW"))?,
                get_num_results: load_sym(module, s!("Everything_GetNumResults"))?,
                get_last_error: load_sym(module, s!("Everything_GetLastError"))?,
                is_folder_result: load_sym(module, s!("Everything_IsFolderResult"))?,
                get_result_full_path_name_w: load_sym(
                    module,
                    s!("Everything_GetResultFullPathNameW"),
                )?,
                get_result_size: load_sym(module, s!("Everything_GetResultSize")),
                get_result_date_modified: load_sym(module, s!("Everything_GetResultDateModified")),
                get_result_attributes: load_sym(module, s!("Everything_GetResultAttributes")),
                is_db_loaded: load_sym(module, s!("Everything_IsDBLoaded")),
                rebuild_db: load_sym(module, s!("Everything_RebuildDB")),
                reset: load_sym(module, s!("Everything_Reset")),
            })
        }
    })
    .as_ref()
}

/// Returns whether the Everything IPC window is currently present (Everything 1.4 or 1.5).
pub(crate) fn is_everything_window_present() -> bool {
    // SAFETY: Static UTF-16 class names; `FindWindowW` only queries window handles.
    unsafe {
        if FindWindowW(w!("EVERYTHING_TASKBAR_NOTIFICATION"), PCWSTR::null())
            .is_ok_and(|h| !h.0.is_null())
        {
            return true;
        }
        if FindWindowW(w!("EVERYTHING_TASKBAR_NOTIFICATION_1.5"), PCWSTR::null())
            .is_ok_and(|h| !h.0.is_null())
        {
            return true;
        }
    }
    false
}

/// Returns whether Everything IPC responds right now.
pub(crate) fn is_everything_running() -> bool {
    if is_everything_window_present() {
        return true;
    }
    let Some(api) = api() else {
        return false;
    };
    let _guard = QUERY_LOCK.lock();
    // SAFETY: Serialized via `QUERY_LOCK`; `Everything_QueryW(0)` is a non-blocking IPC check.
    unsafe {
        let _ = (api.query_w)(0);
        (api.get_last_error)() != EVERYTHING_ERROR_IPC
    }
}

/// Returns whether Everything has finished loading its file database.
pub(crate) fn is_db_loaded() -> bool {
    let Some(api) = api() else {
        return false;
    };
    let _guard = QUERY_LOCK.lock();
    let Some(is_loaded) = api.is_db_loaded else {
        return true;
    };
    // SAFETY: Serialized via `QUERY_LOCK`; `Everything_IsDBLoaded` takes no parameters.
    unsafe { is_loaded() != 0 }
}

/// Probes the system for an installed `Everything.exe` binary.
pub(crate) fn probe_everything_executable() -> Option<PathBuf> {
    let reg_probes = [
        (
            HKEY_CURRENT_USER,
            r"Software\voidtools\Everything",
            "InstallPath",
            true,
        ),
        (
            HKEY_LOCAL_MACHINE,
            r"Software\voidtools\Everything",
            "InstallPath",
            true,
        ),
        (
            HKEY_LOCAL_MACHINE,
            r"Software\Microsoft\Windows\CurrentVersion\App Paths\Everything.exe",
            "",
            false,
        ),
    ];

    for (root, subkey, value_name, is_dir) in reg_probes {
        if let Some(val) = read_reg_string(root, subkey, value_name) {
            let trimmed = val.trim().trim_matches('"');
            if trimmed.is_empty() {
                continue;
            }
            let candidate = if is_dir {
                Path::new(trimmed).join("Everything.exe")
            } else {
                PathBuf::from(trimmed)
            };
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join("Everything.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    for candidate in [
        r"C:\Program Files\Everything\Everything.exe",
        r"C:\Program Files (x86)\Everything\Everything.exe",
        r"C:\Program Files\Everything 1.5a\Everything64.exe",
        r"D:\Install_path\Everything\Everything.exe",
    ] {
        let p = PathBuf::from(candidate);
        if p.is_file() {
            return Some(p);
        }
    }

    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        for rel in ["Everything.exe", r"tools\Everything.exe"] {
            let candidate = dir.join(rel);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    None
}

fn read_reg_string(root: HKEY, subkey: &str, value_name: &str) -> Option<String> {
    let wide_subkey = to_wide(OsStr::new(subkey));
    let mut handle = HKEY::default();
    // SAFETY: `wide_subkey` is NUL-terminated; `handle` is closed before returning.
    let opened = unsafe {
        RegOpenKeyExW(
            root,
            PCWSTR(wide_subkey.as_ptr()),
            None,
            KEY_READ,
            &raw mut handle,
        )
    };
    if opened.is_err() {
        return None;
    }

    let wide_val = to_wide(OsStr::new(value_name));
    let val_ptr = if value_name.is_empty() {
        PCWSTR::null()
    } else {
        PCWSTR(wide_val.as_ptr())
    };
    let mut buf = vec![0u16; 512];
    let mut byte_len = u32::try_from(buf.len() * 2).unwrap_or(u32::MAX);
    // SAFETY: `handle` is valid; `buf` holds `byte_len` bytes.
    let res = unsafe {
        RegQueryValueExW(
            handle,
            val_ptr,
            None,
            None,
            Some(buf.as_mut_ptr().cast::<u8>()),
            Some(&raw mut byte_len),
        )
    };
    // SAFETY: `handle` was opened above.
    let _ = unsafe { RegCloseKey(handle) };

    if res.is_err() || byte_len == 0 {
        return None;
    }
    let u16_len = (byte_len as usize / 2).min(buf.len());
    let text = String::from_utf16_lossy(&buf[..u16_len]);
    Some(text.trim_end_matches('\0').to_string())
}

/// Ensures `Everything64.dll` is loaded and the Everything background service is running with its
/// database loaded. Call from a background thread; may sleep briefly while waiting for startup.
pub(crate) fn ensure_service(cancel: &AtomicBool) -> EverythingState {
    if api().is_none() {
        return EverythingState::DllMissing;
    }

    if !is_everything_running() {
        let Some(exe) = probe_everything_executable() else {
            log::warn!("Everything.exe not found on this machine");
            return EverythingState::NotInstalled;
        };
        log::info!("launching Everything silently: {}", exe.display());
        if let Err(err) = Command::new(&exe).arg("-startup").spawn() {
            log::warn!("failed to launch Everything ({}): {err}", exe.display());
            return EverythingState::NotRunning;
        }
        // Poll up to 4 seconds for Everything IPC window to appear.
        for _ in 0..40 {
            if cancel.load(Ordering::Relaxed) {
                return EverythingState::Starting;
            }
            if is_everything_running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if !is_everything_running() {
            log::warn!("Everything launched, but IPC did not become ready in time");
            return EverythingState::NotRunning;
        }
    }

    // Wait briefly if Everything is still loading its database right after startup.
    for _ in 0..30 {
        if cancel.load(Ordering::Relaxed) {
            return EverythingState::Starting;
        }
        if is_db_loaded() {
            return EverythingState::Ready;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    if is_db_loaded() {
        EverythingState::Ready
    } else {
        EverythingState::Starting
    }
}

/// Requests Everything to rebuild its index database.
pub(crate) fn rebuild_db() -> bool {
    let Some(api) = api() else {
        return false;
    };
    let Some(rebuild) = api.rebuild_db else {
        return false;
    };
    let _guard = QUERY_LOCK.lock();
    // SAFETY: Serialized via `QUERY_LOCK`; `Everything_RebuildDB` takes no parameters.
    unsafe { rebuild() != 0 }
}

/// Executes an Everything query and returns matched items with full metadata.
///
/// Must be called from a background thread (blocks on Everything IPC).
pub(crate) fn execute_query(
    query: &str,
    max_results: u32,
    sort: u32,
    cancel: &AtomicBool,
) -> Result<EverythingOutput, QueryError> {
    let Some(api) = api() else {
        return Err(QueryError::DllMissing);
    };
    let trimmed = query.trim();
    if trimmed.is_empty() || cancel.load(Ordering::Relaxed) {
        return Ok(EverythingOutput::default());
    }

    let wide_query = to_wide(OsStr::new(trimmed));
    let _guard = QUERY_LOCK.lock();
    if cancel.load(Ordering::Relaxed) {
        return Ok(EverythingOutput::default());
    }

    // SAFETY: All Everything SDK calls are serialized under `QUERY_LOCK`. Buffers passed to
    // Everything are valid and properly sized for the duration of each call.
    unsafe {
        if let Some(reset) = api.reset {
            reset();
        }
        if let Some(is_loaded) = api.is_db_loaded
            && is_loaded() == 0
        {
            return if is_everything_window_present() {
                Err(QueryError::DbLoading)
            } else {
                Err(QueryError::IpcNotRunning)
            };
        }

        // MatchPath=0 lets Everything match paths only for tokens containing `\` or `path:`.
        (api.set_match_case)(0);
        (api.set_match_whole_word)(0);
        (api.set_regex)(0);
        (api.set_match_path)(0);
        (api.set_search_w)(wide_query.as_ptr());

        let mut flags = EVERYTHING_REQUEST_FILE_NAME | EVERYTHING_REQUEST_PATH;
        if api.get_result_size.is_some() {
            flags |= EVERYTHING_REQUEST_SIZE;
        }
        if api.get_result_date_modified.is_some() {
            flags |= EVERYTHING_REQUEST_DATE_MODIFIED;
        }
        if api.get_result_attributes.is_some() {
            flags |= EVERYTHING_REQUEST_ATTRIBUTES;
        }
        (api.set_request_flags)(flags);
        (api.set_max)(max_results);
        (api.set_sort)(sort);

        if (api.query_w)(1) == 0 {
            let err = (api.get_last_error)();
            return Err(if err == EVERYTHING_ERROR_IPC {
                QueryError::IpcNotRunning
            } else {
                QueryError::Sdk(err)
            });
        }

        let count = (api.get_num_results)();
        let mut items = Vec::with_capacity(count as usize);
        let mut path_buf = vec![0u16; 1024];

        for i in 0..count {
            if i.trailing_zeros() >= 6 && cancel.load(Ordering::Relaxed) {
                break;
            }
            if let Some(item) = read_result_item(api, i, &mut path_buf) {
                items.push(item);
            }
        }

        Ok(EverythingOutput { items })
    }
}

/// Reads metadata for result row `index` from Everything's active result buffer.
///
/// # Safety
/// Caller must hold `QUERY_LOCK` immediately after a successful `Everything_QueryW` call and
/// `index` must be `< Everything_GetNumResults()`.
unsafe fn read_result_item(
    api: &EverythingApi,
    index: u32,
    path_buf: &mut Vec<u16>,
) -> Option<EverythingItem> {
    // SAFETY: Caller holds `QUERY_LOCK` and guarantees `index` is within result bounds.
    unsafe {
        let buf_cap = u32::try_from(path_buf.len()).unwrap_or(1024);
        let mut len = (api.get_result_full_path_name_w)(index, path_buf.as_mut_ptr(), buf_cap);
        if len >= buf_cap {
            path_buf.resize(len as usize + 1, 0);
            let new_cap = u32::try_from(path_buf.len()).unwrap_or(u32::MAX);
            len = (api.get_result_full_path_name_w)(index, path_buf.as_mut_ptr(), new_cap);
        }
        if len == 0 {
            return None;
        }
        let full = String::from_utf16_lossy(&path_buf[..len as usize]);
        let is_dir = (api.is_folder_result)(index) != 0;

        let size = if is_dir {
            0
        } else {
            api.get_result_size.map_or(0, |get_size| {
                let mut raw: i64 = 0;
                if get_size(index, &raw mut raw) != 0 && raw > 0 {
                    raw.cast_unsigned()
                } else {
                    0
                }
            })
        };

        let modified = api.get_result_date_modified.and_then(|get_dm| {
            let mut ft = FILETIME::default();
            if get_dm(index, &raw mut ft) != 0 {
                filetime_to_system_time(ft)
            } else {
                None
            }
        });

        let hidden = api.get_result_attributes.is_some_and(|get_attr| {
            let attrs = get_attr(index);
            attrs != INVALID_FILE_ATTRIBUTES && (attrs & FILE_ATTRIBUTE_HIDDEN) != 0
        });

        Some(EverythingItem {
            path: PathBuf::from(full),
            is_dir,
            size,
            modified,
            hidden,
        })
    }
}

/// Converts a Windows `FILETIME` (100ns ticks since 1601-01-01 UTC) to a Rust [`SystemTime`].
fn filetime_to_system_time(ft: FILETIME) -> Option<SystemTime> {
    let ticks = (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime);
    if ticks == 0 || ticks == u64::MAX {
        return None;
    }
    let unix_ticks = ticks.checked_sub(WINDOWS_TO_UNIX_TICKS)?;
    let secs = unix_ticks / 10_000_000;
    let nanos = u32::try_from((unix_ticks % 10_000_000) * 100).ok()?;
    UNIX_EPOCH.checked_add(Duration::new(secs, nanos))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_unix_epoch_converts_cleanly() {
        let ft = FILETIME {
            dwLowDateTime: (WINDOWS_TO_UNIX_TICKS & 0xFFFF_FFFF) as u32,
            dwHighDateTime: (WINDOWS_TO_UNIX_TICKS >> 32) as u32,
        };
        assert_eq!(filetime_to_system_time(ft), Some(UNIX_EPOCH));
        assert_eq!(filetime_to_system_time(FILETIME::default()), None);
    }

    #[test]
    fn bundled_everything_dll_loads_dynamically() {
        assert!(
            api().is_some(),
            "build.rs should copy third_party/everything/Everything64.dll into target dir"
        );
        if is_everything_running() && is_db_loaded() {
            let cancel = AtomicBool::new(false);
            let out = execute_query(
                "Cargo.toml",
                5,
                EVERYTHING_SORT_RUN_COUNT_DESCENDING,
                &cancel,
            )
            .expect("live Everything query should succeed when daemon is running");
            assert!(
                !out.items.is_empty(),
                "live Everything query should find Cargo.toml"
            );
        }
    }
}
