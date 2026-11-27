//! Automatic path jump for Windows Open/Save file dialogs (`#32770`).
//!
//! Two trigger modes work together (see `docs/dialog-jump.md`):
//! - **Switch-back auto-jump**: when the user switches focus from an `ArkeWindow` directly to an
//!   external `#32770` common file dialog (by clicking anywhere in it or via `Alt+Tab`), the
//!   dialog navigates to that Arke window's active folder.
//! - **`Ctrl + G` manual jump**: while an external `#32770` file dialog is in the foreground
//!   (e.g. side-by-side split screen), pressing plain `Ctrl + G` pulls the active folder from the
//!   most recently used Arke window without leaving the dialog.
//!
//! Architecture:
//! - A lightweight hook thread (`dialog-jump-hook`) runs a Win32 message loop hosting both
//!   `SetWinEventHook(EVENT_SYSTEM_FOREGROUND, ..., WINEVENT_OUTOFCONTEXT)` (without
//!   `WINEVENT_SKIPOWNPROCESS`, so `ArkeWindow` activations arrive too) and a minimal
//!   `WH_KEYBOARD_LL` hook for `Ctrl + G`. It never sleeps or blocks.
//! - A single serial worker thread (`dialog-jump-worker`) executes path injection: validating the
//!   dialog, waiting for physical mouse release, locking focus to the filename `Edit`, injecting
//!   the normalized directory path with loopback verification against `ToolbarWindow32`, and
//!   restoring any pre-existing filename.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, GetLastError, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, IsWindowEnabled, SetFocus, VK_LBUTTON, VK_RETURN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, EVENT_SYSTEM_FOREGROUND, EnumChildWindows, GW_OWNER,
    GetClassNameW, GetDlgCtrlID, GetForegroundWindow, GetMessageW, GetParent,
    GetWindow, GetWindowThreadProcessId, HC_ACTION, IsWindow, IsWindowVisible, KBDLLHOOKSTRUCT,
    MSG, PostMessageW, PostThreadMessageW, SMTO_ABORTIFHUNG, SMTO_NORMAL, SendMessageTimeoutW,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, WH_KEYBOARD_LL, WINEVENT_OUTOFCONTEXT,
    WM_GETTEXT, WM_GETTEXTLENGTH, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SETTEXT, WM_SYSKEYDOWN,
};
use windows::core::BOOL;

/// Window class of Arke's main windows (`third_party/gpui-pre-windows`).
const ARKE_WINDOW_CLASS: &str = "ArkeWindow";
/// Window class of Arke's popups (menus, tooltips, overlays).
const ARKE_POPUP_CLASS: &str = "ArkeWindow.Popup";
/// Standard Win32 dialog window class.
const DIALOG_CLASS: &str = "#32770";

const VK_G: u16 = 0x47;
const VK_SHIFT: u16 = 0x10;
const VK_CONTROL: u16 = 0x11;
const VK_MENU: u16 = 0x12;
const VK_LWIN: u16 = 0x5B;
const VK_RWIN: u16 = 0x5C;

/// `EM_SETSEL` selects a character range inside an `Edit` control.
const EM_SETSEL: u32 = 0x00B1;
/// Control ID of the hidden editable box inside the modern file dialog's top address bar.
const ADDRESS_BAR_EDIT_ID: i32 = 41477;
/// Minimum interval between repeated automatic injections into the same dialog handle.
const DEBOUNCE_WINDOW: Duration = Duration::from_millis(300);
/// Maximum time to wait for the physical left mouse button to be released.
const MOUSE_RELEASE_TIMEOUT_MS: u32 = 400;
/// Settle window after mouse release so child controls finish click/focus transitions.
const MOUSE_SETTLE_MS: u64 = 35;
/// Wait after sending `VK_RETURN` before verifying the address bar path.
const NAVIGATE_VERIFY_MS: u64 = 90;
/// Timeout for cross-process `SendMessageTimeoutW` calls.
const MSG_TIMEOUT_MS: u32 = 200;

/// Which kind of source window supplied the target directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceKind {
    /// An Arke main window (`ArkeWindow`).
    Arke,
}

/// Coarse classification of a foreground window for transition tracking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowClassKind {
    /// Arke main window (`ArkeWindow`).
    ArkeMain,
    /// Arke popup/menu/tooltip (`ArkeWindow.Popup`).
    ArkePopup,
    /// Win32 dialog (`#32770`).
    Dialog,
    /// Any other application window.
    Other,
}

/// What the keyboard hook does with one key event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HotkeyVerdict {
    /// Not ours: pass straight to Windows and the foreground app.
    Pass,
    /// Plain `Ctrl + G` pressed: check whether the foreground is a `#32770` file dialog.
    Trigger,
    /// Repeat or release of a `Ctrl + G` we already consumed: swallow so it does not leak.
    Swallow,
}

/// Pure focus-transition state machine enforcing the Strict MRU Policy and Focus Transition Guard.
#[derive(Debug, Default)]
pub(crate) struct FocusTracker {
    /// Most recently focused valid source window.
    pub(crate) last_source_hwnd: Option<isize>,
    /// Kind of `last_source_hwnd`.
    pub(crate) last_source_kind: Option<SourceKind>,
    /// Previous non-popup foreground window handle.
    pub(crate) prev_foreground_hwnd: Option<isize>,
    /// Debounce record: `(dialog_hwnd, instant)` of the last triggered injection.
    pub(crate) last_trigger: Option<(isize, Instant)>,
}

impl FocusTracker {
    /// Processes a foreground window change and returns `Some((source_hwnd, source_kind))` only
    /// when a valid, non-debounced `ArkeWindow -> #32770` transition occurs.
    pub(crate) fn on_foreground_change(
        &mut self,
        hwnd: isize,
        kind: WindowClassKind,
        now: Instant,
    ) -> Option<(isize, SourceKind)> {
        match kind {
            WindowClassKind::ArkeMain => {
                self.last_source_hwnd = Some(hwnd);
                self.last_source_kind = Some(SourceKind::Arke);
                self.prev_foreground_hwnd = Some(hwnd);
                None
            }
            WindowClassKind::ArkePopup => {
                // Keep `prev_foreground_hwnd` intact so opening a context menu or popup inside
                // Arke before clicking a file dialog does not break the transition chain.
                None
            }
            WindowClassKind::Dialog => {
                let is_valid_chain = self.prev_foreground_hwnd.is_some()
                    && self.prev_foreground_hwnd == self.last_source_hwnd;
                let debounced = self.last_trigger.is_some_and(|(last_dlg, at)| {
                    last_dlg == hwnd && now.saturating_duration_since(at) < DEBOUNCE_WINDOW
                });
                // Always update `prev_foreground_hwnd` so internal focus events inside the same
                // `#32770` dialog cannot re-trigger injection.
                self.prev_foreground_hwnd = Some(hwnd);

                if is_valid_chain
                    && !debounced
                    && let (Some(src), Some(src_kind)) =
                        (self.last_source_hwnd, self.last_source_kind)
                {
                    self.last_trigger = Some((hwnd, now));
                    return Some((src, src_kind));
                }
                None
            }
            WindowClassKind::Other => {
                // Break the automatic switch-back chain, but keep `last_source_hwnd` so manual
                // `Ctrl + G` can still pull from the last active Arke window.
                self.prev_foreground_hwnd = Some(hwnd);
                None
            }
        }
    }

