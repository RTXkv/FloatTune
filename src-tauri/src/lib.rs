mod drag;
mod modes;
mod netease;
mod settings;
mod smtc;
mod tray;
mod visibility;
mod volume;
mod win32;

use netease::NeteaseClient;
use settings::Settings;
use smtc::{MediaCommand, TrackSnapshot};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use tauri::{Emitter, Manager};

/// 位置记忆落盘节流
static LAST_POS_SAVE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn throttle_save(app: &tauri::AppHandle) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let last = LAST_POS_SAVE.load(std::sync::atomic::Ordering::Relaxed);
    if now.saturating_sub(last) > 2000 {
        LAST_POS_SAVE.store(now, std::sync::atomic::Ordering::Relaxed);
        let state = app.state::<AppState>();
        let dir = state.data_dir.clone();
        let snap = state.settings.lock().unwrap().clone();
        snap.save(&dir);
    }
}

pub struct AppState {
    pub settings: Mutex<Settings>,
    pub snapshot: Arc<Mutex<TrackSnapshot>>,
    pub media_tx: mpsc::Sender<MediaCommand>,
    pub data_dir: PathBuf,
    pub lyric_key_cache: Mutex<HashMap<String, netease::TrackKey>>,
    pub fullscreen: std::sync::atomic::AtomicBool,
}

/// 浮窗点击穿透分区：前端上报 [x,y,w,h,kind]（物理像素，kind: 0=可交互 1=拖动区），
/// 其余区域全部穿透。后台线程 30ms 轮询光标切换 ignore_cursor_events。
static WINDOW_REGIONS: std::sync::OnceLock<Mutex<HashMap<String, Vec<(f64, f64, f64, f64, u8)>>>> =
    std::sync::OnceLock::new();

fn window_regions() -> &'static Mutex<HashMap<String, Vec<(f64, f64, f64, f64, u8)>>> {
    WINDOW_REGIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[tauri::command]
fn update_strip_regions(
    window: String,
    rects: Vec<(f64, f64, f64, f64, u8)>,
) {
    window_regions().lock().unwrap().insert(window, rects);
}

fn spawn_region_passthrough(app: tauri::AppHandle) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    std::thread::Builder::new()
        .name("region-passthrough".into())
        .spawn(move || {
            let mut last_ignore: HashMap<String, bool> = HashMap::new();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(30));
                let locked = {
                    use tauri::Manager;
                    app.state::<AppState>()
                        .settings
                        .lock()
                        .unwrap()
                        .clickthrough_lock
                };
                for label in ["strip", "card"] {
                    let Some(w) = app.get_webview_window(label) else { continue };
                    if crate::visibility::is_dissolved(label) {
                        continue;
                    }
                    let Ok(win_pos) = w.outer_position() else { continue };
                    let mut pt = POINT::default();
                    unsafe {
                        let _ = GetCursorPos(&mut pt);
                    }
                    let rx = (pt.x - win_pos.x) as f64;
                    let ry = (pt.y - win_pos.y) as f64;
                    let hit = window_regions()
                        .lock()
                        .unwrap()
                        .get(label)
                        .map(|rs| {
                            rs.iter()
                                .any(|(x, y, rw, rh, _)| {
                                    rx >= *x && ry >= *y && rx <= x + rw && ry <= y + rh
                                })
                        })
                        .unwrap_or(true); // 未上报区域的窗口保持可交互
                    let ignore = locked || !hit;
                    let changed = last_ignore.get(label) != Some(&ignore);
                    if changed {
                        last_ignore.insert(label.to_string(), ignore);
                        let _ = w.set_ignore_cursor_events(ignore);
                    }
                }
            }
        })
        .expect("spawn region passthrough");
}

#[tauri::command]
fn debug_log(msg: String) {
    eprintln!("[fe] {}", msg);
}

/// 应用设置变更：持久化 → 窗口布局 → 托盘刷新 → 广播前端
pub fn apply_and_broadcast(app: &tauri::AppHandle) {
    let snap = {
        let state = app.state::<AppState>();
        let cur = state.settings.lock().unwrap();
        cur.save(&state.data_dir);
        cur.clone()
    };
    let _ = modes::show_mode(app, &snap.mode, &snap);
    if let Some(w) = app.get_webview_window(&snap.mode) {
        let _ = w.set_ignore_cursor_events(snap.clickthrough_lock);
    }
    tray::refresh_tray(app);
    visibility::update_visibility(app);
    let _ = app.emit("settings", snap);
}

// ---------- Tauri 命令 ----------

#[tauri::command]
fn get_settings(state: tauri::State<AppState>) -> Settings {
    state.settings.lock().unwrap().clone()
}

#[tauri::command]
fn set_settings(app: tauri::AppHandle, state: tauri::State<AppState>, settings: Settings) {
    let autostart_changed;
    {
        let mut cur = state.settings.lock().unwrap();
        autostart_changed = cur.autostart != settings.autostart;
        // 位置记忆由窗口拖动维护，设置窗口不动它
        let mut new = settings;
        new.positions = cur.positions.clone();
        *cur = new;
    }
    if autostart_changed {
        use tauri_plugin_autostart::ManagerExt;
        let on = state.settings.lock().unwrap().autostart;
        let al = app.autolaunch();
        let _ = if on { al.enable() } else { al.disable() };
    }
    apply_and_broadcast(&app);
}

