<p align="center">
  <a href="README.md">English</a> &nbsp;|&nbsp;
  <a href="README.zh-CN.md">简体中文</a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/platform-Windows%2010%2B-blue?logo=windows" alt="Platform: Windows 10+">
  <img src="https://img.shields.io/badge/license-MIT-blue" alt="License: MIT">
</p>

# RustTimeNoter

**Ultra-light Windows foreground-app usage tracker.**

A single native binary. Foreground changes use events; periodic samples check idle state and save progress.
Runs in the background, records which apps you use (and for how long),
and renders a self-contained HTML report in your browser.

- **No runtime, no framework** — raw Win32 via `windows-sys`.
- **Encrypted at rest** — AES-256-GCM, key sealed with Windows DPAPI.
- **Compact storage** — binary fixed-length records + string-dict pool.
- **Confirmed shutdown writes** — single-instance lock, bounded shutdown waits, preserved damaged logs.
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
| `tracker status` | Daemon status, today's local-time totals, data directory size |
| `tracker tail [--interval 2]` | Follow today's local-time activity in real time |
| `tracker report [--today\|--week\|--month\|--from\|--to] [--by app\|category\|title] [--top N]` | Console report |
| `tracker export --format csv\|json [--out PATH]` | Export raw records |
| `tracker config show\|init\|set <K> <V>\|get <K>` | Read/write configuration |

### Configuration (`config.toml`)

| Key | Default | Meaning |
|---|---|---|
| `afk_minutes` | 5 | Idle threshold — no keyboard/mouse input for N minutes is considered away |
| `capture_titles` | `false` | Record window titles (off by default, privacy-first) |
| `flush_interval_secs` | 30 | Progress checkpoint / write-back interval; ordered capture adds a 2-second tail |
| `flush_block_records` | 256 | Max records per encryption block (capped at 4096) |
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

Measured on 2026-09-07: Ryzen 9 7945HX, Windows build 26200, Rust 1.97.1,
x64 release with LTO. Real desktop and tray, five-second warmup, two 64-second
runs per setting; no working-set trimming. These are local measurements, not a
universal resource guarantee. These figures describe the September 7 snapshot (`8dc6c54`).
The subsequent [delayed-title correction and validation](docs/title-review.md) has its own replay measurements.
The [independent review](docs/self-review.md) records further fixes and paired measurements against `2d421dd`.
The latest [closing review](docs/final-review.md) covers a capture-timeout correction and its regression tests.
See the [September 7 review and paired comparison](docs/pr-review.md),
[September 5 second review](docs/performance-review.md) and [first optimization results](docs/performance.md).

| Metric | September 7 snapshot (`8dc6c54`) |
|---|---|
| Binary size | 1,057,280 B (~1.008 MiB); 512 B smaller than the preceding snapshot |
| Default working set, per-run medians | 12.64–12.86 MiB, including shared resident pages |
| Default private committed memory, per-run medians | ~2.09 MiB |
| Default CPU time over 64 seconds | 0 ms reported; near timer resolution, not zero work |
| Accounting replay of 100,000 ordinary foreground observations | 24.57 ms versus 24.02 ms in the paired preceding snapshot |
| Replay of 4000 unique titles, durable writes | 290.9 ms versus 283.6 ms; no additional write speedup |
| Live Rust heap after that replay | 948,406 B; previous allocation savings retained |

Private committed memory and total working set are different metrics; the older
approximate “2–3 MB working set” claim was not reproduced. Replay improvements do
not imply the same percentage reduction in whole-process CPU. With titles enabled,
paired CPU cycles increased by 5.3% and 7.3% in this final review; measured CPU time
was 390.625–500 ms per 64 seconds (0.61%–0.78% of one logical core).
Startup durability and layout checks added about 4.71 ms on the 65,536-record sample.
The linked reports retain the historical baselines, raw summaries and these costs.

---

## Data Layout

