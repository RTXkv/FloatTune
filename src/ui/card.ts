// 角落毛玻璃卡片：封面(动态取色) + 歌名/歌手 + 歌词 + 进度 + 控制 + 音量
import "../styles/card.css";
import { store } from "../core/store";
import { PositionClock, fmtTime } from "../core/clock";
import { loadLyrics, currentLineIndex, LyricLine } from "../core/lyrics";
import { renderKaraoke, currentKaraokeIndex, KaraokeLine } from "../core/karaoke";
import { restartLyricScroll } from "./marquee";
import { marqueeSetPlaying } from "./marquee";
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

/** 从封面 data URL 提取主色调（简单量化平均） */
function extractAccent(dataUrl: string): Promise<[number, number, number]> {
  return new Promise((resolve) => {
    const img = new Image();
    img.onload = () => {
      const c = document.createElement("canvas");
      c.width = 16;
      c.height = 16;
      const ctx = c.getContext("2d");
      if (!ctx) return resolve([64, 58, 88]);
      ctx.drawImage(img, 0, 0, 16, 16);
      const data = ctx.getImageData(0, 0, 16, 16).data;
      let r = 0,
        g = 0,
        b = 0,
        n = 0;
      for (let i = 0; i < data.length; i += 4) {
        r += data[i];
        g += data[i + 1];
        b += data[i + 2];
        n++;
      }
      resolve([Math.round(r / n), Math.round(g / n), Math.round(b / n)]);
    };
    img.onerror = () => resolve([64, 58, 88]);
    img.src = dataUrl;
  });
}

