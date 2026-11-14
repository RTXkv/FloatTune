//! 网易云公开接口：免登录搜索歌曲、拉取歌词（LRC + 逐字），带本地缓存。
//! 不使用任何账号 Cookie，不影响播放记录。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Clone)]
pub struct NeteaseClient {
    http: reqwest::Client,
}

impl NeteaseClient {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36")
            .timeout(std::time::Duration::from_secs(8))
            .build()
            .expect("reqwest client");
        Self { http }
    }
}

fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

#[derive(Deserialize, Debug, Clone)]
struct SearchResp {
    #[serde(default)]
    result: Option<SearchResult>,
}

#[derive(Deserialize, Debug, Clone)]
struct SearchResult {
    #[serde(default)]
    songs: Option<Vec<SearchSong>>,
}

#[derive(Deserialize, Debug, Clone)]
struct SearchSong {
    id: u64,
    name: String,
    #[serde(default)]
    artists: Vec<Artist>,
    #[serde(default)]
    duration: u64,
    #[serde(default, rename = "album")]
    album: Option<SearchAlbum>,
}

#[derive(Deserialize, Debug, Clone)]
struct SearchAlbum {
    #[serde(default, rename = "picUrl")]
    pic_url: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
struct Artist {
    #[serde(default)]
    name: String,
}

/// 歌词载荷（返回给前端，兼作缓存格式）
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LyricPayload {
    pub song_id: u64,
    pub title: String,
    pub artist: String,
    pub duration_ms: Option<u64>,
    /// 纯 LRC 文本（[mm:ss.xx] 行）
    pub lrc: String,
    /// 逐字歌词原始文本（yrc/qrc/ttml，可能为空）
    pub timed: Option<String>,
    /// 专辑封面直链
    pub cover_url: Option<String>,
    /// 封面已转好的 data URL（缓存后离线可用）
    pub cover_b64: Option<String>,
    pub source: String,
}

impl NeteaseClient {
    /// 标题+歌手 → (song_id, duration_ms, 封面直链)
    pub async fn resolve(&self, title: &str, artist: &str) -> Option<(u64, Option<u64>, Option<String>)> {
        let query = format!("{title} {artist}");
        let resp: SearchResp = self
            .http
            .get("https://music.163.com/api/search/get")
            .query(&[
                ("s", query),
                ("type", "1".to_string()),
                ("limit", "8".to_string()),
                ("offset", "0".to_string()),
            ])
            .header("Referer", "https://music.163.com")
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;

        let songs = resp.result?.songs?;
        if songs.is_empty() {
            return None;
        }
        let nt = norm(title);
        let artist_tokens: Vec<String> = artist
            .split(['/', '、', ','])
            .map(norm)
            .filter(|s| !s.is_empty())
            .collect();

        let score = |s: &SearchSong| -> i32 {
            let mut sc = 0;
            if norm(&s.name) == nt {
                sc += 100;
            } else if norm(&s.name).contains(&nt) || nt.contains(&norm(&s.name)) {
                sc += 40;
            }
            let joined = norm(
                &s.artists.iter().map(|a| a.name.clone()).collect::<Vec<_>>().join("/"),
            );
            for t in &artist_tokens {
                if joined.contains(t) {
                    sc += 30;
                }
            }
            sc
        };

        let mut best = songs.clone();
        best.sort_by(|a, b| score(b).cmp(&score(a)));
        let hit = &best[0];
        let pic = hit
            .album
            .as_ref()
            .and_then(|a| a.pic_url.clone())
            .map(|u| {
                let base = u.split('?').next().unwrap_or(&u).to_string();
                format!("{base}?param=400y400")
            });
        Some((
            hit.id,
            if hit.duration > 0 { Some(hit.duration) } else { None },
            pic,
        ))
    }

