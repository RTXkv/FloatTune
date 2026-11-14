// 入口：根据窗口参数挂载对应形态的 UI
import { store } from "./core/store";
import "./styles/base.css";

// 前端运行时错误直接写到窗口标题，便于外部诊断
window.addEventListener("error", (e) => {
  document.title = `ERR|${e.message?.slice(0, 120)}`;
});
window.addEventListener("unhandledrejection", (e) => {
  const r = (e as PromiseRejectionEvent).reason;
  document.title = `REJ|${String(r?.message ?? r).slice(0, 120)}`;
});

import { invoke } from "@tauri-apps/api/core";

const win = new URLSearchParams(location.search).get("win") ?? "strip";
invoke("debug_log", { msg: `boot ${win}` }).catch(() => {});

store
  .init()
  .then(async () => {
    await invoke("debug_log", { msg: `init-ok ${win}` }).catch(() => {});
    switch (win) {
      case "strip": {
        const m = await import("./ui/strip");
        m.mountStrip();
        break;
      }
      case "card": {
        const m = await import("./ui/card");
        m.mountCard();
        break;
      }
      case "settings": {
        const m = await import("./ui/settings");
        m.mountSettings();
        break;
      }
    }
    await invoke("debug_log", { msg: `mount-ok ${win}` }).catch(() => {});
  })
  .catch((e) => {
    invoke("debug_log", { msg: `init-fail ${win}: ${String(e).slice(0, 150)}` }).catch(() => {});
  });
