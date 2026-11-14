use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// 每种形态的窗口几何缓存（物理像素 [x, y, w, h]，旧版 [x, y] 兼容）
pub type Positions = HashMap<String, Vec<f64>>;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Settings {
    /// 当前形态: strip | card | taskbar
    pub mode: String,
    /// 歌词样式: off | single | multi | karaoke
    pub lyric_style: String,
    /// 活跃时透明度 0.1-1.0
    pub active_opacity: f32,
    /// 鼠标离开后是否自动变淡
    pub idle_dim: bool,
    /// 闲置变淡后的透明度
    pub idle_opacity: f32,
    /// 闲置多少秒后变淡
    pub idle_seconds: u32,
    /// 锁定点击穿透（鼠标完全穿过窗口；用托盘菜单解除）
    pub clickthrough_lock: bool,
    /// 前台应用全屏时自动隐藏
    pub hide_fullscreen: bool,
    /// 暂停时自动隐藏、播放时自动显示
    pub show_only_playing: bool,
    /// 暂停后延迟多少秒再开始渐隐（0=立即）
    pub hide_delay_secs: u32,
    /// 显示专辑封面
    pub show_cover: bool,
    /// 显示进度条
    pub show_progress: bool,
    /// 开机自启动
    pub autostart: bool,
    /// 各窗口记住的位置
    pub positions: Positions,
    /// 界面缩放（0.7-1.6，1.0=标准尺寸）
    pub ui_scale: f64,
    /// 歌词字号（逻辑像素）
    pub lyric_font_size: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: "strip".into(),
            lyric_style: "single".into(),
            active_opacity: 0.92,
            idle_dim: true,
            idle_opacity: 0.28,
            idle_seconds: 6,
            clickthrough_lock: false,
            hide_fullscreen: false,
            show_only_playing: true,
            hide_delay_secs: 0,
                    show_cover: true,
            show_progress: true,
            autostart: false,
            positions: HashMap::new(),
            ui_scale: 1.0,
            lyric_font_size: 15.0,
        }
    }
}

impl Settings {
    pub fn load(dir: &PathBuf) -> Self {
        let path = dir.join("settings.json");
        let mut s: Settings = fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        // 旧版位置条目是 [x, y]，统一补齐为 [x, y, w, h]
        for v in s.positions.values_mut() {
            if v.len() < 4 {
                v.resize(4, 0.0);
            }
        }
        s
    }

    pub fn save(&self, dir: &PathBuf) {
        let path = dir.join("settings.json");
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = fs::create_dir_all(dir);
            let _ = fs::write(path, s);
        }
    }
}
