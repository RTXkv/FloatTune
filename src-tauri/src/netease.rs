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

/// 进程级共享客户端（reqwest 内部连接池跨请求复用；此前每次命令重建 Client，
/// 连接池白白丢弃）
static SHARED_CLIENT: std::sync::OnceLock<NeteaseClient> = std::sync::OnceLock::new();

pub fn shared_client() -> &'static NeteaseClient {
    SHARED_CLIENT.get_or_init(NeteaseClient::new)
}

/// 歌名归一：去空白/常见标点 + 小写。
/// （"晴天 Live" vs "晴天 (Live)" 的括号差异不应破坏精确匹配）
fn norm_title(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !c.is_whitespace()
                && !matches!(
                    c,
                    '(' | ')' | '（' | '）' | '[' | ']' | '【' | '】' | '-' | '－' | '·' | '～' | '~'
                )
        })
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// 匹配打分（纯函数，可测试）。
/// 特征：歌名精确/包含、歌手集合精确匹配、时长差、版本差异词。
fn score_candidate(
    cand_name: &str,
    cand_artists: &[String],
    cand_duration_ms: u64,
    query_title: &str,
    query_artist_tokens: &[String],
    query_duration_ms: Option<u64>,
) -> i32 {
    let nt = norm_title(query_title);
    let sn = norm_title(cand_name);
    let mut sc = 0;
    if sn == nt {
        sc += 100;
    } else if sn.contains(&nt) || nt.contains(&sn) {
        sc += 40;
    }

    // 歌手：候选歌手名与查询 token **互相精确**匹配（防"周杰伦"误配
    // "周杰伦Cover"），全中加分，一无所中重罚
    let cand_artists_n: Vec<String> = cand_artists
        .iter()
        .map(|a| norm(a))
        .filter(|n| !n.is_empty())
        .collect();
    let mut matched = 0usize;
    for t in query_artist_tokens {
        if cand_artists_n.iter().any(|c| c == t) {
            matched += 1;
        }
    }
    if matched > 0 {
        sc += 30 * matched as i32;
        if matched == query_artist_tokens.len() {
            sc += 15;
        }
    } else if !query_artist_tokens.is_empty() {
        sc -= 50;
    }

    // 时长特征：InfLink 在位时有真实歌曲时长
    if let Some(cur) = query_duration_ms {
        if cand_duration_ms > 0 {
            let diff = (cand_duration_ms as i64 - cur as i64).abs();
            if diff <= 2000 {
                sc += 120;
            } else if diff <= 5000 {
                sc += 60;
            } else if diff > 15_000 {
                sc -= 80;
            }
        }
    }

    // 版本差异词：候选是 Live/伴奏/翻唱而查询没提 → 惩罚
    let query_has_marker = VERSION_MARKERS.iter().any(|m| nt.contains(m));
    if !query_has_marker {
        let marker_hit = VERSION_MARKERS
            .iter()
            .any(|m| sn.contains(m) || cand_artists_n.iter().any(|c| c.contains(m)));
        if marker_hit {
            sc -= 60;
        }
    }
    sc
}

/// 搜索失败分类：网络问题（前端可重试）vs 无高置信结果（不重试）
pub enum ResolveFail {
    Net(String),
    NoResult,
    LowConfidence(i32),
}

impl ResolveFail {
    /// 结构化错误前缀：net: / nomatch:（前端按前缀决定是否重试）
    pub fn to_reason(&self) -> String {
        match self {
            ResolveFail::Net(e) => format!("net: {}", e),
            ResolveFail::NoResult => "nomatch: search returned nothing".into(),
            ResolveFail::LowConfidence(s) => {
                format!("nomatch: best score {} below threshold", s)
            }
        }
    }
}

fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// 版本差异词：候选歌名含这些词而查询不含 → 大概率是 Live/伴奏/翻唱等错配
const VERSION_MARKERS: [&str; 10] = [
    "live", "现场", "伴奏", "remix", "cover", "翻唱", "instrumental", "piano", "demo", "rehearsal",
];

