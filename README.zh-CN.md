# suona 唢呐

一只桌面小宠物，把本机各个 coding agent 的运行情况**统一汇报**给你。

它以唢呐为形象：平时静静悬浮呼吸，有消息时喇叭口发光、吹出音符气泡，同时按需推送 macOS 系统通知。

```
        ╭──────────────────────────────╮
        │ Hermes            3 分钟前    │
        │ daily-stock-review：定时任务失败 │
        │ TimeoutError: idle for 976s   │
        ╰──────────────┬───────────────╯
                       ▽
                      (♪)
                       │        ← 唢呐宠物
                    ┌──┴──┐
                    │     │
                    ╲     ╱
                     ╲___╱
```

## 它监控什么

| Agent | 数据来源 | 提取的信息 |
|---|---|---|
| **Hermes** | `~/.hermes/cron/jobs.json`<br>`~/.hermes/cron/executions.db` | 定时任务的逐次运行结果：完成 / 失败 / 投递失败、耗时、超时等错误原文、执行中的 incident；以及任务总数、异常数、暂停数 |
| **Codex** | `~/.codex/sessions/**/rollout-*.jsonl`<br>`~/.codex/session_index.jsonl` | 会话标题、工作目录、对话轮数、CLI 版本、是否仍在进行中 |
| **Claude Code** | `~/.claude/projects/<proj>/<uuid>.jsonl` | AI 生成的会话标题、提问次数、输出 token 数、模型、subagent 数量 |

全部数据**只读**。suona 不写入任何 agent 的目录。

## 设计要点

**统一事件模型。** 每个采集器把各自的磁盘格式归一化成同一个 `AgentEvent`：

```rust
pub struct AgentEvent {
    id: String,            // 稳定去重键，同一次运行只播报一次
    agent: Agent,          // Hermes | Codex | ClaudeCode
    kind: EventKind,       // JobCompleted | JobFailed | SessionCompleted | ...
    severity: Severity,    // Info | Success | Warning | Error
    title: String,         // 气泡标题
    detail: String,        // 补充一行
    project: Option<String>,
    at: i64,
    meta: BTreeMap<String, String>,
}
```

**新闻 vs 状态。** 事件流只承载*发生的事*（任务跑完、会话结束、运行失败）；「还有 2 个任务被暂停」「下次 10:00 执行」这类*当前状态*放进汇总，只在悬停 / 点击时展示。否则宠物会每 20 秒重复念一遍同样的状态。

**不翻旧账。** 播报有一小时的时间窗，并且每次运行只播报一次（按 `id` 去重）。启动时先打招呼，不会把昨天的失败重放一遍。

**紧急优先。** 队列按 severity 排序，报错会插队；同时最多只留 6 条待播，避免积压刷屏。

**读别人的 SQLite 要小心。** SQLite 即使只读打开也会加锁、碰日志文件，直接打开 hermes 正在使用的库可能失败——而且这种失败常常**看起来打开成功、第一次查询才报错**。所以每个句柄都会先用一次真实查询探活，失败则退回到快照副本（连同 `-wal`/`-shm`）再读。

## 运行

```bash
pnpm install
pnpm tauri dev          # 开发模式
pnpm tauri build        # 打包 .app
```

安装到 `/Applications`（自启需要应用位于稳定路径）：

```bash
./scripts/install.sh
```

命令行自检（不需要开图形界面，直接打印统一事件流）：

```bash
cd src-tauri
cargo run -- --scan --days 7 | python3 -m json.tool
```

输出示例：

```
=== SUMMARIES ===
  Hermes       ok=3   fail=1   paused=2   | 4 个任务 · 1 个异常 · 2 个暂停
  Codex        ok=9   fail=0   paused=0   | 9 个会话 · 全部正常
  Claude Code  ok=5   fail=0   paused=0   | 5 个会话 · 全部正常

=== EVENTS ===
  warning hermes  daily-library-sync：任务完成但投递失败
  error   hermes  daily-stock-review：定时任务失败
          └ TimeoutError: Cron job 'daily-stock-review' idle for 976s
  success codex   迁移 MitoZ 注释与可视化到 Rust：会话已结束
```

## 交互

