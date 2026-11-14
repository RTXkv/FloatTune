// 悬浮歌词条：歌名/歌手 + 当前歌词 + 上一首/播放暂停/下一首 + 底部细进度条
import "../styles/strip.css";
import { store } from "../core/store";
import { PositionClock, fmtTime } from "../core/clock";
import { loadLyrics, currentLineIndex, LyricLine } from "../core/lyrics";
import { renderKaraoke, currentKaraokeIndex, KaraokeLine } from "../core/karaoke";
import { marquee, restartLyricScroll, marqueeSetPlaying } from "./marquee";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";

const KAOMOJI = [
  "(´･ω･`)",
  "ヾ(≧▽≦*)o",
  "(๑•̀ㅂ•́)و✧",
  "(＾▽＾)",
  "(*´▽`*)",
  "(￣▽￣)～■",
  "(・ω・)ノ",
  "(◕‿◕)",
];

const ICONS = {
  prev: `<svg viewBox="0 0 24 24"><path d="M6 5h2.4v14H6zM20 5v14l-10-7z"/></svg>`,
  next: `<svg viewBox="0 0 24 24"><path d="M15.6 5H18v14h-2.4zM4 5v14l10-7z"/></svg>`,
  play: `<svg viewBox="0 0 24 24"><path d="M8 5v14l11-7z"/></svg>`,
  pause: `<svg viewBox="0 0 24 24"><path d="M6 19h4V5H6v14zm8-14v14h4V5h-4z"/></svg>`,
  gear: `<svg viewBox="0 0 24 24"><path d="M19.4 13c.04-.32.06-.65.06-1s-.02-.68-.06-1l2.1-1.65a.5.5 0 0 0 .12-.64l-2-3.46a.5.5 0 0 0-.61-.22l-2.49 1a7.3 7.3 0 0 0-1.73-1L14.4 2.4a.5.5 0 0 0-.5-.4h-4a.5.5 0 0 0-.5.42l-.37 2.65c-.63.26-1.2.6-1.74 1l-2.48-1a.5.5 0 0 0-.61.22l-2 3.46a.5.5 0 0 0 .12.64L4.43 11a8.16 8.16 0 0 0 0 2l-2.1 1.65a.5.5 0 0 0-.12.64l2 3.46c.14.24.42.34.61.22l2.49-1c.54.4 1.11.74 1.73 1l.37 2.62c.05.24.26.41.5.41h4c.25 0 .46-.18.5-.42l.37-2.65a7.3 7.3 0 0 0 1.73-1l2.49 1c.23.09.5 0 .61-.22l2-3.46a.5.5 0 0 0-.12-.64L19.4 13zM12 15.5A3.5 3.5 0 1 1 12 8.5a3.5 3.5 0 0 1 0 7z"/></svg>`,
};

