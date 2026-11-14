// 歌词跑马灯：内容溢出时用 WAAPI 做整行往返滚动（Chrome 官方推荐模式：
// scrollWidth/clientWidth 测量 + el.animate）。内容保持版不破坏逐字 span。
const anims = new WeakMap<HTMLElement, Animation>();
const allAnims = new Set<Animation>();

/**
 * 对已构建好内容的行容器做往返滚动（不修改其内容）。
 * 溢出量 = el.scrollWidth - 视口(el.parentElement).clientWidth
 */
export function marqueeElement(el: HTMLElement): void {
  const old = anims.get(el);
  if (old) {
    old.cancel();
    allAnims.delete(old);
    anims.delete(el);
  }
  // 等布局稳定后测量
  requestAnimationFrame(() => {
    const view = el.parentElement;
    if (!view) return;
    // 基准 = 元素自身盒宽（被容器钳宽时 scrollWidth 与 clientWidth 的差才是真实溢出）
    const dist = el.scrollWidth - el.clientWidth;
    if (dist <= 8) return;
    const dur = Math.max(3000, dist * 60); // 约 17px/s 的可读速度
    const a = el.animate(
      [
        { transform: "translateX(0)" },
        { transform: `translateX(${-dist}px)`, offset: 0.5 },
        { transform: "translateX(0)" },
      ],
      { duration: dur * 2, easing: "ease-in-out", iterations: Infinity, delay: 1200 }
    );
    anims.set(el, a);
    allAnims.add(a);
  });
}

/** 设置文本并按需滚动（纯文本行使用） */
export function marquee(el: HTMLElement, text: string): void {
  if (el.textContent !== text) el.textContent = text;
  marqueeElement(el);
}

/** 暂停/恢复所有跑马灯（跟随播放状态） */
export function marqueeSetPlaying(playing: boolean) {
  for (const a of allAnims) {
    if (playing) void a.play();
    else void a.pause();
  }
}


/**
 * 单行歌词单向滚动：仅当行溢出视口时启用 CSS 关键帧动画
 * （终点用 cqw 容器单位表达，无需 JS 测量距离）。
 * durMs = 滚动时长（一般取该句的时长）。
 */
export function restartLyricScroll(el: HTMLElement, durMs: number): void {
  // 门槛+终点：行内容宽度 vs 视口内容宽度（扣除 padding），溢出才滚动
  const view = el.parentElement;
  let dist = 0;
  if (view) {
    const cs = getComputedStyle(view);
    const avail =
      view.clientWidth - parseFloat(cs.paddingLeft) - parseFloat(cs.paddingRight);
    dist = el.scrollWidth - avail + 20; // 终点多留 20px：末字不贴边
  }
  const overflowing = dist > 8;
  el.classList.toggle("lyr-anim", overflowing);
  if (!overflowing) {
    stopLyricScroll(el);
    return;
  }
  el.style.setProperty("--lyr-end", `${-dist}px`);
  // 重启动画（内容变化后必须强制重启）；句内 65% 处完成滚动
  el.style.animation = "none";
  void el.offsetWidth;
  el.style.animation = `lyricScroll ${Math.max(2000, Math.round(durMs * 0.65))}ms linear 0.3s forwards`;
}

/**
 * 停止滚动并复位变换。切歌/纯音乐时必须显式调用：fill:forwards 的
 * lyricScroll 动画完成后会把 transform 钉在终点 --lyr-end 上，
 * 不复位的话新内容（如纯音乐颜文字）会被顶到卡片边缘。
 */
export function stopLyricScroll(el: HTMLElement): void {
  el.classList.remove("lyr-anim");
  el.style.animation = "none";
  el.style.setProperty("--lyr-end", "0px");
}
