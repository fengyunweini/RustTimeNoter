<p align="center">
  <a href="README.md">English</a> &nbsp;|&nbsp;
  <a href="README.zh-CN.md">简体中文</a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/platform-Windows%2010%2B-blue?logo=windows" alt="平台：Windows 10+">
  <img src="https://img.shields.io/badge/二进制-~959%20KB-brightgreen" alt="二进制：~959 KB">
  <img src="https://img.shields.io/badge/内存-~2%20MB-brightgreen" alt="内存：~2 MB">
  <img src="https://img.shields.io/badge/协议-MIT-blue" alt="协议：MIT">
</p>

# RustTimeNoter

**超轻量 Windows 前台应用使用时长记录器。**

单个二进制 ~959 KB，常驻内存 ~2 MB，事件驱动、零轮询、CPU 基本为 0%。
后台静默运行，加密存储每条应用使用记录，并可随时生成自包含 HTML 报表在浏览器中查看。

- **无运行时、无框架** — 纯 `windows-sys` 调 Win32 API。
- **静态加密** — AES-256-GCM，主密钥由 Windows DPAPI 封装。
- **10 年数据预算 ≤ 1 GB** — 二进制定长记录 + 字符串字典池。实测预估 ~140 MB / 10 年。
- **多重退出路径** — 5 条 shutdown 路径（命名事件 / Ctrl+C / SCM 停止 / 注销 / 关机）。
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
| `tracker status` | 查看 daemon 运行状态、当日累计、数据目录大小 |
| `tracker tail [--interval 2]` | 实时跟随当日日志输出 |
| `tracker report [--today\|--week\|--month\|--from\|--to] [--by app\|category\|title] [--top N]` | 控制台报表 |
| `tracker export --format csv\|json [--out PATH]` | 导出原始记录 |
| `tracker config show\|init\|set <K> <V>\|get <K>` | 读写配置项 |

### 配置项 (`config.toml`)

| 键 | 默认值 | 含义 |
|---|---|---|
| `afk_minutes` | 5 | 无键鼠输入超过 N 分钟视为离开（AFK） |
| `capture_titles` | `false` | 是否记录窗口标题（默认关闭，隐私优先） |
| `flush_interval_secs` | 30 | 落盘间隔 |
| `flush_block_records` | 256 | 单个加密块最大记录数 |
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

| 指标 | 值 |
|---|---|
| 二进制大小 | ~959 KB（release，stripped，LTO） |
| Zip 安装包 | ~452 KB |
| 工作集（idle） | ~2–3 MB |
| CPU（idle） | ~0.0% |
| 每日数据量 | ~10–100 KB（取决于切窗频率） |

---

## 数据布局

`%LOCALAPPDATA%\RustTimeNoter\`（user 模式）或 `%PROGRAMDATA%\RustTimeNoter\`（service）：

```
config.toml            配置
rules.toml             分类规则（可选）
key.bin                由 DPAPI 包裹的 AES-256 主密钥
apps.dict              字符串池：exe 路径
titles.dict            字符串池：窗口标题
data\YYYY\MM\YYYY-MM-DD.log   每日加密日志
bin\tracker.exe        autostart 模式下的二进制副本
```

### 文件格式

- **`.dict`** — magic `RTND` + version + 连续 `[u32 len][bytes]`。append-only，ID 0 保留。
- **`.log`** — magic `RTNL` + version + `date_packed` + 连续加密 block。
  每个 block：`[u32 plain_len][12 B nonce][ciphertext + tag]`。
  AAD = `magic ‖ date_packed ‖ block_index`。
  Plaintext = N × 17 字节定长 record（`u32 start_offset ‖ u32 duration ‖ u32 app_id ‖ u32 title_id ‖ u8 flags`）。
- 每条 17 字节。日切换 5000 次 ≈ 85 KB。10 年 ≈ 304 MB（数据本体 ~140 MB + 字典开销）。

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
| 崩溃恢复 | 日志 reader 在解密失败或截断处停下（丢弃未完成 block），不会 panic |

---

## 构建

需要 Rust 1.91+ 和 Windows 10 / 11。

```powershell
cargo build --release   # → target\release\tracker.exe
cargo test              # 20 个单元测试
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
