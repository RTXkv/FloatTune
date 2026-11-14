//! SMTC 监听与控制 —— 快慢分离架构
//!
//! 慢的教训：WinRT 元数据请求（TryGetMediaPropertiesAsync 等）在网易云暂停/切歌时
//! 会阻塞数秒。因此本模块拆成两条互不拖累的通路：
//!
//! - **快路径**（决策循环，~120ms/轮）：只用纯 Win32/CoreAudio 信号——
//!   网易云窗口标题（"歌名 - 歌手"）+ 音频会话静音状态。毫秒级返回，不可能卡。
//!   负责暂停/播放判定、切歌判定、可见性决策。
//! - **慢路径**（后台单飞线程）：SMTC 元数据 + 封面流读取，卡多久都只影响封面。
//!
//! 状态判定（快路径）：
//!   无网易云窗口            → closed（隐藏）
//!   音频会话 Active         → playing（立即显示）
//!   音频会话 Inactive       → paused（立即隐藏），
//!     但若窗口标题刚在 2.5s 内变化（切歌间隙的静音），维持 playing 直到新歌出声。

use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::Emitter;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSessionManager,
    GlobalSystemMediaTransportControlsSessionMediaProperties,
};
use windows::Storage::Streams::DataReader;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, VIRTUAL_KEY,
    VK_MEDIA_NEXT_TRACK, VK_MEDIA_PLAY_PAUSE, VK_MEDIA_PREV_TRACK,
};

/// 发给播放器的控制指令
#[derive(Clone, Copy, Debug)]
pub enum MediaCommand {
    PlayPause,
    Next,
    Prev,
}

/// 当前播放快照（发往前端 + 可见性决策）
#[derive(Serialize, Clone, Debug, Default)]
pub struct TrackSnapshot {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub cover_b64: Option<String>,
    pub status: String,
    pub timeline_valid: bool,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub source: String,
    /// 状态最近一次变化的时刻（进程内毫秒），用于把隐藏延迟锚定到物理暂停
    #[serde(default)]
    pub status_changed_at: u64,
}

/// 切歌间隙：标题刚变过时静音不算暂停
const TRACK_GAP_SUPPRESS_MS: u64 = 2500;
/// 决策循环周期
const TICK_MS: u64 = 120;

/// 读取封面并转为 data URL（可能耗时/挂起，只在后台线程调用）
fn fetch_cover(
    props: &GlobalSystemMediaTransportControlsSessionMediaProperties,
) -> windows::core::Result<String> {
    let thumb = props.Thumbnail()?;
    let stream = thumb.OpenReadAsync()?.get()?;
    let content_type = stream.ContentType()?.to_string();
    let size = stream.Size()? as u32;
    if size == 0 {
        return Err(windows::core::Error::empty());
    }
    let reader = DataReader::CreateDataReader(&stream)?;
    let loaded = reader.LoadAsync(size)?.get()?;
    let mut buf = vec![0u8; loaded as usize];
    reader.ReadBytes(&mut buf)?;
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(buf);
    let mime = if content_type.contains("png") {
        "image/png"
    } else {
        "image/jpeg"
    };
    Ok(format!("data:{};base64,{}", mime, b64))
}

/// 模拟媒体键（兜底控制通道）
pub fn send_media_key(vk: VIRTUAL_KEY) {
    unsafe {
        let key = |wvk: VIRTUAL_KEY, flags: KEYBD_EVENT_FLAGS| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: wvk,
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let seq = [key(vk, KEYBD_EVENT_FLAGS(0)), key(vk, KEYBD_EVENT_FLAGS(2))];
        SendInput(&seq, std::mem::size_of::<INPUT>() as i32);
    }
}

/// 启动 SMTC 服务：决策循环 + 慢路径线程
pub fn spawn(
    app: tauri::AppHandle,
    rx: Receiver<MediaCommand>,
    snapshot: Arc<Mutex<TrackSnapshot>>,
) {
    std::thread::Builder::new()
        .name("smtc-watch".into())
        .spawn(move || watch(app, rx, snapshot))
        .expect("spawn smtc watcher");
}

