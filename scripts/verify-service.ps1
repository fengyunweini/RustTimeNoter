# RustTimeNoter — Windows 服务模式安装/启动/停止/卸载 端到端验证脚本
#
# 使用方法：
#   1. 右键此文件 → 以管理员身份运行 PowerShell
#      （或在管理员 PowerShell 里：cd 到此目录，然后  .\verify-service.ps1）
#   2. 脚本会把全部输出写到 verify-service.log，方便事后回溯
#   3. 跑完后把 verify-service.log 贴给 AI 即可

$ErrorActionPreference = 'Continue'

# 切到仓库根目录
$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

$logFile = Join-Path $PSScriptRoot 'verify-service.log'
"=== RustTimeNoter service verification ===" | Out-File $logFile
"Started:    $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')" | Out-File $logFile -Append
"Repo root:  $repoRoot" | Out-File $logFile -Append
"Tracker:    $repoRoot\target\release\tracker.exe" | Out-File $logFile -Append
"" | Out-File $logFile -Append

function Step($label, $script) {
    $line = "----- $label -----"
    Write-Host $line -ForegroundColor Cyan
    $line | Out-File $logFile -Append
    try {
        $output = & $script 2>&1 | Out-String
    } catch {
        $output = "EXCEPTION: $_"
    }
    Write-Host $output
    $output | Out-File $logFile -Append
    "" | Out-File $logFile -Append
}

# 0. 检查权限
Step "0. Check admin" {
    $current = [Security.Principal.WindowsIdentity]::GetCurrent()
    $isAdmin = ([Security.Principal.WindowsPrincipal]$current).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator)
    if (-not $isAdmin) {
        throw "ERROR: 需要管理员权限。右键 PowerShell 选'以管理员身份运行'。"
    }
    "Admin: OK ($($current.Name))"
}

# 1. 检查二进制存在
Step "1. Check binary" {
    $exe = "$repoRoot\target\release\tracker.exe"
    if (-not (Test-Path $exe)) {
        throw "缺少 $exe。先在仓库根跑 cargo build --release"
    }
    Get-Item $exe | Select-Object Name, Length, LastWriteTime | Format-List | Out-String
}

# 2. 卸载残留（容错，可能没装）
Step "2. Cleanup any prior install (ignore errors)" {
    .\target\release\tracker.exe uninstall service 2>&1 | Out-String
}

# 3. 安装服务
Step "3. Install service" {
    .\target\release\tracker.exe install service 2>&1 | Out-String
}

# 4. 查看服务状态（应该是 Stopped 或 Running，看 install 是否自启）
Step "4. Service status after install" {
    Get-Service RustTimeNoter | Format-List Name, Status, StartType, DisplayName | Out-String
}

# 5. 启动服务
Step "5. Start-Service" {
    Start-Service RustTimeNoter
    Start-Sleep -Seconds 2
    Get-Service RustTimeNoter | Format-List Name, Status | Out-String
}

# 6. 让它跑 12 秒，期间切换几个窗口（用户配合）
Step "6. Run for 12 seconds (please switch windows during this time)" {
    "Sleeping 12s ..."
    Start-Sleep -Seconds 12
    "Done."
}

# 7. 查 status（注意：tracker status 是 user scope，service 是 machine scope，
#    所以 'Running' 检测可能显示 stopped。但 data dir 在 ProgramData。这里直接看文件系统）
Step "7. Inspect ProgramData (machine scope) data dir" {
    $machRoot = "$env:ProgramData\RustTimeNoter"
    if (Test-Path $machRoot) {
        Get-ChildItem $machRoot -Recurse -File | Select-Object FullName, Length, LastWriteTime |
            Format-Table -AutoSize | Out-String
    } else {
        "WARNING: $machRoot 不存在 — service 可能没真正写出数据"
    }
}

# 8. 停止服务（应触发 graceful shutdown → flush 缓冲）
Step "8. Stop-Service" {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    Stop-Service RustTimeNoter
    $sw.Stop()
    "Stop-Service returned in $($sw.ElapsedMilliseconds) ms"
    Start-Sleep -Seconds 1
    Get-Service RustTimeNoter | Format-List Name, Status | Out-String
}

# 9. 再看一下数据目录，应该有更新
Step "9. ProgramData after stop (should contain log file)" {
    $machRoot = "$env:ProgramData\RustTimeNoter"
    if (Test-Path $machRoot) {
        Get-ChildItem $machRoot -Recurse -File | Select-Object FullName, Length, LastWriteTime |
            Format-Table -AutoSize | Out-String
    }
}

# 10. 卸载服务
Step "10. Uninstall service" {
    .\target\release\tracker.exe uninstall service 2>&1 | Out-String
}

# 11. 最终验证：服务应该不存在
Step "11. Final verify (should be 'no service')" {
    try {
        Get-Service RustTimeNoter -ErrorAction Stop | Format-List | Out-String
    } catch {
        "OK: service removed ($($_.Exception.Message))"
    }
}

"" | Out-File $logFile -Append
"Finished:   $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')" | Out-File $logFile -Append
Write-Host ""
Write-Host "全部完成。日志：$logFile" -ForegroundColor Green
Write-Host "把日志内容粘给 AI 即可。"
Write-Host ""
Write-Host "按任意键退出..."
$null = $Host.UI.RawUI.ReadKey('NoEcho,IncludeKeyDown')