export function mountStrip() {
  const app = document.getElementById("app")!;
  app.innerHTML = `
    <div class="strip-glass">
      <div class="cover no-cover" id="strip-cover">♪</div>
      <div class="meta">
        <div class="title">FloatTune</div>
        <div class="artist">等待网易云音乐…</div>
      </div>
      <div class="ctrl">
        <button class="cbtn" id="btn-prev" title="上一首">${ICONS.prev}</button>
        <button class="cbtn" id="btn-play" title="播放/暂停">${ICONS.play}</button>
        <button class="cbtn" id="btn-next" title="下一首">${ICONS.next}</button>
      </div>
      <div class="lyric"><div class="lyric-line">♪</div></div>
      <button class="cbtn small" id="btn-settings" title="设置">${ICONS.gear}</button>
      <div class="progress"><div class="progress-fill"></div></div>
      <div class="rs rs-e"></div>
      <div class="rs rs-s"></div>
      <div class="rs rs-se"></div>
    </div>
  `;

  // 边缘拖拽调整窗口大小
  const cw = getCurrentWindow();
  const bindResize = (sel: string, dir: string) => {
    app.querySelector(sel)!.addEventListener("mousedown", (e) => {
      e.preventDefault();
      void cw.startResizeDragging(dir as never);
    });
  };
  bindResize(".rs-e", "East");
  bindResize(".rs-s", "South");
  bindResize(".rs-se", "SouthEast");

  const coverEl = app.querySelector(".cover") as HTMLDivElement;
  const titleEl = app.querySelector(".title") as HTMLDivElement;
  const artistEl = app.querySelector(".artist") as HTMLDivElement;
  const lyricBox = app.querySelector(".lyric") as HTMLDivElement;
  const playBtn = app.querySelector("#btn-play") as HTMLButtonElement;
  const fillEl = app.querySelector(".progress-fill") as HTMLDivElement;



  // 只有封面可以拖动窗口；功能键+缩放手柄可点击；其余全部穿透
  (app.querySelector("#strip-cover") as HTMLElement).addEventListener("mousedown", (e: MouseEvent) => {
    if (e.button !== 0) return;
    e.preventDefault();
    void invoke("begin_drag").catch(() => {});
  });

  // 滚轮微调歌词偏移：上滚提前 0.5s，下滚延后 0.5s
  const bindNudgeWheel = (el: HTMLElement | null) => {
    if (!el) return;
    el.addEventListener("wheel", (e) => {
      e.preventDefault();
      clock.calibrate(clock.nowMs() + (e.deltaY < 0 ? 500 : -500));
    }, { passive: false });
  };
  bindNudgeWheel(app.querySelector(".cover"));
  bindNudgeWheel(app.querySelector(".ctrl"));

  function reportRegions() {
    const dpr = window.devicePixelRatio || 1;
    const rects: Array<[number, number, number, number, number]> = [];
    const picks: Array<[string, number]> = [
      ["#strip-cover", 1],
      [".ctrl", 0],
      ["#btn-settings", 0],
      [".rs-e", 0],
      [".rs-s", 0],
      [".rs-se", 0],
    ];
    for (const [sel, kind] of picks) {
      const el = app.querySelector(sel) as HTMLElement | null;
      if (!el) continue;
      const r = el.getBoundingClientRect();
      if (r.width > 0 && r.height > 0) {
        rects.push([r.left * dpr, r.top * dpr, r.width * dpr, r.height * dpr, kind]);
      }
    }
    void invoke("update_strip_regions", { window: "strip", rects }).catch(() => {});
  }
  new ResizeObserver(() => reportRegions()).observe(app);

  const clock = new PositionClock();
  // 歌词微调（设置窗口广播）
  void listen<number>("lyric-nudge", (e) => clock.calibrate(clock.nowMs() + e.payload));
  // InfLink 提供真实进度 → 直接校准时钟（歌词与卡拉OK绝对同步）
  // InfLink 真实进度：记录用于暂停回吸；播放中仅在"大跳变"（拖进度/换歌）
  // 时校准时钟——每秒常态校准会与时钟互相拖拽导致 1/16 速度爬行
  let lastRealPos = -1;
  let lastRealApplied = -1;
  void listen<[number, number]>("smtc:timeline", (e) => {
    lastRealPos = e.payload[0];
    const drift = Math.abs(clock.nowMs() - lastRealPos);
    if (drift > 3000) {
      clock.calibrate(lastRealPos);
    }
  });
  // 暂停/播放 → 渐隐/渐显（Rust 决策，前端只做动画）
  void listen<boolean>("vis", (e) => {
    const el = document.getElementById("app");
    // 渐隐到 0.004（Rust 延迟后收缩 1x1；恢复时淡回）。
    // 实测低 alpha 不锁存——之前的锁存结论是 toggle 丢包导致的误判。
    if (el) el.style.opacity = e.payload ? "" : "0.004";
  });
  let lyricLines: LyricLine[] = [];
  let karLines: KaraokeLine[] = [];
  let scrollLines: Array<{
    start: number;
    end: number;
    text: string;
    karLine: KaraokeLine | null;
  }> = [];
  let lyricReqKey = "";
  let apiDuration = 0;
  let apiCover = "";
  let lastLyricIdx = -2;
  let idleMark = "(´･ω･`)";
  let lastPlaying: boolean | null = null;
  let lastMk = "";

  // ---------- 渲染 ----------
  function render() {
    const t = store.track;
    const s = store.settings;
    if (!t || !s) return;

    clock.syncTrack(t.title);
    clock.setPlaying(t.status === "playing");

    const cov = apiCover || t.cover_b64 || "";
    if (coverEl.dataset.src !== cov) {
      coverEl.dataset.src = cov;
      if (cov) {
        coverEl.style.backgroundImage = `url("${cov}")`;
        coverEl.classList.remove("no-cover");
        coverEl.textContent = "";
      } else {
        coverEl.style.backgroundImage = "";
        coverEl.classList.add("no-cover");
        coverEl.textContent = "♪";
      }
    }

    if (titleEl.textContent !== t.title) titleEl.textContent = t.title || "未在播放";
    if (artistEl.textContent !== t.artist) artistEl.textContent = t.artist;
    playBtn.innerHTML = t.status === "playing" ? ICONS.pause : ICONS.play;

    document.body.classList.toggle("paused", t.status !== "playing");
    const playing = t.status === "playing";
    if (playing !== lastPlaying) {
      lastPlaying = playing;
      marqueeSetPlaying(playing);
    }
    applyStyle(s.lyric_style);
    applyIdleSettings();
  }

  function applyStyle(style: string) {
    lyricBox.dataset.style = style;
    if (style === "off") lyricBox.style.display = "none";
    else lyricBox.style.display = "";
    if (style === "multi" && !lyricBox.classList.contains("multi")) {
      lyricBox.classList.add("multi");
      lyricBox.innerHTML = `<div class="l-prev"></div><div class="l-cur"></div><div class="l-next"></div>`;
      lastLyricIdx = -2;
    } else if (style === "single" && lyricBox.classList.contains("multi")) {
      lyricBox.classList.remove("multi");
      lyricBox.innerHTML = `<div class="lyric-line">♪</div>`;
      lastLyricIdx = -2;
    }
  }

  function applyIdleSettings() {
    const s = store.settings!;
    app.style.setProperty("--active-op", String(s.active_opacity));
    app.style.setProperty("--idle-op", String(s.idle_opacity));
    app.style.setProperty("--lyric-size", `${s.lyric_font_size}px`);
  }

  // ---------- 歌词 ----------
  function ensureLyrics() {
    const t = store.track;
    const s = store.settings;
    if (!t || !s) return;
    const key = `${t.title}|${t.artist}`;
    if (key === lyricReqKey) return;
    lyricReqKey = key;
    lyricLines = [];
    karLines = [];
    scrollLines = [];
    apiDuration = 0;
    apiCover = "";
    lastLyricIdx = -2;
    idleMark = KAOMOJI[Math.floor(Math.random() * KAOMOJI.length)];
    setLyricText(idleMark);
    if (s.lyric_style !== "off" && t.title) {
      loadLyrics(t.title, t.artist).then((p) => {
        // 加载失败（网络抖动等）3 秒后自动重试，避免整首歌卡在占位符
        if (!p) {
          window.setTimeout(() => {
            if (lyricReqKey === key) {
              lyricReqKey = "";
              ensureLyrics();
            }
          }, 3000);
          return;
        }
        if (key !== lyricReqKey || !p) return;
        lyricLines = p.lines;
        karLines = p.kar;
        buildScroll();
        if (p.duration_ms && !store.track?.timeline_valid) apiDuration = p.duration_ms;
        if (p.cover_b64) apiCover = p.cover_b64;
        lastLyricIdx = -2;
        render();
      });
    }
  }

  function setLyricText(text: string, durMs = 8000) {
    if (lyricBox.classList.contains("multi")) return; // multi 由 highlight 更新
    const line = lyricBox.querySelector(".lyric-line") as HTMLDivElement | null;
    if (!line) return;
    // 仅当逐字数据存在（卡拉OK分支会接管）才跳过；无逐字的歌走这里正常显示
    if (store.settings?.lyric_style === "karaoke" && karLines.length > 0) return;
    if (line.textContent !== text) {
      line.classList.remove("swap");
      void line.offsetWidth;
      line.classList.add("swap");
    }
    restartLyricScroll(line, durMs);
  }

  // 滚动歌词（条形）：单行横条，逐字歌跟字居中，无逐字按句时长匀速滚动
  function highlightScroll(pos: number) {
    const s = store.settings;
    if (!s) return;
    const lyricBox0 = lyricBox;
    if (karLines.length === 0 && lyricLines.length === 0) {
      if (lyricBox0.querySelector(".lstrip")) {
        lyricBox0.innerHTML = '<div class="lyric-line">♪</div>';
      }
      const line = lyricBox0.querySelector(".lyric-line") as HTMLDivElement | null;
      if (line && line.textContent !== idleMark) marquee(line, idleMark);
      return;
    }
    let idx = -1;
    for (let i = scrollLines.length - 1; i >= 0; i--) {
      if (scrollLines[i].start <= pos) {
        idx = i;
        break;
      }
    }
    if (idx < 0) idx = 0;
    const cur = scrollLines[idx];
    const stripKey = `${lyricReqKey}|${idx}`;
    let strip = lyricBox0.querySelector(".lstrip") as HTMLDivElement | null;
    if (!strip || strip.dataset.key !== stripKey) {
      lyricBox0.innerHTML = '<div class="lstrip"></div>';
      strip = lyricBox0.querySelector(".lstrip") as HTMLDivElement;
      strip.dataset.key = stripKey;
      if (cur.karLine) {
        for (const w of cur.karLine.words) {
          const span = document.createElement("span");
          span.className = "kw";
          span.textContent = w.text;
          strip.appendChild(span);
        }
      } else {
        strip.textContent = cur.text;
      }
    }
    // 长句单向整行滚动（与卡片同款）：行重建时启动，句内 65% 完成
    const mk = `${lyricReqKey}|${idx}`;
    if (lastMk !== mk) {
      lastMk = mk;
      restartLyricScroll(strip, (cur.end - cur.start) * 0.65);
    }
    // 逐字进度点亮
    if (cur.karLine) {
      const spans = strip.querySelectorAll<HTMLElement>(".kw");
      const t = Math.max(cur.start, Math.min(pos, cur.end));
      cur.karLine.words.forEach((wd, i) => {
        const sp = spans[i];
        if (!sp) return;
        const pr = Math.max(0, Math.min(1, (t - wd.start) / wd.dur));
        sp.style.setProperty("--p", pr.toFixed(3));
      });
    }
  }

  function buildScroll() {
    if (karLines.length > 0) {
      scrollLines = karLines.map((l, i) => ({
        start: l.start,
        end: i + 1 < karLines.length ? karLines[i + 1].start : l.start + l.dur,
        text: l.words.map((w) => w.text).join(""),
        karLine: l,
      }));
    } else {
      scrollLines = lyricLines.map((l, i) => ({
        start: l.time,
        end: i + 1 < lyricLines.length ? lyricLines[i + 1].time : l.time + 8000,
        text: l.text,
        karLine: null,
      }));
    }
  }

  function highlight(pos: number) {
    const s = store.settings;
    if (!s || s.lyric_style === "off") return;
    if (s.lyric_style === "scroll") {
      highlightScroll(pos);
      return;
    }
    if (s.lyric_style === "karaoke" && karLines.length > 0) {
      // 逐字点亮（每次心跳都更新点亮进度）
      const lineEl = lyricBox.querySelector(".lyric-line") as HTMLElement | null;
      if (!lineEl) return;
      const idx = currentKaraokeIndex(karLines, pos);
      // 前奏提前显示第一句（未唱部分不点亮），不用颜文字
      const line = idx >= 0 ? karLines[idx] : karLines[0] ?? null;
      renderKaraoke(lineEl, line, pos, idleMark);
      // 长句单向整行滚动：句内 65% 完成滑动，尾部停留可读
      const mk = lineEl.dataset.karKey || "";
      if (lastMk !== mk) {
        lastMk = mk;
        restartLyricScroll(lineEl, line ? line.dur : 8000);
      }
      return;
    }
    if (lyricLines.length === 0) return;
    const idx = currentLineIndex(lyricLines, pos);
    if (idx === lastLyricIdx) return;
    lastLyricIdx = idx;
    const text = idx >= 0 ? lyricLines[idx].text : idleMark;
    if (lyricBox.classList.contains("multi")) {
      const prev = lyricBox.querySelector(".l-prev") as HTMLDivElement;
      const cur = lyricBox.querySelector(".l-cur") as HTMLDivElement;
      const next = lyricBox.querySelector(".l-next") as HTMLDivElement;
      cur.textContent = text;
      prev.textContent = idx > 0 ? lyricLines[idx - 1].text : "";
      next.textContent = idx >= 0 && idx + 1 < lyricLines.length ? lyricLines[idx + 1].text : "";
    } else {
      setLyricText(text);
    }
  }

  // ---------- 控制按钮 ----------
  app.querySelector("#btn-prev")!.addEventListener("click", () => store.media("prev"));
  app.querySelector("#btn-next")!.addEventListener("click", () => store.media("next"));
  playBtn.addEventListener("click", () => store.media("playpause"));
  app.querySelector("#btn-settings")!.addEventListener("click", () => {
    invoke("show_settings_window").catch(() => {});
  });

  // ---------- 闲置变淡 ----------
  let idleTimer = 0;
  function wake() {
    document.body.classList.remove("idle");
    window.clearTimeout(idleTimer);
    const s = store.settings;
    if (s?.idle_dim) {
      idleTimer = window.setTimeout(
        () => document.body.classList.add("idle"),
        Math.max(1, s.idle_seconds) * 1000
      );
    }
  }
  window.addEventListener("mousemove", wake);
  window.addEventListener("mousedown", wake);
  void listen<boolean>("vis", (e) => {
    if (e.payload) setTimeout(() => reportRegions(), 120);
  });
  wake();

  // ---------- 进度/歌词心跳 ----------
  let tick = 0;
  setInterval(() => {
    const t = store.track;
    if (!t) return;
    tick++;
    // 暂停时把进度回吸到最后一个真实位置（InfLink 停止推送前的那一下）
    if (t.status !== "playing" && lastRealPos >= 0 && lastRealApplied !== lastRealPos) {
      clock.calibrate(lastRealPos);
      lastRealApplied = lastRealPos;
    }
    const dur = t.timeline_valid && t.duration_ms > 0 ? t.duration_ms : apiDuration;
    const pos = clock.nowMs();
    if (dur > 0) {
      fillEl.style.width = `${Math.min(100, Math.max(0, (pos / dur) * 100))}%`;
    } else {
      fillEl.style.width = "0%";
    }
    playBtn.title = dur > 0 ? `${fmtTime(pos)} / ${fmtTime(dur)}` : fmtTime(pos);
    highlight(pos);
    if (tick % 16 === 0) {
      invoke("debug_log", {
        msg: `stripst st=${t.status} style=${store.settings?.lyric_style} ll=${lyricLines.length} kl=${karLines.length}`,
      }).catch(() => {});
    }
  }, 100);

  store.subscribe(() => {
    render();
    ensureLyrics();
  });
  render();
  ensureLyrics();
  wake();
}