export function mountCard() {
  const app = document.getElementById("app")!;
  app.innerHTML = `
    <div class="card">
      <div class="card-head dz">
        <div class="cover-sm no-cover" id="cover-sm">♪</div>
        <div class="card-meta">
          <div class="title">FloatTune</div>
          <div class="artist">等待网易云音乐…</div>
        </div>
      </div>
      <div class="card-lyric dz"><div class="line cur">♪</div><div class="line nxt"></div></div>
      <div class="card-progress dz">
        <div class="bar"><div class="fill"></div></div>
        <div class="time">0:00 / --:--</div>
      </div>
      <div class="card-ctrl">
        <button class="cbtn" id="c-prev">${ICONS.prev}</button>
        <button class="cbtn big" id="c-play">${ICONS.play}</button>
        <button class="cbtn" id="c-next">${ICONS.next}</button>
        <button class="cbtn small" id="c-settings" title="设置">${ICONS.gear}</button>
      </div>
      <div class="rs rs-e"></div>
      <div class="rs rs-s"></div>
      <div class="rs rs-se"></div>
    </div>
  `;

  // 小封面拖动窗口；其他分区穿透（同极简浮窗）
  (app.querySelector("#cover-sm") as HTMLElement).addEventListener("mousedown", (e: MouseEvent) => {
    if (e.button !== 0) return;
    e.preventDefault();
    void invoke("begin_drag").catch(() => {});
  });

  // 滚轮微调歌词偏移
  const bindNudgeWheel = (el: HTMLElement | null) => {
    if (!el) return;
    el.addEventListener("wheel", (e) => {
      e.preventDefault();
      clock.calibrate(clock.nowMs() + (e.deltaY < 0 ? 500 : -500));
    }, { passive: false });
  };
  bindNudgeWheel(app.querySelector("#cover-sm"));
  bindNudgeWheel(app.querySelector(".card-ctrl"));

  function reportRegions() {
    const dpr = window.devicePixelRatio || 1;
    const rects: Array<[number, number, number, number, number]> = [];
    const picks: Array<[string, number]> = [
      ["#cover-sm", 1],
      [".card-ctrl", 0],
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
    void invoke("update_strip_regions", { window: "card", rects }).catch(() => {});
  }
  new ResizeObserver(() => reportRegions()).observe(app);

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

  const cardEl = app.querySelector(".card") as HTMLDivElement;
  const coverSm = app.querySelector("#cover-sm") as HTMLDivElement;
  const titleEl = app.querySelector(".title") as HTMLDivElement;
  const artistEl = app.querySelector(".artist") as HTMLDivElement;
  let lineCur = app.querySelector(".line.cur") as HTMLDivElement;
  let lineNxt = app.querySelector(".line.nxt") as HTMLDivElement;
  const fillEl = app.querySelector(".fill") as HTMLDivElement;
  const timeEl = app.querySelector(".time") as HTMLDivElement;
  const playBtn = app.querySelector("#c-play") as HTMLButtonElement;


  // 自绘拖动：跟随鼠标移动窗口（不触发系统 Snap 分屏提示）
  app.querySelectorAll<HTMLElement>(".dz").forEach((el) => {
    el.addEventListener("mousedown", (e) => {
      if (e.button !== 0 || (e.target as HTMLElement).closest("button")) return;
      e.preventDefault();
      void invoke("begin_drag").catch(() => {});
    });
  });

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
    if (e.payload) setTimeout(() => reportRegions(), 120);
  });
  let lyricLines: LyricLine[] = [];
  let karLines: KaraokeLine[] = [];
  let scrollLines: Array<{
    start: number;
    end: number;
    text: string;
    karLine: KaraokeLine | null;
  }> = [];
  let lastMk = "";
  let scrollBuilt = "";
  let lyricReqKey = "";
  let apiDuration = 0;
  let apiCover = "";
  let lastLyricIdx = -2;
  let lastCover = "";
  let idleMark = "(´･ω･`)";
  let lastPlaying: boolean | null = null;

  function render() {
    const t = store.track;
    const s = store.settings;
    if (!t || !s) return;

    clock.syncTrack(t.title);
    clock.setPlaying(t.status === "playing");

    const cov = apiCover || t.cover_b64 || "";
    if (cov && cov !== lastCover) {
      lastCover = cov;
      coverSm.style.backgroundImage = `url("${cov}")`;
      coverSm.classList.remove("no-cover");
      coverSm.textContent = "";
      void extractAccent(cov).then(([r, g, b]) => {
        cardEl.style.setProperty("--accent", `rgb(${r},${g},${b})`);
        cardEl.style.setProperty("--accent-soft", `rgba(${r},${g},${b},0.35)`);
      });
    } else if (!cov && lastCover) {
      lastCover = "";
      coverSm.style.backgroundImage = "";
      coverSm.classList.add("no-cover");
      coverSm.textContent = "♪";
    }

    titleEl.textContent = t.title || "未在播放";
    artistEl.textContent = t.artist;
    playBtn.innerHTML = t.status === "playing" ? ICONS.pause : ICONS.play;
    const playing = t.status === "playing";
    if (playing !== lastPlaying) {
      lastPlaying = playing;
      marqueeSetPlaying(playing);
    }
    lineCur.style.fontSize = `${s.lyric_font_size}px`;
    lineNxt.style.fontSize = `${Math.max(11, s.lyric_font_size - 2)}px`;

    applyIdleSettings();
  }

  // 构建滚动歌词源：逐字优先，无逐字用整句时间
  function buildScroll() {
    const s = store.settings;
    if (!s || s.lyric_style !== "scroll") return;
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
    scrollBuilt = "";
  }

  function applyIdleSettings() {
    const s = store.settings!;
    cardEl.style.setProperty("--active-op", String(s.active_opacity));
    cardEl.style.setProperty("--idle-op", String(s.idle_opacity));
  }

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
    scrollBuilt = "";
    apiDuration = 0;
    apiCover = "";
    lastLyricIdx = -2;
    idleMark = KAOMOJI[Math.floor(Math.random() * KAOMOJI.length)];
    lineCur.textContent = idleMark;
    lineNxt.textContent = "";
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

  function applyStyleScroll() {
    const s = store.settings;
    if (!s) return;
    const lyricBox = app.querySelector(".card-lyric") as HTMLDivElement;
    if (s.lyric_style === "scroll") {
      lyricBox.classList.add("scroll");
      if (lyricBox.querySelector(".line")) {
        lyricBox.innerHTML = "";
        scrollBuilt = "";
      }
    } else {
      lyricBox.classList.remove("scroll");
      if (lyricBox.querySelector(".lstrip")) {
        lyricBox.innerHTML = '<div class="line cur">♪</div><div class="line nxt"></div>';
        lineCur = app.querySelector(".line.cur") as HTMLDivElement;
        lineNxt = app.querySelector(".line.nxt") as HTMLDivElement;
        scrollBuilt = "";
        lastLyricIdx = -2;
      }
    }
  }

  function highlight(pos: number) {
    const s = store.settings;
    if (!s) return;
    applyStyleScroll();
    if (s.lyric_style === "off") {
      lineCur.style.display = "none";
      lineNxt.style.display = "none";
      return;
    }
    if (s.lyric_style === "scroll") {
      if (karLines.length === 0 && lyricLines.length === 0) return;
      highlightScroll(pos);
      return;
    }
    lineCur.style.display = "";
    if (s.lyric_style === "karaoke" && karLines.length > 0) {
      const idx = currentKaraokeIndex(karLines, pos);
      // 前奏提前显示第一句（未唱部分不点亮），不用颜文字
      const line = idx >= 0 ? karLines[idx] : karLines[0] ?? null;
      renderKaraoke(lineCur, line, pos, idleMark);
      // 长句单向整行滚动：从行首滚到行尾，尾部停留
      const mk = line ? String(line.start) : "";
      if (lastMk !== mk) {
        lastMk = mk;
        restartLyricScroll(lineCur, line ? line.dur : 8000);

      }
      lineNxt.textContent =
        idx >= 0 && idx + 1 < karLines.length
          ? karLines[idx + 1].words.map((w) => w.text).join("")
          : "";
      return;
    }
    if (lyricLines.length === 0) return;
    const idx = currentLineIndex(lyricLines, pos);
    if (idx === lastLyricIdx) return;
    lastLyricIdx = idx;
    const text = idx >= 0 ? lyricLines[idx].text : idleMark;
    if (lineCur.textContent !== text) {
      lineCur.classList.remove("swap");
      void lineCur.offsetWidth;
      lineCur.classList.add("swap");
      lineCur.textContent = text; // ← 此前丢失的关键赋值：没有它行内容永远停留在颜文字
    }
    // LRC 行同样单向整行滚动（时长 = 本句到下一句的时间）
    const durMs =
      idx >= 0 && idx + 1 < lyricLines.length
        ? Math.max(2000, lyricLines[idx + 1].time - lyricLines[idx].time)
        : 8000;
    restartLyricScroll(lineCur, durMs);
    lineNxt.textContent = idx >= 0 && idx + 1 < lyricLines.length ? lyricLines[idx + 1].text : "";
  }

  // 滚动歌词：单行横条。逐字歌跟字居中；无逐字按句时长匀速滚动
  function highlightScroll(pos: number) {
    const s = store.settings;
    if (!s) return;
    if (karLines.length === 0 && lyricLines.length === 0) {
      const lyricBox0 = app.querySelector(".card-lyric") as HTMLDivElement;
      if (lyricBox0.textContent !== idleMark) {
        lyricBox0.innerHTML = "";
        const span = document.createElement("span");
        span.className = "lstrip";
        span.textContent = idleMark;
        lyricBox0.appendChild(span);
      }
      return;
    }
    lineNxt.style.display = "none";

    // 构建一次性 DOM：当前行的字条（切换行时重建）
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
    let strip = app.querySelector(".lstrip") as HTMLDivElement | null;
    if (!strip || strip.dataset.key !== stripKey) {
      const lyricBox0 = app.querySelector(".card-lyric") as HTMLDivElement;
      lyricBox0.innerHTML = '<div class="lstrip"></div>';
      strip = app.querySelector(".lstrip") as HTMLDivElement;
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

    const lyricBox0 = app.querySelector(".card-lyric") as HTMLDivElement;
    // 长句单向整行滚动
    const mk = `${lyricReqKey}|${idx}`;
    if (lastMk !== mk) {
      lastMk = mk;
      restartLyricScroll(strip, cur.end - cur.start);
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

  app.querySelector("#c-prev")!.addEventListener("click", () => store.media("prev"));
  app.querySelector("#c-next")!.addEventListener("click", () => store.media("next"));
  playBtn.addEventListener("click", () => store.media("playpause"));
  app.querySelector("#c-settings")!.addEventListener("click", () => {
    invoke("show_settings_window").catch(() => {});
  });

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
  wake();

  let tick = 0;
  setInterval(() => {
    const t = store.track;
    if (!t) return;
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
    const posShown = Math.max(0, pos);
    timeEl.textContent = `${fmtTime(posShown)} / ${dur > 0 ? fmtTime(dur) : "--:--"}`;
    highlight(pos);
    if (tick % 16 === 0) {
      invoke("debug_log", {
        msg: `state st=${t.status} pos=${Math.round(clock.nowMs())} ll=${lyricLines.length} kl=${karLines.length} cur="${lineCur.textContent.slice(0, 24)}" nxt="${lineNxt.textContent.slice(0, 24)}"`,
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