    /// Clears the tracked MRU source if it matches a window handle known to be dead.
    pub(crate) fn clear_source_if(&mut self, hwnd: isize) {
        if self.last_source_hwnd == Some(hwnd) {
            self.last_source_hwnd = None;
            self.last_source_kind = None;
        }
    }
}

/// Pure decision function for the `Ctrl + G` keyboard hook.
///
/// Only plain `Ctrl + G` (no `Shift`, `Alt`, or `Win`) returns [`HotkeyVerdict::Trigger`].
/// `g_taken` remembers a consumed press so its repeats and release are swallowed cleanly.
pub(crate) fn decide_hotkey(
    vk: u16,
    down: bool,
    held: impl Fn(u16) -> bool,
    g_taken: &mut bool,
) -> HotkeyVerdict {
    if vk != VK_G {
        return HotkeyVerdict::Pass;
    }
    if !down {
        return if std::mem::take(g_taken) {
            HotkeyVerdict::Swallow
        } else {
            HotkeyVerdict::Pass
        };
    }
    if *g_taken {
        return HotkeyVerdict::Swallow;
    }
    if held(VK_CONTROL)
        && !held(VK_SHIFT)
        && !held(VK_MENU)
        && !held(VK_LWIN)
        && !held(VK_RWIN)
    {
        *g_taken = true;
        HotkeyVerdict::Trigger
    } else {
        HotkeyVerdict::Pass
    }
}

/// Classifies a Win32 window class name into [`WindowClassKind`].
pub(crate) fn classify_window_class(class_name: &str) -> WindowClassKind {
    match class_name {
        ARKE_WINDOW_CLASS => WindowClassKind::ArkeMain,
        ARKE_POPUP_CLASS => WindowClassKind::ArkePopup,
        DIALOG_CLASS => WindowClassKind::Dialog,
        _ => WindowClassKind::Other,
    }
}

/// Parses a filesystem path from a `#32770` dialog's `ToolbarWindow32` / `Breadcrumb Parent` text
/// regardless of Windows display language (e.g. `"Address: D:\Work"`, `"地址: D:\Work"`,
/// `"位址: D:\Work"`, `"アドレス: D:\Work"`, `"Adresse: \\server\share"`).
///
/// Returns `None` for virtual locations such as `"Desktop"` or `"This PC"`.
pub(crate) fn parse_toolbar_address(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Windows filenames cannot contain `:`, so the first `": "` unambiguously separates a
    // localized address label (e.g. "Address: ", "地址: ") from the path itself.
    let candidate = trimmed
        .split_once(": ")
        .map_or(trimmed, |(_, rest)| rest.trim());
    if is_filesystem_path_text(candidate) {
        Some(candidate.to_owned())
    } else {
        None
    }
}

/// Whether `text` looks like a Windows drive path (`C:\...`, `C:`) or UNC share (`\\host\share`).
fn is_filesystem_path_text(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return bytes.len() == 2 || bytes[2] == b'\\';
    }
    if let Some(rest) = text.strip_prefix(r"\\") {
        return !rest.is_empty() && !rest.starts_with('\\');
    }
    false
}

/// Ensures a directory path ends with a single trailing backslash so Windows file dialogs navigate
/// into the folder instead of interpreting its leaf name as a file.
pub(crate) fn normalize_dir_path(path: &Path) -> String {
    let mut text = path.to_string_lossy().into_owned();
    if !text.ends_with('\\') {
        text.push('\\');
    }
    text
}

/// Case-insensitive directory path equality ignoring trailing backslashes (`C:\` == `C:`).
pub(crate) fn paths_equal(a: &str, b: &str) -> bool {
    a.trim_end_matches('\\')
        .eq_ignore_ascii_case(b.trim_end_matches('\\'))
}

/// Extracts a safe filename from the dialog's `Edit` control for preservation across navigation.
/// Any text containing path separators or drive colons is treated as a residual path and dropped.
pub(crate) fn extract_preservable_filename(text: &str) -> String {
    if !text.is_empty() && !text.contains('\\') && !text.contains(':') {
        text.to_owned()
    } else {
        String::new()
    }
}

// ───────────────────────── Background hook & worker ─────────────────────────

#[derive(Clone, Debug)]
struct InjectJob {
    dialog_hwnd: isize,
    target_path: PathBuf,
    wait_for_click: bool,
}

static TRACKER: OnceLock<Mutex<FocusTracker>> = OnceLock::new();
static WORKER_TX: OnceLock<Mutex<Option<flume::Sender<InjectJob>>>> = OnceLock::new();
static G_TAKEN: AtomicBool = AtomicBool::new(false);

fn tracker() -> &'static Mutex<FocusTracker> {
    TRACKER.get_or_init(Mutex::default)
}

fn worker_tx() -> &'static Mutex<Option<flume::Sender<InjectJob>>> {
    WORKER_TX.get_or_init(Mutex::default)
}

/// Running dialog-jump watcher (foreground WinEvent hook + `Ctrl + G` keyboard hook + serial
/// injection worker). Dropping it unhooks both hooks and ends both threads cleanly.
pub(crate) struct Watcher {
    hook_thread_id: Arc<AtomicU32>,
    stopped: Arc<AtomicBool>,
}

