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
use std::sync::mpsc::{self, Receiver};
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
    /// 歌曲代际：每次确认切歌 +1。异步结果（封面等）写入前必须校验
    /// 代际一致，防止旧歌的慢结果覆盖新歌状态
    #[serde(default)]
    pub track_generation: u64,
    /// InfLink 提供的网易云歌曲精确 ID（genre 字段 NCM-{ID}）。
    /// 在位时歌词管线跳过搜索匹配直接按 ID 拉取，根除同名误配
    #[serde(default)]
    pub song_id: Option<u64>,
}

/// 切歌间隙：标题刚变过时静音不算暂停
const TRACK_GAP_SUPPRESS_MS: u64 = 2500;
/// 决策循环周期
const TICK_MS: u64 = 120;
/// SMTC 采样缓存的过期阈值：超过视为不可信（回退音频会话判定）。
/// GetTimelineProperties 是跨进程 RPC 到网易云，InfLink 挂起时会永久阻塞——
/// 所以 SMTC 采样必须在独立线程，决策循环只消费带时间戳的缓存。
const SAMPLE_STALE_MS: u128 = 600;
/// InfLink 推送停滞判死阈值：播放中（会话 Active）timeline 连续无推进的 tick 数
/// （40 × 120ms ≈ 4.8s）。网易云 SMTC 的 timeline 有残留值，只看 dur>0 会误判在位。
const INFLINK_STALE_TICKS: u32 = 40;

/// SMTC 采样快照（独立线程写入，决策循环读取）
#[derive(Clone)]
struct SmtcSample {
    at: Instant,
    raw: Option<String>,
    /// (pos, dur)，dur>0 才有效
    tl: Option<(u64, u64)>,
}

/// 后台采样线程：把可能阻塞的 WinRT 调用（raw 状态 + timeline）隔离在独立线程，
/// 卡死也只影响采样新鲜度，不拖垮决策循环（快慢分离原则的落地）。
fn spawn_sampler(
    manager: GlobalSystemMediaTransportControlsSessionManager,
    sample: Arc<Mutex<SmtcSample>>,
) {
    std::thread::Builder::new()
        .name("smtc-sample".into())
        .spawn(move || {
            let _com = CoInitGuard::new();
            loop {
                let raw = smtc_raw_status(&manager);
                let tl = find_media_session(&manager).and_then(|s| s.GetTimelineProperties().ok()).and_then(|t| {
                    let pos = t.Position().map(|v| (v.Duration.max(0) as u64) / 10_000).unwrap_or(0);
                    let dur = t.EndTime().map(|v| (v.Duration.max(0) as u64) / 10_000).unwrap_or(0);
                    if dur > 0 { Some((pos, dur)) } else { None }
                });
                if let Ok(mut s) = sample.lock() {
                    *s = SmtcSample { at: Instant::now(), raw, tl };
                }
                std::thread::sleep(Duration::from_millis(TICK_MS));
            }
        })
        .expect("spawn smtc sampler");
}

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

/// COM MTA 初始化 RAII：线程结束自动 CoUninitialize 配对。
/// 短命线程（封面/命令）反复 spawn 时不配对会累积 MTA 引用计数。
struct CoInitGuard;

impl CoInitGuard {
    fn new() -> Self {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        Self
    }
}

impl Drop for CoInitGuard {
    fn drop(&mut self) {
        unsafe {
            windows::Win32::System::Com::CoUninitialize();
        }
    }
}

/// 队列命令：入队时间戳用于等待时间统计
#[derive(Debug, Clone, Copy)]
pub struct QueuedCommand {
    pub cmd: MediaCommand,
    pub enqueued_at: Instant,
}

/// 启动 SMTC 服务：决策循环 + 命令 worker（串行队列）
pub fn spawn(
    app: tauri::AppHandle,
    rx: Receiver<QueuedCommand>,
    snapshot: Arc<Mutex<TrackSnapshot>>,
) {
    std::thread::Builder::new()
        .name("smtc-watch".into())
        .spawn(move || watch(app, snapshot))
        .expect("spawn smtc watcher");
    std::thread::Builder::new()
        .name("command-worker".into())
        .spawn(move || command_worker(rx))
        .expect("spawn command worker");
}

