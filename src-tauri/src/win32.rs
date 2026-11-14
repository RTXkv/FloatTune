//! Win32 杂项：任务栏定位、前台全屏检测、进程句柄 RAII

use std::time::Duration;
use tauri::Emitter;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetClassNameW, GetDesktopWindow, GetForegroundWindow, GetWindowRect,
};

/// RAII 进程句柄：离开作用域自动 CloseHandle。
/// 此前 OpenProcess 的句柄从不释放，快路径每 120ms 轮询一次，
/// 长时间运行句柄数持续增长（且 `?` 提前返回路径同样泄漏）。
pub struct ProcessHandle(pub HANDLE);

impl ProcessHandle {
    /// OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION) + RAII 包装
    pub fn open(pid: u32) -> Option<Self> {
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok().map(Self) }
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// 主任务栏（Shell_TrayWnd）的屏幕矩形；Win11 新任务栏可能返回无效矩形，
/// 所以做尺寸校验，无效则返回 None（由调用方按主屏底部兜底）
pub fn taskbar_rect() -> Option<RECT> {
    unsafe {
        let name = windows::core::w!("Shell_TrayWnd");
        let hwnd = FindWindowW(PCWSTR::from_raw(name.as_ptr()), PCWSTR::null()).ok()?;
        let mut rect = RECT::default();
        GetWindowRect(hwnd, &mut rect).ok()?;
        let w = rect.right - rect.left;
        let h = rect.bottom - rect.top;
        if w < 200 || h < 20 || h > 200 {
            return None;
        }
        Some(rect)
    }
}

/// 重新申明置顶：QQ/微信等小程序面板也是 topmost，会在 z 序上压过我们，
/// 周期性把窗口拉回 TOPMOST（不移动、不激活）
pub fn reassert_topmost(win: &tauri::WebviewWindow) {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    };
    // tauri 的 HWND 来自更新版的 windows crate，内部同样是裸指针，跨版本转换安全
    if let Ok(h) = win.hwnd() {
        let hwnd = windows::Win32::Foundation::HWND(h.0);
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }
}

/// 前台窗口是否为全屏应用（用于自动避让）
pub fn foreground_fullscreen() -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() || fg == GetDesktopWindow() {
            return false;
        }
        let hmon = MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST);
        if hmon.is_invalid() {
            return false;
        }
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
            return false;
        }
        let mut rc = RECT::default();
        if GetWindowRect(fg, &mut rc).is_err() {
            return false;
        }
        if rc != mi.rcMonitor {
            return false;
        }
        // 排除桌面/任务栏等“假全屏”
        let mut buf = [0u16; 64];
        let len = GetClassNameW(fg, &mut buf);
        let class = if len > 0 {
            String::from_utf16_lossy(&buf[..len as usize])
        } else {
            String::new()
        };
        !matches!(
            class.as_str(),
            "Progman" | "WorkerW" | "Shell_TrayWnd" | "XamlExplorerHostIslandWindow"
        )
    }
}

/// 全屏避让监视线程：每 2s 检查，状态变化时更新状态位并触发可见性决策
pub fn spawn_fullscreen_watcher(app: tauri::AppHandle) {
    std::thread::Builder::new()
        .name("fullscreen-watch".into())
        .spawn(move || {
            let mut last = false;
            loop {
                let now = foreground_fullscreen();
                if now != last {
                    last = now;
                    {
                        use tauri::Manager;
                        let state = app.state::<crate::AppState>();
                        state
                            .fullscreen
                            .store(now, std::sync::atomic::Ordering::Relaxed);
                    }
                    crate::visibility::update_visibility(&app);
                    let _ = app.emit("fullscreen", now);
                }
                // 周期性重申置顶 + 透明度安全网（仅当“本应可见”时）
                {
                    use tauri::Manager;
                    let st = app.state::<crate::AppState>();
                    let s = st.settings.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    let fs = st.fullscreen.load(std::sync::atomic::Ordering::Relaxed);
                    let playing = st.snapshot.lock().unwrap_or_else(|e| e.into_inner()).status == "playing";
                    let should_show =
                        (!s.show_only_playing || playing) && (!s.hide_fullscreen || !fs);
                    drop(st);
                    if should_show {
                        if let Some(w) = app.get_webview_window(&s.mode) {
                            if w.is_visible().unwrap_or(false) {
                                reassert_topmost(&w);
                                let _ = w.eval(
                                    "if(window.__TAURI__&&window.__TAURI__.core){var a=document.getElementById('app');if(a){a.style.opacity='';}}",
                                );
                            }
                        }
                    }
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        })
        .expect("spawn fullscreen watcher");
}

/// 网易云主窗口标题（"歌名 - 歌手"），纯 Win32 调用，毫秒级返回。
/// 优先取 OrpheusBrowserHost 类（CEF 主界面），回退任意可见的有题窗口。
pub fn netease_window_title() -> Option<String> {
    use windows::Win32::Foundation::BOOL;
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{LPARAM, HWND};
    use windows::Win32::System::Threading::{
        QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetWindowTextLengthW, GetWindowTextW,
        GetWindowThreadProcessId,
    };

    struct Ctx {
        hits: Vec<(String, String)>, // (class, title)
        pids: std::collections::HashMap<u32, String>,
    }
    unsafe extern "system" fn cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let ctx = &mut *(lparam.0 as *mut Ctx);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return BOOL(1);
        }
        let exe = ctx.pids.entry(pid).or_insert_with(|| {
            ProcessHandle::open(pid)
                .and_then(|h| {
                    let mut buf = [0u16; 1024];
                    let mut len = buf.len() as u32;
                    QueryFullProcessImageNameW(h.0, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len)
                        .ok()?;
                    Some(
                        String::from_utf16_lossy(&buf[..len as usize])
                            .rsplit(['\\', '/'])
                            .next()
                            .unwrap_or("")
                            .to_lowercase(),
                    )
                })
                .unwrap_or_default()
        });
        if exe != "cloudmusic.exe" {
            return BOOL(1);
        }
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return BOOL(1);
        }
        let mut tb = [0u16; 512];
        let n = GetWindowTextW(hwnd, &mut tb);
        let title = String::from_utf16_lossy(&tb[..n.max(0) as usize]);
        if title.is_empty() {
            return BOOL(1);
        }
        let mut cbuf = [0u16; 64];
        let cn = GetClassNameW(hwnd, &mut cbuf);
        let class = String::from_utf16_lossy(&cbuf[..cn.max(0) as usize]);
        ctx.hits.push((class, title));
        BOOL(1)
    }

    let mut ctx = Ctx { hits: Vec::new(), pids: std::collections::HashMap::new() };
    unsafe {
        let _ = EnumWindows(
            Some(cb),
            LPARAM(&mut ctx as *mut Ctx as isize),
        );
    }
    // 选出真正的"歌名 - 歌手"标题：排除桌面歌词/客户端本身/GDI+/SMTC 系统窗口，
    // 优先含 " - " 的标题（歌名 - 歌手），其次 OrpheusBrowserHost 类
    let excluded = ["桌面歌词", "网易云音乐", "GDI+ Window", "MediaPlayer SMTC window"];
    let is_excluded = |t: &str| excluded.iter().any(|e| t.contains(e));
    let hit = ctx
        .hits
        .iter()
        .find(|(_, t)| !is_excluded(t) && t.contains(" - "))
        .or_else(|| ctx.hits.iter().find(|(c, t)| c == "OrpheusBrowserHost" && !is_excluded(t)))
        .or_else(|| ctx.hits.iter().find(|(_, t)| !is_excluded(t)))?;
    Some(hit.1.clone())
}

