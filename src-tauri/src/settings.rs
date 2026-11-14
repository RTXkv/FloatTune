use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;

/// 每种形态的窗口几何缓存（物理像素 [x, y, w, h]，旧版 [x, y] 兼容）
pub type Positions = HashMap<String, Vec<f64>>;

/// 当前配置文件版本。v1 = 引入版本号前的旧格式（所有字段均有默认值，
/// 反序列化即可读，无需显式迁移逻辑）；后续若有不兼容变更，在此递增
/// 并在 load 里按 config_version 分支迁移。
const CONFIG_VERSION: u32 = 2;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Settings {
    /// 当前形态: strip | card | taskbar
    pub mode: String,
    /// 歌词样式: off | single | multi | scroll | karaoke
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
    /// 配置文件版本（v2 起写入；旧文件缺省按 v1 读取）
    pub config_version: u32,
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
            config_version: CONFIG_VERSION,
        }
    }
}

impl Settings {
    pub fn load(dir: &std::path::Path) -> Self {
        let path = dir.join("settings.json");
        let mut s = match fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str::<Settings>(&raw) {
                Ok(v) => v,
                Err(e) => {
                    // 解析失败 = 配置损坏：保留原文件供人工恢复，回退默认值
                    crate::flog!("[settings] 配置解析失败（{}），回退默认值", e);
                    backup_corrupted(&path);
                    Settings::default()
                }
            },
            Err(_) => Settings::default(), // 首次运行，无配置文件
        };
        sanitize(&mut s);
        // 旧版位置条目是 [x, y]，统一补齐为 [x, y, w, h]
        for v in s.positions.values_mut() {
            if v.len() < 4 {
                v.resize(4, 0.0);
            }
        }
        s
    }

    /// 原子写入：先写临时文件再改名替换，进程中途被杀不会留下截断的配置。
    /// （Windows 上 std::fs::rename 经 MoveFileEx(REPLACE_EXISTING) 可覆盖目标）
    pub fn save(&self, dir: &std::path::Path) {
        if let Err(e) = fs::create_dir_all(dir) {
            crate::flog!("[settings] 创建配置目录失败: {}", e);
            return;
        }
        let json = match serde_json::to_string_pretty(self) {
            Ok(v) => v,
            Err(e) => {
                crate::flog!("[settings] 配置序列化失败: {}", e);
                return;
            }
        };
        let tmp = dir.join("settings.json.tmp");
        if let Err(e) = fs::write(&tmp, &json) {
            crate::flog!("[settings] 配置写入失败: {}", e);
            return;
        }
        let path = dir.join("settings.json");
        if let Err(e) = fs::rename(&tmp, &path) {
            crate::flog!("[settings] 配置替换失败: {}（原配置保持不变）", e);
            let _ = fs::remove_file(&tmp);
        }
    }
}

/// 配置损坏时把原文件改名为 settings.bak（若备份名被占用则直接覆盖备份，
/// 最新损坏现场优先）
fn backup_corrupted(path: &std::path::Path) {
    let bak = path.with_extension("bak");
    match fs::rename(path, &bak) {
        Ok(_) => crate::flog!("[settings] 原配置已保留为 {}", bak.display()),
        Err(e) => crate::flog!("[settings] 配置备份失败: {}", e),
    }
}

/// 数值/枚举合法性校验：手改或旧版本写入的垃圾值钳回安全区间。
/// （字段名与默认值不动，只收窄取值范围，不改变配置格式）
fn sanitize(s: &mut Settings) {
    s.active_opacity = s.active_opacity.clamp(0.05, 1.0);
    s.idle_opacity = s.idle_opacity.clamp(0.0, 1.0);
    s.idle_seconds = s.idle_seconds.min(300);
    s.hide_delay_secs = s.hide_delay_secs.min(120);
    s.ui_scale = s.ui_scale.clamp(0.5, 2.0); // modes.rs 使用时再钳 0.7-1.6
    s.lyric_font_size = s.lyric_font_size.clamp(8.0, 60.0);
    if !matches!(
        s.lyric_style.as_str(),
        "off" | "single" | "multi" | "scroll" | "karaoke"
    ) {
        s.lyric_style = "single".into();
    }
    s.config_version = CONFIG_VERSION;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_clamps_garbage_values() {
        let mut s = Settings::default();
        s.active_opacity = 5.0;
        s.idle_opacity = -1.0;
        s.ui_scale = 9.9;
        s.lyric_font_size = 500.0;
        s.hide_delay_secs = 9999;
        s.lyric_style = "hacker".into();
        sanitize(&mut s);
        assert_eq!(s.active_opacity, 1.0);
        assert_eq!(s.idle_opacity, 0.0);
        assert_eq!(s.ui_scale, 2.0);
        assert_eq!(s.lyric_font_size, 60.0);
        assert_eq!(s.hide_delay_secs, 120);
        assert_eq!(s.lyric_style, "single");
        assert_eq!(s.config_version, CONFIG_VERSION);
    }

    #[test]
    fn old_style_value_survives() {
        let mut s = Settings::default();
        s.lyric_style = "scroll".into();
        sanitize(&mut s);
        assert_eq!(s.lyric_style, "scroll");
    }
}
