# 发布

主分支保护要求 `fmt`、`clippy`、`test-debug`、`test-release` 四项 Windows CI 检查成功。
CI 与 Release 使用固定 Rust 1.97.1、锁定 Cargo 依赖和固定提交的 Actions。

1. 在 PR 中同步更新 `Cargo.toml`、`Cargo.lock` 根包版本、`app.manifest` 四段版本和 `docs/releases/vX.Y.Z.md`。
2. 四项检查通过后合并 PR，并等待合并提交在 `master` 上的 Windows CI 全部成功。
3. 在该提交创建并推送 `vX.Y.Z` 标签。Release 会检查标签与 Cargo 版本一致、提交属于 `master`，且该精确提交已有成功的主分支 CI。
4. Release 构建 Windows x64 程序，验证 `tracker --version`，打包现有安装脚本并生成 SHA256。全部附件上传到草稿后才公开发布。

本地可运行 `powershell -ExecutionPolicy Bypass -File scripts/build-installer.ps1`；如需独立构建目录，追加 `-TargetDirectory target/implementation`。脚本只构建和打包，不安装、不启动后台记录。

附件包括 `tracker.exe`、`RustTimeNoter-vX.Y.Z.zip`、`SHA256SUMS.txt`。工作流不替换已发布附件；若上传或公开发布失败，先检查已有草稿与附件状态，再恢复发布或清理该草稿后重试。