/// 最低匹配分：低于此值视为不可信，宁可不显示歌词（前端降级为颜文字）
const MIN_MATCH_SCORE: i32 = 60;

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
    /// 标题+歌手 → (song_id, duration_ms, 封面直链, 匹配分)
    ///
    /// 打分特征：歌名精确/包含、歌手集合精确匹配、时长差（InfLink 提供真实时长）、
    /// 版本差异词惩罚。低于 MIN_MATCH_SCORE 返回 None（不强行选错配结果）。
    pub async fn resolve(
        &self,
        title: &str,
        artist: &str,
        current_duration_ms: Option<u64>,
    ) -> Result<(u64, Option<u64>, Option<String>, i32), ResolveFail> {
        let query = format!("{title} {artist}");
        let resp: SearchResp = get_json_with_retry(
            &self.http,
            "https://music.163.com/api/search/get",
            &[
                ("s", query),
                ("type", "1".to_string()),
                ("limit", "8".to_string()),
                ("offset", "0".to_string()),
            ],
        )
        .await
        .ok_or_else(|| ResolveFail::Net("search request failed".into()))?;

        let songs = resp.result.and_then(|r| r.songs).unwrap_or_default();
        if songs.is_empty() {
            return Err(ResolveFail::NoResult);
        }
        let _nt = norm(title);
        // 查询歌手按集合切分（、/,/／），用于候选歌手的精确匹配
        let artist_tokens: Vec<String> = artist
            .split(['/', '、', ',', '，', '＆', '&'])
            .map(norm)
            .filter(|s| !s.is_empty())
            .collect();

        let score = |s: &SearchSong| -> i32 {
            score_candidate(
                &s.name,
                &s.artists.iter().map(|a| a.name.clone()).collect::<Vec<_>>(),
                s.duration,
                title,
                &artist_tokens,
                current_duration_ms,
            )
        };

        let mut best = songs.clone();
        best.sort_by_key(|s| std::cmp::Reverse(score(s)));
        let best_score = score(&best[0]);
        crate::flog!(
            "[netease] resolve \"{}|{}\" dur={:?} -> id={} score={} (candidates={})",
            title,
            artist,
            current_duration_ms,
            best[0].id,
            best_score,
            best.len()
        );
        if best_score < MIN_MATCH_SCORE {
            crate::flog!("[netease] match below threshold, refusing to guess");
            return Err(ResolveFail::LowConfidence(best_score));
        }
        let hit = &best[0];
        let pic = hit
            .album
            .as_ref()
            .and_then(|a| a.pic_url.clone())
            .map(|u| {
                let base = u.split('?').next().unwrap_or(&u).to_string();
                format!("{base}?param=400y400")
            });
        Ok((
            hit.id,
            if hit.duration > 0 { Some(hit.duration) } else { None },
            pic,
            best_score,
        ))
    }

    /// 拉取歌词：优先 v1 接口（可能含逐字 yrc/qrc/ttml），回退老接口。
    /// Err = 两个接口都网络失败（前端可重试）；Ok 空串 = 确实没有歌词（不重试）
    pub async fn fetch_lyric(&self, id: u64) -> Result<(String, Option<String>), String> {
        let id_s = id.to_string();
        let v1: serde_json::Value =
            get_json_with_retry(
                &self.http,
                "https://music.163.com/api/song/lyric/v1",
                &[
                    ("os", "pc".into()),
                    ("id", id_s.clone()),
                    ("lv", "-1".into()),
                    ("kv", "-1".into()),
                    ("tv", "-1".into()),
                    ("rv", "-1".into()),
                    ("yv", "-1".into()),
                    ("yt", "-1".into()),
                ],
            )
            .await
            .unwrap_or(serde_json::Value::Null);

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
                return Ok((lrc, timed));
            }
        }

        // 老接口兜底
        let legacy: serde_json::Value =
            get_json_with_retry(
                &self.http,
                "https://music.163.com/api/song/lyric",
                &[
                    ("os", "pc".into()),
                    ("id", id_s.clone()),
                    ("lv", "-1".into()),
                    ("kv", "-1".into()),
                    ("tv", "-1".into()),
                ],
            )
            .await
            .unwrap_or(serde_json::Value::Null);

        let lrc = legacy
            .get("lrc")
            .and_then(|k| k.get("lyric"))
            .and_then(|l| l.as_str())
            .unwrap_or("")
            .to_string();
        if lrc.is_empty() && v1.is_null() && legacy.is_null() {
            return Err("net: both lyric endpoints unreachable".into());
        }
        Ok((lrc, None))
    }
}

