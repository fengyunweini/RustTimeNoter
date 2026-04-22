# RustTimeNoter (`tracker`)

> Windows 独占的极低占用前台应用使用时长记录器。
> 单二进制 ~881 KB，常驻内存 ~2 MB，CPU 99% 时间为 0%。
> 数据本地加密存储，10 年预算 1 GB，10 年实际预估 ~140 MB（未压缩）。

---

## 设计目标

来自 `启动须知.md`：

1. **极低资源占用** — 比多数系统服务更小，零轮询，事件驱动。
2. **长期稳定后台运行** — 优雅停机、单实例锁、崩溃日志。
3. **Windows 独占** — 直接用 `windows-sys` 调 Win32 API，不带运行时。
4. **存储 10 年内 ≤ 1 GB** — 二进制定长记录 + 字符串字典池 + AES-GCM 加密块。
5. **正确处理边界** — 锁屏 / 休眠 / AFK / UWP `ApplicationFrameHost.exe` / 进程权限。

---

## 安装

### 方案 A：HKCU 自启动（普通权限，推荐日常使用）

```powershell
.\tracker.exe install autostart
```

会把自身复制到 `%LOCALAPPDATA%\RustTimeNoter\bin\tracker.exe`，并在
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 写入启动项。
登录后由 explorer 拉起，无 UAC 弹窗。

缺点：无法读取以管理员权限运行的进程的完整路径（会降级为 PID/进程名）。

### 方案 B：Windows 服务（需管理员）

在**管理员 PowerShell** 中：

```powershell
.\tracker.exe install service
Start-Service RustTimeNoter
```

服务以 `LocalSystem` 运行，可读取所有进程信息，登录前即开始记录。

### 卸载

```powershell
.\tracker.exe uninstall autostart   # 或 service
```

---

## 命令速查

| 命令 | 说明 |
|---|---|
| `tracker` 或 `tracker run` | 前台启动 daemon（dev/调试用） |
| `tracker stop` | 通过命名事件通知 daemon 优雅退出（flush 缓冲） |
| `tracker status` | 查看 daemon 是否在跑、当日累计、数据目录大小 |
| `tracker tail [--interval 2] [--once] [--history 10]` | 实时跟随当日日志 |
| `tracker report [--today\|--yesterday\|--week\|--month\|--from --to] [--by app\|category\|title] [--top N]` | 报表 |
| `tracker export --format csv\|json [--out PATH] ...` | 导出明细 |
| `tracker config show \| init \| path \| set <K> <V> \| get <K>` | 配置 |
| `tracker install autostart\|service` | 安装 |
| `tracker uninstall autostart\|service` | 卸载 |

### 配置项

| key | 默认 | 含义 |
|---|---|---|
| `afk_minutes` | 5 | 无键鼠输入超过 N 分钟视为离开 |
| `capture_titles` | false | 是否采集窗口标题（隐私敏感，默认关） |
| `flush_interval_secs` | 30 | 落盘批处理间隔 |
| `flush_block_records` | 256 | 单加密块最大记录数 |
| `idle_tick_secs` | 30 | AFK 检测心跳周期 |
| `title_max_chars` | 256 | 标题截断长度 |
| `title_blacklist` | `[]` | 这些 exe basename 的标题不记录（逗号分隔） |

示例：`tracker config set capture_titles true`

---

## 数据布局

`%LOCALAPPDATA%\RustTimeNoter\`（user 模式）或 `%PROGRAMDATA%\RustTimeNoter\`（service 模式）：

```
config.toml          # 配置
rules.toml           # 分类规则（可选）
key.bin              # DPAPI 包裹的 32 字节 AES-256 主密钥
crash.log            # 崩溃日志
apps.dict            # 字符串池：exe path
titles.dict          # 字符串池：window title
data\YYYY\MM\YYYY-MM-DD.log   # 每日加密日志
bin\tracker.exe      # autostart 模式的副本
```

### 文件格式

- **`.dict`**：magic `RTND` + version + 一串 `[u32 len][bytes]`。append-only，ID 0 保留。
- **`.log`**：magic `RTNL` + version + `date_packed` + 一串加密 block。
  每 block = `[u32 plain_len][12B nonce][ciphertext+tag]`，AAD 绑定 `magic||date||block_index`。
  Plaintext = N × 17 字节定长 record（`u32 start_offset|u32 dur|u32 app_id|u32 title_id|u8 flags`）。
- 单条记录 17 字节；高频用户每天 5000 条 ≈ 85 KB；10 年 ≈ 304 MB（含字典开销 ~140 MB 数据本体）。

### 加密

- AES-256-GCM，主密钥用 Windows DPAPI 包裹后落 `key.bin`：
  - user 模式：`CRYPTPROTECT_UI_FORBIDDEN`，仅当前用户可解
  - service 模式：`CRYPTPROTECT_LOCAL_MACHINE`，本机任何账户可解（含 `LocalSystem`）

---

## 隐私与安全

- 默认**不**采集窗口标题（很多场景标题含敏感信息）。需要时显式 `config set capture_titles true`。
- 所有日志文件都是加密的，离线复制走也读不出（除了能解 DPAPI 的本机用户/系统）。
- 标题黑名单：`config set title_blacklist Code.exe,1Password.exe`。

---

## 边界处理（已实现）

- ✅ AFK 截断：`GetLastInputInfo` + 配置阈值，超时段 cap 到 `last_input + threshold`
- ✅ 锁屏：监听 `WM_WTSSESSION_CHANGE` (`WTS_SESSION_LOCK`/`UNLOCK`)，suppress 期间不计时
- ✅ 休眠：`WM_POWERBROADCAST` (`PBT_APMSUSPEND`/`PBT_APMRESUMEAUTOMATIC`)
- ✅ UWP：检测到前台为 `ApplicationFrameHost.exe` 时遍历子窗口找 PID 不同的真实承载进程
- ✅ 优雅停机：`tracker stop`（命名事件）/ Ctrl+C / SCM Stop / 控制台关闭 → flush 后退出
- ✅ 单实例：命名 mutex `Global\RustTimeNoter.Daemon`
- ✅ 崩溃恢复：日志 reader 在解密失败/截断处停下，后续 block 丢弃，不 panic

---

## 资源占用（实测）

| 指标 | 值 |
|---|---|
| 二进制大小 | ~881 KB（release，stripped，LTO，lexopt 取代 clap） |
| 工作集（idle） | ~2-3 MB |
| CPU（idle） | ~0.0% |
| 每日数据写入 | ~10-100 KB（取决于切窗频率） |

---

## 构建

```powershell
cargo build --release
# → target\release\tracker.exe
cargo test
```

需要 Rust 1.91+ 与 Windows 10/11。

---

## 已知限制

- Windows 独占。Linux/macOS `tracker run` 会报错；CLI 子命令（report/export/config）跨平台可用（用于离线分析备份数据）。
- 没有 GUI。所有交互走 CLI。
- service 模式下日志文件归 `LocalSystem`，普通用户读取需先 `tracker stop` → 调整 ACL，或干脆用 user 模式。
- 体积优化暂停在 881 KB；继续压到 < 500 KB 需要换掉 `aes-gcm`（手撸 GCM）以及精简 `windows-service`，权衡见 commit 历史。
