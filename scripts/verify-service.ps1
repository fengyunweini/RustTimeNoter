# RustTimeNoter -- service install/start/stop/uninstall verifier
# Run from an ELEVATED PowerShell:
#   cd E:\Code\RustTimeNoter
#   .\scripts\verify-service.ps1
#
# Output is captured to .\scripts\verify-service.log

$ErrorActionPreference = "Continue"

$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

$logFile = Join-Path $PSScriptRoot "verify-service.log"
"=== RustTimeNoter service verification ===" | Out-File $logFile -Encoding utf8
"Started:    $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')" | Out-File $logFile -Append -Encoding utf8
"Repo root:  $repoRoot" | Out-File $logFile -Append -Encoding utf8
"Tracker:    $repoRoot\target\release\tracker.exe" | Out-File $logFile -Append -Encoding utf8
"" | Out-File $logFile -Append -Encoding utf8

# ---- Admin gate (hard exit if not elevated) -------------------------------
$current = [Security.Principal.WindowsIdentity]::GetCurrent()
$isAdmin = ([Security.Principal.WindowsPrincipal]$current).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    $msg = "ERROR: This script needs Administrator. Right-click PowerShell -> Run as Administrator, then re-run."
    Write-Host $msg -ForegroundColor Red
    $msg | Out-File $logFile -Append -Encoding utf8
    exit 1
}
Write-Host "Admin OK: $($current.Name)" -ForegroundColor Green
"Admin OK: $($current.Name)" | Out-File $logFile -Append -Encoding utf8

function Step($label, $script) {
    $line = "----- $label -----"
    Write-Host ""
    Write-Host $line -ForegroundColor Cyan
    "" | Out-File $logFile -Append -Encoding utf8
    $line | Out-File $logFile -Append -Encoding utf8
    try {
        $output = & $script 2>&1 | Out-String
    } catch {
        $output = "EXCEPTION: $_"
    }
    Write-Host $output
    $output | Out-File $logFile -Append -Encoding utf8
}

# 1. Binary check
Step "1. Check binary" {
    $exe = "$repoRoot\target\release\tracker.exe"
    if (-not (Test-Path $exe)) {
        throw "Missing $exe. First run: cargo build --release"
    }
    Get-Item $exe | Select-Object Name, Length, LastWriteTime | Format-List | Out-String
}

# 2. Cleanup any prior install (ignore errors)
Step "2. Cleanup any prior install (ignore errors expected if not installed)" {
    .\target\release\tracker.exe uninstall service 2>&1 | Out-String
    $tracePath = "$env:ProgramData\RustTimeNoter\service-trace.log"
    if (Test-Path $tracePath) {
        Remove-Item $tracePath -ErrorAction SilentlyContinue
        "cleared old service-trace.log"
    }
}

# 3. Install
Step "3. Install service" {
    .\target\release\tracker.exe install service 2>&1 | Out-String
}

# 4. Status after install
Step "4. Service status after install" {
    Get-Service RustTimeNoter -ErrorAction SilentlyContinue |
        Format-List Name, Status, StartType, DisplayName | Out-String
}

# 5. Start
Step "5. Start-Service" {
    Start-Service RustTimeNoter
    Start-Sleep -Seconds 2
    Get-Service RustTimeNoter | Format-List Name, Status | Out-String
}

# 6. Run for 12 seconds (please switch some windows during this time)
Step "6. Run for 12 seconds (switch some windows now)" {
    "Sleeping 12s ..."
    Start-Sleep -Seconds 12
    "Done."
}

# 7. Inspect ProgramData (machine scope) data dir
Step "7. Inspect ProgramData data dir" {
    $machRoot = "$env:ProgramData\RustTimeNoter"
    if (Test-Path $machRoot) {
        Get-ChildItem $machRoot -Recurse -File |
            Select-Object FullName, Length, LastWriteTime |
            Format-Table -AutoSize | Out-String
    } else {
        "WARNING: $machRoot not found -- service may not have written data"
    }
}

# 8. Stop
Step "8. Stop-Service" {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    Stop-Service RustTimeNoter
    $sw.Stop()
    "Stop-Service returned in $($sw.ElapsedMilliseconds) ms"
    Start-Sleep -Seconds 1
    Get-Service RustTimeNoter | Format-List Name, Status | Out-String
}

# 9. Inspect after stop
Step "9. ProgramData after stop (should contain log file)" {
    $machRoot = "$env:ProgramData\RustTimeNoter"
    if (Test-Path $machRoot) {
        Get-ChildItem $machRoot -Recurse -File |
            Select-Object FullName, Length, LastWriteTime |
            Format-Table -AutoSize | Out-String
    }
}

# 10. Uninstall
Step "10. Uninstall service" {
    .\target\release\tracker.exe uninstall service 2>&1 | Out-String
}

# 11. Final verify
Step "11. Final verify (should report no service)" {
    try {
        Get-Service RustTimeNoter -ErrorAction Stop | Format-List | Out-String
    } catch {
        "OK: service removed ($($_.Exception.Message))"
    }
}

# 12. Dump service-trace.log written by the daemon process itself
Step "12. service-trace.log (written by tracker.exe inside the service process)" {
    $tracePath = "$env:ProgramData\RustTimeNoter\service-trace.log"
    if (Test-Path $tracePath) {
        Get-Content $tracePath -Raw
    } else {
        "WARNING: $tracePath not found -- service process never started, or never reached the trace point"
    }
}

# 13. Last 30 SCM events for our service
Step "13. Recent SCM events for RustTimeNoter" {
    Get-WinEvent -LogName System -MaxEvents 200 -ErrorAction SilentlyContinue |
        Where-Object { $_.Message -match 'RustTimeNoter' -and $_.TimeCreated -gt (Get-Date).AddMinutes(-5) } |
        Select-Object TimeCreated, Id, ProviderName, LevelDisplayName, Message |
        Format-List | Out-String
}

# 14. Recent app crash entries (if any)
Step "14. Recent Application Errors (if any)" {
    Get-WinEvent -LogName Application -MaxEvents 200 -ErrorAction SilentlyContinue |
        Where-Object { $_.TimeCreated -gt (Get-Date).AddMinutes(-5) -and ($_.ProviderName -match 'Application Error|Windows Error') } |
        Select-Object TimeCreated, Id, ProviderName, @{n='Msg';e={$_.Message.Substring(0,[Math]::Min(500,$_.Message.Length))}} |
        Format-List | Out-String
}

"" | Out-File $logFile -Append -Encoding utf8
"Finished:   $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')" | Out-File $logFile -Append -Encoding utf8
Write-Host ""
Write-Host "All done. Log: $logFile" -ForegroundColor Green
Write-Host "Paste the log content back."