#[tauri::command]
fn get_state(state: tauri::State<AppState>) -> TrackSnapshot {
    state.snapshot.lock().unwrap().clone()
}

#[tauri::command]
fn media_cmd(state: tauri::State<AppState>, cmd: String) -> Result<(), String> {
    let c = match cmd.as_str() {
        "playpause" => MediaCommand::PlayPause,
        "next" => MediaCommand::Next,
        "prev" => MediaCommand::Prev,
        _ => return Err(format!("未知指令 {cmd}")),
    };
    state.media_tx.send(c).map_err(|e| e.to_string())
}

#[tauri::command]
async fn fetch_lyrics(
    app: tauri::AppHandle,
    title: String,
    artist: String,
) -> Result<netease::LyricPayload, String> {
    let state = app.state::<AppState>();
    let dir = state.data_dir.clone();
    let client = NeteaseClient::new();
    netease::fetch_lyrics_full(&client, &dir, &title, &artist, &state.lyric_key_cache).await
}

#[tauri::command]
fn set_clickthrough(app: tauri::AppHandle, on: bool) {
    {
        let state = app.state::<AppState>();
        state.settings.lock().unwrap().clickthrough_lock = on;
    }
    apply_and_broadcast(&app);
}

#[tauri::command]
fn reposition(app: tauri::AppHandle) {
    {
        let state = app.state::<AppState>();
        let mut s = state.settings.lock().unwrap();
        let mode = s.mode.clone();
        s.positions.remove(&mode);
    }
    apply_and_broadcast(&app);
}

#[tauri::command]
fn show_settings_window(app: tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}

#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
}

// ---------- 启动 ----------

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .invoke_handler(tauri::generate_handler![
            get_settings,
            set_settings,
            get_state,
            media_cmd,
            fetch_lyrics,
            set_clickthrough,
            reposition,
            show_settings_window,
            quit_app,
            debug_log,
            drag::begin_drag,
            update_strip_regions,
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            let dir = handle.path().app_data_dir()?;
            let _ = fs::create_dir_all(&dir);
            let mut settings = Settings::load(&dir);
            // 旧版遗留的形态名（如 taskbar）统一回落到极简浮窗
            if !modes::MODES.contains(&settings.mode.as_str()) {
                settings.mode = "strip".to_string();
            }

            // SMTC 监听线程（RequestAsync 不依赖窗口句柄）
            let (tx, rx) = mpsc::channel::<MediaCommand>();
            let snapshot = Arc::new(Mutex::new(TrackSnapshot::default()));
            smtc::spawn(handle.clone(), rx, snapshot.clone());

            // 全屏避让
            win32::spawn_fullscreen_watcher(handle.clone());

            // 极简浮窗分区点击穿透
            spawn_region_passthrough(handle.clone());


            handle.manage(AppState {
                settings: Mutex::new(settings.clone()),
                snapshot,
                media_tx: tx,
                data_dir: dir.clone(),
                lyric_key_cache: Mutex::new(HashMap::new()),
                fullscreen: std::sync::atomic::AtomicBool::new(false),
            });

            // 初始布局 + 托盘
            let _ = modes::show_mode(&handle, &settings.mode, &settings);
            tray::create_tray(&handle)?;

            // 设置窗口关闭 → 隐藏而非销毁（按钮可反复打开）
            if let Some(sw) = handle.get_webview_window("settings") {
                let sw2 = sw.clone();
                sw.on_window_event(move |ev| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = ev {
                        api.prevent_close();
                        let _ = sw2.hide();
                    }
                });
            }

            // 自启动
            {
                use tauri_plugin_autostart::ManagerExt;
                let al = handle.autolaunch();
                let _ = if settings.autostart { al.enable() } else { al.disable() };
            }

            // 窗口拖动/调整大小 → 记忆几何（内存即时更新，落盘节流 2s）
            for label in modes::MODES {
                let Some(w) = handle.get_webview_window(label) else { continue };
                let h2 = handle.clone();
                let l = label.to_string();
                w.on_window_event(move |ev| match ev {
                    tauri::WindowEvent::Moved(pos) => {
                        // 溶解态（1x1）产生的坐标不写入位置记忆
                        if pos.x < -15000 || pos.y < -15000 || crate::visibility::is_dissolved(&l) {
                            return;
                        }
                        let state = h2.state::<AppState>();
                        let mut s = state.settings.lock().unwrap();
                        let e = s.positions.entry(l.clone()).or_insert_with(|| vec![0.0; 4]);
                        if e.len() < 4 {
                            e.resize(4, 0.0);
                        }
                        e[0] = pos.x as f64;
                        e[1] = pos.y as f64;
                        drop(s);
                        throttle_save(&h2);
                    }
                    tauri::WindowEvent::Resized(size) => {
                        // 溶解态（1x1）的尺寸不写入记忆；布局自适应由前端 CSS 完成
                        if crate::visibility::is_dissolved(&l) {
                            return;
                        }
                        let state = h2.state::<AppState>();
                        let mut s = state.settings.lock().unwrap();
                        let e = s.positions.entry(l.clone()).or_insert_with(|| vec![0.0; 4]);
                        if e.len() < 4 {
                            e.resize(4, 0.0);
                        }
                        e[2] = size.width as f64;
                        e[3] = size.height as f64;
                        drop(s);
                        throttle_save(&h2);
                    }
                    _ => {}
                });
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("FloatTune 启动失败");
}
