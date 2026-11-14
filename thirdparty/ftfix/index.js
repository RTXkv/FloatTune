// FloatTuneFix：修复网易云 v3 首页推荐卡片轮播横向溢出窗口边缘的问题
// （.item-sub 轮播容器不裁剪，最后一张卡戳出窗口外，贴着关闭键形成深色条）
// 同时隐藏 BetterNCM/InfLink 注入的面板与导航项（保留服务功能）。
(function () {
  // 溢出裁剪
  const style = document.createElement("style");
  style.textContent = ".item-sub{overflow-x:clip !important}";
  document.head.appendChild(style);

  const HIDE_TEXTS = ["InfLink", "插件市场", "BetterNCM"];
  function sweep() {
    try {
      document.querySelectorAll('[class*="_panel_"]').forEach((el) => {
        if (el instanceof HTMLElement && el.style.display !== "none") el.style.display = "none";
      });
      document.querySelectorAll("div,button,a,span,li,p").forEach((el) => {
        if (!(el instanceof HTMLElement)) return;
        if (el.childElementCount > 3) return;
        const t = (el.textContent || "").trim();
        if (t && t.length <= 24 && HIDE_TEXTS.some((x) => t.includes(x)) && el.style.display !== "none") {
          el.style.display = "none";
        }
      });
    } catch (e) {}
  }
  setInterval(sweep, 1500);
  document.addEventListener("DOMContentLoaded", sweep);
  sweep();
})();
