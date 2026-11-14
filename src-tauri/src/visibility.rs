//! 可见性管理：显示/隐藏统一由 Rust 决策。
//!
//! 两段式隐藏（WebView2 157 透明窗口冻结陷阱的实测最终方案）：
//!   溶解 dissolve = 页面淡出到 opacity 0.004（CSS 过渡 0.45s）→ 480ms 后窗口 hide()。
//!   hide 的目的：低 alpha 常驻仍有合成开销且偶发残影，彻底隐藏最干净；
//!   恢复 restore = show + 尺寸抖动强制重绘（防恢复空白）+ 直接注入 opacity='' 淡回。
//!
//! 可见 = （未开启仅播放时显示 或 正在播放）且（未开启全屏隐藏 或 前台非全屏）
//!
//! 竞态防护（generation 机制）：每个窗口一个代数计数器。所有延迟任务
//! （隐藏延迟线程 / 溶解 hide 定时器 / 恢复淡入线程）持有启动时的代数，
//! 执行前校验代数未变；期间任何一次状态翻转都会推进代数使旧任务失效。
//! 代数推进与溶解标记在同一个互斥锁内完成，杜绝“检查-执行”间隙竞态。

use std::collections::HashMap;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};

#[derive(Default, Clone)]
struct VisState {
    dissolved: bool,
    gen: u64,
}

static VIS: std::sync::OnceLock<Mutex<HashMap<String, VisState>>> = std::sync::OnceLock::new();

fn vis_map() -> &'static Mutex<HashMap<String, VisState>> {
    VIS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn vis_state_of(label: &str) -> VisState {
    vis_map()
        .lock()
        .unwrap()
        .get(label)
        .cloned()
        .unwrap_or_default()
}

fn with_vis(label: &str, f: impl FnOnce(&mut VisState)) {
    let mut m = vis_map().lock().unwrap_or_else(|e| e.into_inner());
    f(m.entry(label.to_string()).or_default());
}

fn gen_of(label: &str) -> u64 {
    vis_state_of(label).gen
}

/// 推进代数：使该窗口所有未决延迟任务失效。返回新代数。
fn bump_gen(label: &str) -> u64 {
    let mut g = 0;
    with_vis(label, |st| {
        st.gen += 1;
        g = st.gen;
    });
    g
}

pub fn is_dissolved(label: &str) -> bool {
    vis_state_of(label).dissolved
}

/// 标记窗口恢复显示：清除溶解标记并推进代数（使未决的隐藏/溶解任务失效）。
/// 仅应在“窗口变为可见”的路径调用（show_mode / 恢复分支）。
pub fn mark_onscreen(label: &str) {
    with_vis(label, |st| {
        st.dissolved = false;
        st.gen += 1;
    });
}

/// 尝试开始溶解：校验、标记、推进代数在锁内原子完成。
/// expected = Some(g) 时要求当前代数仍为 g（延迟线程用，防止与恢复决策竞态）。
/// 成功返回用于 hide 定时器校验的新代数；已溶解或代数过期返回 None。
fn begin_dissolve(label: &str, expected: Option<u64>) -> Option<u64> {
    let mut m = vis_map().lock().unwrap_or_else(|e| e.into_inner());
    let st = m.entry(label.to_string()).or_default();
    if st.dissolved {
        return None;
    }
    if let Some(g) = expected {
        if st.gen != g {
            return None;
        }
    }
    st.gen += 1;
    st.dissolved = true;
    Some(st.gen)
}

/// 把窗口收缩隐藏（两段式：淡出 → 480ms 后 hide）。无条件版本，
/// 供 show_mode 收缩“其他形态”窗口使用；已溶解时为 no-op。
pub fn dissolve(w: &tauri::WebviewWindow, app: &AppHandle) {
    run_dissolve(w, app, None);
}

/// 带代数校验的溶解：供延迟隐藏线程使用，防止与恢复决策竞态。
fn dissolve_checked(w: &tauri::WebviewWindow, app: &AppHandle, expected_gen: u64) {
    run_dissolve(w, app, Some(expected_gen));
}

fn run_dissolve(w: &tauri::WebviewWindow, app: &AppHandle, expected: Option<u64>) {
    let label = w.label().to_string();
    let Some(gen) = begin_dissolve(&label, expected) else {
        return;
    };
    crate::flog!(
        "[vis] dissolve {} (fade) tid={:?}",
        label,
        std::thread::current().id()
    );
    let _ = w.set_ignore_cursor_events(true);
    // 同步分区穿透线程的状态缓存（轮询对已溶解窗口停摆，需外部告知实际值）
    crate::passthrough_note(&label, true);
    // 淡出（前端 CSS 过渡 0.45s）→ 完全透明后 hide
    let _ = w.eval("document.getElementById('app').style.opacity='0.004';");
    let _ = app.emit_to(&label, "vis", false);
    let app2 = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(480));
        // 期间发生过恢复/切换（代数已推进）→ 放弃隐藏，交由最新决策
        if gen_of(&label) != gen {
            return;
        }
        if let Some(w2) = app2.get_webview_window(&label) {
            let _ = w2.hide();
            crate::flog!(
                "[vis] dissolve done (faded+hidden) tid={:?}",
                std::thread::current().id()
            );
        }
    });
}

