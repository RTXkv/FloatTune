// 全局状态：SMTC 快照、设置、全屏状态 + 事件订阅
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export interface TrackSnapshot {
  title: string;
  artist: string;
  album: string;
  cover_b64: string | null;
  status: string;
  timeline_valid: boolean;
  position_ms: number;
  duration_ms: number;
  source: string;
  /** InfLink 提供的网易云歌曲精确 ID（null = 不可用） */
  song_id: number | null;
  track_generation: number;
}

export interface Settings {
  mode: string;
  lyric_style: string;
  active_opacity: number;
  idle_dim: boolean;
  idle_opacity: number;
  idle_seconds: number;
  clickthrough_lock: boolean;
  hide_fullscreen: boolean;
  show_only_playing: boolean;
  hide_delay_secs: number;
  show_cover: boolean;
  show_progress: boolean;
  autostart: boolean;
  lyric_font_size: number;
  positions: Record<string, number[]>;
  ui_scale: number;
}

type Listener = () => void;

class Store {
  track: TrackSnapshot | null = null;
  settings: Settings | null = null;
  fullscreen = false;
  private listeners = new Set<Listener>();

  subscribe(fn: Listener): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  notify() {
    for (const fn of this.listeners) {
      try {
        fn();
      } catch (e) {
        // 单个窗口的渲染异常不能杀死其他更新
        console.error("[store] listener error", e);
      }
    }
  }

  media(cmd: "playpause" | "next" | "prev") {
    invoke("media_cmd", { cmd }).catch(() => {});
  }

  async init() {
    // Rust setup 与页面 JS 存在启动竞态：state 未就绪时重试而不是放弃
    let lastErr: unknown = null;
    for (let attempt = 0; attempt < 30; attempt++) {
      try {
        this.settings = await invoke<Settings>("get_settings");
        this.track = await invoke<TrackSnapshot>("get_state");
        lastErr = null;
        break;
      } catch (e) {
        lastErr = e;
        await new Promise((r) => setTimeout(r, 250));
      }
    }
    if (lastErr !== null) {
      invoke("debug_log", { msg: `init-fail persistent: ${String(lastErr).slice(0, 100)}` }).catch(() => {});
      throw lastErr;
    }

    await listen<TrackSnapshot>("smtc:track", (e) => {
      this.track = { ...this.track, ...e.payload };
      this.notify();
    });
    await listen<TrackSnapshot>("smtc:status", (e) => {
      this.track = { ...this.track, ...e.payload };
      this.notify();
    });
    await listen<[number, number]>("smtc:timeline", (e) => {
      if (!this.track) return;
      this.track = {
        ...this.track,
        timeline_valid: true,
        position_ms: e.payload[0],
        duration_ms: e.payload[1],
      };
      this.notify();
    });
    await listen<boolean>("fullscreen", (e) => {
      this.fullscreen = e.payload;
      this.notify();
    });
    await listen<Settings>("settings", (e) => {
      this.settings = e.payload;
      this.notify();
    });

    this.notify();
  }
}

export const store = new Store();
