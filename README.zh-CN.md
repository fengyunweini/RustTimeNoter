<p align="center">
  <a href="README.md">English</a> &nbsp;|&nbsp;
  <a href="README.zh-CN.md">简体中文</a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/platform-Windows%2010%2B-blue?logo=windows" alt="平台：Windows 10+">
  <img src="https://img.shields.io/badge/协议-MIT-blue" alt="协议：MIT">
</p>

# RustTimeNoter

**超轻量 Windows 前台应用使用时长记录器。**

单个原生可执行文件。切换前台应用由事件驱动，定时采样负责检查离开状态和保存进度。
后台静默运行，加密存储每条应用使用记录，并可随时生成自包含 HTML 报表在浏览器中查看。

- **无运行时、无框架** — 纯 `windows-sys` 调 Win32 API。
- **静态加密** — AES-256-GCM，主密钥由 Windows DPAPI 封装。
- **紧凑存储** — 二进制定长记录 + 字符串字典池。
- **退出时确认保存** — 单实例保护、有限时间的退出等待、损坏日志原件保留。
- **系统托盘** — 右键打开报表、浏览数据目录、停止记录；双击 = 打开报表。

---

## 快速开始

从 [Releases](https://github.com/fengyunweini/RustTimeNoter/releases) 下载最新的
`RustTimeNoter-vX.Y.Z.zip`，解压到任意目录，双击 **`install.bat`**。

脚本会自动：

1. 复制 `tracker.exe` 到 `%LOCALAPPDATA%\RustTimeNoter\bin\`
2. 写入 `HKCU\Run` 自启动项（无需管理员，无 UAC 弹窗）
3. 在后台拉起 daemon
4. 用默认浏览器打开 HTML 报表

安装完成后 daemon 静默运行，每次登录自动启动。右键系统托盘图标可快速操作。

```
RustTimeNoter/
├── install.bat      ← 双击安装
├── view.bat         ← 双击查看报表
├── uninstall.bat    ← 双击卸载
├── tracker.exe      ← 单二进制（命令行入口）
└── README.txt       ← 简要说明
```

---

## 命令

| 命令 | 说明 |
|---|---|
| `tracker setup` | 一键安装：autostart + 启动 daemon + 打开 HTML 报表 |
| `tracker view [--days N]` | 生成最近 N 天的自包含 HTML 报表并用浏览器打开 |
| `tracker run` | 前台启动 daemon（开发 / 调试用） |
| `tracker stop` | 通过命名事件通知 daemon 优雅退出（flush 缓冲后关闭） |
| `tracker status` | 查看 daemon 运行状态、本地当日累计、数据目录大小 |
| `tracker tail [--interval 2]` | 实时跟随本地当日活动 |
| `tracker report [--today\|--week\|--month\|--from\|--to] [--by app\|category\|title] [--top N]` | 控制台报表 |
| `tracker export --format csv\|json [--out PATH]` | 导出原始记录 |
| `tracker config show\|init\|set <K> <V>\|get <K>` | 读写配置项 |

### 配置项 (`config.toml`)

| 键 | 默认值 | 含义 |
|---|---|---|
| `afk_minutes` | 5 | 无键鼠输入超过 N 分钟视为离开（AFK） |
| `capture_titles` | `false` | 是否记录窗口标题（默认关闭，隐私优先） |
| `flush_interval_secs` | 30 | 保存进度的间隔；事件排序还需保留最近 2 秒 |
| `flush_block_records` | 256 | 单个加密块最大记录数，最高 4096 |
| `idle_tick_secs` | 30 | AFK 检测心跳间隔 |
| `title_max_chars` | 256 | 标题截断长度 |
| `title_blacklist` | `[]` | 不记录标题的 exe basename 列表 |

示例：`tracker config set capture_titles true`

---

## 安装 / 卸载（命令行）

**HKCU 自启动**（用户级，无需管理员）：

```powershell
tracker install autostart
tracker uninstall autostart
```

二进制复制到 `%LOCALAPPDATA%`，注册表写入 `HKCU\Run`。无法读取管理员权限进程的完整路径（降级为 basename）。

**Windows 服务**（机器级，需管理员 PowerShell）：

```powershell
tracker install service
Start-Service RustTimeNoter

tracker uninstall service
```

以 `LocalSystem` 运行，可读取所有进程信息，登录前即启动。
*注意：LocalSystem 运行在 Session 0，无法看到用户前台窗口——日常使用请用 autostart 路径。*

---

## 资源占用

2026-09-07 在 Ryzen 9 7945HX、Windows build 26200、Rust 1.97.1 上测量，x64 release + LTO。
真实桌面与托盘，每组预热 5 秒、测量 64 秒，默认和标题配置各重复两轮，没有修剪工作集。
这些是本机样本，不是所有机器的资源保证。详见[最终复查与配对结果](docs/pr-review.md)、
[9 月 5 日第二轮复查](docs/performance-review.md)及[第一轮优化](docs/performance.md)。

| 指标 | 最终复查构建 |
|---|---|
| 程序体积 | 1,057,280 B（约 1.008 MiB），比上一快照少 512 B |
| 默认设置总工作集，各轮中位数 | 12.64–12.86 MiB，包含共享驻留页面 |
| 默认设置私有提交内存，各轮中位数 | 约 2.09 MiB |
| 默认设置 64 秒累计 CPU 时间 | 读数为 0 ms；接近计量分辨率，不代表零工作 |
| 10 万条普通切窗观测的记账回放 | 24.57 ms，配对上一快照为 24.02 ms |
| 4000 个唯一标题回放，确认落盘 | 290.9 ms，上一快照为 283.6 ms；本轮没有进一步写盘提速 |
| 上述回放完成后的 Rust 存活堆 | 948,406 B，保留此前减少分配的收益 |

私有提交内存与总工作集是不同指标，旧版“2–3 MB 工作集”的近似值没有在本机复现。
回放提速比例不等于整个进程的 CPU 降幅。本轮开启标题时，两轮配对 CPU cycles 高约 5.3% 和 7.3%；
64 秒 CPU 时间为 390.625–500 ms，约占单个逻辑核的 0.61%–0.78%，不能宣称整体 CPU 改善。
新增启动同步和布局检查，在 65,536 条记录样本上增加约 4.71 ms。以上报告保留历史基线、测量汇总及这些成本。

---

## 数据布局

`%LOCALAPPDATA%\RustTimeNoter\`（user 模式）或 `%PROGRAMDATA%\RustTimeNoter\`（service）：

```
config.toml            配置
rules.toml             分类规则（可选）
key.bin                由 DPAPI 包裹的 AES-256 主密钥
apps.dict              字符串池：exe 路径
titles.dict            字符串池：窗口标题
data\YYYY\MM\YYYY-MM-DD.log   按 UTC 日分片的加密日志
data\YYYY\MM\YYYY-MM-DD.part-000001.log   可恢复损坏后的续写文件
bin\tracker.exe        autostart 模式下的二进制副本
```

### 时区语义

- format v1 日志继续按 UTC 日分片，不复制本地日文件，也无需迁移现有活动记录。
- 所有面向用户的日期和时间默认使用当前系统时区；查询时按本地日历边界扫描并裁切所需 UTC 分片。
- 导出新增 `start_timestamp`，使用带明确数字 offset 的 RFC 3339 时间戳。
- 遇到夏令时切换，本地自然日可能是 23 或 25 小时。
- 修改系统时区后，历史记录会在查询时按新时区重新归日；加密源数据不会重写。

### 文件格式

- **`.dict`** — magic `RTND` + version + 连续 `[u32 id][u32 len][bytes]`。append-only，ID 0 保留。
- **`.log`** — format v1，magic `RTNL` + UTC `date_packed` + 连续加密 block。
  每个 block：`[u32 plain_len][12 B nonce][ciphertext + tag]`。
  AAD = `magic ‖ date_packed ‖ block_index`。
  Plaintext = N × 17 字节定长 record（`u32 start_offset ‖ u32 duration ‖ u32 app_id ‖ u32 title_id ‖ u8 flags`）。
- 每条 17 字节，另有加密和字典开销；定期保存进度也会产生记录。
- `flags` 的 `0x80` 位配合字典 ID 0 表示记录缺口。查询时先合并缺口，再从活动中扣除重叠时间，
  包括发现迟到事件之前已经保存的活动。

### 计时与恢复的处理原则

- **最近两秒先留着调整。** 切窗、输入快照、锁屏、休眠和保存进度都按采集时间排队处理。
  回调队列有固定容量，普通回调不会等待磁盘。
- **是否离开只看真实键鼠输入。** 切窗、标题变化不算输入。离开后回来，或前台应用一度无法识别，
  都从重新确认状态的时刻开始计时，不把没有观察到的时间补给某个应用。
- **无法确认就显示缺口。** 事件迟到超过可调整范围、队列溢出、前台无法识别时，明确记录不确定区间。
  已知的离开、锁屏和休眠不计入使用时长，也不会因此被称为缺失。
  缺口向外取整到秒，边界可能保守地少计一秒。
- **各个查询入口使用同一口径。** 报表、status、tail 和导出都会提示记录不完整。
  CSV/JSON 在原字段后追加 `record_type`，值为 `activity` 或 `gap`；缺口时长不算应用使用时长。
- **一直使用同一应用也会定期保存。** 调度和磁盘正常时，尚未保存的尾段约为
  `min(idle_tick_secs, flush_interval_secs) + 2` 秒；系统卡住或磁盘出错时不能保证这个期限。
  强制结束进程仍可能丢失这段尾部。
- **重启后重新确认。** 程序未运行期间的时间不会补给上次使用的应用。
- **损坏日志保留原件。** 能验证有效前缀、但后面损坏时，在编号续写文件中继续记录；查询同时读取各部分并提示损坏。
  首块无法验证、文件头损坏、密钥错误，或已有日志却缺少密钥/字典时，明确报错，不自动替换数据。
- **字典修复先备份。** 只有未写完的尾部可以在完整备份并同步到 `.recovery-NNNNNN.bak` 后修复；
  字典结构错误会停止启动。保存日志前先确保它引用的字典已经保存。
- **旧编号不可重用。** 启动先流式核验所有历史日志引用；已引用字典项丢失时保留原件并报错。
  这会增加启动读取量；任意旧日的坏文件头或无法认证的首块也会阻止启动。
  已有密钥和字典需要写权限以完成启动同步；数据目录或日志若使用链接、目录联接等 reparse 布局，
  会明确报错，避免静默漏扫历史引用。
- **旧数据可读，旧程序不理解新标记。** 出现缺口或续写文件后，应使用本版本查询；备份时保留完整数据目录。

### 加密

- AES-256-GCM，主密钥由 Windows DPAPI 封装后存入 `key.bin`。
- user 模式：`CRYPTPROTECT_UI_FORBIDDEN`，仅当前用户可解密。
- service 模式：`CRYPTPROTECT_LOCAL_MACHINE`，本机任意账户可解密（含 `LocalSystem`）。

---

## 隐私与安全

- **默认不记录窗口标题**（很多场景标题含敏感信息）。如需记录，显式执行 `tracker config set capture_titles true`。
- 所有日志文件静态加密。离线复制走后无法读取（除非能解本机 DPAPI）。
- 标题黑名单：`tracker config set title_blacklist Code.exe,1Password.exe`

---

## 边界处理

| 场景 | 处理方式 |
|---|---|
| AFK / 离开 | `GetLastInputInfo` — 当前时段 cap 到 `last_input + threshold` |
| 锁屏 | `WM_WTSSESSION_CHANGE` (`WTS_SESSION_LOCK` / `UNLOCK`) — 锁屏期间不计时 |
| 休眠 / 睡眠 | `WM_POWERBROADCAST` (`PBT_APMSUSPEND` / `PBT_APMRESUMEAUTOMATIC`) |
| UWP 应用 | 前台为 `ApplicationFrameHost.exe` 时遍历子窗口查找真实宿主 PID |
| 优雅停机 | `tracker stop`（命名事件）/ Ctrl+C / SCM 停止 / 控制台关闭 / 注销 / 关机 → 全部 flush 后退出 |
| 单实例 | 命名 mutex `Global\RustTimeNoter.Daemon` — 重复运行立刻退出 |
| 系统托盘 | 右键：打开报表 / 浏览数据目录 / 停止记录。双击 = 打开报表 |
| 崩溃恢复 | 保留损坏原件，读取可验证前缀及编号续写文件，并提示数据不完整 |

---

## 构建

需要 Rust 1.91+ 和 Windows 10 / 11。

```powershell
cargo build --release   # → target\release\tracker.exe
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

---

## 已知限制

- **仅支持 Windows。** `run` 子命令在 Linux/macOS 下会报错；`report`、`export`、`config`、
  `view` 子命令跨平台可用，可用于在其他机器上分析备份数据。
- **无 GUI。** 交互方式为 CLI、系统托盘（右键菜单）、或浏览器 HTML 报表。
- **Service 模式文件属主为 `LocalSystem`。** 普通用户需先 `tracker stop`、调整 ACL 才能读取，
  或直接使用 user 模式的 `autostart`。

---

## 协议

MIT — 详见 [LICENSE](LICENSE)。