fn watch(app: tauri::AppHandle, rx: Receiver<MediaCommand>, snapshot: Arc<Mutex<TrackSnapshot>>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let start = Instant::now();

    // 慢路径资源：SMTC 管理器（控制指令 + 封面）。创建失败不影响快路径。
    let manager: Option<GlobalSystemMediaTransportControlsSessionManager> =
        GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
            .ok()
            .and_then(|op| op.get().ok());

    let cover_busy = Arc::new(AtomicBool::new(false));

    let mut last_title_key: Option<(String, String)> = None;
    let mut last_status = String::new();
    let mut inflink = false;
    let mut last_tl = (0u64, 0u64);
    let mut title_changed_at = Instant::now() - Duration::from_secs(60);
    let mut tick: u64 = 0;

    loop {
        // 1. 控制指令：非阻塞取出，后台线程处理（SMTC 可能卡，不拖累循环）
        while let Ok(cmd) = rx.try_recv() {
            let m = manager.clone();
            // 记录当前标题供切歌验证
            if let Some(mm) = m.as_ref() {
                if let Some(props) =
                    find_media_session(mm).and_then(|s| s.TryGetMediaPropertiesAsync().and_then(|o| o.get()).ok())
                {
                    let t = props.Title().map(|x| x.to_string()).unwrap_or_default();
                    TITLE_BEFORE.with(|c| *c.borrow_mut() = t);
                }
            }
            std::thread::Builder::new()
                .name("media-cmd".into())
                .spawn(move || handle_command(m.as_ref(), cmd))
                .ok();
        }

        // 2. 快路径采样：窗口标题（Win32）+ 音频会话（CoreAudio）+ SMTC raw（同步属性）
        let raw_title = crate::win32::netease_window_title();
        let inactive = crate::volume::cloudmusic_session_inactive();
        let smtc_raw = manager
            .as_ref()
            .and_then(smtc_raw_status);

        // 3. 状态判定（自适应）：
        //    - InfLink 在位（timeline 有效）：raw 即时可信，直接采用
        //    - 原生网易云：raw 延迟 ~11s，用媒体会话（~1.2s 翻转）
        let effective: String = if raw_title.is_none() {
            "closed".to_string()
        } else if inflink {
            match smtc_raw.as_deref() {
                Some("playing") => "playing".to_string(),
                Some("paused") | Some("stopped") => "paused".to_string(),
                _ => match inactive {
                    Some(false) => "playing".to_string(),
                    Some(true) => {
                        if (title_changed_at.elapsed().as_millis() as u64) < TRACK_GAP_SUPPRESS_MS {
                            "playing".to_string()
                        } else {
                            "paused".to_string()
                        }
                    }
                    None => last_status.clone(),
                },
            }
        } else {
            match inactive {
                Some(false) => "playing".to_string(),
                Some(true) => {
                    if (title_changed_at.elapsed().as_millis() as u64) < TRACK_GAP_SUPPRESS_MS {
                        "playing".to_string()
                    } else {
                        "paused".to_string()
                    }
                }
                None => match smtc_raw.as_deref() {
                    Some("playing") => "playing".to_string(),
                    Some("paused") | Some("stopped") => "paused".to_string(),
                    _ => {
                        if last_status.is_empty() || last_status == "closed" {
                            "paused".to_string()
                        } else {
                            last_status.clone()
                        }
                    }
                },
            }
        };

        // 4. 状态差分 → 事件 + 可见性
        if effective != last_status {
            eprintln!(
                "[smtc] status -> {} (t+{}ms)",
                effective,
                start.elapsed().as_millis()
            );
            {
                let mut snap = snapshot.lock().unwrap();
                snap.status = effective.clone();
                snap.status_changed_at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
            }
            let snap = snapshot.lock().unwrap().clone();
            let _ = app.emit("smtc:status", &snap);
            crate::visibility::update_visibility(&app);
            last_status = effective.clone();
        }

        // 4.5 InfLink timeline：原生网易云恒空；出现即 InfLink 在位（状态转用 raw）
        if let Some(m) = manager.as_ref() {
            if let Some(session) = find_media_session(m) {
                if let Ok(tl) = session.GetTimelineProperties() {
                    let pos = tl.Position().map(|v| (v.Duration.max(0) as u64) / 10_000).unwrap_or(0);
                    let dur = tl.EndTime().map(|v| (v.Duration.max(0) as u64) / 10_000).unwrap_or(0);
                    if dur > 0 {
                        if !inflink {
                            inflink = true;
                            eprintln!("[smtc] InfLink detected (timeline available)");
                        }
                        if (pos, dur) != last_tl {
                            last_tl = (pos, dur);
                            {
                                let mut snap = snapshot.lock().unwrap();
                                snap.timeline_valid = true;
                                snap.position_ms = pos;
                                snap.duration_ms = dur;
                            }
                            let _ = app.emit("smtc:timeline", (pos, dur));
                        }
                    } else if inflink {
                        inflink = false;
                        {
                            let mut snap = snapshot.lock().unwrap();
                            snap.timeline_valid = false;
                        }
                        let snap = snapshot.lock().unwrap().clone();
                        let _ = app.emit("smtc:status", &snap);
                    }
                }
            }
        }

        // 5. 曲目差分（窗口标题 "歌名 - 歌手"）
        if let Some(raw) = &raw_title {
            if let Some((title, artist)) = parse_window_title(&raw) {
                let key = (title.clone(), artist.clone());
                if last_title_key.as_ref() != Some(&key) {
                    title_changed_at = Instant::now();
                    eprintln!(
                        "[smtc] track -> {} / {} (t+{}ms)",
                        title,
                        artist,
                        start.elapsed().as_millis()
                    );
                    {
                        let mut snap = snapshot.lock().unwrap();
                        snap.title = title;
                        snap.artist = artist;
                        snap.album.clear();
                        snap.source = "cloudmusic".into();
                    }
                    let snap = snapshot.lock().unwrap().clone();
                    let _ = app.emit("smtc:track", &snap);
                    last_title_key = Some(key);

                    // 慢路径：后台取封面（单飞，卡住也不影响状态）
                    kick_cover(&app, &snapshot, &manager, &cover_busy);
                }
            }
        }

        // 6. 静默诊断（每 ~12s 一条，观测快路径健康度）
        tick += 1;
        if tick % 100 == 0 {
            eprintln!(
                "[smtc] tick={} title={:?} inactive={:?} t+{}ms",
                tick,
                raw_title.as_deref().unwrap_or(""),
                inactive,
                start.elapsed().as_millis()
            );
        }

        std::thread::sleep(Duration::from_millis(TICK_MS));
    }
}

