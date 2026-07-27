# RustTimeNoter

超轻量 Windows 应用使用时长记录器。

## 安装

1. 把这一整个文件夹解压到任意位置
2. 双击 `install.bat`
3. 浏览器自动打开报表页面，daemon 已经在后台跑了

下次开机会自动启动；不需要管理员权限。

## 日常使用

- **看报表**：双击 `view.bat`，或命令行 `tracker view`
- **看状态**：命令行 `tracker status`
- **临时停止**：命令行 `tracker stop`（会被下次开机重启）
- **彻底卸载**：双击 `uninstall.bat`

## 数据放哪儿

`%LOCALAPPDATA%\RustTimeNoter\`

```
config.toml          配置（AFK 阈值、是否记标题等）
key.bin              主密钥（DPAPI 包裹）
apps.dict            进程名字典
titles.dict          窗口标题字典（默认关，隐私优先）
data\YYYY\MM\YYYY-MM-DD.log   按 UTC 日分片的加密日志
```

存储始终按 UTC 日分片；报表、状态、跟随和导出按当前系统的本地日历时间查询和展示。
切换系统时区只会重新划分查询结果，不会改写原始日志。

每条记录 17 字节（明文），加密后含 block 头约 21 字节/条均摊。
正常使用一天通常 < 100 KB。

## 更小一点

`config.toml` 默认就是节省版：

- `capture_titles = false`  完全不记窗口标题
- `afk_minutes = 5`         5 分钟无键鼠输入即视为离开，不计入

要更激进可以改 `flush_block_records = 1024`（更少 block 头开销）。

## 卸载

`uninstall.bat` 只移除自启 + 停止进程；数据文件夹不动。
要连数据一起删，手动删 `%LOCALAPPDATA%\RustTimeNoter`。
