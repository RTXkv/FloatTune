// 逐字歌词（yrc）解析与渲染
// yrc 行格式: [行起始ms,行时长ms](字起始,字时长,0)字(字起始,字时长,0)字...

export interface KaraokeWord {
  text: string;
  start: number;
  dur: number;
}

export interface KaraokeLine {
  start: number;
  dur: number;
  words: KaraokeWord[];
}

/** 解析 yrc 文本；元数据行（JSON）与非标准行自动跳过 */
export function parseYrc(yrc: string): KaraokeLine[] {
  const lines: KaraokeLine[] = [];
  for (const raw of yrc.split(/\r?\n/)) {
    const t = raw.trim();
    if (!t || t.startsWith("{")) continue;
    const m = t.match(/^\[(\d+),(\d+)\](.*)$/);
    if (!m) continue;
    const words: KaraokeWord[] = [];
    const re = /\((\d+),(\d+),\d+\)([^(]*)/g;
    let wm: RegExpExecArray | null;
    while ((wm = re.exec(m[3]))) {
      const text = wm[3];
      if (text) words.push({ text, start: +wm[1], dur: Math.max(1, +wm[2]) });
    }
    if (words.length) lines.push({ start: +m[1], dur: +m[2], words });
  }
  lines.sort((a, b) => a.start - b.start);
  return lines;
}

/** 当前应显示的逐字行下标（-1 = 间奏/未开始） */
export function currentKaraokeIndex(lines: KaraokeLine[], pos: number): number {
  let idx = -1;
  for (let i = 0; i < lines.length; i++) {
    if (lines[i].start <= pos) idx = i;
    else break;
  }
  return idx;
}

/**
 * 把一行逐字歌词渲染进容器并按 pos 更新每个字的点亮进度。
 * 结构: <span class="kw" style="--p:0..1">字</span>...
 */
export function renderKaraoke(
  el: HTMLElement,
  line: KaraokeLine | null,
  pos: number,
  idle = "♪"
): void {
  const key = line ? line.start + "|" + line.words.map((w) => w.text).join("") : "";
  if (el.dataset.karKey !== key) {
    el.dataset.karKey = key;
    el.innerHTML = "";
    if (line) {
      for (const w of line.words) {
        const span = document.createElement("span");
        span.className = "kw";
        span.textContent = w.text;
        el.appendChild(span);
      }
    } else {
      // 行间隙/前奏：显示占位（通常为随机颜文字）而不是清空
      el.textContent = idle;
    }
  }
  if (!line) return;
  const spans = el.querySelectorAll<HTMLElement>(".kw");
  line.words.forEach((wd, i) => {
    const span = spans[i];
    if (!span) return;
    const p = Math.max(0, Math.min(1, (pos - wd.start) / wd.dur)).toFixed(3);
    // 值没变不写样式（心跳 100ms × 每字一次的无效写会强制样式重算）
    if (span.style.getPropertyValue("--p") !== p) {
      span.style.setProperty("--p", p);
    }
  });
}


/**
 * 单行跟字滚动：把正在唱的字滚到视口中心（行溢出时才滚动）。
 * stripEl = 行内容容器（内含 .kw 字序列，inline-block）
 * viewEl = 可视裁剪区
 */