/// SMTC 原始播放状态（同步调用，快路径安全；失败返回 None）
fn smtc_raw_status(
    manager: &GlobalSystemMediaTransportControlsSessionManager,
) -> Option<String> {
    use windows::Media::Control::GlobalSystemMediaTransportControlsSessionPlaybackStatus as S;
    let sessions = manager.GetSessions().ok()?;
    let count = sessions.Size().ok()?;
    for i in 0..count {
        let Ok(s) = sessions.GetAt(i) else { continue };
        let Ok(src) = s.SourceAppUserModelId() else { continue };
        let src = src.to_string().to_lowercase();
        if src.contains("cloudmusic") || src.contains("netease") {
            return s
                .GetPlaybackInfo()
                .ok()?
                .PlaybackStatus()
                .ok()
                .map(|v| {
                    if v == S::Playing {
                        "playing"
                    } else if v == S::Paused {
                        "paused"
                    } else if v == S::Stopped {
                        "stopped"
                    } else {
                        "unknown"
                    }
                })
                .map(|x| x.to_string());
        }
    }
    None
}

/// "歌名 - 歌手" 解析：以最后一个 " - " 分割（歌名里含 " - " 时歌手仍在最后）
fn parse_window_title(raw: &str) -> Option<(String, String)> {
    let t = raw.trim();
    if t.is_empty() || t == "网易云音乐" {
        return None;
    }
    match t.rsplit_once(" - ") {
        Some((title, artist)) if !title.trim().is_empty() => {
            Some((title.trim().to_string(), artist.trim().to_string()))
        }
        _ => Some((t.to_string(), String::new())),
    }
}

/// 后台单飞取封面：SMTC 元数据请求可能卡数秒，完全隔离在独立线程
fn kick_cover(
    app: &tauri::AppHandle,
    snapshot: &Arc<Mutex<TrackSnapshot>>,
    manager: &Option<GlobalSystemMediaTransportControlsSessionManager>,
    busy: &Arc<AtomicBool>,
) {
    let busy = busy.clone();
    if busy.swap(true, Ordering::Relaxed) {
        return;
    }
    let Some(manager) = manager.clone() else {
        busy.store(false, Ordering::Relaxed);
        return;
    };
    let app = app.clone();
    let snapshot = snapshot.clone();
    std::thread::Builder::new()
        .name("cover-fetch".into())
        .spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            // 最多 5 次（网易云封面流慢半拍；单次请求可能挂起数秒）
            for attempt in 0..5u32 {
                let props = find_session_props(&manager);
                if let Some(props) = props {
                    if let Ok(cov) = fetch_cover(&props) {
                        {
                            let mut s = snapshot.lock().unwrap();
                            s.cover_b64 = Some(cov);
                        }
                        let s = snapshot.lock().unwrap().clone();
                        let _ = app.emit("smtc:track", &s);
                        break;
                    }
                }
                if attempt == 4 {
                    // 放弃：置 None 让前端走 API 封面兜底
                    let mut s = snapshot.lock().unwrap();
                    if s.cover_b64.is_none() {
                        s.cover_b64 = None;
                    }
                    let s = snapshot.lock().unwrap().clone();
                    let _ = app.emit("smtc:track", &s);
                }
                std::thread::sleep(Duration::from_millis(1200));
            }
            busy.store(false, Ordering::Relaxed);
        })
        .ok();
}

