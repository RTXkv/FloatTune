mod drag;
mod modes;
mod netease;
mod settings;
mod smtc;
mod tray;
mod visibility;
mod volume;
mod win32;

use settings::Settings;
use smtc::{QueuedCommand, TrackSnapshot};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use tauri::{Emitter, Manager};

// ---------- 日志：stderr + 文件持久化 ----------
// 打包版直接启动时 stderr 无处可去，文件是唯一可回溯的诊断通道。
// 超过 1MB 轮转为 floatune.log.1，最多保留一代。

const LOG_MAX_BYTES: u64 = 1_000_000;
static LOG_FILE: std::sync::OnceLock<Mutex<Option<fs::File>>> = std::sync::OnceLock::new();

fn open_log_file(dir: &PathBuf) -> Option<fs::File> {
    let _ = fs::create_dir_all(dir);
    let path = dir.join("floatune.log");
    // 轮转：超过上限时把旧文件改名让位（保留一代）
    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() > LOG_MAX_BYTES {
            let _ = fs::rename(&path, dir.join("floatune.log.1"));
        }
    }
    fs::OpenOptions::new().create(true).append(true).open(&path).ok()
}

/// 写一条日志（时间戳 + 消息）到 stderr 与文件。所有文件操作不 panic：
/// 日志路径出问题不能拖垮主程序
pub fn log_write(msg: &str) {
    let line = format!("[{}] {}\n", win32::local_time_str(), msg);
    eprint!("{}", line);
    let m = LOG_FILE.get_or_init(|| {
        let dir = dirs_data_dir().unwrap_or_else(|| PathBuf::from("."));
        Mutex::new(open_log_file(&dir))
    });
    let mut guard = m.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(f) = guard.as_mut() {
        use std::io::Write;
        let _ = f.write_all(line.as_bytes());
    }
}

/// 带时间戳的日志宏（全 crate 通用）
#[macro_export]
macro_rules! flog {
    ($($arg:tt)*) => { $crate::log_write(&format!($($arg)*)) };
}

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
        let snap = state.settings.lock().unwrap_or_else(|e| e.into_inner()).clone();
        snap.save(&dir);
    }
}

pub struct AppState {
    pub settings: Mutex<Settings>,
    pub snapshot: Arc<Mutex<TrackSnapshot>>,
    pub media_tx: mpsc::SyncSender<QueuedCommand>,
    pub data_dir: PathBuf,
    pub lyric_key_cache: Mutex<HashMap<String, netease::TrackKey>>,
    pub fullscreen: std::sync::atomic::AtomicBool,
}

/// 浮窗点击穿透分区：前端上报 [x,y,w,h,kind]（物理像素，kind: 0=可交互 1=拖动区），
/// 其余区域全部穿透。后台线程 30ms 轮询光标切换 ignore_cursor_events。
/// 分区矩形 [x, y, w, h, kind]
type RegionRect = (f64, f64, f64, f64, u8);
static WINDOW_REGIONS: std::sync::OnceLock<Mutex<HashMap<String, Vec<RegionRect>>>> =
    std::sync::OnceLock::new();

/// 各窗口“实际已应用”的穿透状态缓存。轮询线程用它去重（避免每 30ms 重复调用
/// set_ignore_cursor_events）；dissolve/restore 在轮询线程之外改动真实状态时，
/// 必须经 passthrough_note 同步，否则缓存失步会导致恢复后整窗拦截点击，
/// 直到光标进出一次功能区才自愈。
/// 穿透状态缓存 label -> ignore
type PassthroughMap = HashMap<String, bool>;
static PASSTHROUGH_LAST: std::sync::OnceLock<Mutex<PassthroughMap>> =
    std::sync::OnceLock::new();

fn passthrough_last() -> &'static Mutex<PassthroughMap> {
    PASSTHROUGH_LAST.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 记录某窗口穿透状态的最新实际值（轮询线程与 dissolve/restore 共用）
pub fn passthrough_note(label: &str, ignore: bool) {
    passthrough_last()
        .lock()
        .unwrap()
        .insert(label.to_string(), ignore);
}

fn window_regions() -> &'static Mutex<HashMap<String, Vec<RegionRect>>> {
    WINDOW_REGIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[tauri::command]
fn update_strip_regions(
    window: String,
    rects: Vec<(f64, f64, f64, f64, u8)>,
) {
    window_regions().lock().unwrap_or_else(|e| e.into_inner()).insert(window, rects);
}