/// 命令 worker：rx 的唯一消费者，**串行**处理所有媒体指令。
/// 并发处理会让两条命令互相触发对方的验证误判（A 的媒体键把 B 等的状态
/// 变化确认掉），串行队列同时天然实现命令排队。
///
/// 积压管理：连续**同类型**命令折叠成批处理（≤8），不同类型经 pending
/// 缓冲严格保序——绝不丢弃用户命令：
/// - PlayPause ×偶数 → no-op（回到原状态，意图等价）；×奇数 → 一次 toggle
/// - Next/Prev ×N → 连发 N 次（保留连续点击意图），只验证最后一次
///
/// SMTC manager 创建失败时按 5s→30s 指数退避重试；退避期间到达的命令
/// 走媒体键兜底路径执行，不积压。
fn command_worker(rx: Receiver<QueuedCommand>) {
    let _com = CoInitGuard::new();
    let mut manager: Option<GlobalSystemMediaTransportControlsSessionManager> = None;
    let mut seq: u64 = 0;
    let mut backoff: u64 = 5;
    let mut pending: Option<QueuedCommand> = None;
    loop {
        let first = match pending.take() {
            Some(c) => Some(c),
            None if manager.is_some() => match rx.recv_timeout(Duration::from_secs(30)) {
                Ok(c) => Some(c),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv_timeout(Duration::from_secs(backoff)) {
                Ok(c) => Some(c),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
                        .ok()
                        .and_then(|op| op.get().ok());
                    if manager.is_some() {
                        crate::flog!("[worker] smtc manager acquired after backoff");
                        backoff = 5;
                    } else {
                        backoff = (backoff * 2).min(30);
                        crate::flog!("[worker] smtc manager unavailable, retry in {}s", backoff);
                    }
                    None
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
        };
        let Some(first) = first else { continue };

        // 收集同类型积压（≤8），异类进 pending 保序
        let mut batch = vec![first];
        while batch.len() < 8 {
            match rx.try_recv() {
                Ok(c) if kind_of(c.cmd) == kind_of(batch[0].cmd) => batch.push(c),
                Ok(c) => {
                    pending = Some(c);
                    break;
                }
                Err(_) => break,
            }
        }
        seq += 1;
        let waited_ms = first.enqueued_at.elapsed().as_millis();
        let t0 = Instant::now();
        let last_id = seq + batch.len() as u64 - 1;
        execute_batch(manager.as_ref(), &batch, seq);
        crate::flog!(
            "[cmd#{}..{}] batch n={} waited={}ms exec={}ms",
            seq,
            last_id,
            batch.len(),
            waited_ms,
            t0.elapsed().as_millis()
        );
    }
}

/// 命令类型标识（折叠分组用）
fn kind_of(cmd: MediaCommand) -> u8 {
    match cmd {
        MediaCommand::PlayPause => 0,
        MediaCommand::Next => 1,
        MediaCommand::Prev => 2,
    }
}

/// 执行一个同类型批次
fn execute_batch(
    manager: Option<&GlobalSystemMediaTransportControlsSessionManager>,
    batch: &[QueuedCommand],
    seq: u64,
) {
    let id = format!("cmd#{}", seq);
    let session = manager.and_then(smtc_session);
    match batch[0].cmd {
        MediaCommand::PlayPause => {
            if batch.len().is_multiple_of(2) {
                // 连续偶数次 toggle 回到原状态，意图等价于什么都不做
                crate::flog!(
                    "[{}] playpause×{} folded to no-op (even toggles)",
                    id,
                    batch.len()
                );
                return;
            }
            match manager {
                Some(m) => handle_playpause(m, session.as_ref(), id),
                None => {
                    crate::flog!("[{}] no smtc manager, media key toggle", id);
                    send_media_key(VK_MEDIA_PLAY_PAUSE);
                }
            }
        }
        MediaCommand::Next | MediaCommand::Prev => match manager {
            Some(m) => handle_skip_n(m, session.as_ref(), batch[0].cmd, batch.len(), id),
            None => {
                crate::flog!("[{}] no smtc manager, media key {:?}", id, batch[0].cmd);
                send_media_key(match batch[0].cmd {
                    MediaCommand::Next => VK_MEDIA_NEXT_TRACK,
                    _ => VK_MEDIA_PREV_TRACK,
                });
            }
        },
    }
}

fn watch(app: tauri::AppHandle, snapshot: Arc<Mutex<TrackSnapshot>>) {
    let _com = CoInitGuard::new();
    let start = Instant::now();

    // 慢路径资源：SMTC 管理器（控制指令 + 封面）。创建失败不影响快路径。
    let manager: Option<GlobalSystemMediaTransportControlsSessionManager> =
        GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
            .ok()
            .and_then(|op| op.get().ok());

    // SMTC 采样（独立线程）：raw 状态 + timeline，决策循环只读缓存
    let sample = Arc::new(Mutex::new(SmtcSample {
        at: Instant::now() - Duration::from_secs(60),
        raw: None,
        tl: None,
    }));
    if let Some(m) = manager.clone() {
        spawn_sampler(m, sample.clone());
    }

    let cover_busy = Arc::new(AtomicBool::new(false));

    let mut last_title_key: Option<(String, String)> = None;
    let mut last_status = String::new();
    let mut inflink = false;
    let mut last_tl_pos: u64 = 0;
    let mut stale_ticks: u32 = 0;
    let mut title_changed_at = Instant::now() - Duration::from_secs(60);
    let mut tick: u64 = 0;

    loop {
        // 1. 快路径采样：窗口标题（Win32）+ 音频会话（CoreAudio）+ SMTC 采样缓存
        // （控制指令由 command-worker 串行消费，与决策循环隔离）
        let raw_title = crate::win32::netease_window_title();
        let inactive = crate::volume::cloudmusic_session_inactive();
        let (smtc_raw, tl_now) = {
            let s = sample
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if s.at.elapsed().as_millis() <= SAMPLE_STALE_MS {
                (s.raw, s.tl)
            } else {
                // 采样线程卡死（网易云 SMTC 端挂起）：数据过期，回退音频会话判定
                (None, None)
            }
        };

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
            crate::flog!(
                "[smtc] status -> {} (t+{}ms)",
                effective,
                start.elapsed().as_millis()
            );
            {
                let mut snap = snapshot.lock().unwrap_or_else(|e| e.into_inner());
                snap.status = effective.clone();
                snap.status_changed_at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
            }
            let snap = snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let _ = app.emit("smtc:status", &snap);
            crate::visibility::update_visibility(&app);
            last_status = effective.clone();
        }

        // 4.5 InfLink 判定：激活需要 timeline **推进**（网易云残留 timeline 恒值，
        // 只看 dur>0 会误判在位 → 信任 11-20s 延迟的 raw，正是响应慢的根因）；
        // 播放中（会话 Active）timeline 停滞 ~5s 判死 → 回退音频会话路径。
        if let Some((pos, dur)) = tl_now {
            let moved = pos != last_tl_pos;
            if !inflink {
                if moved {
                    inflink = true;
                    stale_ticks = 0;
                    crate::flog!("[smtc] InfLink detected (timeline advancing)");
                }
            } else if moved {
                stale_ticks = 0;
            } else if inactive == Some(false)
                && smtc_raw.as_deref() == Some("playing")
                && title_changed_at.elapsed().as_millis() as u64 >= TRACK_GAP_SUPPRESS_MS
            {
                // 真的在出声（音频会话 Active + raw 报 playing）但 timeline 不动：
                // 推送端挂起。三个条件缺一不可——暂停后音频会话有 10-20s OS 迟滞
                // 仍报 Active，若只看会话状态，暂停会被误判死并回退到迟滞路径，
                // 造成"暂停后卡片重现几秒再消失"的振荡；切歌间隙同样需要宽限。
                stale_ticks += 1;
                if stale_ticks >= INFLINK_STALE_TICKS {
                    inflink = false;
                    stale_ticks = 0;
                    crate::flog!(
                        "[smtc] InfLink push stalled (timeline frozen ~{}s while playing), falling back",
                        (INFLINK_STALE_TICKS * TICK_MS as u32) / 1000
                    );
                    {
                        let mut snap = snapshot.lock().unwrap_or_else(|e| e.into_inner());
                        snap.timeline_valid = false;
                    }
                    let snap = snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    let _ = app.emit("smtc:status", &snap);
                }
            } else {
                // 暂停中 timeline 不动是正常的，不计时
                stale_ticks = 0;
            }
            if inflink && moved {
                {
                    let mut snap = snapshot.lock().unwrap_or_else(|e| e.into_inner());
                    snap.timeline_valid = true;
                    snap.position_ms = pos;
                    snap.duration_ms = dur;
                }
                let _ = app.emit("smtc:timeline", (pos, dur));
            }
            last_tl_pos = pos;
        } else if inflink {
            // timeline 消失（会话关闭/SMTC 端清空）
            inflink = false;
            stale_ticks = 0;
            crate::flog!("[smtc] InfLink lost (timeline gone)");
            {
                let mut snap = snapshot.lock().unwrap_or_else(|e| e.into_inner());
                snap.timeline_valid = false;
            }
            let snap = snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let _ = app.emit("smtc:status", &snap);
        }

        // 5. 曲目差分（窗口标题 "歌名 - 歌手"）
        if let Some(raw) = &raw_title {
            if let Some((title, artist)) = parse_window_title(raw) {
                let key = (title.clone(), artist.clone());
                if last_title_key.as_ref() != Some(&key) {
                    title_changed_at = Instant::now();
                    crate::flog!(
                        "[smtc] track -> {} / {} (t+{}ms)",
                        title,
                        artist,
                        start.elapsed().as_millis()
                    );
                    {
                        let mut snap = snapshot.lock().unwrap_or_else(|e| e.into_inner());
                        snap.title = title;
                        snap.artist = artist;
                        snap.album.clear();
                        // 旧歌封面立即清除（SMTC 封面流更新慢半拍，不清会残留旧封面；
                        // 纯音乐时前端没有 API 封面兜底，必须靠这里清干净）
                        snap.cover_b64 = None;
                        snap.source = "cloudmusic".into();
                        snap.song_id = None;
                        // timeline 还是上一首的残留值：立即失效，等新歌的第一次推送
                        // 再置有效（防止歌词匹配用旧歌时长打分误伤）
                        snap.timeline_valid = false;
                        snap.duration_ms = 0;
                        // 歌曲代际 +1：在途异步结果（封面重试等）以此判断是否过期
                        snap.track_generation += 1;
                    }
                    let snap = snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    let _ = app.emit("smtc:track", &snap);
                    last_title_key = Some(key);

                    // 慢路径：后台取封面（单飞，卡住也不影响状态）
                    kick_cover(&app, &snapshot, &manager, &cover_busy);
                }
            }
        }

        // 6. 静默诊断（每 ~12s 一条，观测快路径健康度）
        tick += 1;
        if tick.is_multiple_of(100) {
            crate::flog!(
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

/// 宽松归一（封面标题比对用）：去空白 + 小写；SMTC 元数据与窗口标题的
/// 细微差异（全半角空格等）不应导致封面被误判为旧歌
fn norm_loose(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// 从 SMTC 元数据 genre 字段提取 InfLink 的歌曲 ID（格式 NCM-{ID}）
fn extract_ncm_id(
    props: &GlobalSystemMediaTransportControlsSessionMediaProperties,
) -> Option<u64> {
    let genres = props.Genres().ok()?;
    for g in genres {
        let s = g.to_string();
        if let Some(rest) = s.strip_prefix("NCM-") {
            if let Ok(id) = rest.trim().parse::<u64>() {
                return Some(id);
            }
        }
    }
    None
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
            let _com = CoInitGuard::new();
            // 本次封面任务所属的歌曲代际：重试窗口（最长 ~7s）内用户再切歌时，
            // 旧任务的写入必须作废（generation 是比标题更严格的归属校验）
            let gen = snapshot
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .track_generation;
            // 最多 6 次 × 1.2s（网易云封面流慢半拍；单次请求可能挂起数秒）。
            // 每次先校验 SMTC 元数据标题已跟上当前歌——元数据还是上一首时
            // 取到的封面必然也是旧的，"成功即停"会让旧封面永久停留
            for attempt in 0..6u32 {
                let cur_gen = snapshot
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .track_generation;
                if cur_gen != gen {
                    // 已切歌：新歌有自己的封面任务，本任务全部作废
                    break;
                }
                let props = find_session_props(&manager);
                if let Some(props) = props {
                    let ptitle = props.Title().map(|x| x.to_string()).unwrap_or_default();
                    let cur_title = snapshot
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .title
                        .clone();
                    let stale = norm_loose(&ptitle) != norm_loose(&cur_title);
                    if !stale {
                        // 元数据已跟上当前歌：提取 InfLink 歌曲精确 ID（genre 字段
                        // NCM-{ID}），供歌词管线跳过搜索匹配
                        if let Some(sid) = extract_ncm_id(&props) {
                            crate::flog!("[smtc] InfLink song_id={} extracted", sid);
                            let mut s = snapshot.lock().unwrap_or_else(|e| e.into_inner());
                            if s.track_generation == gen {
                                s.song_id = Some(sid);
                            }
                        }
                        if let Ok(cov) = fetch_cover(&props) {
                            {
                                let mut s = snapshot.lock().unwrap_or_else(|e| e.into_inner());
                                if s.track_generation == gen {
                                    s.cover_b64 = Some(cov);
                                }
                            }
                            let s = snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
                            let _ = app.emit("smtc:track", &s);
                            break;
                        }
                    }
                }
                if attempt == 5 {
                    // 放弃：cover_b64 保持 None（切歌时已清），前端走 API 封面兜底
                    let s = snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
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
/// 获取网易云的 SMTC 会话（定向指令用）
fn smtc_session(
    manager: &GlobalSystemMediaTransportControlsSessionManager,
) -> Option<windows::Media::Control::GlobalSystemMediaTransportControlsSession> {
    find_media_session(manager)
}

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

/// PlayPause 执行计划（纯函数可测试）
#[derive(Debug, Clone, PartialEq, Eq)]
enum PlayPauseAction {
    /// 定向 Play（当前 paused/stopped）——幂等：重复执行不会反向
    DirectedPlay,
    /// 定向 Pause（当前 playing）——幂等
    DirectedPause,
    /// 状态未知只能 toggle（非幂等）
    Toggle,
}

/// 由当前已知状态决定 PlayPause 的执行方式。
/// 有明确状态时优先定向控制，根除"二次 toggle 反向执行"的可能。
fn plan_playpause(before: Option<&str>) -> PlayPauseAction {
    match before {
        Some("playing") => PlayPauseAction::DirectedPause,
        Some("paused") | Some("stopped") => PlayPauseAction::DirectedPlay,
        _ => PlayPauseAction::Toggle,
    }
}

/// PlayPause 验证超时后的下一步（纯函数可测试）。
/// attempt: 0 = 首次已发，1/2 = 第几次重试。
/// 核心原则：**验证超时不反向**——重试只用同向定向指令（幂等）；
/// 唯一允许媒体键 toggle 的场景是 SMTC 通道确认从未送达（此时第一次
/// 必然没执行，toggle 无反向风险）。
#[derive(Debug, PartialEq, Eq)]
enum PlayPauseStep {
    /// 再发一次同向定向指令
    RetryDirected(PlayPauseAction),
    /// 媒体键 toggle（仅 SMTC 通道确认不可达时）
    MediaKey,
    /// 放弃（可能已执行，状态延迟中）
    Stop,
}

fn next_playpause_step(plan: &PlayPauseAction, smtc_ok_so_far: bool, attempt: u32) -> PlayPauseStep {
    match plan {
        // toggle 非幂等：验证超时绝不补发
        PlayPauseAction::Toggle => PlayPauseStep::Stop,
        directed => {
            if attempt >= 2 {
                PlayPauseStep::Stop
            } else if smtc_ok_so_far {
                // 通道可达，只是状态延迟 → 同向定向重试（幂等安全）
                PlayPauseStep::RetryDirected(directed.clone())
            } else {
                // 定向指令从未送达 → 媒体键 toggle（此时无反向风险）
                PlayPauseStep::MediaKey
            }
        }
    }
}

// 补发策略（InfLink 的 SMTC 转发实测会丢命令，返回 True 但无效果，所以带
// 验证；**但验证失败 ≠ 没执行**——状态更新有延迟，补发策略按指令性质区分）：
// - PlayPause：有明确状态时用定向 Play/Pause（幂等），验证超时后同向
//   定向重试——即使第一次已执行也不会被反向；只有确认 SMTC 通道从未
//   送达时才用媒体键 toggle。
// - Next/Prev 非幂等：重复执行 = 跳两首。SMTC 发送成功 → 只验证绝不
//   补发；发送失败（通道不可用）→ 媒体键一次。
// （分发入口为 execute_batch：同类型命令先折叠再执行）

/// PlayPause：定向控制 + 幂等重试链
fn handle_playpause(
    m: &GlobalSystemMediaTransportControlsSessionManager,
    session: Option<&windows::Media::Control::GlobalSystemMediaTransportControlsSession>,
    id: String,
) {
    let before = smtc_raw_status(m);
    let plan = plan_playpause(before.as_deref());
    let expect: &str = match &plan {
        PlayPauseAction::DirectedPlay => "playing",
        PlayPauseAction::DirectedPause => "paused",
        PlayPauseAction::Toggle => "",
    };
    crate::flog!(
        "[{}] playpause plan={:?} before={:?}",
        id,
        plan,
        before.as_deref().unwrap_or("unknown")
    );
    // 验证目标：定向模式验证"达到期望状态"，toggle 模式验证"与 before 不同"
    let expect_opt = if expect.is_empty() { None } else { Some(expect) };

    // 首次发送
    let (mut smtc_ok, mut ever_smtc_ok) = match (&plan, session) {
        (PlayPauseAction::Toggle, _) => {
            let ok = try_smtc_once(m, MediaCommand::PlayPause);
            (ok, ok)
        }
        (_, Some(s)) => {
            let send = match &plan {
                PlayPauseAction::DirectedPlay => SmtcSend::Play,
                _ => SmtcSend::Pause,
            };
            let ok = send_smtc_direct(s, &send);
            (ok, ok)
        }
        (_, None) => (false, false),
    };
    if smtc_ok && verify_status(m, expect_opt, before.as_deref(), 2500) {
        crate::flog!("[{}] confirmed (smtc channel)", id);
        return;
    }

    // 重试链（由纯函数决策，永不反向）
    for attempt in 1..=2u32 {
        match next_playpause_step(&plan, ever_smtc_ok, attempt) {
            PlayPauseStep::RetryDirected(action) => {
                if let Some(s) = session {
                    crate::flog!("[{}] retry {:?} (idempotent, attempt {})", id, action, attempt);
                    smtc_ok = send_smtc_direct(
                        s,
                        &match action {
                            PlayPauseAction::DirectedPlay => SmtcSend::Play,
                            _ => SmtcSend::Pause,
                        },
                    );
                    ever_smtc_ok |= smtc_ok;
                    if smtc_ok && verify_status(m, expect_opt, before.as_deref(), 2500) {
                        crate::flog!("[{}] confirmed (directed retry {})", id, attempt);
                        return;
                    }
                } else {
                    break;
                }
            }
            PlayPauseStep::MediaKey => {
                // SMTC 通道确认从未送达 → toggle 无反向风险
                crate::flog!("[{}] smtc channel dead, media key toggle", id);
                send_media_key(VK_MEDIA_PLAY_PAUSE);
                if verify_status(m, expect_opt, before.as_deref(), 2500) {
                    crate::flog!("[{}] confirmed (media key)", id);
                } else {
                    crate::flog!("[{}] unconfirmed (media key)", id);
                }
                return;
            }
            PlayPauseStep::Stop => {
                crate::flog!("[{}] unconfirmed (may still be executing, not resending)", id);
                return;
            }
        }
    }
}

/// Next/Prev：非幂等，SMTC 发送成功绝不补发
/// Next/Prev 连发 N 次（保留连续点击意图），每次独立尝试：
/// SMTC 可达走 SMTC，失败的那次用媒体键保意图（该次确定没执行）；
/// 全部发送后只验证一次——中间跳过的歌无需逐首确认
fn handle_skip_n(
    m: &GlobalSystemMediaTransportControlsSessionManager,
    session: Option<&windows::Media::Control::GlobalSystemMediaTransportControlsSession>,
    cmd: MediaCommand,
    n: usize,
    id: String,
) {
    let baseline = CommandBaseline::capture(Some(m));
    let send = match cmd {
        MediaCommand::Next => SmtcSend::Next,
        _ => SmtcSend::Prev,
    };
    let vk = match cmd {
        MediaCommand::Next => VK_MEDIA_NEXT_TRACK,
        _ => VK_MEDIA_PREV_TRACK,
    };
    let mut via_smtc = 0usize;
    let mut via_key = 0usize;
    for i in 0..n {
        let ok = match session {
            Some(s) => send_smtc_direct(s, &send),
            None => false,
        };
        if ok {
            via_smtc += 1;
        } else {
            // 该次 SMTC 未送达（确定没执行）→ 媒体键保意图
            send_media_key(vk);
            via_key += 1;
        }
        if i + 1 < n {
            std::thread::sleep(Duration::from_millis(150));
        }
    }
    crate::flog!("[{}] {:?}×{} sent (smtc={} key={})", id, cmd, n, via_smtc, via_key);
    if verify_track_change(m, &baseline, 2500) {
        crate::flog!("[{}] confirmed", id);
    } else {
        crate::flog!(
            "[{}] unconfirmed ({} skips may have executed, title lagging)",
            id,
            via_smtc + via_key
        );
    }
}

/// SMTC 定向指令
#[derive(Debug, Clone, PartialEq, Eq)]
enum SmtcSend {
    Play,
    Pause,
    Next,
    Prev,
}

fn send_smtc_direct(
    s: &windows::Media::Control::GlobalSystemMediaTransportControlsSession,
    a: &SmtcSend,
) -> bool {
    let op = match a {
        SmtcSend::Play => s.TryPlayAsync(),
        SmtcSend::Pause => s.TryPauseAsync(),
        SmtcSend::Next => s.TrySkipNextAsync(),
        SmtcSend::Prev => s.TrySkipPreviousAsync(),
    };
    op.and_then(|o| o.get()).is_ok()
}

/// 状态验证：Some(expect) = 定向验证达到期望状态（状态已经是期望值也算
/// 成功，兼容手动操作后的场景）；None = toggle 验证与 before 不同（翻转）。
/// 定向模式比"翻转"精确：状态延迟不再被误判为失败。
fn verify_status(
    m: &GlobalSystemMediaTransportControlsSessionManager,
    expect: Option<&str>,
    before: Option<&str>,
    window_ms: u64,
) -> bool {
    let checks = (window_ms / 250).max(1);
    for _ in 0..checks {
        std::thread::sleep(Duration::from_millis(250));
        let now = smtc_raw_status(m);
        match expect {
            Some(e) => {
                if now.as_deref() == Some(e) {
                    return true;
                }
            }
            None => {
                if let (Some(n), Some(b)) = (now.as_deref(), before) {
                    if n != b {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// 命令基准：操作前**两个数据源各自**的快照。验证时同源前后比较——
/// 跨源比较会因格式差异（全半角/空白/后缀）产生假阳性/假阴性；
/// 源缺失（窗口标题暂时为空等）时该源弃权，不参与判定
struct CommandBaseline {
    smtc_title: String,
    window_title: String,
}

impl CommandBaseline {
    fn capture(
        m: Option<&GlobalSystemMediaTransportControlsSessionManager>,
    ) -> Self {
        let smtc_title = m
            .and_then(|m| {
                find_media_session(m)
                    .and_then(|s| s.TryGetMediaPropertiesAsync().and_then(|o| o.get()).ok())
            })
            .map(|p| p.Title().map(|t| t.to_string()).unwrap_or_default())
            .unwrap_or_default();
        let window_title = crate::win32::netease_window_title()
            .and_then(|r| parse_window_title(&r))
            .map(|(t, _)| t)
            .unwrap_or_default();
        Self { smtc_title, window_title }
    }

    /// 单源前后变化判定：None = 该源数据缺失（弃权）
    fn source_changed(&self, now: &str, before: &str) -> Option<bool> {
        if before.is_empty() || now.is_empty() {
            return None;
        }
        Some(norm_loose(now) != norm_loose(before))
    }

    /// 任一数据源检测到切歌
    fn track_changed(
        &self,
        m: &GlobalSystemMediaTransportControlsSessionManager,
    ) -> bool {
        let smtc_now = find_media_session(m)
            .and_then(|s| s.TryGetMediaPropertiesAsync().and_then(|o| o.get()).ok())
            .map(|p| p.Title().map(|t| t.to_string()).unwrap_or_default())
            .unwrap_or_default();
        if let Some(true) = self.source_changed(&smtc_now, &self.smtc_title) {
            return true;
        }
        if let Some((wtitle, _)) = crate::win32::netease_window_title()
            .and_then(|r| parse_window_title(&r))
        {
            if let Some(true) = self.source_changed(&wtitle, &self.window_title) {
                return true;
            }
        }
        false
    }
}

/// 切歌验证：任一数据源出现标题变化即确认（两个源更新时间不同，先后到达
/// 都算数；同名歌曲连续来回切换无法用标题检测，按 unconfirmed 处理——
/// 对不补发策略无害）
fn verify_track_change(
    m: &GlobalSystemMediaTransportControlsSessionManager,
    baseline: &CommandBaseline,
    window_ms: u64,
) -> bool {
    let checks = (window_ms / 250).max(1);
    for _ in 0..checks {
        std::thread::sleep(Duration::from_millis(250));
        if baseline.track_changed(m) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_title() {
        assert_eq!(
            parse_window_title("晴天 - 周杰伦"),
            Some(("晴天".into(), "周杰伦".into()))
        );
    }

    #[test]
    fn title_with_dash_splits_at_last() {
        // 歌名自身含 " - "：以最后一个分割，歌手仍在最后
        assert_eq!(
            parse_window_title("NHK - 拾歌曲 - 某歌手"),
            Some(("NHK - 拾歌曲".into(), "某歌手".into()))
        );
    }

    #[test]
    fn rejects_client_self_title() {
        assert_eq!(parse_window_title("网易云音乐"), None);
        assert_eq!(parse_window_title(""), None);
    }

    #[test]
    fn no_dash_means_title_only() {
        assert_eq!(
            parse_window_title("纯歌名"),
            Some(("纯歌名".into(), String::new()))
        );
    }

    #[test]
    fn norm_loose_ignores_whitespace_and_case() {
        assert_eq!(norm_loose("A B c"), norm_loose("abc"));
        assert_ne!(norm_loose("a"), norm_loose("b"));
    }

    #[test]
    fn ncm_id_format_roundtrip() {
        // extract_ncm_id 需要 WinRT 对象，测格式解析等价逻辑
        let raw = "NCM-2309212";
        let parsed = raw.strip_prefix("NCM-").and_then(|r| r.trim().parse::<u64>().ok());
        assert_eq!(parsed, Some(2309212));
    }
}

#[cfg(test)]
mod playpause_tests {
    use super::*;

    /// 任务书场景：第一次命令成功执行但状态延迟 → 重试必须是同向定向
    /// 指令（幂等），绝不允许二次 toggle 反向
    #[test]
    fn verify_timeout_retry_is_same_direction_not_toggle() {
        let plan = plan_playpause(Some("playing")); // 想 Pause
        // 首次发送成功但验证超时
        let step = next_playpause_step(&plan, true, 1);
        match step {
            PlayPauseStep::RetryDirected(a) => assert_eq!(a, PlayPauseAction::DirectedPause),
            ref other => panic!("expected same-direction retry, got {:?}", other),
        }
    }

    #[test]
    fn known_playing_plans_directed_pause() {
        assert_eq!(plan_playpause(Some("playing")), PlayPauseAction::DirectedPause);
    }

    #[test]
    fn known_paused_plans_directed_play() {
        assert_eq!(plan_playpause(Some("paused")), PlayPauseAction::DirectedPlay);
        assert_eq!(plan_playpause(Some("stopped")), PlayPauseAction::DirectedPlay);
    }

    #[test]
    fn unknown_state_plans_toggle_and_never_retries() {
        let plan = plan_playpause(None);
        assert_eq!(plan, PlayPauseAction::Toggle);
        // toggle 非幂等：验证超时不补发（无论通道是否可达）
        assert_eq!(next_playpause_step(&plan, true, 1), PlayPauseStep::Stop);
        assert_eq!(next_playpause_step(&plan, false, 1), PlayPauseStep::Stop);
    }

    #[test]
    fn smtc_channel_dead_falls_to_media_key() {
        // 定向指令从未送达 → toggle 无反向风险（第一次必然没执行）
        let plan = plan_playpause(Some("playing"));
        assert_eq!(next_playpause_step(&plan, false, 1), PlayPauseStep::MediaKey);
    }

    #[test]
    fn retry_capped_at_two() {
        let plan = plan_playpause(Some("paused"));
        assert_eq!(next_playpause_step(&plan, true, 2), PlayPauseStep::Stop);
        assert_eq!(next_playpause_step(&plan, true, 3), PlayPauseStep::Stop);
    }

    #[test]
    fn directed_actions_are_idempotent_set() {
        // 同向重试集合里只有 Play/Pause，绝无 Toggle
        let retries = [
            next_playpause_step(&plan_playpause(Some("playing")), true, 1),
            next_playpause_step(&plan_playpause(Some("paused")), true, 1),
        ];
        for r in retries {
            match r {
                PlayPauseStep::RetryDirected(a) => {
                    assert!(matches!(a, PlayPauseAction::DirectedPlay | PlayPauseAction::DirectedPause));
                }
                other => panic!("unexpected {:?}", other),
            }
        }
    }
}

#[cfg(test)]
mod baseline_tests {
    use super::*;

    fn baseline(smtc: &str, window: &str) -> CommandBaseline {
        CommandBaseline { smtc_title: smtc.into(), window_title: window.into() }
    }

    /// 同源比较：SMTC 标题的变化用 SMTC 基准判定，窗口标题用窗口基准判定
    #[test]
    fn same_source_comparison() {
        let b = baseline("晴天", "晴天 - 周杰伦");
        // SMTC 源变化（格式差异被归一吸收）
        assert_eq!(b.source_changed("晴天 (Live)", "晴天"), Some(true));
        // SMTC 源未变
        assert_eq!(b.source_changed("晴天", "晴天"), Some(false));
    }

    /// 窗口标题暂时为空 → 该源弃权（None），既不误报也不漏报
    #[test]
    fn missing_current_value_abstains() {
        let b = baseline("晴天", "晴天 - 周杰伦");
        assert_eq!(b.source_changed("", "晴天"), None);
        // 基准缺失同样弃权
        let b2 = baseline("", "晴天 - 周杰伦");
        assert_eq!(b2.source_changed("别的歌", ""), None);
    }

    /// 归一化统一：空白/大小写差异不构成变化
    #[test]
    fn normalization_unified_across_sources() {
        let b = baseline("Walk On", "Walk On - U2");
        assert_eq!(b.source_changed("walk  on", "Walk On"), Some(false));
    }

    /// 防跨源误判：SMTC 标题与窗口标题格式不同但同歌 → 未变化
    /// （旧实现拿窗口标题与 SMTC 基准比，这种场景会假阳性）
    #[test]
    fn cross_source_format_difference_not_a_change() {
        let b = baseline("晴天", "晴天 (Live) - 周杰伦");
        // 窗口标题带后缀是它自己的常态——用自己的基准判，未变化
        assert_eq!(b.source_changed("晴天 (Live)", "晴天 (Live)"), Some(false));
    }
}