| 操作 | 效果 |
|---|---|
| 拖拽唢呐 | 移动位置，喇叭口始终朝向屏幕中央 |
| 拖到**空闲侧**的边沿 | 自动磁吸并藏起来，只露喇叭 |
| 停靠时点击 | 图标往外多探一点，列表在旁边展开；关掉即缩回 |
| 点击唢呐 | 展开完整运行记录 |
| 拖拽唢呐 | 只移动，**不会**弹出列表 |
| 点击别处 / `Esc` / 点面板内 × | 收起列表 |
| 点击列表中的某条 | 让唢呐念这一条（并播放对应音效） |
| **右键点击** | **弹出菜单 →「修改配置」/「退出程序」** |
| 今天 / 全部 | 列表时间范围，默认只显示当天 |
| S / M / L | 唢呐大小三档（默认 L） |
| 音效开关 | 汇报提示音，默认关闭 |
| 悬停 | 显示一行状态汇总 |

### 点击与拖拽的区分

拖拽结束时浏览器**同样会派发 `click`**，所以最初拖一下就会弹出列表。

不能用 `clientX/clientY` 区分：拖拽时窗口是跟着光标走的，光标**相对窗口**的位置几乎不变，前后差值近似为零。可靠的问题是"窗口本身动了没有"——而这正是几何线程已经在算的量（`travelled`，本来就是为防止点击抖动误触发磁吸而算的）。

所以判定放在后端：几何线程在拖拽过程中实时发布结论，前端在 `click` 里用 `take_was_drag()` 查询。两个细节：

- **读取是消费式的**（`swap`），否则一次拖拽留下的"真"会吞掉下一次真正的点击。
- **阈值由后端决定**，前端不接触缩放因子——阈值是 `4 × 屏幕缩放`，混在 DPI 环境下不该让前端去猜。

```
SUONA_CLICKTEST: still window  -> was_drag=false (want false)
SUONA_CLICKTEST: window moved  -> was_drag=true  (want true)
SUONA_CLICKTEST: second ask    -> was_drag=false (want false, consuming) PASS
```

### 拖动

唢呐身上任意位置都能拖动，包括喇叭、杆身和指孔。这需要 `data-tauri-drag-region="deep"`，不能省成裸属性：

Tauri 的拖拽判定会先过滤掉非 `HTMLElement` 的目标，而唢呐是 `<path>`/`<ellipse>` 画的，连 `<svg>` 一起全被跳过；走到 `.pet` 时若属性值为空，还要求「点击目标正好是 `.pet` 本身」。两者叠加的结果是——**只有图案两侧那条约 19px 的透明内边距能拖**，点在唢呐上纹丝不动。`deep` 表示整个子树都算拖拽区域，才对。

### 界面

窗口上没有常驻按钮，只在悬停时显示状态汇总：

```
┌─────────────────────────────────────┐
│  ┌───────────────────────────────┐  │
│  │ 运行汇报 今天全部  12 条    × │  │
│  ├───────────────────────────────┤  │   ← 点唢呐展开，
│  │ 自启  音效      S M L         │  │     面板自带 × 收起
│  ├───────────────────────────────┤  │
│  │ …                             │  │
│  └───────────────────────────────┘  │
│                                     │
│               (♪)                   │   ← 唢呐
│              ╲___╱                  │
└─────────────────────────────────────┘
```

退出走**右键菜单**（「退出程序」）。菜单在 Rust 侧用 `WebviewWindow::popup_menu` 构建，是**原生 macOS 菜单**而不是 HTML 画的：原生菜单能超出窗口边界，不会被 380px 的窗口裁掉，观感也和系统其他菜单一致。

右键不受拖拽区域影响——Tauri 的拖拽判定只处理 `e.button === 0`，右键会正常到达 DOM。

## 修改配置

右键 →「修改配置」，面板切换成设置页：

```
┌─────────────────────────────────────┐
│ 修改配置                         ×  │
├─────────────────────────────────────┤
│ 勾选要监控的 agent。取消勾选只是让   │
│ suona 停止汇报它，不会动它的任何数据  │
│ 文件。                              │
├─────────────────────────────────────┤
│ ☑ Hermes              已检测到       │
│   [~/.hermes                      ] │
│   找到 cron/jobs.json                │
│ ☐ Codex                 已停用       │
│   [~/.codex                       ] │
│   已停用，不再汇报                    │
│ ☑ Claude Code         目录不匹配     │
│   [/Volumes/backup/.claude        ] │
│   目录存在，但没有该工具的数据         │
│   恢复默认路径                        │
├─────────────────────────────────────┤
│ [返回汇报] [重新检测]  检测完成，无变化│
└─────────────────────────────────────┘
```