fn find_media_session(
    manager: &GlobalSystemMediaTransportControlsSessionManager,
) -> Option<windows::Media::Control::GlobalSystemMediaTransportControlsSession> {
    let sessions = manager.GetSessions().ok()?;
    let count = sessions.Size().ok()?;
    for i in 0..count {
        let Ok(s) = sessions.GetAt(i) else { continue };
        let Ok(src) = s.SourceAppUserModelId() else { continue };
        let src = src.to_string().to_lowercase();
        if src.contains("cloudmusic") || src.contains("netease") {
            return Some(s);
        }
    }
    None
}

fn find_session_props(
    manager: &GlobalSystemMediaTransportControlsSessionManager,
) -> Option<GlobalSystemMediaTransportControlsSessionMediaProperties> {
    let sessions = manager.GetSessions().ok()?;
    let count = sessions.Size().ok()?;
    for i in 0..count {
        let Ok(s) = sessions.GetAt(i) else { continue };
        let Ok(src) = s.SourceAppUserModelId() else { continue };
        let src = src.to_string().to_lowercase();
        if src.contains("cloudmusic") || src.contains("netease") {
            return s.TryGetMediaPropertiesAsync().and_then(|op| op.get()).ok();
        }
    }
    None
}

/// 发送 SMTC 指令（只发不发结果判定）
fn try_smtc_once(
    manager: &GlobalSystemMediaTransportControlsSessionManager,
    cmd: MediaCommand,
) -> bool {
    let sessions = match manager.GetSessions() {
        Ok(v) => v,
        Err(_) => return false,
    };
    let Ok(count) = sessions.Size() else { return false };
    for i in 0..count {
        let Ok(s) = sessions.GetAt(i) else { continue };
        let Ok(src) = s.SourceAppUserModelId() else { continue };
        let src = src.to_string().to_lowercase();
        if src.contains("cloudmusic") || src.contains("netease") {
            let op = match cmd {
                MediaCommand::PlayPause => s.TryTogglePlayPauseAsync(),
                MediaCommand::Next => s.TrySkipNextAsync(),
                MediaCommand::Prev => s.TrySkipPreviousAsync(),
            };
            return op.and_then(|o| o.get()).is_ok();
        }
    }
    false
}

fn handle_command(
    manager: Option<&GlobalSystemMediaTransportControlsSessionManager>,
    cmd: MediaCommand,
) {
    // InfLink 的 SMTC 转发实测会丢命令（返回 True 但无效果），
    // 所以每条指令都带"验证 + 媒体键补发"。
    let Some(m) = manager else {
        send_media_key(match cmd {
            MediaCommand::PlayPause => VK_MEDIA_PLAY_PAUSE,
            MediaCommand::Next => VK_MEDIA_NEXT_TRACK,
            MediaCommand::Prev => VK_MEDIA_PREV_TRACK,
        });
        return;
    };
    let before = smtc_raw_status(m);

    // 三次尝试验证链：SMTC toggle → 媒体键 → 媒体键。
    // InfLink 转发实测丢 ~50% 命令，且返回 True 不代表生效，必须验证。
    // 验证窗口 2.5s（InfLink 状态推送为 1s 节奏）。
    if try_smtc_once(m, cmd) && verify_changed(m, &before, cmd, 2500) {
        return;
    }
    let vk = match cmd {
        MediaCommand::PlayPause => VK_MEDIA_PLAY_PAUSE,
        MediaCommand::Next => VK_MEDIA_NEXT_TRACK,
        MediaCommand::Prev => VK_MEDIA_PREV_TRACK,
    };
    send_media_key(vk);
    if verify_changed(m, &before, cmd, 2500) {
        return;
    }
    send_media_key(vk);
    let _ = verify_changed(m, &before, cmd, 2500);
}

/// 验证指令是否真的生效：playpause 看播放状态翻转，切歌看标题变化
fn verify_changed(
    m: &GlobalSystemMediaTransportControlsSessionManager,
    before: &Option<String>,
    cmd: MediaCommand,
    window_ms: u64,
) -> bool {
    let title_before = TITLE_BEFORE.with(|c| c.borrow().clone());
    let checks = (window_ms / 250).max(1);
    for _ in 0..checks {
        std::thread::sleep(Duration::from_millis(250));
        match cmd {
            MediaCommand::PlayPause => {
                let now = smtc_raw_status(m);
                if now.is_some() && before.is_some() && now != *before {
                    return true;
                }
            }
            _ => {
                let title = find_media_session(m)
                    .and_then(|s| s.TryGetMediaPropertiesAsync().and_then(|o| o.get()).ok())
                    .map(|p| p.Title().map(|t| t.to_string()).unwrap_or_default())
                    .unwrap_or_default();
                if !title.is_empty() && title != title_before {
                    return true;
                }
            }
        }
    }
    false
}

thread_local! {
    static TITLE_BEFORE: std::cell::RefCell<String> = std::cell::RefCell::new(String::new());
}
