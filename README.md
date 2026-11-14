# FloatTune 浮音

轻巧不碍眼的网易云音乐伴生桌面小工具。播放留在官方客户端（播放记录/播放量不受影响），浮窗负责展示与控制。

## 功能

- **两种形态**（托盘右键随时切换）：
  - 「极简浮窗」一行歌词 + 控制，最不碍眼
  - 「全览浮窗」小封面 + 歌名/歌词/进度/控制
- **歌词四档**：关闭 / 当前一句 / 多行渐隐 / **歌词滚动**（单行横向：逐字歌跟字居中，无逐字按句时长匀速滚动）/ **逐字卡拉OK**（yrc 字级点亮）；超长句自动滚动，无歌词时随机颜文字
- **实时性**（依赖 InfLink 插件，见下）：
  - 暂停/播放 ~1 秒响应，渐隐/渐显动画（「暂停后延迟消失」0-30s 可调）
  - **真实播放进度**（InfLink 每秒推送，歌词/卡拉OK绝对同步）
- **不碍眼机制**：鼠标离开自动变淡、锁定点击穿透（托盘解除）、分区穿透（浮窗内非功能区直接点到背后应用）、全屏避让（可关）
- **控制**：切歌/暂停（SMTC 优先、媒体键兜底）、进度显示、位置/大小记忆（边缘拖拽调大小、按住封面拖动）

## 关键架构决策

### InfLink 注入路线
网易云音乐客户端**不向系统上报播放进度、暂停状态上报延迟 11-21 秒**——这是外部工具的物理天花板（实测穷尽：SMTC 时间线恒空、本地端口无接口、CEF 界面无法读取、音频会话状态暂停方向有 ~20s OS 迟滞）。因此本应用设计为**配合 [InfLink-rs](https://github.com/apoint123/inflink-rs) 插件工作**（经 BetterNCM 注入网易云内部，提供即时状态+厘秒级进度+歌曲ID）。FloatTune 自动检测 InfLink：在位则用即时信号与真实进度；不在位则回退音频会话推断（恢复 ~1.2s / 暂停 ~20s）与本地时钟估算。

配套的 **FloatTuneFix** 插件（本仓库 `thirdparty/ftfix/`）负责隐藏 BetterNCM/InfLink 注入的界面元素，还原网易云原生外观。

### 隐藏 = 两段式溶解
WebView2 透明窗口的三种隐藏路径都会**永久冻结渲染面**（恢复后空白）：
1. hide()/show() 循环 → 合成面冻结
2. 移出屏幕驻留 → 遮挡判定挂起
3. 页面 opacity 触底 → alpha 锁存（WebView2 157）

最终方案：**两段式溶解**——先页面淡出到 opacity 0.004（视觉不可见但 alpha 不触底）并开启点击穿透，480ms 淡出动画结束后窗口 hide()（止损低 alpha 常驻的合成开销与偶发残影）；恢复 = show + 尺寸抖动强制重绘 + 直接注入 opacity='' 淡回。所有延迟任务（隐藏延迟/溶解 hide 定时器/恢复淡入）持有 generation 代数，状态翻转时旧任务整体失效，杜绝快速暂停-恢复/连续切歌时的竞态错乱。

### 暂停检测（无 InfLink 时的回退）
网易云暂停时 SMTC 仍报 playing（实现缺陷），用**音频会话静音**交叉验证；媒体会话（第一个网易云会话）约 1.2s 翻转，辅助会话有 ~10s OS 空闲迟滞——只读第一个。切歌间隙 4s 宽限防误判（原生 paused 上报不受宽限影响）。

## 代码结构

```
Tauri 2 + WebView2 + 原生 TS 前端（无框架）
src-tauri/src/
  lib.rs        入口、状态、Tauri 命令、窗口事件、分区穿透线程
  visibility.rs 可见性决策（两段式溶解 + 延迟渐隐 + generation 竞态防护）
  smtc.rs       快慢分离采样（快:窗口标题+会话状态 / 慢:SMTC 元数据+封面单飞线程）
  drag.rs       自绘窗口拖动（绕开系统拖放，不触发 Win11 分屏提示）
  volume.rs     音频会话状态检测（暂停推断信号源）
  win32.rs      任务栏矩形、全屏检测、置顶维护、媒体键、窗口标题探测
  netease.rs    公开接口：搜索→id→歌词(LRC+yrc)→封面，双层本地缓存
  modes.rs      两种形态的布局与几何记忆（污染位置校验）
  tray.rs       托盘菜单
  settings.rs   设置持久化
src/            前端：store(状态总线) clock(进度时钟) lyrics(解析) karaoke(逐字)
                marquee(跑马灯) ui/(各窗口)
thirdparty/     BetterNCM 安装器、InfLink 插件、FloatTuneFix 源码
```

## 数据来源与限制

- 歌词/封面：网易公开接口（免登录，不碰账号），缓存于 `%APPDATA%/com.floatune.app/`
- 进度：InfLink 在位 = 真实；否则本地时钟估算（客户端内拖进度会漂移，滚轮微调 ±0.5s，切歌自动校准）
- 逐字卡拉OK跟字精度 = 进度精度；歌词匹配在 InfLink 在位时可用歌曲 ID 精确化（待接入）

## 开发 / 构建

```bash
npm install
npm run app:dev    # 开发运行（需 Rust + VS Build Tools）
npm run app:build  # 产出 NSIS 安装包
```

## 依赖的外部组件

- [BetterNCM](https://github.com/std-microblock/BetterNCM-Installer) + [InfLink-rs](https://github.com/apoint123/inflink-rs)：可选但强推荐（即时状态 + 真实进度）
- 已测试网易云 3.1.43 + InfLink v3.4.0