impl Watcher {
    /// Installs the foreground & `Ctrl + G` hooks and starts the serial injection worker thread.
    pub(crate) fn start() -> Option<Self> {
        let (tx, rx) = flume::unbounded::<InjectJob>();
        *worker_tx().lock() = Some(tx);

        let hook_thread_id = Arc::new(AtomicU32::new(0));
        let stopped = Arc::new(AtomicBool::new(false));

        let worker_spawned = std::thread::Builder::new()
            .name("dialog-jump-worker".into())
            .spawn(move || run_worker_loop(&rx));
        if let Err(err) = worker_spawned {
            log::warn!("dialog jump: cannot spawn worker thread: {err}");
            *worker_tx().lock() = None;
            return None;
        }

        let (id, flag) = (Arc::clone(&hook_thread_id), Arc::clone(&stopped));
        let hook_spawned = std::thread::Builder::new()
            .name("dialog-jump-hook".into())
            .spawn(move || run_hook_loop(&id, &flag));
        if let Err(err) = hook_spawned {
            log::warn!("dialog jump: cannot spawn hook thread: {err}");
            *worker_tx().lock() = None;
            return None;
        }

        Some(Self {
            hook_thread_id,
            stopped,
        })
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        // Closing the channel sender lets the worker thread exit once idle.
        *worker_tx().lock() = None;
        let id = self.hook_thread_id.load(Ordering::SeqCst);
        if id != 0 {
            // SAFETY: posts WM_QUIT to our own hook thread, which unhooks and exits.
            let _ = unsafe { PostThreadMessageW(id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        }
    }
}

fn run_hook_loop(thread_id: &AtomicU32, stopped: &AtomicBool) {
    // SAFETY: plain Win32 call for this hook thread's id.
    thread_id.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);

    // SAFETY: out-of-context foreground hook on this message-loop thread; intentionally omits
    // WINEVENT_SKIPOWNPROCESS so activations of our own `ArkeWindow` are observed.
    let win_event_hook = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(on_foreground_event),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    };
    if win_event_hook.is_invalid() {
        log::warn!("dialog jump: failed to install EVENT_SYSTEM_FOREGROUND hook");
        return;
    }

    // SAFETY: module handle of our own exe; `on_key` lives for the whole run.
    let module = unsafe { GetModuleHandleW(None) }.ok();
    // SAFETY: low-level keyboard hook installed and removed on this thread; callback stays short.
    let kbd_hook = match unsafe {
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(on_key), module.map(Into::into), 0)
    } {
        Ok(h) => Some(h),
        Err(err) => {
            log::warn!("dialog jump: cannot install Ctrl+G keyboard hook (auto-jump still on): {err}");
            None
        }
    };

    log::info!("dialog jump: on");
    if !stopped.load(Ordering::SeqCst) {
        let mut msg = MSG::default();
        // SAFETY: standard Win32 message pump for this thread; ends on WM_QUIT from `Watcher::drop`.
        while unsafe { GetMessageW(&raw mut msg, None, 0, 0) }.0 > 0 {
            // SAFETY: dispatches messages retrieved by GetMessageW above.
            unsafe {
                let _ = TranslateMessage(&raw const msg);
                DispatchMessageW(&raw const msg);
            }
        }
    }

    if let Some(h) = kbd_hook {
        // SAFETY: unhooks the keyboard hook installed above on this same thread.
        let _ = unsafe { UnhookWindowsHookEx(h) };
    }
    // SAFETY: unhooks the WinEvent hook installed above on this same thread.
    let _ = unsafe { UnhookWinEvent(win_event_hook) };
    G_TAKEN.store(false, Ordering::SeqCst);
    *tracker().lock() = FocusTracker::default();
    log::info!("dialog jump: off");
}

fn run_worker_loop(rx: &flume::Receiver<InjectJob>) {
    while let Ok(mut job) = rx.recv() {
        // Coalesce bursty focus transitions: if newer jobs are already queued, skip stale ones.
        while let Ok(newer) = rx.try_recv() {
            job = newer;
        }
        let dlg = HWND(job.dialog_hwnd as *mut _);
        let _ = inject_path_to_dialog(dlg, &job.target_path, job.wait_for_click);
    }
}

fn enqueue_job(job: InjectJob) {
    let tx = worker_tx().lock().clone();
    if let Some(tx) = tx {
        let _ = tx.try_send(job);
    }
}

/// Resolves the live filesystem folder for a tracked source window; clears stale MRU on dead HWND.
fn resolve_source_folder(source_hwnd: isize, kind: SourceKind) -> Option<PathBuf> {
    let alive = is_window_alive(HWND(source_hwnd as *mut _));
    if !alive {
        tracker().lock().clear_source_if(source_hwnd);
        return None;
    }
    match kind {
        SourceKind::Arke => crate::api::active_folder_for_hwnd(source_hwnd),
    }
}

/// Resolves the target folder for explicit manual `Ctrl + G`: uses the alive MRU source first,
/// and falls back to the frontmost open `ArkeWindow` in Z-order if the MRU window was closed or
/// never focused yet.
fn resolve_manual_folder() -> Option<PathBuf> {
    let mru = {
        let guard = tracker().lock();
        guard.last_source_hwnd.zip(guard.last_source_kind)
    };
    if let Some((src_hwnd, src_kind)) = mru
        && let Some(path) = resolve_source_folder(src_hwnd, src_kind)
    {
        return Some(path);
    }
    crate::api::active_folder_of_frontmost().map(|(_, path)| path)
}

/// Foreground window hook callback. Runs on the `dialog-jump-hook` thread and must never block.
///
/// # Safety
///
/// Called by Windows from `SetWinEventHook` on the hook thread.
unsafe extern "system" fn on_foreground_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _id_event_thread: u32,
    _event_time: u32,
) {
    if event != EVENT_SYSTEM_FOREGROUND || hwnd.0.is_null() {
        return;
    }

    let class_name = window_class_name(hwnd);
    let kind = classify_window_class(&class_name);
    let hwnd_raw = hwnd.0 as isize;
    let now = Instant::now();

    let triggered = {
        let mut guard = tracker().lock();
        guard.on_foreground_change(hwnd_raw, kind, now)
    };

    if let Some((source_hwnd, source_kind)) = triggered {
        if is_arke_owned_dialog(hwnd) {
            return;
        }
        if let Some(target_path) = resolve_source_folder(source_hwnd, source_kind) {
            log::debug!(
                "dialog jump: Arke -> #32770 transition, target={}",
                target_path.display()
            );
            enqueue_job(InjectJob {
                dialog_hwnd: hwnd_raw,
                target_path,
                wait_for_click: true,
            });
        }
    }
}

