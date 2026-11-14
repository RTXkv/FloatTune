// 设置窗口：所有选项即时生效并广播到各形态窗口
import "../styles/settings.css";
import { store, Settings } from "../core/store";
import { invoke } from "@tauri-apps/api/core";
import { emit } from "@tauri-apps/api/event";

// 模块级：设置页实时同步订阅（重挂载前退订旧的，防止叠加泄漏）
let settingsUnsub: (() => void) | null = null;

const MODES: Array<[string, string, string]> = [
  ["strip", "极简浮窗", "一行歌词 · 最不碍眼"],
  ["card", "全览浮窗", "封面卡片 · 信息全"],
];

const LYRIC_STYLES: Array<[string, string, string]> = [
  ["off", "关闭", "只看歌名"],
  ["single", "当前一句", "最克制"],
  ["multi", "多行渐隐", "上下文滚动"],
  ["scroll", "歌词滚动", "全曲滚动 · 居中高亮"],
  ["karaoke", "逐字卡拉OK", "逐字点亮 · 精美"],
];

export function mountSettings() {
  const app = document.getElementById("app")!;
  const s = store.settings;
  if (!s) return;

  app.innerHTML = `
    <div class="set-wrap">
      <h1>FloatTune 设置</h1>

      <section>
        <h2>显示形态</h2>
        <div class="seg" id="seg-mode">
          ${MODES.map(
            ([v, t, d]) => `
            <button class="seg-card" data-v="${v}">
              <b>${t}</b><i>${d}</i>
            </button>`
          ).join("")}
        </div>
      </section>

      <section>
        <h2>歌词样式</h2>
        <div class="seg" id="seg-lyric">
          ${LYRIC_STYLES.map(
            ([v, t, d]) => `
            <button class="seg-card" data-v="${v}">
              <b>${t}</b><i>${d}</i>
            </button>`
          ).join("")}
        </div>
      </section>

      <section>
        <h2>外观</h2>
        <div class="row">
          <div class="row-label">活跃时透明度<b id="v-active">${Math.round(s.active_opacity * 100)}%</b></div>
          <input type="range" id="r-active" min="5" max="100" value="${Math.round(s.active_opacity * 100)}" />
        </div>
        <div class="row row-tight">
          <div class="row-label">鼠标离开后自动变淡</div>
          <label class="switch"><input type="checkbox" id="c-idle" ${s.idle_dim ? "checked" : ""}/><span class="track"><span class="knob"/></span></label>
        </div>
        <div class="row">
          <div class="row-label">变淡后透明度<b id="v-idle">${Math.round(s.idle_opacity * 100)}%</b></div>
          <input type="range" id="r-idle" min="1" max="100" value="${Math.round(s.idle_opacity * 100)}" />
        </div>
        <div class="row">
          <div class="row-label">变淡延迟<b id="v-delay">${s.idle_seconds}s</b></div>
          <input type="range" id="r-delay" min="2" max="30" value="${s.idle_seconds}" />
        </div>
        <div class="row row-tight">
          <div class="row-label">显示专辑封面</div>
          <label class="switch"><input type="checkbox" id="c-cover" ${s.show_cover ? "checked" : ""}/><span class="track"><span class="knob"/></span></label>
        </div>
      </section>

      <section>
        <h2>行为</h2>
        <div class="row row-tight">
          <div class="row-label">锁定点击穿透 <i class="hint">开启后从托盘菜单解除</i></div>
          <label class="switch"><input type="checkbox" id="c-ct" ${s.clickthrough_lock ? "checked" : ""}/><span class="track"><span class="knob"/></span></label>
        </div>
        <div class="row row-tight">
          <div class="row-label">暂停时自动隐藏，播放时自动显示</div>
          <label class="switch"><input type="checkbox" id="c-onlyplaying" ${s.show_only_playing ? "checked" : ""}/><span class="track"><span class="knob"/></span></label>
        </div>
        <div class="row">
          <div class="row-label">暂停后延迟消失<b id="v-hidedelay">${s.hide_delay_secs}s</b><i class="hint">0 = 检测到暂停就开始渐隐</i></div>
          <input type="range" id="r-hidedelay" min="0" max="30" value="${s.hide_delay_secs}" />
        </div>
        <div class="row row-tight">
          <div class="row-label">前台应用全屏时自动隐藏</div>
          <label class="switch"><input type="checkbox" id="c-fs" ${s.hide_fullscreen ? "checked" : ""}/><span class="track"><span class="knob"/></span></label>
        </div>
        <div class="row row-tight">
          <div class="row-label">开机自动启动</div>
          <label class="switch"><input type="checkbox" id="c-autostart" ${s.autostart ? "checked" : ""}/><span class="track"><span class="knob"/></span></label>
        </div>
        <div class="row">
          <div class="row-label">歌词字号<b id="v-font">${Math.round(s.lyric_font_size)}px</b></div>
          <input type="range" id="r-font" min="12" max="24" value="${Math.round(s.lyric_font_size)}" />
        </div>
        <div class="row">
          <div class="row-label">窗口大小<b id="v-scale">${Math.round(s.ui_scale * 100)}%</b></div>
          <input type="range" id="r-scale" min="70" max="160" value="${Math.round(s.ui_scale * 100)}" />
        </div>
        <div class="row">
          <div class="row-label">歌词微调 <i class="hint">在网易云里拖动进度后，用这里把歌词对齐回来</i></div>
          <div class="nudge-row">
            <button class="ghost-btn half" id="b-nudge-back">歌词延后 0.5s</button>
            <button class="ghost-btn half" id="b-nudge-fwd">歌词提前 0.5s</button>
          </div>
        </div>
        <button class="ghost-btn" id="b-reposition">重新定位当前窗口</button>
      </section>

      <footer>
        进度为估算值：网易云未通过系统接口上报播放进度，若在客户端内拖动进度，歌词会短暂漂移，切歌后自动恢复。
      </footer>
    </div>
  `;

  const current: Settings = JSON.parse(JSON.stringify(s));

  async function apply() {
    await invoke("set_settings", { settings: current });
    // set_settings 会广播 "settings" 事件，store 自动更新
  }

  // 形态 / 歌词样式 分段选择
  function bindSeg(id: string, key: "mode" | "lyric_style") {
    const seg = app.querySelector(`#${id}`) as HTMLDivElement;
    const sync = () => {
      seg.querySelectorAll(".seg-card").forEach((el) => {
        el.classList.toggle("on", (el as HTMLElement).dataset.v === current[key]);
      });
    };
    seg.querySelectorAll(".seg-card").forEach((el) => {
      el.addEventListener("click", () => {
        current[key] = (el as HTMLElement).dataset.v!;
        sync();
        void apply();
      });
    });
    sync();
  }
  bindSeg("seg-mode", "mode");
  bindSeg("seg-lyric", "lyric_style");

  // 滑杆
  const bindRange = (
    id: string,
    labelId: string,
    get: () => number,
    set: (v: number) => void,
    fmt: (v: number) => string
  ) => {
    const r = app.querySelector(`#${id}`) as HTMLInputElement;
    const label = app.querySelector(`#${labelId}`) as HTMLElement;
    let t: number | undefined;
    r.addEventListener("input", () => {
      set(Number(r.value));
      label.textContent = fmt(Number(r.value));
      // 拖动即保存（防抖 400ms），杜绝 change 事件丢失导致设置回退
      window.clearTimeout(t);
      t = window.setTimeout(() => void apply(), 400);
    });
    label.textContent = fmt(get());
  };
  bindRange(
    "r-active",
    "v-active",
    () => current.active_opacity,
    (v) => (current.active_opacity = v / 100),
    (v) => `${v}%`
  );
  bindRange(
    "r-idle",
    "v-idle",
    () => current.idle_opacity,
    (v) => (current.idle_opacity = v / 100),
    (v) => `${v}%`
  );
  bindRange(
    "r-delay",
    "v-delay",
    () => current.idle_seconds,
    (v) => (current.idle_seconds = v),
    (v) => `${v}s`
  );
  bindRange(
    "r-hidedelay",
    "v-hidedelay",
    () => current.hide_delay_secs,
    (v) => (current.hide_delay_secs = v),
    (v) => `${v}s`
  );
  bindRange(
    "r-font",
    "v-font",
    () => current.lyric_font_size,
    (v) => (current.lyric_font_size = v),
    (v) => `${v}px`
  );
  bindRange(
    "r-scale",
    "v-scale",
    () => current.ui_scale,
    (v) => (current.ui_scale = v / 100),
    (v) => `${v}%`
  );

  // 开关
  const bindToggle = (id: string, set: (v: boolean) => void) => {
    (app.querySelector(`#${id}`) as HTMLInputElement).addEventListener("change", (e) => {
      set((e.target as HTMLInputElement).checked);
      void apply();
    });
  };
  bindToggle("c-idle", (v) => (current.idle_dim = v));
  bindToggle("c-cover", (v) => (current.show_cover = v));
  bindToggle("c-ct", (v) => (current.clickthrough_lock = v));
  bindToggle("c-onlyplaying", (v) => (current.show_only_playing = v));
  bindToggle("c-fs", (v) => (current.hide_fullscreen = v));
  bindToggle("c-autostart", (v) => (current.autostart = v));

  app.querySelector("#b-reposition")!.addEventListener("click", () => {
    void invoke("reposition");
  });

  // 歌词微调：广播给所有形态窗口的时钟
  const bindNudge = (id: string, delta: number) => {
    app.querySelector(`#${id}`)!.addEventListener("click", () => {
      void emit("lyric-nudge", delta);
    });
  };
  bindNudge("b-nudge-back", -500);
  bindNudge("b-nudge-fwd", 500);

  // 托盘/其他窗口改了设置 → 重新渲染本页（重挂载前退订旧订阅者，防止叠加泄漏）
  if (settingsUnsub) settingsUnsub();
  settingsUnsub = store.subscribe(() => {
    const s = store.settings;
    if (!s) return;
    if (JSON.stringify(s) === JSON.stringify(current)) return;
    mountSettings();
  });
}
