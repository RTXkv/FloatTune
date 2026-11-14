// 播放位置估算时钟：
// SMTC 不上报网易云的播放进度，所以本地自走时钟：
// 切歌归零 → Playing 时累加 → Paused 时冻结。
// 若未来 SMTC 时间线有效则直接采用真实进度。

// 状态检测存在延迟（轮询 ~0.5-0.8s），用半个周期做系统性补偿
const DETECT_LAG_MS = 150;

export class PositionClock {
  private anchorWall = 0;
  private anchorPos = 0;
  private playing = false;
  private trackKey = "";

  /** 歌曲变化时归零：切歌是检测到才知道的，所以起点前移一个检测延迟 */
  syncTrack(title: string) {
    if (this.trackKey !== title) {
      this.trackKey = title;
      this.reset(-DETECT_LAG_MS, this.playing);
    }
  }

  reset(posMs: number, playing: boolean) {
    this.anchorPos = posMs;
    this.anchorWall = Date.now();
    this.playing = playing;
  }

  setPlaying(p: boolean) {
    if (p === this.playing) return;
    const cur = this.nowMs();
    if (p) {
      // 恢复播放比检测到更早开始 → 锚点前移
      this.anchorPos = cur;
      this.anchorWall = Date.now() - DETECT_LAG_MS;
    } else {
      // 暂停实际发生得比检测到更早 → 回退
      this.anchorPos = cur - DETECT_LAG_MS;
      this.anchorWall = Date.now();
    }
    this.playing = p;
  }

  /** 校准（用户手动纠偏） */
  calibrate(posMs: number) {
    this.reset(posMs, this.playing);
  }

  nowMs(): number {
    if (!this.playing) return this.anchorPos;
    return this.anchorPos + (Date.now() - this.anchorWall);
  }
}

export function fmtTime(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  const m = Math.floor(s / 60);
  return `${m}:${String(s % 60).padStart(2, "0")}`;
}