### 停用 ≠ 删除

suona 对 agent 的数据**严格只读**。「删除某种 agent 的消息」在 suona 里实现为**停用**：该 agent 不再被采集、不再出现在汇报里，磁盘上任何文件都不动，随时可以勾回来。

这是刻意的取舍——真去删 `~/.hermes`、`~/.codex` 里的记录，会破坏 agent 自己的工作数据，而 suona 只是个观察者，没资格改别人的档案。

### 探测逻辑

每个 agent 有一组「标志文件」，命中任一即算已安装：

| Agent | 标志 |
|---|---|
| Hermes | `cron/jobs.json`、`cron/executions.db`、`config.yaml` |
| Codex | `sessions`、`session_index.jsonl` |
| Claude Code | `projects` |

用**多个**标志是有意的：刚装好的工具可能还没写过任何会话文件，只认一个就会误判成「未安装」。

三种状态：**已检测到**（找到标志）、**目录不匹配**（目录在，但没有该工具的数据——通常是填错路径）、**未找到**（路径不存在）。

### 路径

默认 `~/.hermes`、`~/.codex`、`~/.claude`。可改成任意绝对路径，支持 `~` 写法（输入 `~/.claude` 自动展开，显示时又缩回 `~` 便于阅读）。

改动**在失焦或回车时提交**，不是每敲一个字符提交一次——路径只有写完整才有意义，而每次提交都会重扫全部 agent。填相对路径会被拒绝并标红，而不是默默接受一个拼错的值、然后永远显示「未找到」。

配置存在 `settings.json` 的 `agents` 段；路径留空表示「用默认」，这样配置在不同机器间可移植。改动在下一个轮询周期（≤20 秒）生效，无需重启；保存后前端也会立刻主动重扫一次，界面即时更新。

### 重新检测

设置页每次打开都会重新探测一遍，但页面可能一直开着——所以底部另有一个**「重新检测」**按钮，用来处理「页面开着的时候装了新软件」。

点完会说明**变化了什么**（如「Codex 已检测到」），而不是只闪一下。只提示「完成」的话，用户无法区分「没变化」和「按钮没生效」。检测本身没有缓存：`views()` 每次调用都真的去读磁盘，有一个单测专门守着这条——如果日后有人为了性能加上缓存，那个测试会失败。

检测同时也会重扫一次事件，因为刚装好的 agent 可能已经有数据值得列出来。

「今天 / 全部」只影响列表显示，是纯前端视图状态；默认「今天」，所以每次启动都是当天的视角。当天没有记录时会提示「今天暂无记录 · 切到「全部」看历史」，而不是干巴巴一句「暂无记录」。超过 60 条时计数显示成 `60/120 条`。

点击唢呐不会让窗口获得焦点（Tauri 的拖拽区域在 mousedown 上调了 `preventDefault()`），因此展开列表时会显式 `set_focus()`——否则后面永远等不到失焦，「点击别处收起」就会静默失效。

「点击别处收起」靠窗口失去焦点实现——窗口只看得见自己矩形内的点击，收不到「别处」的事件。代价是点击唢呐时 suona 会成为前台应用，你原来那个应用会失去焦点。如果不喜欢这个副作用，可以去掉自动收起，只保留 `Esc` / 面板内的 ×。

展开时窗口会**向上生长**并把原点同步上移，所以唢呐本身不会跳动；窗口原点同时被限制在屏幕可用区域内，列表不会钻到菜单栏底下。展开期间的窗口位置不会被记成「唢呐停靠点」。

面板里的「开机自启」开关直接反映系统 LoginAgent 的真实状态——启用失败时开关会回弹，不会显示一个假的成功态。

### 喇叭朝向

喇叭口永远指向屏幕中心。方向由窗口位置到屏幕中心的向量算出：