    /// 拉取歌词：优先 v1 接口（可能含逐字 yrc/qrc/ttml），回退老接口
    pub async fn fetch_lyric(&self, id: u64) -> (String, Option<String>) {
        // v1 接口
        let id_s = id.to_string();
        let v1: serde_json::Value = match self
            .http
            .get("https://music.163.com/api/song/lyric/v1")
            .query(&[
                ("os", "pc"),
                ("id", id_s.as_str()),
                ("lv", "-1"),
                ("kv", "-1"),
                ("tv", "-1"),
                ("rv", "-1"),
                ("yv", "-1"),
                ("yt", "-1"),
            ])
            .header("Referer", "https://music.163.com")
            .send()
            .await
        {
            Ok(r) => match r.json().await {
                Ok(v) => v,
                Err(_) => serde_json::Value::Null,
            },
            Err(_) => serde_json::Value::Null,
        };

        if !v1.is_null() {
            let get_lrc = |key: &str| -> Option<String> {
                v1.get(key)
                    .and_then(|k| k.get("lyric"))
                    .and_then(|l| l.as_str())
                    .map(|s| s.to_string())
                    .filter(|s| !s.is_empty())
            };
            let lrc = get_lrc("lrc");
            let timed = get_lrc("yrc")
                .or_else(|| get_lrc("qrc"))
                .or_else(|| get_lrc("ttml"));
            if let Some(lrc) = lrc {
                return (lrc, timed);
            }
        }

        // 老接口兜底
        let legacy: serde_json::Value = match self
            .http
            .get("https://music.163.com/api/song/lyric")
            .query(&[
                ("os", "pc"),
                ("id", id_s.as_str()),
                ("lv", "-1"),
                ("kv", "-1"),
                ("tv", "-1"),
            ])
            .header("Referer", "https://music.163.com")
            .send()
            .await
        {
            Ok(r) => match r.json().await {
                Ok(v) => v,
                Err(_) => serde_json::Value::Null,
            },
            Err(_) => serde_json::Value::Null,
        };

        let lrc = legacy
            .get("lrc")
            .and_then(|k| k.get("lyric"))
            .and_then(|l| l.as_str())
            .unwrap_or("")
            .to_string();
        (lrc, None)
    }
}

/// 曲目 id 映射缓存（标题|歌手 → id），避免重复搜索
fn trackmap_path(dir: &PathBuf) -> PathBuf {
    dir.join("trackmap.json")
}

fn load_trackmap(dir: &PathBuf) -> HashMap<String, TrackKey> {
    fs::read_to_string(trackmap_path(dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_trackmap(dir: &PathBuf, map: &HashMap<String, TrackKey>) {
    if let Ok(s) = serde_json::to_string(map) {
        let _ = fs::write(trackmap_path(dir), s);
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct TrackKey {
    pub song_id: u64,
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub pic_url: Option<String>,
}

impl NeteaseClient {
    /// 下载封面图为 data URL
    async fn fetch_cover_image(&self, url: &str) -> Option<String> {
        let resp = self.http.get(url).send().await.ok()?;
        let bytes = resp.bytes().await.ok()?;
        if bytes.is_empty() || bytes.len() > 1_500_000 {
            return None;
        }
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        Some(format!("data:image/jpeg;base64,{}", b64))
    }
}

/// 歌词缓存文件：appdata/lyrics/{id}.json
fn lyric_cache_path(dir: &PathBuf, id: u64) -> PathBuf {
    dir.join("lyrics").join(format!("{id}.json"))
}

/// 完整管线：缓存 → 搜索 → 歌词 → 缓存
pub async fn fetch_lyrics_full(
    client: &NeteaseClient,
    dir: &PathBuf,
    title: &str,
    artist: &str,
    key: &Mutex<HashMap<String, TrackKey>>,
) -> Result<LyricPayload, String> {
    let nk = format!("{}|{}", norm(title), norm(artist));

    // 1. id 映射缓存
    let cached = load_trackmap(dir);
    let mut map = cached;
    let (song_id, dur_cached, pic_url) = match map.get(&nk) {
        // 旧缓存可能没有封面直链，这种情况重新解析一次
        Some(k) if k.pic_url.is_some() => (k.song_id, k.duration_ms, k.pic_url.clone()),
        _ => {
            let (sid, dur, pic) = client
                .resolve(title, artist)
                .await
                .ok_or("搜索不到这首歌（标题+歌手）")?;
            map.insert(
                nk.clone(),
                TrackKey { song_id: sid, duration_ms: dur, pic_url: pic.clone() },
            );
            save_trackmap(dir, &map);
            *key.lock().unwrap() = map.clone();
            (sid, dur, pic)
        }
    };

    // 2. 歌词缓存
    let cache = lyric_cache_path(dir, song_id);
    if let Ok(s) = fs::read_to_string(&cache) {
        if let Ok(p) = serde_json::from_str::<LyricPayload>(&s) {
            return Ok(p);
        }
    }

    // 3. 在线拉取
    let (lrc, timed) = client.fetch_lyric(song_id).await;
    if lrc.trim().is_empty() && timed.is_none() {
        return Err("这首歌没有找到歌词".into());
    }
    let cover_b64 = match &pic_url {
        Some(u) => client.fetch_cover_image(u).await,
        None => None,
    };
    let payload = LyricPayload {
        song_id,
        title: title.into(),
        artist: artist.into(),
        duration_ms: dur_cached,
        lrc,
        timed,
        cover_url: pic_url,
        cover_b64,
        source: "netease-public".into(),
    };
    let _ = fs::create_dir_all(cache.parent().unwrap());
    if let Ok(s) = serde_json::to_string(&payload) {
        let _ = fs::write(&cache, s);
    }
    Ok(payload)
}