/// 曲目 id 映射缓存（标题|歌手 → id），避免重复搜索
fn trackmap_path(dir: &std::path::Path) -> PathBuf {
    dir.join("trackmap.json")
}

fn load_trackmap(dir: &std::path::Path) -> HashMap<String, TrackKey> {
    fs::read_to_string(trackmap_path(dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

#[derive(Serialize, Deserialize, Clone)]
pub struct TrackKey {
    pub song_id: u64,
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub pic_url: Option<String>,
    /// 匹配分（v2 缓存字段；旧缓存缺省 0，低于门槛的条目会被重新解析修正）
    #[serde(default)]
    pub score: i32,
}

/// 单次 GET-JSON，失败自动重试一次（网络抖动；500ms 退避）
async fn get_json_with_retry<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    url: &str,
    query: &[(&str, String)],
) -> Option<T> {
    for attempt in 0..2u32 {
        let r = http
            .get(url)
            .query(query)
            .header("Referer", "https://music.163.com")
            .send()
            .await;
        if let Ok(resp) = r {
            if let Ok(v) = resp.json::<T>().await {
                return Some(v);
            }
        }
        if attempt == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }
    None
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
fn lyric_cache_path(dir: &std::path::Path, id: u64) -> PathBuf {
    dir.join("lyrics").join(format!("{id}.json"))
}

/// 原子写：临时文件 + rename 替换（进程被杀不留截断文件）
fn atomic_write(path: &std::path::Path, data: &str) {
    let tmp = path.with_extension("tmp");
    if let Err(e) = fs::write(&tmp, data) {
        crate::flog!("[netease] 缓存写入失败 {}: {}", path.display(), e);
        return;
    }
    if let Err(e) = fs::rename(&tmp, path) {
        crate::flog!("[netease] 缓存替换失败 {}: {}", path.display(), e);
        let _ = fs::remove_file(&tmp);
    }
}

/// 内存 payload 缓存 + 全局请求闸门：并发请求（两个窗口同时请求同一首歌）
/// 在闸门上排队，后到者命中内存缓存直接返回，避免重复网络请求
static FETCH_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static PAYLOAD_MEM: std::sync::OnceLock<Mutex<HashMap<String, LyricPayload>>> =
    std::sync::OnceLock::new();

fn payload_mem() -> &'static Mutex<HashMap<String, LyricPayload>> {
    PAYLOAD_MEM.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 完整管线：内存 payload 缓存 →（闸门）→ 内存映射缓存 → 磁盘映射缓存 →
/// InfLink ID 直取 / 搜索匹配 → 歌词 → 落盘。
/// 错误带结构化前缀（net:/nomatch:/nolyric:），前端按前缀决定是否重试。
pub async fn fetch_lyrics_full(
    client: &NeteaseClient,
    dir: &std::path::Path,
    title: &str,
    artist: &str,
    duration_ms: Option<u64>,
    song_id_hint: Option<u64>,
    key: &Mutex<HashMap<String, TrackKey>>,
) -> Result<LyricPayload, String> {
    let nk = format!("{}|{}", norm(title), norm(artist));
    if let Some(p) = payload_mem().lock().unwrap_or_else(|e| e.into_inner()).get(&nk) {
        return Ok(p.clone());
    }
    let _gate = FETCH_GATE.lock().await;
    // 双重检查：排队期间同请求可能已被先到者完成
    if let Some(p) = payload_mem().lock().unwrap_or_else(|e| e.into_inner()).get(&nk) {
        return Ok(p.clone());
    }

    // 1. 定位歌曲 ID：InfLink 精确 ID 优先（零误配），
    //    否则走 id 映射缓存（内存优先，磁盘兜底；低置信条目重新解析）
    let (song_id, dur_cached, pic_url) = if let Some(sid) = song_id_hint {
        (sid, duration_ms, None)
    } else {
        let mut map = key.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if map.is_empty() {
            map = load_trackmap(dir);
        }
        match map.get(&nk) {
            Some(k) if k.pic_url.is_some() && k.score >= MIN_MATCH_SCORE => {
                (k.song_id, k.duration_ms, k.pic_url.clone())
            }
            _ => {
                let (sid, dur, pic, score) = client
                    .resolve(title, artist, duration_ms)
                    .await
                    .map_err(|e| e.to_reason())?;
                map.insert(
                    nk.clone(),
                    TrackKey { song_id: sid, duration_ms: dur, pic_url: pic.clone(), score },
                );
                if let Ok(s) = serde_json::to_string(&map) {
                    atomic_write(&trackmap_path(dir), &s);
                }
                *key.lock().unwrap_or_else(|e| e.into_inner()) = map.clone();
                (sid, dur, pic)
            }
        }
    };

    // 2. 歌词缓存（歌词内容不变，永久有效）
    let cache = lyric_cache_path(dir, song_id);
    if let Ok(s) = fs::read_to_string(&cache) {
        if let Ok(p) = serde_json::from_str::<LyricPayload>(&s) {
            payload_mem()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(nk, p.clone());
            return Ok(p);
        }
    }

    // 3. 在线拉取
    let (lrc, timed) = client.fetch_lyric(song_id).await?;
    if lrc.trim().is_empty() && timed.is_none() {
        return Err("nolyric: 这首歌没有歌词（可能是纯音乐）".into());
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
        atomic_write(&cache, &s);
    }
    payload_mem()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(nk, payload.clone());
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artists(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn exact_title_and_artist_scores_high() {
        let sc = score_candidate(
            "晴天", &artists(&["周杰伦"]), 269000, "晴天", &["周杰伦".into()], Some(269000),
        );
        // 歌名 100 + 歌手 30+15 + 时长 120 = 265
        assert_eq!(sc, 265);
    }

    #[test]
    fn artist_mismatch_is_penalized_below_threshold() {
        // 同名不同人：歌名 100 - 歌手不匹配 50 = 50 < 60 门槛 → 拒选
        let sc = score_candidate(
            "晴天", &artists(&["某人Cover"]), 269000, "晴天", &["周杰伦".into()], None,
        );
        assert!(sc < MIN_MATCH_SCORE);
    }

    #[test]
    fn duration_mismatch_penalized() {
        // 歌名歌手都对但时长差 30s：100+45-80 = 65，仍然过线但明显降分
        let sc = score_candidate(
            "晴天", &artists(&["周杰伦"]), 239000, "晴天", &["周杰伦".into()], Some(269000),
        );
        assert_eq!(sc, 65);
    }

    #[test]
    fn live_version_penalized() {
        let base = score_candidate(
            "晴天", &artists(&["周杰伦"]), 0, "晴天", &["周杰伦".into()], None,
        );
        let live = score_candidate(
            "晴天 (Live)", &artists(&["周杰伦"]), 0, "晴天", &["周杰伦".into()], None,
        );
        // Live 版歌名带后缀：歌名匹配等级从精确(100)降为包含(40)，再加
        // 版本词惩罚(-60)，总分必须显著低于原版
        assert!(live <= base - 60, "live={} base={}", live, base);
        assert!(live < MIN_MATCH_SCORE + 45, "live={} 应接近被拒选", live);
    }

    #[test]
    fn query_with_marker_not_penalized() {
        // 用户明确搜 Live 版：候选 Live 不惩罚
        let sc = score_candidate(
            "晴天 (Live)", &artists(&["周杰伦"]), 0, "晴天 Live", &["周杰伦".into()], None,
        );
        assert!(sc >= MIN_MATCH_SCORE);
    }

    #[test]
    fn multi_artist_partial_match() {
        // 合唱曲：查询两个歌手只中一个 → 30 分，无全中奖励
        let sc = score_candidate(
            "曲", &artists(&["甲", "乙"]), 0, "曲", &["甲".into(), "丙".into()], None,
        );
        // 歌名 100 + 歌手 30 - 全中未达成 = 130
        assert_eq!(sc, 130);
    }
}