```
θ = atan2(-dx, dy)      // 局部 +Y 是喇叭指向；CSS rotate 把 (0,1) 映射到 (-sinθ, cosθ)
```

自由状态是任意角度，吸到边沿后取该边的法线方向——两者一致，所以吸附时不会突然扭转（有单测守着这条）。

### 磁吸与隐藏

![四个方向的磁吸效果](assets/preview/dock-simulation.png)

窗口贴着屏幕边沿，喇叭口朝内，杆身被窗口裁掉，只剩喇叭露在外面。

**吸附判定用的是窗口矩形，不是唢呐本身。** 唢呐位于 380px 宽窗口的正中，若按唢呐中心判定，窗口得往屏幕外拖 144px 才可能触发——实际上永远拖不到。这个 bug 是 `scripts/simulate_dock.py` 抓出来的。

**规则只有一条：避开程序坞那一侧，其余都能用。**

我在这里走过两次弯路，都是把规则收得比需要更紧：

1. 先假设"左右两侧总是安全"——错。程序坞可以放在左、右、下任意一侧，用户把它放在右侧时，右侧恰恰最不该吸。
2. 改成"只保留左右"——仍然错。程序坞在右侧时，这等于只剩左侧，而左侧往往拖不到，结果**任何一侧都吸不住**。

正确做法是问系统每条边被占了多少，然后**只排除真正被程序坞占住的那一条**：`NSScreen.frame` 是完整屏幕，`visibleFrame` 是扣掉菜单栏和程序坞之后的可用区域，两者之差就是每条边的占用量。

菜单栏那条边不排除，只是**避让**。菜单栏固定在顶边且永远在，一刀切禁用是过度收缩；浮层窗口只要下移菜单栏的高度就能正常显示。避让量用**实测值**，不是硬编码——我先前写死 28pt，而实测菜单栏是 34pt，唢呐就被压在了菜单栏下半部分。

AppKit 只能在主线程读，所以在启动时和**唢呐获得焦点时**采样并缓存——后者是个自然又便宜的时机，能顺带发现你刚挪动了程序坞。

实测（`defaults read com.apple.dock orientation` 独立佐证）：

```
screens: insets top=34 bottom=0 left=0 right=47 -> dockable top=true bottom=true left=true right=false
com.apple.dock orientation = right
```

`visibleFrame` 测出右侧被占 47pt，与程序坞设置吻合，于是只排除右侧；顶边靠 34pt 避让保留，底边因为程序坞不在那里而保留。

顺带一提：**开多少窗口不影响这个判断**——`visibleFrame` 是每块屏幕的属性，与窗口无关。

### 什么时候不该停靠

判据只有一条：**窗口是否真的横跨了两块屏幕**。

这里踩过一个代价很大的坑。我原先要求窗口**完整落在**某块屏幕内才允许停靠——本意是防混合 DPI 下的跨屏换算错误。但拖到屏幕边沿时窗口本来就会探出去一点，日志里留下了铁证：

```
evaluate: rect=(-426,68,760x1484) straddles displays — not docking
```

`x = -426`：窗口左缘已经在屏幕外。这被当成"横跨两块屏"拒绝了，于是**左边和底边永远吸不住**——因为把唢呐拖到边沿的正常动作，恰好就长这样。

"挂在一块屏的外沿"和"横跨两块屏"必须分开。现在只统计窗口与几块屏幕相交：超过一块才拒绝，一块（哪怕探出去）照常停靠。

### 停靠失败时不能改状态

另一个相关 bug：`set_dock` 会**先改状态、再移动窗口**，且忽略移动失败。于是出现状态说"贴在上边"、窗口却还停在屏幕中间的情况——前端按停在边沿的样式去渲染，看起来就是莫名其妙跳到了上方某个位置。

现在改成**先移窗口，成功了才提交状态**；移动被拒绝就保持原状并记日志。

### 多显示器安全

混合 DPI 的多屏环境踩过两个坑，都补上了。前提是内建 2x + 外接 1x、外接在 y 负方向。

**坑一：窗口跨屏时的缩放因子歧义。** 原先用 `current_monitor()` 取窗口**中心**所在的屏幕，但所有定位都以窗口**左上角**为锚点。跨屏时取到的可能是窗口只占一小部分的屏幕，于是每次逻辑↔物理换算都用错缩放因子。现在改为按左上角选屏。