/// 当前可见性决策（与具体形态无关的公共条件）
fn visible_decision(app: &AppHandle) -> bool {
    let state = app.state::<crate::AppState>();
    let s = state.settings.lock().unwrap_or_else(|e| e.into_inner());
    let fs = state.fullscreen.load(std::sync::atomic::Ordering::Relaxed);
    let playing = state.snapshot.lock().unwrap_or_else(|e| e.into_inner()).status == "playing";
    (!s.show_only_playing || playing) && (!s.hide_fullscreen || !fs)
}

/// 依据播放状态/全屏状态重算当前形态窗口的可见性。
/// 隐藏流程：锚定延迟（扣除检测耗时）→ 复核 → 收缩动画。
/// 恢复流程：推进代数 → show + 置顶 + 抖动重绘 → 淡回。
pub fn update_visibility(app: &AppHandle) {
    let state = app.state::<crate::AppState>();
    let s = state.settings.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let visible = visible_decision(app);
    let Some(w) = app.get_webview_window(&s.mode) else { return };
    let hidden = is_dissolved(&s.mode);

    if visible {
        if hidden {
            // 恢复：mark_onscreen 推进代数，使 dissolve 的 hide 定时器等旧任务失效
            crate::flog!("[vis] restore {} tid={:?}", s.mode, std::thread::current().id());
            mark_onscreen(&s.mode);
            let gen = gen_of(&s.mode);
            let _ = w.show();
            crate::win32::reassert_topmost(&w);
            let _ = w.set_ignore_cursor_events(s.clickthrough_lock);
            crate::passthrough_note(&s.mode, s.clickthrough_lock);
            let sz = w.inner_size().unwrap_or_default();
            if sz.width == 0 || sz.height == 0 {
                // 取不到尺寸（窗口异常）：跳过抖动，直接淡回
                let _ = w.eval("document.getElementById('app').style.opacity='';");
                let _ = app.emit_to(&s.mode, "vis", true);
                return;
            }
            // show 后尺寸抖动强制 WebView2 重绘（防冻结 Blank）
            let _ = w.set_size(tauri::PhysicalSize::new(sz.width + 1, sz.height + 1));
            let app2 = app.clone();
            let mode2 = s.mode.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(90));
                // 期间又发生状态翻转（如快速暂停）→ 放弃，交给最新决策
                if gen_of(&mode2) != gen {
                    return;
                }
                if let Some(w2) = app2.get_webview_window(&mode2) {
                    let _ = w2.set_size(tauri::PhysicalSize::new(sz.width, sz.height));
                    // 淡回
                    let _ = w2.eval("document.getElementById('app').style.opacity='';");
                    let _ = app2.emit_to(&mode2, "vis", true);
                }
                crate::flog!("[vis] restore done (nudge+fadein)");
            });
        } else {
            // 已可见：无需视觉动作，但推进代数取消所有未决的隐藏任务
            bump_gen(&s.mode);
        }
    } else if !hidden {
        crate::flog!(
            "[vis] hide scheduled {} (delay={}s)",
            s.mode, s.hide_delay_secs
        );
        let app2 = app.clone();
        let mode = s.mode.clone();
        // 记录当前代数：延迟期间任何状态翻转都会推进代数，使本任务作废
        let gen = gen_of(&s.mode);
        // 延迟锚定到"物理暂停时刻"：扣除状态检测本身的耗时，
        // 保证从真实暂停到消失的总时长 = 用户设定值
        let detected_lag_ms = {
            let snap = state.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            now_ms.saturating_sub(snap.status_changed_at)
        };
        let wait_ms = (s.hide_delay_secs as u64 * 1000).saturating_sub(detected_lag_ms);
        let delay = std::time::Duration::from_millis(wait_ms);
        crate::flog!("[vis] hide delay={}ms", wait_ms);
        std::thread::spawn(move || {
            // 等待锚定后的延迟；期间恢复播放/切换形态则整个取消
            std::thread::sleep(delay);
            if gen_of(&mode) != gen {
                return;
            }
            if visible_decision(&app2) {
                return; // 延迟期间恢复播放，什么都不用做
            }
            if let Some(w2) = app2.get_webview_window(&mode) {
                dissolve_checked(&w2, &app2, gen);
            }
        });
    }
    // !visible && hidden：已溶解，无需动作
}
