# 启动错误复审：2026-09-08

本页修复后快照为 `2d421dd`；后续修复、验证与性能数据见[独立自查](self-review.md)。

本轮基于 PR #10 的 `3188471`，修复合并前复审确认的启动错误丢失问题。
[上一轮标题复审](title-review.md)的测试和性能数据保留为该提交的历史快照。

## 复现与修复

先用 writer 写入并读回一条有效加密历史，再删除 `apps.dict`。
旧版 writer 返回“已有历史但字典缺失，拒绝重用编号”，启动层却丢弃线程退出结果，
最终只显示 `durable write not confirmed: channel is empty and sending half is closed`。
损坏已有 `key.bin` 也会遮蔽 DPAPI 解密错误。两项新增回归均在修复前失败，确认与用户复现一致。

启动确认失败后仍按原流程请求 writer 退出并有界等待，但现在优先传播 writer 返回的原始
`io::Error`，保留错误类别及其携带的 OS 错误码。只有 writer 正常结束，才返回先前的 ACK 错误。
因此，控制台入口能显示字典缺失或 `CryptUnprotectData failed`，后台入口的 `crash.log` 也保留相同原因。

同时纠正 ACK 错误分类：通道断开是 `BrokenPipe`，只有等待超时才是 `TimedOut`。
writer 退出等待真正超时时仍返回等待错误，不吞掉失败或误报启动成功。
等待沿用既有的分阶段上限；不是整个启动流程总共 10 秒，ACK 超时后仍可能再等待 writer 最多 10 秒。

## 相邻路径复查

- 正常运行和退出路径已优先返回 writer 结果，未再发现相同的错误遮蔽。
- hook 注册、线程创建失败的分支保留各自的主错误；未将这些独立初始化失败替换成清理错误。
- `setup` 对已有坏配置缺少预检，可能先安装并启动一个立即失败的子进程，再继续生成报表。
  现在复用 `Config::load`，在创建目录、替换程序、注册自启之前拒绝无效配置。
  缺失配置仍使用默认值。这个预检不代表提供了子进程完整就绪握手，后台启动流程的既有边界没有改变。

修复后两轮独立交叉检查未发现新的可复现阻断项。实际系统关机、休眠、服务安装仍未实测。
本轮没有运行安装或修改注册表、自启动设置；`setup` 的调用顺序通过代码复查确认。

## 验证与资源

Debug 和 Release 各 **153 项测试通过**：143 库测试、1 程序测试、3 数据完整性测试、6 隔离端到端测试。
Clippy `-D warnings`、变更文件格式及 diff 空白检查通过。

新增两项端到端测试，每项分别覆盖 `tracker run` 和无参数后台入口：

| 已有有效历史后的故障 | 期望诊断 | 同时检查 |
|---|---|---|
| 删除 `apps.dict` | 历史存在但字典缺失，拒绝重用编号 | 退出码 1，字典不重建，其他数据文件清单及字节不变 |
| 损坏 `key.bin` | DPAPI 解密操作失败 | 退出码 1，损坏密钥保留，其他数据文件清单及字节不变 |

后台模式允许追加 `crash.log`，并单独断言其包含原始原因。测试使用独立数据目录、命名对象和隐藏子进程，
不依赖桌面活动来生成历史；退出等待有上限，失败时只清理测试自己的子进程。
另有一项库测试先接收持久化请求再断开 ACK，验证接收侧断连不会被误分类成超时。

本轮没有改变成功记账、采集回调、队列或存储写入算法，未重跑这些路径的性能基准。
`setup` 多一次安装前配置读取；运行中的 daemon 没有新增周期任务或常驻对象。
发布版 `tracker.exe` 与 `3188471` 相同，均为 **1,058,816 字节**；这不等同于重新测量了 CPU 或 RSS。

| 二进制 | SHA-256 |
|---|---|
| 修复前 `3188471` | `f91618122e965bcbaeac226372733a7af5f63125a21095ed397e67803cf09a7b` |
| 本轮修复后 | `2156b6a1af6fd81b3666212d33e0baf0948f264c2af61c73ab7ccc340c01e8cd` |

修复前失败日志、修复后回归及完整验证日志和二进制保存在本地 `target/startup-review-20260908/`。
验证命令：

```powershell
$env:CARGO_INCREMENTAL = '0'
cargo test --locked --all-targets --target-dir target/implementation
cargo test --release --locked --all-targets --target-dir target/implementation
cargo clippy --locked --all-targets --target-dir target/implementation -- -D warnings
cargo build --release --locked --bin tracker --target-dir target/implementation
```
