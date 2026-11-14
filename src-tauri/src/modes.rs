//! 两种形态的窗口布局管理：极简浮窗(strip) / 全览浮窗(card)
//! 尺寸统一用逻辑像素定义，按显示器缩放系数和用户设置的界面缩放换算。

use crate::settings::Settings;
use crate::win32;
use tauri::{AppHandle, Manager, PhysicalPosition, PhysicalSize, WebviewWindow};

pub const MODES: [&str; 2] = ["strip", "card"];

// 以下均为逻辑像素（ui_scale=1.0 时）；卡片 = 头部(封面+歌名) + 歌词区
const STRIP_SIZE: (f64, f64) = (620.0, 56.0);
const CARD_SIZE: (f64, f64) = (250.0, 232.0);

fn get_window(app: &AppHandle, label: &str) -> Option<WebviewWindow> {
    app.get_webview_window(label)
}

/// 校验记忆位置：窗口至少要有 160x80 物理像素在主屏内，否则视为污染数据返回 None
fn sanitize_pos(pos: (f64, f64, f64, f64), mw: f64, mh: f64) -> Option<(f64, f64, f64, f64)> {
    let (x, y, w, h) = pos;
    // 只校验位置是否大半在主屏内；w/h 为 0 表示从未调整过大小（尺寸走默认值）
    let min_w = w.max(300.0);
    let min_h = h.max(200.0);
    if x < 160.0 - min_w || y < 80.0 - min_h || x > mw - 160.0 || y > mh - 80.0 {
        return None;
    }
    Some(pos)
}

/// 主显示器：物理宽高 + 缩放系数
fn primary_monitor_info(win: &WebviewWindow) -> (f64, f64, f64) {
    win.primary_monitor()
        .ok()
        .flatten()
        .map(|m| {
            (
                m.size().width as f64,
                m.size().height as f64,
                m.scale_factor(),
            )
        })
        .unwrap_or((1920.0, 1200.0, 1.0))
}

/// 主任务栏高度（物理像素），取不到时按逻辑 48px 兜底
fn taskbar_height_phys(win: &WebviewWindow) -> f64 {
    win32::taskbar_rect()
        .map(|r| (r.bottom - r.top) as f64)
        .filter(|h| *h > 20.0)
        .unwrap_or_else(|| 48.0 * primary_monitor_info(win).2)
}

/// 切换并定位窗口；同时应用点击穿透状态
pub fn show_mode(app: &AppHandle, mode: &str, settings: &Settings) -> Result<(), String> {
    crate::flog!("[modes] show_mode {} begin tid={:?}", mode, std::thread::current().id());
    for m in MODES {
        if m != mode {
            if let Some(w) = get_window(app, m) {
                crate::visibility::dissolve(&w, app);
            }
        }
    }
    let Some(win) = get_window(app, mode) else {
        return Err(format!("窗口 {mode} 不存在"));
    };

    let (mw, mh, sc) = primary_monitor_info(&win);
    let remembered = settings
        .positions
        .get(mode)
        .map(|p| {
            (
                p.first().copied().unwrap_or(0.0),
                p.get(1).copied().unwrap_or(0.0),
                p.get(2).copied().unwrap_or(0.0),
                p.get(3).copied().unwrap_or(0.0),
            )
        })
        // 记忆位置可能被屏幕外坐标污染（历史 bug），先校验
        .and_then(|p| sanitize_pos(p, mw, mh));
    let u = settings.ui_scale.clamp(0.7, 1.6);
    let (x, y, w, h) = match mode {
        "strip" => {
            let dw = STRIP_SIZE.0 * sc * u;
            let dh = STRIP_SIZE.1 * sc * u;
            let tb_h = taskbar_height_phys(&win);
            let pos = remembered.unwrap_or_else(|| ((mw - dw) / 2.0, mh - tb_h - dh - 6.0 * sc, dw, dh));
            // 用户拖拽调整过的大小优先（w/h>0），限制合理范围
            let sw = if pos.2 > 0.0 { pos.2.clamp(340.0 * sc, 1100.0 * sc) } else { dw };
            let sh = if pos.3 > 0.0 { pos.3.clamp(48.0 * sc, 240.0 * sc) } else { dh };
            (pos.0, pos.1, sw, sh)
        }
        "card" => {
            let dw = CARD_SIZE.0 * sc * u;
            // 默认高度 = 头部(封面+歌名) + 歌词区；垂直拖拽调整歌词区高度
            let dh = CARD_SIZE.1 * sc * u;
            let tb_h = taskbar_height_phys(&win);
            let pos = remembered.unwrap_or((mw - dw - 20.0 * sc, mh - tb_h - dh - 14.0 * sc, dw, dh));
            let cw = if pos.2 > 0.0 { pos.2.clamp(250.0 * sc, 560.0 * sc) } else { dw };
            // 高度可自由调整（歌词区弹性伸缩）
            let ch = if pos.3 > 0.0 { pos.3.clamp(200.0 * sc, 700.0 * sc) } else { dh };
            (pos.0, pos.1, cw, ch)
        }
        other => return Err(format!("未知形态 {other}")),
    };

    let (x, y, w, h) = (x as i32, y as i32, w.max(120.0) as i32, h.max(30.0) as i32);
    let _ = win.set_size(PhysicalSize::new(w as u32, h as u32));
    let _ = win.set_position(PhysicalPosition::new(x, y));
    let _ = win.show();
    crate::visibility::mark_onscreen(mode);
    crate::flog!("[modes] show_mode {} end tid={:?}", mode, std::thread::current().id());
    let _ = win.set_ignore_cursor_events(settings.clickthrough_lock);
    // 同步分区穿透线程的缓存（mark_onscreen 后轮询恢复，需告知实际值）
    crate::passthrough_note(mode, settings.clickthrough_lock);
    // 恢复透明度（淡出可能把 inline opacity 置 0；直接注入 JS 兜底，不依赖事件）
    let _ = win.eval("var a=document.getElementById('app'); if(a) a.style.opacity='';");
    // 只定向当前形态窗口：全局广播会让刚被 dissolve 的其他窗口收到 vis=true
    // 恢复 opacity，出现幽灵显示
    use tauri::Emitter;
    let _ = app.emit_to(mode, "vis", true);
    Ok(())
}
