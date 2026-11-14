//! 托盘菜单：形态切换 / 歌词样式 / 点击穿透解除 / 设置 / 退出

use crate::AppState;
use tauri::menu::{CheckMenuItem, Menu, PredefinedMenuItem};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::Manager;

fn check(
    app: &tauri::AppHandle,
    id: &str,
    text: &str,
    checked: bool,
) -> tauri::Result<CheckMenuItem<tauri::Wry>> {
    CheckMenuItem::with_id(app, id, text, true, checked, None::<&str>)
}

/// 组装托盘菜单（根据当前设置勾选）
pub fn build_menu(app: &tauri::AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let state: tauri::State<AppState> = app.state();
    let s = state.settings.lock().unwrap().clone();

    let m_strip = check(app, "mode-strip", "极简浮窗 · 一行歌词", s.mode == "strip")?;
    let m_card = check(app, "mode-card", "全览浮窗 · 封面卡片", s.mode == "card")?;

    let l_off = check(app, "lyr-off", "关闭歌词", s.lyric_style == "off")?;
    let l_single = check(app, "lyr-single", "当前一句", s.lyric_style == "single")?;
    let l_multi = check(app, "lyr-multi", "多行渐隐", s.lyric_style == "multi")?;
    let l_scroll = check(app, "lyr-scroll", "歌词滚动", s.lyric_style == "scroll")?;
    let l_karaoke = check(app, "lyr-karaoke", "逐字卡拉OK", s.lyric_style == "karaoke")?;

    let unlock = tauri::menu::MenuItem::with_id(
        app,
        "unlock",
        if s.clickthrough_lock {
            "解除点击穿透"
        } else {
            "点击穿透已关"
        },
        s.clickthrough_lock,
        None::<&str>,
    )?;
    let reposition = tauri::menu::MenuItem::with_id(app, "reposition", "重新定位窗口", true, None::<&str>)?;
    let settings_item =
        tauri::menu::MenuItem::with_id(app, "show-settings", "设置…", true, None::<&str>)?;
    let quit = tauri::menu::MenuItem::with_id(app, "quit", "退出 FloatTune", true, None::<&str>)?;

    Menu::with_items(
        app,
        &[
            &m_strip,
            &m_card,
            &PredefinedMenuItem::separator(app)?,
            &l_off,
            &l_single,
            &l_multi,
            &l_scroll,
            &l_karaoke,
            &PredefinedMenuItem::separator(app)?,
            &unlock,
            &reposition,
            &settings_item,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )
}

/// 创建托盘（setup 时调用一次）
pub fn create_tray(app: &tauri::AppHandle) -> tauri::Result<TrayIcon> {
    let menu = build_menu(app)?;
    let icon = tauri::include_image!("icons/tray.png");
    TrayIconBuilder::with_id("floatune-tray")
        .icon(icon)
        .tooltip("FloatTune - 网易云伴生小工具")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| {
            let id = event.id().as_ref().to_string();
            handle_menu(app, &id);
        })
        .build(app)
}

/// 菜单事件 → 更新设置 → 应用并广播
fn handle_menu(app: &tauri::AppHandle, id: &str) {
    match id {
        "show-settings" => {
            if let Some(w) = app.get_webview_window("settings") {
                let _ = w.show();
                let _ = w.set_focus();
            }
            return;
        }
        "quit" => {
            app.exit(0);
            return;
        }
        _ => {}
    }

    {
        let state: tauri::State<AppState> = app.state();
        let mut s = state.settings.lock().unwrap();
        match id {
            "mode-strip" | "mode-card" => {
                s.mode = id.trim_start_matches("mode-").to_string();
            }
            "lyr-off" | "lyr-single" | "lyr-multi" | "lyr-scroll" | "lyr-karaoke" => {
                s.lyric_style = id.trim_start_matches("lyr-").to_string();
            }
            "unlock" => s.clickthrough_lock = false,
            "reposition" => {
                let mode = s.mode.clone();
                s.positions.remove(&mode);
            }
            _ => {}
        }
    }
    crate::apply_and_broadcast(app);
}

/// 重建托盘菜单以刷新勾选
pub fn refresh_tray(app: &tauri::AppHandle) {
    if let Some(tray) = app.tray_by_id("floatune-tray") {
        if let Ok(menu) = build_menu(app) {
            let _ = tray.set_menu(Some(menu));
        }
    }
}
