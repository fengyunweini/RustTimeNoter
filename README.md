<p align="center">
  <a href="README.md">English</a> &nbsp;|&nbsp;
  <a href="README.zh-CN.md">简体中文</a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/platform-Windows%2010%2B-blue?logo=windows" alt="Platform: Windows 10+">
  <img src="https://img.shields.io/badge/binary-~959%20KB-brightgreen" alt="Binary: ~959 KB">
  <img src="https://img.shields.io/badge/memory-~2%20MB-brightgreen" alt="Memory: ~2 MB">
  <img src="https://img.shields.io/badge/license-MIT-blue" alt="License: MIT">
</p>

# RustTimeNoter

**Ultra-light Windows foreground-app usage tracker.**

A single ~959 KB binary. ~2 MB working set. Event-driven — zero polling, near-zero CPU.
Runs in the background, records which apps you use (and for how long),
and renders a self-contained HTML report in your browser.

- **No runtime, no framework** — raw Win32 via `windows-sys`.
- **Encrypted at rest** — AES-256-GCM, key sealed with Windows DPAPI.
- **10-year storage budget: 1 GB** — binary fixed-length records + string-dict pool.
  Real-world estimate: ~140 MB over 10 years (uncompressed).
- **Graceful everywhere** — 5 shutdown paths, single-instance lock, crash recovery.
- **System tray** — right-click to open the report, browse the data folder, or stop tracking.

---

## Quick Start