`%LOCALAPPDATA%\RustTimeNoter\` (user mode) or `%PROGRAMDATA%\RustTimeNoter\` (service):

```
config.toml            Configuration
rules.toml             Classification rules (optional)
key.bin                AES-256 master key (DPAPI-wrapped)
apps.dict              String pool: exe paths
titles.dict            String pool: window titles
data\YYYY\MM\YYYY-MM-DD.log   Encrypted UTC-day shard
data\YYYY\MM\YYYY-MM-DD.part-000001.log   Continuation after recoverable damage
bin\tracker.exe        Autostart binary copy
```

### Time Zones

- Format-v1 logs are sharded by UTC day. No duplicate
  local-time files and no migration.
- All user-facing dates and times default to the current system time zone. Queries scan
  the required UTC shards and clip records at local calendar boundaries.
- Export includes `start_timestamp` as RFC 3339 with an explicit numeric UTC offset.
- A daylight-saving local day may be 23 or 25 hours.
- Changing the system time zone re-buckets historical records at query time; encrypted
  source data is never rewritten.

### File Format

- **`.dict`** — magic `RTND`, version, series of `[u32 id][u32 len][bytes]`. Append-only; ID 0 reserved.
- **`.log`** — format v1, magic `RTNL`, UTC `date_packed`, series of encrypted blocks.
  Each block: `[u32 plain_len][12 B nonce][ciphertext + tag]`.
  AAD = `magic ‖ date_packed ‖ block_index`.
  Plaintext = N × 17-byte fixed records (`u32 start_offset ‖ u32 duration ‖ u32 app_id ‖ u32 title_id ‖ u8 flags`).
- 17 bytes per record, before encryption and dictionary overhead. Checkpoints also produce records.
- Flag `0x80` with dictionary IDs zero represents uncertain capture. Queries merge these intervals
  and exclude their overlap from activity, including activity saved before a late correction.

### Reliability and recovery

- Keep the most recent two seconds adjustable, then process foreground changes, input snapshots,
  lock/suspend events and checkpoints in capture-time order. Callback queues are bounded and never wait for disk.
- AFK uses actual keyboard/mouse input. Window or title changes do not count as input. A return from
  idle or an unknown foreground starts from a reliable observation; unobserved activity is not backfilled.
- Late events beyond the adjustable tail, queue overflow and unresolved foreground intervals become
  explicit gaps. Known idle, locked and suspended time is excluded without being labelled missing.
  Gap boundaries round outward to whole seconds, so a correction may conservatively remove a boundary second.
- Reports, status, tail and exports warn about incomplete capture. CSV/JSON append `record_type`
  (`activity` or `gap`); gap duration is not application usage.
- Long-running foreground sessions are checkpointed. Under normal scheduling and storage operation,
  the unsaved tail is approximately `min(idle_tick_secs, flush_interval_secs) + 2` seconds;
  this is not a deadline guarantee during a blocked OS or disk failure. Forced termination can lose that tail.
- On restart, tracking begins at a fresh observation; downtime is not attributed to the last app.
- When a log has a verified prefix followed by damage, preserve the entire original and write a numbered
  continuation. Readers include all parts and report the damage. An unverifiable first block, invalid header,
  wrong key, or missing key/dictionary with existing logs causes an explicit error rather than an automatic repair.
- An incomplete dictionary tail is repaired only after a full, synchronized `.recovery-NNNNNN.bak` backup.
  Structural dictionary errors stop startup. Confirmed writes synchronize dictionaries before referring logs.
- Startup streams historical logs to check dictionary references before any ID can be reused.
  Missing referenced entries preserve the original files and fail startup. This adds startup reads;
  a bad header or unverifiable first block in any historical day also prevents startup.
  Existing keys and dictionaries must be writable for startup synchronization. Linked/reparse data
  directories and log files are rejected so that historical references cannot silently escape the scan.
- Startup preserves the writer's original storage error: `tracker run` prints it to the console,
  and no-argument background startup records it in `crash.log` in the data directory.
  See the [startup error review](docs/startup-review.md). `setup` validates configuration before installing.
- Existing v1 activity files remain readable. Older binaries do not understand gaps or continuation parts;
  use this version for queries once either appears, and retain the complete data directory when backing up.

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
| Crash recovery | Preserve damaged originals; read authenticated prefixes and numbered continuations with an incomplete-data warning |

---

## Build

Requires Rust 1.91+ and Windows 10 or 11.

```powershell
cargo build --release   # → target\release\tracker.exe
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
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