fn key_held(vk: u16) -> bool {
    // SAFETY: plain Win32 call querying async key state.
    unsafe { GetAsyncKeyState(i32::from(vk)) < 0 }
}

/// Low-level keyboard hook callback for `Ctrl + G` manual dialog jump.
///
/// # Safety
///
/// Called by Windows from `SetWindowsHookExW(WH_KEYBOARD_LL)` on the hook thread.
unsafe extern "system" fn on_key(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION.cast_signed() {
        // SAFETY: for WH_KEYBOARD_LL with HC_ACTION, lparam points to a valid KBDLLHOOKSTRUCT.
        let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let down = wparam.0 == WM_KEYDOWN as usize || wparam.0 == WM_SYSKEYDOWN as usize;
        let vk = u16::try_from(info.vkCode).unwrap_or(0);
        let mut taken = G_TAKEN.load(Ordering::SeqCst);
        let verdict = decide_hotkey(vk, down, key_held, &mut taken);
        G_TAKEN.store(taken, Ordering::SeqCst);

        match verdict {
            HotkeyVerdict::Pass => {}
            HotkeyVerdict::Swallow => return LRESULT(1),
            HotkeyVerdict::Trigger => {
                // SAFETY: plain Win32 call.
                let fg = unsafe { GetForegroundWindow() };
                if !fg.0.is_null()
                    && window_class_name(fg) == DIALOG_CLASS
                    && !is_arke_owned_dialog(fg)
                    && find_dialog_filename_edit(fg).is_some()
                {
                    if let Some(target_path) = resolve_manual_folder() {
                        log::debug!(
                            "dialog jump: Ctrl+G triggered, target={}",
                            target_path.display()
                        );
                        enqueue_job(InjectJob {
                            dialog_hwnd: fg.0 as isize,
                            target_path,
                            wait_for_click: false,
                        });
                    }
                    return LRESULT(1);
                }
                // Foreground is not an external file dialog: release `G_TAKEN` and pass Ctrl+G on.
                G_TAKEN.store(false, Ordering::SeqCst);
            }
        }
    }
    // SAFETY: passes the event on unchanged to the next hook in the chain.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

// ───────────────────────── Win32 dialog inspection & injection ─────────────────────────

fn is_window_alive(hwnd: HWND) -> bool {
    if hwnd.0.is_null() {
        return false;
    }
    // SAFETY: plain Win32 validity check on an HWND.
    unsafe { IsWindow(Some(hwnd)) }.as_bool()
}

fn window_class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    // SAFETY: `buf` is writable for its full length.
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    usize::try_from(len)
        .ok()
        .and_then(|n| buf.get(..n))
        .map_or_else(String::new, String::from_utf16_lossy)
}

/// Returns `true` if `dialog_hwnd` belongs to Arke itself or is owned by an `ArkeWindow`
/// (such as `arke.exe --pick-file` launched from Settings -> Browse, see `win_picker.rs`).
fn is_arke_owned_dialog(dialog_hwnd: HWND) -> bool {
    let my_pid = std::process::id();
    let mut dlg_pid = 0u32;
    // SAFETY: `dlg_pid` is a valid out pointer.
    unsafe { GetWindowThreadProcessId(dialog_hwnd, Some(&raw mut dlg_pid)) };
    if dlg_pid == my_pid {
        return true;
    }
    // SAFETY: queries owner window of `dialog_hwnd`.
    if let Ok(owner) = unsafe { GetWindow(dialog_hwnd, GW_OWNER) }
        && !owner.0.is_null()
    {
        let mut owner_pid = 0u32;
        // SAFETY: `owner_pid` is a valid out pointer.
        unsafe { GetWindowThreadProcessId(owner, Some(&raw mut owner_pid)) };
        if owner_pid == my_pid || window_class_name(owner) == ARKE_WINDOW_CLASS {
            return true;
        }
    }
    false
}

/// Verifies that `dialog_hwnd` is a common file dialog (contains `SHELLDLL_DefView`,
/// `DUIViewWndClassName`, or `ComboBoxEx32`) and locates its filename `Edit` control.
///
/// Explicitly excludes search boxes (`SearchEditBox*`), the hidden address-bar edit (ID `41477`),
/// and refuses to match arbitrary `Edit` controls inside non-file `#32770` dialogs.
pub(crate) fn find_dialog_filename_edit(dialog_hwnd: HWND) -> Option<HWND> {
    struct SearchContext {
        has_file_view: bool,
        has_combo_box_ex: bool,
        preferred_edit: Option<HWND>,
        combo_edit: Option<HWND>,
    }

    /// # Safety
    /// Called synchronously by `EnumChildWindows` with `lparam` pointing to `SearchContext`.
    unsafe extern "system" fn enum_child(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is `&mut SearchContext` on the caller's stack for this call's duration.
        let ctx = unsafe { &mut *(lparam.0 as *mut SearchContext) };
        let class_name = window_class_name(hwnd);

        if class_name == "SHELLDLL_DefView" || class_name == "DUIViewWndClassName" {
            ctx.has_file_view = true;
            return BOOL(1);
        }
        if class_name == "ComboBoxEx32" {
            ctx.has_combo_box_ex = true;
            return BOOL(1);
        }

        // SAFETY: plain Win32 state queries.
        let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
        // SAFETY: plain Win32 state queries.
        let enabled = unsafe { IsWindowEnabled(hwnd) }.as_bool();
        if !visible || !enabled {
            return BOOL(1);
        }

        // SAFETY: plain Win32 control ID query.
        let ctrl_id = unsafe { GetDlgCtrlID(hwnd) };
        if class_name.contains("SearchEditBox") || ctrl_id == ADDRESS_BAR_EDIT_ID {
            return BOOL(1);
        }

        if class_name == "Edit" {
            // SAFETY: queries direct parent window of the Edit control.
            let parent_class = unsafe { GetParent(hwnd) }
                .ok()
                .filter(|p| !p.0.is_null())
                .map_or_default(window_class_name);

            if parent_class.contains("SearchEditBox") {
                return BOOL(1);
            }
            if matches!(ctrl_id, 0x47c | 1001 | 1152) {
                if ctx.preferred_edit.is_none() {
                    ctx.preferred_edit = Some(hwnd);
                }
            } else if matches!(parent_class.as_str(), "ComboBox" | "ComboBoxEx32")
                && ctx.combo_edit.is_none()
            {
                ctx.combo_edit = Some(hwnd);
            }
        }
        BOOL(1)
    }

    let mut ctx = SearchContext {
        has_file_view: false,
        has_combo_box_ex: false,
        preferred_edit: None,
        combo_edit: None,
    };
    // SAFETY: `ctx` lives until `EnumChildWindows` returns.
    unsafe {
        let _ = EnumChildWindows(
            Some(dialog_hwnd),
            Some(enum_child),
            LPARAM((&raw mut ctx) as isize),
        );
    }

    if !ctx.has_file_view && !ctx.has_combo_box_ex {
        return None;
    }
    ctx.preferred_edit.or(ctx.combo_edit)
}

