//! 可见性管理：显示/隐藏统一由 Rust 决策。
//!
//! “消失”= 页面淡出到 opacity 0.004（CSS 过渡 0.45s），窗口本身全程
//! 不动、不 hide、保持置顶与渲染；恢复 = 淡回。
//!
//! 可见 = （未开启仅播放时显示 或 正在播放）且（未开启全屏隐藏 或 前台非全屏）

use std::collections::HashMap;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};

/// 溶解状态（label -> 是否已收缩隐藏）
static DISSOLVED: std::sync::OnceLock<Mutex<HashMap<String, bool>>> = std::sync::OnceLock::new();

pub fn is_dissolved(label: &str) -> bool {
    DISSOLVED
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .get(label)
        .copied()
        .unwrap_or(false)
}

/// 标记窗口已恢复显示（清除溶解状态）
pub fn mark_onscreen(label: &str) {
    DISSOLVED
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .insert(label.to_string(), false);
}

/// 把窗口收缩成 1×1 像素（带收缩动画）。
/// 注意：不调用 set_ignore_cursor_events（附加 WS_EX_LAYERED 有冻结风险），
/// 1×1 窗口本身点击面积可忽略。
pub fn dissolve(w: &tauri::WebviewWindow, app: &AppHandle) {
    let label = w.label().to_string();
    if is_dissolved(&label) {
        return;
    }
    eprintln!(
        "[vis] dissolve {} (fade) tid={:?}",
        label,
        std::thread::current().id()
    );
    let _ = w.set_ignore_cursor_events(true);
    // 淡出（前端 CSS 过渡 0.45s）→ 完全透明后 hide
    let _ = w.eval("document.getElementById('app').style.opacity='0.004';");
    let app2 = app.clone();
    let label2 = label.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(480));
        if let Some(w2) = app2.get_webview_window(&label2) {
            let _ = w2.hide();
            eprintln!("[vis] dissolve done (faded+hidden) tid={:?}", std::thread::current().id());
        }
    });
    DISSOLVED
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .insert(label, true);
}

/// 依据播放状态/全屏状态重算当前形态窗口的可见性。
/// 隐藏流程：锚定延迟（扣除检测耗时）→ 复核 → 收缩动画。
pub fn update_visibility(app: &AppHandle) {
    let state = app.state::<crate::AppState>();
    let s = state.settings.lock().unwrap().clone();
    let fs = state.fullscreen.load(std::sync::atomic::Ordering::Relaxed);
    let playing = state.snapshot.lock().unwrap().status == "playing";
    let visible = (!s.show_only_playing || playing) && (!s.hide_fullscreen || !fs);
    let Some(w) = app.get_webview_window(&s.mode) else { return };
    let hidden = is_dissolved(&s.mode);

    if visible && hidden {
        eprintln!("[vis] restore {} (playing={})", s.mode, playing);
        let _ = w.show();
        crate::win32::reassert_topmost(&w);
        let _ = w.set_ignore_cursor_events(s.clickthrough_lock);
        mark_onscreen(&s.mode);
        // show 后尺寸抖动强制 WebView2 重绘（防冻结 Blank）
        let sz = w.inner_size().unwrap_or_default();
        let _ = w.set_size(tauri::PhysicalSize::new(sz.width + 1, sz.height + 1));
        let app2 = app.clone();
        let mode2 = s.mode.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(90));
            if let Some(w2) = app2.get_webview_window(&mode2) {
                let _ = w2.set_size(tauri::PhysicalSize::new(sz.width, sz.height));
                // 淡回
                let _ = w2.eval("document.getElementById('app').style.opacity='';");
                let _ = app2.emit_to(mode2, "vis", true);
            }
            eprintln!("[vis] restore done (nudge+fadein)");
        });
    } else if !visible && !hidden {
        eprintln!(
            "[vis] hide scheduled {} (playing={}, delay={}s)",
            s.mode, playing, s.hide_delay_secs
        );
        let app2 = app.clone();
        let mode = s.mode.clone();
        // 延迟锚定到"物理暂停时刻"：扣除状态检测本身的耗时，
        // 保证从真实暂停到消失的总时长 = 用户设定值
        let detected_lag_ms = {
            let snap = state.snapshot.lock().unwrap().clone();
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            now_ms.saturating_sub(snap.status_changed_at)
        };
        let wait_ms = (s.hide_delay_secs.max(0) as u64 * 1000).saturating_sub(detected_lag_ms);
        let delay = std::time::Duration::from_millis(wait_ms);
        eprintln!("[vis] hide delay={}ms", wait_ms);
        std::thread::spawn(move || {
            // 等待锚定后的延迟；期间恢复播放则整个取消
            std::thread::sleep(delay);
            let st = app2.state::<crate::AppState>();
            let s2 = st.settings.lock().unwrap().clone();
            let fs2 = st.fullscreen.load(std::sync::atomic::Ordering::Relaxed);
            let playing2 = st.snapshot.lock().unwrap().status == "playing";
            let should_hide = !((!s2.show_only_playing || playing2) && (!s2.hide_fullscreen || !fs2));
            if !should_hide {
                return; // 延迟期间恢复播放，什么都不用做
            }
            if let Some(w2) = app2.get_webview_window(&mode) {
                dissolve(&w2, &app2);
            }
        });
    }
}