**坑二：唢呐被停在没有屏幕覆盖的死区。** 这个更要命：`window.json` 存的是物理坐标，而同一数值在 1x 和 2x 屏的点空间里含义不同。一次 `set_position` 瞄准正确的位置，落地后可能差一倍——实测存的是 `(2706,1382)`，重启后变成 `(5412,2764)`，正好 2 倍。唢呐落在既超出内建屏右缘、又不在外接屏范围内的死区，看不见也拖不回来；**重启会忠实地恢复到同一位置**，所以重开没有用。

修法是三层，核心是**不再相信一次性校验**：

1. 恢复位置时校验**唢呐**（窗口底部中心）而不是窗口矩形——窗口和屏幕有重叠不代表唢呐可见。
2. 窗口跨屏时**不执行停靠**，宁可不动也不猜。
3. **每秒持续自愈**：只要唢呐不在任何屏幕内，就拉回主屏居中。

第 3 层是必需的，日志证明了这点——一次性校验通过后调用的「居中」自己又落错了地方，只有持续检查才抓住：

```
restore: rejecting (5412,2764) — pet would land at (5792,3264), outside every display. Recentring.
watchdog: pet at (2946,1348) is on no display — recentring
```

这些规则都有单测守着（`a_pet_between_two_displays_is_not_reachable` 构造两块不同缩放的屏，验证跨屏判定和死区判定）。

### 音效

用 Web Audio 合成，不带音频文件。四种严重级别各有动机，闭着眼也能分辨：

| 类型 | 音型 | 听感 |
|---|---|---|
| error | 下行两音（G4→C4） | 出事了 |
| warning | 同音重复两次（D5） | 需要看一眼 |
| success | 上行三和弦（C5-E5-G5） | 顺利完成 |
| info | 单音（E5） | 一般更新 |

默认关闭。开启时会在同一个手势里建立 `AudioContext`——浏览器不允许无手势播放音频，这也正是它默认关闭的原因。

### 大小三档

| 档位 | 缩放 | 唢呐 | 窗口 |
|---|---|---|---|
| L（默认） | 1.0 | 158×214 | 380×430 |
| M | 0.785 | 124×168 | 380×384 |
| S | 0.635 | 100×136 | 380×352 |

宽度三档一致，这样气泡始终放得下。

## 通知权限

首次触发警告/错误时，macOS 会询问通知权限。放行后，失败类事件才会弹出系统通知；成功类事件只在气泡里出现，不打扰你。

唢呐吸在边沿时没有气泡可用，此时消息只通过**发光 + 音效 + 系统通知**传达；点一下让它出来，期间攒下的重要事件会依次播报。

## 异常退出上报

三种失败方式，各自都能被看见：

| 失败方式 | 后果 | 上报途径 |
|---|---|---|
| 主线程 panic | 进程直接死 | 写入 `crash.log`，下次启动时通知你 |
| **后台线程 panic** | **进程活着，但该功能永久失效** | 守护线程在 3 秒内实时气泡 + 系统通知 |
| 被 kill / 断电 | 无机会清理 | 下次启动时通知你 |

中间那一行是最要命的：汇报线程或几何线程死掉之后，唢呐看上去一切正常，只是从此不再说话。没有守护线程的话，这种故障永远不会被发现。

实现上留两条记录：

- **`crash.log`** — 取证。panic 钩子记录线程名、位置、消息和调用栈（超过 256 KiB 轮转）。
- **`session.json`** — 一位状态位。启动时写 `clean: false`，正常退出时写 `clean: true`。下次启动若发现标志仍为 `false`，就说明上一次是死掉的。

两者都在：

```
~/Library/Application Support/dev.suona.pet/
```

正常退出**不会**误报——`RunEvent::Exit` 里会清掉标志。panic 钩子里用 `try_lock` 而非 `lock`：panic 可能发生在持锁时，钩子一旦阻塞就会把进程吊死在退出路上。

## 验证方式

几何不靠肉眼。`scripts/simulate_dock.py` 用真实 SVG 复刻 CSS 定位与裁剪，渲染四个方向的磁吸结果和多个位置的朝向到 `assets/preview/`；Rust 侧 24 个单测覆盖几何（`aim_at` / `dock_angle` / `edge_for`）、配置（路径展开、停用、探测）和异常退出：