fn get_window_text(hwnd: HWND) -> String {
    let mut len_res: usize = 0;
    // SAFETY: `len_res` is a valid out pointer; timeout guards against hung target threads.
    let ok = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_GETTEXTLENGTH,
            WPARAM(0),
            LPARAM(0),
            SMTO_ABORTIFHUNG | SMTO_NORMAL,
            MSG_TIMEOUT_MS,
            Some(&raw mut len_res),
        )
    };
    if ok.0 == 0 || len_res == 0 || len_res >= 32768 {
        return String::new();
    }
    let mut buf = vec![0u16; len_res + 1];
    let mut copied: usize = 0;
    // SAFETY: `buf` has space for `buf.len()` UTF-16 units and lives across the call.
    let ok2 = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_GETTEXT,
            WPARAM(buf.len()),
            LPARAM(buf.as_mut_ptr() as isize),
            SMTO_ABORTIFHUNG | SMTO_NORMAL,
            MSG_TIMEOUT_MS,
            Some(&raw mut copied),
        )
    };
    if ok2.0 != 0 && copied > 0 {
        let count = copied.min(len_res);
        if let Some(slice) = buf.get(..count) {
            return String::from_utf16_lossy(slice);
        }
    }
    String::new()
}

/// Reads the current filesystem directory shown by a modern `#32770` dialog's `ToolbarWindow32`
/// / `Breadcrumb Parent` control. Returns `None` for legacy dialogs without a toolbar or when
/// the dialog is at a virtual location.
pub(crate) fn get_dialog_current_path(dialog_hwnd: HWND) -> Option<String> {
    struct ToolbarContext {
        address: Option<String>,
        has_toolbar: bool,
    }

    /// # Safety
    /// Called synchronously by `EnumChildWindows` with `lparam` pointing to `ToolbarContext`.
    unsafe extern "system" fn enum_toolbar(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is `&mut ToolbarContext` on the caller's stack for this call's duration.
        let ctx = unsafe { &mut *(lparam.0 as *mut ToolbarContext) };
        let class_name = window_class_name(hwnd);
        if class_name == "ToolbarWindow32" || class_name == "Breadcrumb Parent" {
            ctx.has_toolbar = true;
            let text = get_window_text(hwnd);
            if let Some(addr) = parse_toolbar_address(&text) {
                ctx.address = Some(addr);
                return BOOL(0);
            }
        }
        BOOL(1)
    }

    let mut ctx = ToolbarContext {
        address: None,
        has_toolbar: false,
    };
    // SAFETY: `ctx` lives until `EnumChildWindows` returns.
    unsafe {
        let _ = EnumChildWindows(
            Some(dialog_hwnd),
            Some(enum_toolbar),
            LPARAM((&raw mut ctx) as isize),
        );
    }
    ctx.address
}