fn spawn_region_passthrough(app: tauri::AppHandle) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    std::thread::Builder::new()
        .name("region-passthrough".into())
        .spawn(move || {
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
                    let changed = {
                        let mut m = passthrough_last().lock().unwrap_or_else(|e| e.into_inner());
                        let changed = m.get(label) != Some(&ignore);
                        if changed {
                            m.insert(label.to_string(), ignore);
                        }
                        changed
                    };
                    if changed {
                        let _ = w.set_ignore_cursor_events(ignore);
                    }
                }
            }
        })
        .expect("spawn region passthrough");
}

#[tauri::command]
fn debug_log(msg: String) {
    crate::flog!("[fe] {}", msg);
}

/// 应用设置变更：持久化 → 窗口布局 → 托盘刷新 → 广播前端
pub fn apply_and_broadcast(app: &tauri::AppHandle) {
    let snap = {
        let state = app.state::<AppState>();
        let cur = state.settings.lock().unwrap_or_else(|e| e.into_inner());
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
    state.settings.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

#[tauri::command]
fn set_settings(app: tauri::AppHandle, state: tauri::State<AppState>, settings: Settings) {
    let autostart_changed;
    {
        let mut cur = state.settings.lock().unwrap_or_else(|e| e.into_inner());
        autostart_changed = cur.autostart != settings.autostart;
        // 位置记忆由窗口拖动维护，设置窗口不动它
        let mut new = settings;
        new.positions = cur.positions.clone();
        *cur = new;
    }
    if autostart_changed {
        use tauri_plugin_autostart::ManagerExt;
        let on = state.settings.lock().unwrap_or_else(|e| e.into_inner()).autostart;
        let al = app.autolaunch();
        let _ = if on { al.enable() } else { al.disable() };
    }
    apply_and_broadcast(&app);
}

#[tauri::command]
fn get_state(state: tauri::State<AppState>) -> TrackSnapshot {
    state.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

#[tauri::command]
fn media_cmd(state: tauri::State<AppState>, cmd: String) -> Result<(), String> {
    let c = match cmd.as_str() {
        "playpause" => smtc::MediaCommand::PlayPause,
        "next" => smtc::MediaCommand::Next,
        "prev" => smtc::MediaCommand::Prev,
        _ => return Err(format!("未知指令 {cmd}")),
    };
    // 有界队列：满时丢弃该条（极端连点的过期处理，worker 侧折叠积压）
    let qc = QueuedCommand { cmd: c, enqueued_at: std::time::Instant::now() };
    state.media_tx.try_send(qc).map_err(|e| {
        crate::flog!("[media] queue full: {}", e);
        e.to_string()
    })
}

#[tauri::command]
async fn fetch_lyrics(
    app: tauri::AppHandle,
    title: String,
    artist: String,
    duration_ms: Option<u64>,
    song_id: Option<u64>,
) -> Result<netease::LyricPayload, String> {
    let state = app.state::<AppState>();
    let dir = state.data_dir.clone();
    // 共享客户端：连接池跨请求复用
    let client = netease::shared_client();
    netease::fetch_lyrics_full(
        client,
        &dir,
        &title,
        &artist,
        duration_ms,
        song_id,
        &state.lyric_key_cache,
    )
    .await
}

#[tauri::command]
fn set_clickthrough(app: tauri::AppHandle, on: bool) {
    {
        let state = app.state::<AppState>();
        state.settings.lock().unwrap_or_else(|e| e.into_inner()).clickthrough_lock = on;
    }
    apply_and_broadcast(&app);
}

#[tauri::command]
fn reposition(app: tauri::AppHandle) {
    {
        let state = app.state::<AppState>();
        let mut s = state.settings.lock().unwrap_or_else(|e| e.into_inner());
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

/// 数据目录（panic hook 用，避免依赖 AppState）
fn dirs_data_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("com.floatune.app"))
}

pub fn run() {
    // panic 落盘：stderr 可能连不到终端（直接启动的 exe），独立文件保证有证据
    std::panic::set_hook(Box::new(|info| {
        let msg = format!(
            "[panic] {} thread={:?}\n",
            info,
            std::thread::current().id()
        );
        eprintln!("{}", msg);
        if let Some(dir) = dirs_data_dir() {
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("crash.log"))
                .and_then(|mut f| std::io::Write::write_all(&mut f, msg.as_bytes()));
        }
    }));

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
            // 有界命令队列：满时丢弃（极端连点的过期处理），worker 侧折叠积压
            let (tx, rx) = mpsc::sync_channel::<smtc::QueuedCommand>(32);
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
                        let mut s = state.settings.lock().unwrap_or_else(|e| e.into_inner());
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
                        let mut s = state.settings.lock().unwrap_or_else(|e| e.into_inner());
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