[Download the latest `RustTimeNoter-vX.Y.Z.zip`](https://github.com/fengyunweini/RustTimeNoter/releases),
unzip anywhere, and double-click **`install.bat`**.

What it does:

1. Copies `tracker.exe` into `%LOCALAPPDATA%\RustTimeNoter\bin\`
2. Registers an `HKCU\Run` autostart entry (no admin, no UAC prompt)
3. Launches the daemon immediately
4. Opens the HTML report in your default browser

After installation the daemon runs silently in the background and auto-starts on
every logon. Right-click the tray icon for quick actions.

```
RustTimeNoter/
├── install.bat      ← Double-click to install
├── view.bat         ← Double-click to open the HTML report
├── uninstall.bat    ← Double-click to remove
├── tracker.exe      ← Single binary (CLI entry-point)
└── README.txt       ← Quick reference
```

---

## Commands

| Command | Description |
|---|---|
| `tracker setup` | One-shot install: autostart + launch daemon + open report |
| `tracker view [--days N]` | Generate an HTML report and open it in the browser |
| `tracker run` | Start the daemon in the foreground (dev / debugging) |
| `tracker stop` | Graceful shutdown via named event (flushes buffers) |
| `tracker status` | Daemon status, today's totals, data directory size |
| `tracker tail [--interval 2]` | Follow today's log in real time |
| `tracker report [--today\|--week\|--month\|--from\|--to] [--by app\|category\|title] [--top N]` | Console report |
| `tracker export --format csv\|json [--out PATH]` | Export raw records |
| `tracker config show\|init\|set <K> <V>\|get <K>` | Read/write configuration |

### Configuration (`config.toml`)

| Key | Default | Meaning |
|---|---|---|
| `afk_minutes` | 5 | Idle threshold — no keyboard/mouse input for N minutes is considered away |
| `capture_titles` | `false` | Record window titles (off by default, privacy-first) |
| `flush_interval_secs` | 30 | Write-back interval |
| `flush_block_records` | 256 | Max records per encryption block |
| `idle_tick_secs` | 30 | How often the AFK check fires |
| `title_max_chars` | 256 | Title truncation length |
| `title_blacklist` | `[]` | Exe basenames whose titles are never recorded |

Example: `tracker config set capture_titles true`

---

## Install / Uninstall (CLI)

**Autostart** (user scope, no admin):

```powershell
tracker install autostart
tracker uninstall autostart
```

Copies the binary to `%LOCALAPPDATA%` and writes `HKCU\Run`. Cannot read process
paths of elevated (admin) processes — falls back to basename.

**Windows Service** (machine scope, requires admin PowerShell):

```powershell
tracker install service
Start-Service RustTimeNoter

tracker uninstall service
```

Runs as `LocalSystem`, can read all process info, starts before user logon.
*Caveat: LocalSystem runs in Session 0 and cannot see user foreground windows
— use the autostart path for daily tracking.*

---

## Resource Footprint

| Metric | Value |
|---|---|
| Binary size | ~959 KB (release, stripped, LTO) |
| Zip installer | ~452 KB |
| Working set (idle) | ~2–3 MB |
| CPU (idle) | ~0.0% |
| Daily data written | ~10–100 KB (depends on window-switch frequency) |

---

## Data Layout

`%LOCALAPPDATA%\RustTimeNoter\` (user mode) or `%PROGRAMDATA%\RustTimeNoter\` (service):

```
config.toml            Configuration
rules.toml             Classification rules (optional)
key.bin                AES-256 master key (DPAPI-wrapped)
apps.dict              String pool: exe paths
titles.dict            String pool: window titles
data\YYYY\MM\YYYY-MM-DD.log   Daily encrypted log
bin\tracker.exe        Autostart binary copy
```

### File Format

- **`.dict`** — magic `RTND`, version, series of `[u32 len][bytes]`. Append-only; ID 0 reserved.
- **`.log`** — magic `RTNL`, version, `date_packed`, series of encrypted blocks.
  Each block: `[u32 plain_len][12 B nonce][ciphertext + tag]`.
  AAD = `magic ‖ date_packed ‖ block_index`.
  Plaintext = N × 17-byte fixed records (`u32 start_offset ‖ u32 duration ‖ u32 app_id ‖ u32 title_id ‖ u8 flags`).
- 17 bytes per record. ~5000 switches/day ≈ 85 KB. 10 years ≈ 304 MB total (disk data ~140 MB + dict overhead).

### Encryption

- AES-256-GCM. Master key sealed by Windows DPAPI and stored in `key.bin`.
- User scope: `CRYPTPROTECT_UI_FORBIDDEN` — only the current user can decrypt.
- Machine scope: `CRYPTPROTECT_LOCAL_MACHINE` — any local account (including `LocalSystem`) can decrypt.

---

## Privacy & Security

- Window titles are **not recorded by default**. Enable explicitly with
  `tracker config set capture_titles true`.
- All log files are encrypted at rest. Offline copies are unreadable without the
  DPAPI-bound master key.
- Title blacklist: `tracker config set title_blacklist Code.exe,1Password.exe`

---

## Edge Cases (Implemented)

| Scenario | How |
|---|---|
| AFK / idle | `GetLastInputInfo` — caps the current segment at `last_input + threshold` |
| Lock screen | `WM_WTSSESSION_CHANGE` (`WTS_SESSION_LOCK` / `UNLOCK`) — suppresses timing while locked |
| Sleep / hibernate | `WM_POWERBROADCAST` (`PBT_APMSUSPEND` / `PBT_APMRESUMEAUTOMATIC`) |
| UWP apps | When foreground is `ApplicationFrameHost.exe`, enumerates child windows to find the real host PID |
| Graceful shutdown | `tracker stop` (named event) / Ctrl+C / SCM stop / console close / logoff / shutdown → flushes, then exits |
| Single instance | Named mutex `Global\RustTimeNoter.Daemon` — second launch exits immediately |
| System tray | Right-click: Open report / Open data folder / Stop tracking. Double-click = open report |
| Crash recovery | Log reader stops at first decryption failure or truncation (drops the incomplete block), never panics |

---

## Build

Requires Rust 1.91+ and Windows 10 or 11.

```powershell
cargo build --release   # → target\release\tracker.exe
cargo test              # 20 unit tests
```

---

## Known Limitations

- **Windows only.** The `run` subcommand errors on Linux/macOS; `report`, `export`,
  `config`, and `view` are cross-platform and can be used to analyze backed-up data
  from another machine.
- **No GUI.** Interaction is via CLI, the system tray, or the browser-based HTML report.
- **Service-mode files are owned by `LocalSystem`.** Normal users need to stop the
  service and adjust ACLs to read them, or use the user-scope `autostart` mode instead.

---

## License

MIT — see [LICENSE](LICENSE).