fn has_address_toolbar(dialog_hwnd: HWND) -> bool {
    struct Flag {
        found: bool,
    }

    /// # Safety
    /// Called synchronously by `EnumChildWindows` with `lparam` pointing to `Flag`.
    unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` is `&mut Flag` on the caller's stack.
        let flag = unsafe { &mut *(lparam.0 as *mut Flag) };
        let class_name = window_class_name(hwnd);
        if class_name == "ToolbarWindow32" || class_name == "Breadcrumb Parent" {
            flag.found = true;
            return BOOL(0);
        }
        BOOL(1)
    }

    let mut flag = Flag { found: false };
    // SAFETY: `flag` lives until `EnumChildWindows` returns.
    unsafe {
        let _ = EnumChildWindows(
            Some(dialog_hwnd),
            Some(enum_cb),
            LPARAM((&raw mut flag) as isize),
        );
    }
    flag.found
}

fn wait_for_mouse_release() {
    let mut waited_ms = 0u32;
    while key_held(VK_LBUTTON.0) && waited_ms < MOUSE_RELEASE_TIMEOUT_MS {
        std::thread::sleep(Duration::from_millis(10));
        waited_ms += 10;
    }
    // Brief settle window so DirectUIHWND / SysTreeView32 finish WM_LBUTTONUP & focus transition.
    std::thread::sleep(Duration::from_millis(MOUSE_SETTLE_MS));
}

/// RAII guard for `AttachThreadInput`, ensuring the threads are always detached on drop.
struct ThreadInputAttachment {
    cur: u32,
    target: u32,
    attached: bool,
}

impl ThreadInputAttachment {
    fn attach(dialog_hwnd: HWND) -> Self {
        // SAFETY: plain Win32 thread queries.
        let cur = unsafe { GetCurrentThreadId() };
        // SAFETY: plain Win32 thread queries.
        let target = unsafe { GetWindowThreadProcessId(dialog_hwnd, None) };
        let attached = if target != 0 && target != cur {
            // SAFETY: attaches this worker thread's input queue to the dialog thread; paired in Drop.
            unsafe { AttachThreadInput(cur, target, true) }.as_bool()
        } else {
            false
        };
        Self {
            cur,
            target,
            attached,
        }
    }
}

impl Drop for ThreadInputAttachment {
    fn drop(&mut self) {
        if self.attached {
            // SAFETY: pairs with successful AttachThreadInput in `attach`.
            let _ = unsafe { AttachThreadInput(self.cur, self.target, false) };
        }
    }
}

fn focus_dialog_edit(dialog_hwnd: HWND, edit_hwnd: HWND) {
    {
        let _attach = ThreadInputAttachment::attach(dialog_hwnd);
        // SAFETY: moves keyboard focus to the dialog's filename Edit control.
        let _ = unsafe { SetFocus(Some(edit_hwnd)) };
    }
    let mut res: usize = 0;
    // SAFETY: `res` is a valid out pointer; selects all text in the Edit control.
    let _ = unsafe {
        SendMessageTimeoutW(
            edit_hwnd,
            EM_SETSEL,
            WPARAM(0),
            LPARAM(-1),
            SMTO_ABORTIFHUNG | SMTO_NORMAL,
            100,
            Some(&raw mut res),
        )
    };
}

fn encode_wide_nul(text: &str) -> Vec<u16> {
    OsStr::new(text)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Injects `target_path` into a `#32770` file dialog, preserving any pre-existing filename.
///
/// Runs synchronously on the worker thread; returns `true` when navigation is confirmed (or was
/// already at `target_path`).
pub(crate) fn inject_path_to_dialog(
    dialog_hwnd: HWND,
    target_path: &Path,
    wait_for_click: bool,
) -> bool {
    if !is_window_alive(dialog_hwnd) {
        return false;
    }
    let Some(edit_hwnd) = find_dialog_filename_edit(dialog_hwnd) else {
        log::debug!("dialog jump: not a common file dialog or filename Edit not found");
        return false;
    };

    let normalized_path = normalize_dir_path(target_path);

    // 1. Idempotency check: skip if the dialog is already at the target directory.
    if let Some(curr_path) = get_dialog_current_path(dialog_hwnd)
        && paths_equal(&curr_path, &normalized_path)
    {
        log::debug!("dialog jump: already in {}, skipping", curr_path);
        return true;
    }

    // 2. Backup the original filename before mouse click settlement can overwrite it.
    let original_filename = extract_preservable_filename(&get_window_text(edit_hwnd));

    // 3. Synchronize with physical mouse click release when triggered by switch-back.
    if wait_for_click {
        wait_for_mouse_release();
        // Re-verify the dialog is still alive and in the foreground after waiting.
        // SAFETY: plain Win32 foreground check.
        if !is_window_alive(dialog_hwnd) || unsafe { GetForegroundWindow() } != dialog_hwnd {
            log::debug!("dialog jump: dialog lost focus or closed during click settle, aborting");
            return false;
        }
    }

    let wide_path = encode_wide_nul(&normalized_path);
    let expect_toolbar = has_address_toolbar(dialog_hwnd);

    // 4. Inject normalized path with explicit focus lock and loopback verification (up to 3 tries).
    let mut navigated = false;
    for attempt in 1..=3 {
        let current_edit = find_dialog_filename_edit(dialog_hwnd).unwrap_or(edit_hwnd);
        focus_dialog_edit(dialog_hwnd, current_edit);

        let mut set_res: usize = 0;
        // SAFETY: `wide_path` is NUL-terminated UTF-16 and outlives the call.
        let ok = unsafe {
            SendMessageTimeoutW(
                current_edit,
                WM_SETTEXT,
                WPARAM(0),
                LPARAM(wide_path.as_ptr() as isize),
                SMTO_ABORTIFHUNG | SMTO_NORMAL,
                MSG_TIMEOUT_MS,
                Some(&raw mut set_res),
            )
        };
        if ok.0 == 0 {
            // SAFETY: reads thread-local last error from the failed SendMessageTimeoutW above.
            let err = unsafe { GetLastError() };
            if err == ERROR_ACCESS_DENIED {
                log::debug!("dialog jump: blocked by UIPI (elevated target dialog), aborting");
                return false;
            }
            log::debug!("dialog jump: WM_SETTEXT failed on attempt {attempt} ({err:?})");
            continue;
        }

        // Re-lock focus immediately before dispatching VK_RETURN so the Edit processes it.
        focus_dialog_edit(dialog_hwnd, current_edit);
        // SAFETY: posts keydown/keyup for Enter to the filename Edit control.
        unsafe {
            let _ = PostMessageW(
                Some(current_edit),
                WM_KEYDOWN,
                WPARAM(usize::from(VK_RETURN.0)),
                LPARAM(0),
            );
            let _ = PostMessageW(
                Some(current_edit),
                WM_KEYUP,
                WPARAM(usize::from(VK_RETURN.0)),
                LPARAM(0),
            );
        }

        std::thread::sleep(Duration::from_millis(NAVIGATE_VERIFY_MS));
        if let Some(curr_path) = get_dialog_current_path(dialog_hwnd) {
            if paths_equal(&curr_path, &normalized_path) {
                navigated = true;
                break;
            }
        } else if !expect_toolbar {
            // Legacy file dialog without a ToolbarWindow32 breadcrumb bar.
            navigated = true;
            break;
        }
    }

    // 5. Restore the original filename (or clear residual injected path if originally empty).
    restore_dialog_filename(dialog_hwnd, edit_hwnd, &original_filename);

    if navigated {
        log::info!("dialog jump: navigated dialog to {normalized_path}");
    } else {
        log::debug!("dialog jump: could not confirm dialog reached {normalized_path}");
    }
    navigated
}

fn restore_dialog_filename(dialog_hwnd: HWND, fallback_edit: HWND, original_filename: &str) {
    let final_edit = find_dialog_filename_edit(dialog_hwnd).unwrap_or(fallback_edit);
    let current_edit_text = get_window_text(final_edit);
    let should_restore = !original_filename.is_empty()
        || current_edit_text.contains('\\')
        || current_edit_text.contains(':');

    if !should_restore {
        return;
    }
    let wide_name = encode_wide_nul(original_filename);
    let mut set_res: usize = 0;
    // SAFETY: `wide_name` is NUL-terminated UTF-16 and outlives the call.
    let _ = unsafe {
        SendMessageTimeoutW(
            final_edit,
            WM_SETTEXT,
            WPARAM(0),
            LPARAM(wide_name.as_ptr() as isize),
            SMTO_ABORTIFHUNG | SMTO_NORMAL,
            MSG_TIMEOUT_MS,
            Some(&raw mut set_res),
        )
    };
}

// ───────────────────────── Tests ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_tracker_enforces_strict_transition_chain_and_debounce() {
        let mut t = FocusTracker::default();
        let t0 = Instant::now();

        // 1. Direct #32770 without prior Arke activation: silent.
        assert_eq!(t.on_foreground_change(0xD1, WindowClassKind::Dialog, t0), None);

        // 2. Arke -> #32770: triggers.
        assert_eq!(t.on_foreground_change(0xA1, WindowClassKind::ArkeMain, t0), None);
        assert_eq!(
            t.on_foreground_change(0xD1, WindowClassKind::Dialog, t0),
            Some((0xA1, SourceKind::Arke))
        );

        // 3. Repeated foreground event on same #32770 does not re-trigger.
        assert_eq!(
            t.on_foreground_change(0xD1, WindowClassKind::Dialog, t0 + Duration::from_millis(50)),
            None
        );

        // 4. Rapid Arke -> same #32770 within 300ms is debounced.
        assert_eq!(
            t.on_foreground_change(0xA1, WindowClassKind::ArkeMain, t0 + Duration::from_millis(100)),
            None
        );
        assert_eq!(
            t.on_foreground_change(0xD1, WindowClassKind::Dialog, t0 + Duration::from_millis(150)),
            None
        );

        // 5. After >= 300ms, Arke -> same #32770 triggers again.
        let t_later = t0 + Duration::from_millis(350);
        assert_eq!(t.on_foreground_change(0xA1, WindowClassKind::ArkeMain, t_later), None);
        assert_eq!(
            t.on_foreground_change(0xD1, WindowClassKind::Dialog, t_later),
            Some((0xA1, SourceKind::Arke))
        );

        // 6. Arke -> Other -> #32770: chain broken, stays silent, but last_source_hwnd stays for Ctrl+G.
        let t_other = t0 + Duration::from_secs(1);
        assert_eq!(t.on_foreground_change(0xA1, WindowClassKind::ArkeMain, t_other), None);
        assert_eq!(t.on_foreground_change(0xB1, WindowClassKind::Other, t_other), None);
        assert_eq!(t.on_foreground_change(0xD1, WindowClassKind::Dialog, t_other), None);
        assert_eq!(t.last_source_hwnd, Some(0xA1));

        // 7. Arke -> ArkePopup -> #32770: popup does not break transition chain.
        let t_popup = t0 + Duration::from_secs(2);
        assert_eq!(t.on_foreground_change(0xA1, WindowClassKind::ArkeMain, t_popup), None);
        assert_eq!(t.on_foreground_change(0xA2, WindowClassKind::ArkePopup, t_popup), None);
        assert_eq!(
            t.on_foreground_change(0xD1, WindowClassKind::Dialog, t_popup),
            Some((0xA1, SourceKind::Arke))
        );

        // 8. Dead source cleared cleanly.
        t.clear_source_if(0xA1);
        assert_eq!(t.last_source_hwnd, None);
        assert_eq!(t.last_source_kind, None);
    }

    #[test]
    fn hotkey_decides_plain_ctrl_g_and_passes_other_chords() {
        let mut taken = false;
        let ctrl_only = |vk| vk == VK_CONTROL;
        let ctrl_shift = |vk| vk == VK_CONTROL || vk == VK_SHIFT;
        let ctrl_alt = |vk| vk == VK_CONTROL || vk == VK_MENU;
        let ctrl_win = |vk| vk == VK_CONTROL || vk == VK_LWIN;
        let none = |_: u16| false;

        // Unrelated key passes.
        assert_eq!(decide_hotkey(0x41, true, ctrl_only, &mut taken), HotkeyVerdict::Pass);
        // Plain G without Ctrl passes.
        assert_eq!(decide_hotkey(VK_G, true, none, &mut taken), HotkeyVerdict::Pass);
        // Modified chords pass.
        assert_eq!(decide_hotkey(VK_G, true, ctrl_shift, &mut taken), HotkeyVerdict::Pass);
        assert_eq!(decide_hotkey(VK_G, true, ctrl_alt, &mut taken), HotkeyVerdict::Pass);
        assert_eq!(decide_hotkey(VK_G, true, ctrl_win, &mut taken), HotkeyVerdict::Pass);

        // Plain Ctrl+G triggers, repeats swallow, release swallows and resets.
        assert_eq!(decide_hotkey(VK_G, true, ctrl_only, &mut taken), HotkeyVerdict::Trigger);
        assert!(taken);
        assert_eq!(decide_hotkey(VK_G, true, ctrl_only, &mut taken), HotkeyVerdict::Swallow);
        assert_eq!(decide_hotkey(VK_G, false, ctrl_only, &mut taken), HotkeyVerdict::Swallow);
        assert!(!taken);
        // Stray key-up passes.
        assert_eq!(decide_hotkey(VK_G, false, none, &mut taken), HotkeyVerdict::Pass);
    }

    #[test]
    fn toolbar_address_parsing_handles_all_locales_and_unc() {
        assert_eq!(
            parse_toolbar_address(r"Address: C:\Windows\System32").as_deref(),
            Some(r"C:\Windows\System32")
        );
        assert_eq!(
            parse_toolbar_address(r"地址: D:\Work\项目").as_deref(),
            Some(r"D:\Work\项目")
        );
        assert_eq!(
            parse_toolbar_address(r"位址: E:\").as_deref(),
            Some(r"E:\")
        );
        assert_eq!(
            parse_toolbar_address(r"アドレス: C:\Users\Dev").as_deref(),
            Some(r"C:\Users\Dev")
        );
        assert_eq!(
            parse_toolbar_address(r"Adresse: \\fileserver\share\docs").as_deref(),
            Some(r"\\fileserver\share\docs")
        );
        assert_eq!(parse_toolbar_address(r"C:\Direct").as_deref(), Some(r"C:\Direct"));
        assert_eq!(parse_toolbar_address("Address: Desktop"), None);
        assert_eq!(parse_toolbar_address("地址: 此电脑"), None);
        assert_eq!(parse_toolbar_address(""), None);
    }

    #[test]
    fn path_normalization_equality_and_filename_preservation() {
        assert_eq!(normalize_dir_path(Path::new(r"D:\Work")), r"D:\Work\");
        assert_eq!(normalize_dir_path(Path::new(r"D:\Work\")), r"D:\Work\");
        assert_eq!(normalize_dir_path(Path::new(r"C:\")), r"C:\");
        assert_eq!(normalize_dir_path(Path::new(r"\\srv\share")), r"\\srv\share\");

        assert!(paths_equal(r"C:\Windows\System32", r"c:\windows\system32\"));
        assert!(paths_equal(r"C:\", r"c:"));
        assert!(!paths_equal(r"C:\Windows", r"C:\Users"));

        assert_eq!(
            extract_preservable_filename("QuarterlyReport.docx"),
            "QuarterlyReport.docx"
        );
        assert_eq!(extract_preservable_filename(r"C:\Temp\foo.txt"), "");
        assert_eq!(extract_preservable_filename(r"\\srv\share"), "");
        assert_eq!(extract_preservable_filename(""), "");
    }

    #[test]
    fn window_class_classification() {
        assert_eq!(classify_window_class("ArkeWindow"), WindowClassKind::ArkeMain);
        assert_eq!(classify_window_class("ArkeWindow.Popup"), WindowClassKind::ArkePopup);
        assert_eq!(classify_window_class("#32770"), WindowClassKind::Dialog);
        assert_eq!(classify_window_class("Chrome_WidgetWin_1"), WindowClassKind::Other);
    }

    static LIVE_FOUND_DLG: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

    struct LiveControls {
        dui: Option<HWND>,
        tree: Option<HWND>,
        toolbar: Option<HWND>,
    }

    /// # Safety
    /// Called synchronously by `EnumWindows`.
    unsafe extern "system" fn find_live_dlg(hwnd: HWND, _: LPARAM) -> BOOL {
        use windows::Win32::System::Threading::GetCurrentProcessId;
        let mut pid = 0u32;
        // SAFETY: valid out pointer.
        unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut pid)) };
        // SAFETY: plain Win32 calls.
        let ours = pid == unsafe { GetCurrentProcessId() };
        // SAFETY: plain Win32 call.
        let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
        if ours && visible && window_class_name(hwnd) == DIALOG_CLASS {
            LIVE_FOUND_DLG.store(hwnd.0 as isize, Ordering::SeqCst);
            return BOOL(0);
        }
        BOOL(1)
    }

    /// # Safety
    /// Called synchronously by `EnumChildWindows`.
    unsafe extern "system" fn find_live_controls(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: `lparam` points to `LiveControls` on caller stack.
        let ctrls = unsafe { &mut *(lparam.0 as *mut LiveControls) };
        let cls = window_class_name(hwnd);
        // SAFETY: plain Win32 parent lookup.
        let pcls = unsafe { GetParent(hwnd) }
            .ok()
            .filter(|p| !p.0.is_null())
            .map_or_default(window_class_name);
        if cls == "DirectUIHWND" && pcls == "SHELLDLL_DefView" {
            ctrls.dui = Some(hwnd);
        } else if cls == "SysTreeView32" && ctrls.tree.is_none() {
            ctrls.tree = Some(hwnd);
        } else if cls == "ToolbarWindow32" && pcls == "Breadcrumb Parent" {
            ctrls.toolbar = Some(hwnd);
        }
        BOOL(1)
    }

    fn live_click_and_inject(dlg_hwnd: HWND, ctrl: HWND, target: &'static str) -> (bool, String) {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, mouse_event,
        };
        use windows::Win32::UI::WindowsAndMessaging::{
            GetWindowRect, SetCursorPos, SetForegroundWindow,
        };
        // SAFETY: simulates a real user click on `ctrl` inside the live dialog.
        unsafe {
            let _ = SetForegroundWindow(dlg_hwnd);
            let mut r = RECT::default();
            let _ = GetWindowRect(ctrl, &raw mut r);
            let x = r.left + (r.right - r.left) * 3 / 4;
            let y = r.top + (r.bottom - r.top) * 3 / 4;
            let _ = SetCursorPos(x, y);
            std::thread::sleep(Duration::from_millis(20));
            mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
        }
        let dlg_raw = dlg_hwnd.0 as isize;
        let handle = std::thread::spawn(move || {
            inject_path_to_dialog(HWND(dlg_raw as *mut _), Path::new(target), true)
        });
        std::thread::sleep(Duration::from_millis(80));
        // SAFETY: releases the mouse button pressed above.
        unsafe {
            mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);
        }
        let ok = handle.join().unwrap_or(false);
        let curr = get_dialog_current_path(dlg_hwnd).unwrap_or_default();
        (ok, curr)
    }

    #[test]
    #[ignore = "spawns a real Windows file dialog and moves the mouse"]
    fn live_dialog_jump_open_and_save() {
        use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
        use windows::Win32::UI::Shell::{FileOpenDialog, IFileOpenDialog};
        use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, WM_CLOSE};

        let dlg_thread = std::thread::spawn(|| {
            let Ok(_com) = crate::win_fileops::Com::init() else {
                return;
            };
            // SAFETY: COM STA initialized on this thread.
            if let Ok(dialog) = unsafe {
                CoCreateInstance::<_, IFileOpenDialog>(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)
            } {
                // SAFETY: modal dialog shown on dedicated STA thread.
                let _ = unsafe { dialog.Show(None) };
            }
        });

        LIVE_FOUND_DLG.store(0, Ordering::SeqCst);
        let mut dlg_hwnd = HWND(std::ptr::null_mut());
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(100));
            // SAFETY: callback only updates static atomic.
            unsafe {
                let _ = EnumWindows(Some(find_live_dlg), LPARAM(0));
            }
            let raw = LIVE_FOUND_DLG.load(Ordering::SeqCst);
            if raw != 0 {
                dlg_hwnd = HWND(raw as *mut _);
                break;
            }
        }
        assert!(!dlg_hwnd.0.is_null(), "failed to spawn live #32770 dialog");
        std::thread::sleep(Duration::from_millis(300));

        // Guard verification: dialog belongs to our own process, so `is_arke_owned_dialog` is true.
        assert!(is_arke_owned_dialog(dlg_hwnd));

        let mut ctrls = LiveControls {
            dui: None,
            tree: None,
            toolbar: None,
        };
        // SAFETY: `ctrls` outlives `EnumChildWindows`.
        unsafe {
            let _ = EnumChildWindows(
                Some(dlg_hwnd),
                Some(find_live_controls),
                LPARAM((&raw mut ctrls) as isize),
            );
        }

        let (ok1, p1) =
            live_click_and_inject(dlg_hwnd, ctrls.dui.expect("DirectUIHWND"), r"C:\Windows\System32");
        let (ok2, p2) =
            live_click_and_inject(dlg_hwnd, ctrls.tree.expect("SysTreeView32"), r"C:\Users");
        let (ok3, p3) =
            live_click_and_inject(dlg_hwnd, ctrls.toolbar.expect("ToolbarWindow32"), r"C:\Windows");

        // Close dialog before asserting so the test thread never hangs on failure.
        // SAFETY: closes the test dialog cleanly.
        unsafe {
            let _ = PostMessageW(Some(dlg_hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        let _ = dlg_thread.join();

        assert!(ok1 && paths_equal(&p1, r"C:\Windows\System32"), "p1={p1}");
        assert!(ok2 && paths_equal(&p2, r"C:\Users"), "p2={p2}");
        assert!(ok3 && paths_equal(&p3, r"C:\Windows"), "p3={p3}");
    }
}