```bash
python3 scripts/simulate_dock.py
cd src-tauri && cargo test --lib   # 14 个测试
```

另有一组环境变量钩子，用来验证进程内部、从外面看不见的行为（不设置就完全惰性）：

```bash
cd src-tauri

# 展开列表不会被自己的窗口移动误判成拖拽
SUONA_SELFTEST=1 ./target/debug/suona
# SUONA_SELFTEST: 3s later expanded=true dock=None (want expanded=true dock=None)

# 异常退出上报：后台 panic 实时上报，且进程存活
SUONA_CONFIG_DIR=/tmp/cfg SUONA_PANIC_AFTER_MS=3000 SUONA_QUIT_AFTER_MS=8000 ./target/debug/suona
# crash.log 含线程/位置/调用栈；退出时 session.json 为 clean:true（证明进程没被 panic 杀死）

# 点击与拖拽被正确区分
SUONA_CONFIG_DIR=/tmp/cfg4 SUONA_CLICKTEST=1 ./target/debug/suona
# SUONA_CLICKTEST: ... PASS

# 曾被拒绝的边沿位置现在能停靠了
SUONA_CONFIG_DIR=/tmp/cfg5 SUONA_EDGETEST=1 ./target/debug/suona
# SUONA_EDGETEST: -> Left (want Some(Left)) PASS

# 停靠 → 开列表 → 关列表，唢呐要回到原来的侧边
SUONA_CONFIG_DIR=/tmp/cfg6 SUONA_DOCKTEST=1 ./target/debug/suona
# SUONA_DOCKTEST: list closed -> Left (want Some(Left)) PASS

# 正常退出不会被误报为异常
SUONA_CONFIG_DIR=/tmp/cfg2 SUONA_QUIT_AFTER_MS=4000 ./target/debug/suona
# session.json → {"clean":true,...}
```

配置同样可以无界面验证——写一份 `settings.json` 再跑 `--scan`，看停用和自定义路径是否真的生效：

```bash
mkdir -p /tmp/cfg3 && cat > /tmp/cfg3/settings.json <<'JSON'
{ "agents": { "agents": { "codex": { "enabled": false, "path": "" } } } }
JSON
SUONA_CONFIG_DIR=/tmp/cfg3 ./target/debug/suona --scan --days 7
# Codex 汇总为「已停用」，且事件里 codex 数量为 0
```

`SUONA_CONFIG_DIR` 把配置目录整个挪走，便于测试而不动真实档案。

### 诊断日志

磁吸 / 展开状态机只对窗口几何做反应，从进程外面看不见。运行时会往

```
~/Library/Application Support/dev.suona.pet/events.log
```

追加展开、停靠、以及每次吸附判定的结果。列表再出现「自己收起来」时，先看这个文件。

## 结构

```
src-tauri/src/
  model.rs              统一事件模型
  app.rs                轮询、去重、事件推送、通知、窗口位置持久化、展开/自启
  collectors/
    mod.rs              共享工具（时间解析、JSONL 头尾读取、文件遍历）
    hermes.rs           jobs.json × executions.db 关联
    codex.rs            rollout 转录解析
    claude.rs           projects 转录解析
src/
  main.ts               气泡队列、播报策略、展开面板
  style.css             透明窗口、唢呐动画、面板样式
index.html              宠物标记 + 内联唢呐 SVG
assets/
  suona.svg             唢呐形象（前端与图标同源）
  icon.svg              macOS 应用图标
scripts/
  install.sh            构建并安装到 /Applications
```

## 已知取舍

- **Claude Code 没有「结束」记录。** 转录文件只是停止增长，所以「进行中」用「文件 5 分钟内被写过」来近似。
- **token 数取自转录尾部 1MB。** 超长会话的累计值会偏低，但避免了解析整个几 MB 的文件。
- **轮询间隔 20 秒。** 对本场景足够，且比文件监听（FSEvents）实现简单、跨工具可靠。
- **列表不分组。** 所有 agent 混在一条时间线里，靠行尾色块区分来源——比分组更省纵向空间。
