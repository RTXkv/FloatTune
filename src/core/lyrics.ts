// 歌词获取与解析（Rust 侧负责网络与缓存，前端负责解析与滚动）
import { invoke } from "@tauri-apps/api/core";
import { parseYrc, KaraokeLine } from "./karaoke";

export interface LyricPayload {
  song_id: number;
  title: string;
  artist: string;
  duration_ms: number | null;
  lrc: string;
  timed: string | null;
  cover_url: string | null;
  cover_b64: string | null;
  source: string;
}

/** LRC 行（整句时间戳） */
export interface LyricLine {
  time: number;
  text: string;
}

const memCache = new Map<number, LyricPayload & { lines: LyricLine[]; kar: KaraokeLine[] }>();
let lastRequest = "";

/** 最近一次加载失败的原因分类：net（可重试）/ nomatch / nolyric（不可重试） */
export let lastFailKind: "" | "net" | "nomatch" | "nolyric" = "";

function parseLrc(lrc: string): LyricLine[] {
  const lines: LyricLine[] = [];
  for (const raw of lrc.split(/\r?\n/)) {
    // 支持多时间戳 [00:12.00][01:34.00]
    const stamps = [...raw.matchAll(/\[(\d{1,2}):(\d{1,2})(?:[.:](\d{1,3}))?\]/g)];
    if (stamps.length === 0) continue;
    const text = raw.replace(/\[[^\]]*\]/g, "").trim();
    if (!text) continue;
    for (const m of stamps) {
      const min = parseInt(m[1], 10);
      const sec = parseInt(m[2], 10);
      const fracRaw = m[3] ?? "0";
      const frac = parseInt(fracRaw.padEnd(3, "0").slice(0, 3), 10);
      lines.push({ time: (min * 60 + sec) * 1000 + frac, text });
    }
  }
  lines.sort((a, b) => a.time - b.time);
  return lines;
}

export async function loadLyrics(
  title: string,
  artist: string,
  durationMs: number | null
): Promise<(LyricPayload & { lines: LyricLine[]; kar: KaraokeLine[] }) | null> {
  const key = `${title}|${artist}`;
  if (key === lastRequest && memCache.size > 0) {
    // 同一首歌已在加载/已加载
    const first = memCache.values().next().value;
    if (first && first.title === title && first.artist === artist) return first;
  }
  lastRequest = key;
  lastFailKind = "";
  try {
    const payload = await invoke<LyricPayload>("fetch_lyrics", {
      title,
      artist,
      durationMs,
    });
    let lines = parseLrc(payload.lrc);
    const kar = payload.timed ? parseYrc(payload.timed) : [];
    // 纯音乐/无歌词检测：网易会给纯音乐标注“纯音乐，请欣赏”类歌词；
    // 歌词为空或行数很少且命中关键词 → 视为纯音乐，交给 UI 显示随机颜文字
    const joined = lines.map((l) => l.text).join("|");
    const instrumental =
      lines.length === 0 ||
      (lines.length <= 4 && /纯音乐|请欣赏|instrumental/i.test(joined));
    if (instrumental) lines = [];
    const full = { ...payload, lines, kar };
    memCache.clear();
    memCache.set(payload.song_id, full);
    void invoke("debug_log", {
      msg: `lyrok lines=${lines.length} kar=${full.kar.length} lrcLen=${payload.lrc.length}`,
    }).catch(() => {});
    return full;
  } catch (e) {
    const msg = String(e);
    // 错误分类：net: 网络问题（可重试）；nomatch:/nolyric: 结论性问题（重试无意义，
    // 避免每 3s 打一次搜索接口）
    lastFailKind = msg.startsWith("net:")
      ? "net"
      : msg.startsWith("nomatch:")
        ? "nomatch"
        : msg.startsWith("nolyric:")
          ? "nolyric"
          : "net";
    void invoke("debug_log", {
      msg: `lyrfail(${lastFailKind}) "${title}": ${msg.slice(0, 140)}`,
    }).catch(() => {});
    return null;
  }
}

/** 当前应该高亮的行下标 */
export function currentLineIndex(lines: LyricLine[], posMs: number): number {
  let lo = 0;
  let idx = -1;
  for (let i = lines.length - 1; i >= 0; i--) {
    if (lines[i].time <= posMs) {
      idx = i;
      break;
    }
    void lo;
  }
  return idx;
}
