<#
.SYNOPSIS
Measure isolated, real Windows daemons on the same unchanged desktop.
.DESCRIPTION
Runs supplied release binaries concurrently. After a five-second warmup it
samples each PID once per second for at least 60 seconds. It never sends input,
changes foreground focus, fires synthetic WinEvents, trims working sets,
installs the application, or changes system settings.

Before/Optimized require RUSTTIMENOTER_TEST_ROOT and TEST_INSTANCE support.
Master must be a source-identical master build except for the two named-object
constants suffixed .PerfMaster20260905. The script checks those binary strings
before starting it, and gives it a private LOCALAPPDATA directory.

WorkingSetBytes is resident memory; PrivateBytes is private committed memory,
not private resident memory. CPU percent is reported both per logical core and
normalized to the machine's logical processor count. CPU cycles are also
reported as raw deltas, never converted to time or percentages. GetProcessIoCounters
counts process I/O requests/transfer bytes, not physical disk traffic.
.EXAMPLE
pwsh -File scripts/measure-resources.ps1 -MasterExe target/perf-20260905/master.exe -BeforeExe target/perf-20260905/before.exe -OptimizedExe target/perf-20260905/optimized.exe -Scenario Both
#>
[CmdletBinding()]
param(
    [string] $MasterExe,
    [string] $BeforeExe,
    [string] $OptimizedExe,
    [ValidateSet('Default', 'Titles', 'Both')]
    [string] $Scenario = 'Both',
    [ValidateRange(60, 3600)]
    [int] $Seconds = 64,
    [ValidateRange(5, 300)]
    [int] $WarmupSeconds = 5,
    [switch] $RequireDesktop,
    [string] $OutputDirectory = (Join-Path $PSScriptRoot '../target/perf-20260905')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'This benchmark requires Windows.'
}

$masterMutexName = 'Global\RustTimeNoter.Daemon.PerfMaster20260905'
$masterStopName = 'Global\RustTimeNoter.Stop.PerfMaster20260905'
$utf8 = [System.Text.UTF8Encoding]::new($false)
$versions = [System.Collections.Generic.List[object]]::new()
foreach ($entry in @(
    @{ Label = 'master'; Path = $MasterExe; Mode = 'MasterLocalAppData' },
    @{ Label = 'before'; Path = $BeforeExe; Mode = 'TestEnvironment' },
    @{ Label = 'optimized'; Path = $OptimizedExe; Mode = 'TestEnvironment' }
)) {
    if ([string]::IsNullOrWhiteSpace($entry.Path)) { continue }
    $absolute = (Resolve-Path -LiteralPath $entry.Path).Path
    $binaryText = [System.Text.Encoding]::UTF8.GetString([System.IO.File]::ReadAllBytes($absolute))
    if ($entry.Mode -eq 'MasterLocalAppData') {
        if (!$binaryText.Contains($masterMutexName) -or !$binaryText.Contains($masterStopName)) {
            throw "Refusing unisolated master binary: $absolute"
        }
    } elseif (!$binaryText.Contains('RUSTTIMENOTER_TEST_ROOT') -or !$binaryText.Contains('RUSTTIMENOTER_TEST_INSTANCE')) {
        throw "Binary does not advertise the required test isolation: $absolute"
    }
    $versions.Add([pscustomobject]@{
        Label = $entry.Label
        Exe = $absolute
        Mode = $entry.Mode
        Sha256 = (Get-FileHash -LiteralPath $absolute -Algorithm SHA256).Hash.ToLowerInvariant()
        BinaryBytes = (Get-Item -LiteralPath $absolute).Length
    })
}
if ($versions.Count -eq 0) { throw 'Supply at least one release binary.' }
[System.IO.Directory]::CreateDirectory([System.IO.Path]::GetFullPath($OutputDirectory)) | Out-Null
$outputRoot = (Resolve-Path -LiteralPath $OutputDirectory).Path

if ($null -eq ('RustTimeNoterPerf.Native' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
namespace RustTimeNoterPerf {
    [StructLayout(LayoutKind.Sequential)]
    public struct IoCounters {
        public ulong ReadOperationCount;
        public ulong WriteOperationCount;
        public ulong OtherOperationCount;
        public ulong ReadTransferCount;
        public ulong WriteTransferCount;
        public ulong OtherTransferCount;
    }
    [StructLayout(LayoutKind.Sequential)]
    internal struct LastInputInfo {
        public uint Size;
        public uint Time;
    }
    public static class Native {
        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool GetProcessIoCounters(IntPtr process, out IoCounters counters);
        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool QueryProcessCycleTime(IntPtr process, out ulong cycles);
        [DllImport("user32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool GetLastInputInfo(ref LastInputInfo info);
        [DllImport("kernel32.dll")]
        private static extern ulong GetTickCount64();
        [DllImport("user32.dll")]
        private static extern IntPtr GetForegroundWindow();
        [DllImport("user32.dll")]
        private static extern IntPtr GetShellWindow();
        public static bool ForegroundAvailable() { return GetForegroundWindow() != IntPtr.Zero; }
        public static bool ShellAvailable() { return GetShellWindow() != IntPtr.Zero; }
        public static double? DesktopIdleSeconds() {
            var info = new LastInputInfo { Size = (uint)Marshal.SizeOf(typeof(LastInputInfo)) };
            if (!GetLastInputInfo(ref info)) return null;
            uint age = unchecked((uint)GetTickCount64() - info.Time);
            return age / 1000.0;
        }
    }
}
'@
}

function New-IsolatedProcessInfo {
    param([object] $Version, [string] $Arguments, [string] $DataParent, [string] $Instance)
    $info = [System.Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $Version.Exe
    $info.Arguments = $Arguments
    $info.WorkingDirectory = Split-Path -Parent $Version.Exe
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.WindowStyle = [System.Diagnostics.ProcessWindowStyle]::Hidden
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.EnvironmentVariables['RUSTTIMENOTER_SCOPE'] = 'user'
    $info.EnvironmentVariables.Remove('RUSTTIMENOTER_TEST_ROOT')
    $info.EnvironmentVariables.Remove('RUSTTIMENOTER_TEST_INSTANCE')
    if ($Version.Mode -eq 'MasterLocalAppData') {
        $info.EnvironmentVariables['LOCALAPPDATA'] = $DataParent
    } else {
        $info.EnvironmentVariables['RUSTTIMENOTER_TEST_ROOT'] = $DataParent
        $info.EnvironmentVariables['RUSTTIMENOTER_TEST_INSTANCE'] = $Instance
    }
    return $info
}

function Read-ResourceSample {
    param([object] $Run, [int] $Index, [double] $ElapsedSeconds, [object] $DesktopIdleSeconds)
    $process = $Run.Process
    $process.Refresh()
    if ($process.HasExited) { throw "$($Run.Version.Label) exited during measurement (code $($process.ExitCode))." }
    $io = [RustTimeNoterPerf.IoCounters]::new()
    if (![RustTimeNoterPerf.Native]::GetProcessIoCounters($process.Handle, [ref] $io)) {
        throw [System.ComponentModel.Win32Exception]::new([Runtime.InteropServices.Marshal]::GetLastWin32Error())
    }
    $cycles = [uint64] 0
    if (![RustTimeNoterPerf.Native]::QueryProcessCycleTime($process.Handle, [ref] $cycles)) {
        throw [System.ComponentModel.Win32Exception]::new([Runtime.InteropServices.Marshal]::GetLastWin32Error())
    }
    return [pscustomobject][ordered]@{
        Label = $Run.Version.Label
        Pid = $process.Id
        Index = $Index
        Utc = [DateTime]::UtcNow.ToString('o')
        ElapsedSeconds = [math]::Round($ElapsedSeconds, 6)
        DesktopIdleSeconds = $DesktopIdleSeconds
        WorkingSetBytes = $process.WorkingSet64
        PrivateBytes = $process.PrivateMemorySize64
        CpuMilliseconds = $process.TotalProcessorTime.TotalMilliseconds
        CpuCycles = $cycles
        UserCpuMilliseconds = $process.UserProcessorTime.TotalMilliseconds
        KernelCpuMilliseconds = $process.PrivilegedProcessorTime.TotalMilliseconds
        Threads = $process.Threads.Count
        Handles = $process.HandleCount
        ReadOperations = $io.ReadOperationCount
        WriteOperations = $io.WriteOperationCount
        OtherOperations = $io.OtherOperationCount
        ReadBytes = $io.ReadTransferCount
        WriteBytes = $io.WriteTransferCount
        OtherBytes = $io.OtherTransferCount
    }
}

function Stop-OwnedDaemon {
    param([object] $Run, [string] $LogDirectory)
    $daemon = $Run.Process
    $stop = $null
    $forced = $false
    $stopOutput = ''
    $stopError = ''
    $stopCode = $null
    $cleanupError = $null
    try {
        if (!$daemon.HasExited) {
            $stop = [System.Diagnostics.Process]::new()
            $stop.StartInfo = New-IsolatedProcessInfo $Run.Version 'stop' $Run.DataParent $Run.Instance
            if (!$stop.Start()) { throw 'Failed to start isolated stop command.' }
            $stopOutTask = $stop.StandardOutput.ReadToEndAsync()
            $stopErrTask = $stop.StandardError.ReadToEndAsync()
            if (!$stop.WaitForExit(5000)) {
                $stop.Kill()
                [void] $stop.WaitForExit(5000)
                throw 'Isolated stop command timed out.'
            }
            $stopCode = $stop.ExitCode
            $stopOutput = $stopOutTask.GetAwaiter().GetResult()
            $stopError = $stopErrTask.GetAwaiter().GetResult()
            if (!$daemon.WaitForExit(15000)) {
                $forced = $true
                $daemon.Kill()
                if (!$daemon.WaitForExit(5000)) { throw 'Owned daemon did not exit after termination.' }
            }
        }
    } catch {
        $cleanupError = $_.Exception.Message
    } finally {
        # These Process objects refer only to children started in this run. No
        # process-name searches or shutdown signals to production names occur.
        if ($null -ne $stop) {
            if (!$stop.HasExited) { $stop.Kill(); [void] $stop.WaitForExit(5000) }
            $stop.Dispose()
        }
        if (!$daemon.HasExited) {
            $forced = $true
            $daemon.Kill()
            [void] $daemon.WaitForExit(5000)
        }
    }
    $stdout = $Run.Stdout.GetAwaiter().GetResult()
    $stderr = $Run.Stderr.GetAwaiter().GetResult()
    [System.IO.File]::WriteAllText((Join-Path $LogDirectory ($Run.Version.Label + '.stdout.txt')), $stdout, $utf8)
    [System.IO.File]::WriteAllText((Join-Path $LogDirectory ($Run.Version.Label + '.stderr.txt')), $stderr, $utf8)
    $result = [pscustomobject]@{
        Label = $Run.Version.Label
        Pid = $daemon.Id
        ExitCode = $daemon.ExitCode
        ForcedTermination = $forced
        StopCommandExitCode = $stopCode
        StopCommandStdout = $stopOutput.Trim()
        StopCommandStderr = $stopError.Trim()
        CleanupError = $cleanupError
    }
    $daemon.Dispose()
    return $result
}

function Get-SeriesSummary {
    param([object[]] $Rows, [int] $LogicalProcessors)
    $first = $Rows[0]
    $last = $Rows[$Rows.Count - 1]
    $elapsed = $last.ElapsedSeconds - $first.ElapsedSeconds
    $cpu = $last.CpuMilliseconds - $first.CpuMilliseconds
    $ws = @($Rows.WorkingSetBytes | Sort-Object)
    $private = @($Rows.PrivateBytes | Sort-Object)
    $p95 = [math]::Max(0, [math]::Ceiling($Rows.Count * 0.95) - 1)
    $median = [int][math]::Floor($Rows.Count / 2)
    return [pscustomobject][ordered]@{
        Label = $first.Label
        SampleCount = $Rows.Count
        MeasuredSeconds = $elapsed
        WorkingSetMeanBytes = [math]::Round(($Rows.WorkingSetBytes | Measure-Object -Average).Average, 2)
        WorkingSetMedianBytes = $ws[$median]
        WorkingSetP95Bytes = $ws[$p95]
        WorkingSetMinBytes = $ws[0]
        WorkingSetMaxBytes = $ws[$ws.Count - 1]
        PrivateMeanBytes = [math]::Round(($Rows.PrivateBytes | Measure-Object -Average).Average, 2)
        PrivateMedianBytes = $private[$median]
        PrivateP95Bytes = $private[$p95]
        PrivateMinBytes = $private[0]
        PrivateMaxBytes = $private[$private.Count - 1]
        CpuMilliseconds = $cpu
        CpuCyclesDelta = $last.CpuCycles - $first.CpuCycles
        CpuPercentOneCore = if ($elapsed -gt 0) { 100 * $cpu / ($elapsed * 1000) } else { $null }
        CpuPercentMachine = if ($elapsed -gt 0) { 100 * $cpu / ($elapsed * 1000 * $LogicalProcessors) } else { $null }
        ThreadsMin = ($Rows.Threads | Measure-Object -Minimum).Minimum
        ThreadsMax = ($Rows.Threads | Measure-Object -Maximum).Maximum
        HandlesMin = ($Rows.Handles | Measure-Object -Minimum).Minimum
        HandlesMax = ($Rows.Handles | Measure-Object -Maximum).Maximum
        ReadOperations = $last.ReadOperations - $first.ReadOperations
        WriteOperations = $last.WriteOperations - $first.WriteOperations
        OtherOperations = $last.OtherOperations - $first.OtherOperations
        ReadBytes = $last.ReadBytes - $first.ReadBytes
        WriteBytes = $last.WriteBytes - $first.WriteBytes
        OtherBytes = $last.OtherBytes - $first.OtherBytes
        DesktopIdleSecondsFirst = $first.DesktopIdleSeconds
        DesktopIdleSecondsLast = $last.DesktopIdleSeconds
    }
}

# The master build has a fixed, dedicated name. Prevent overlapping benchmark
# orchestrators and refuse to touch any independently started isolated master.
$created = $false
$benchmarkMutex = [Threading.Mutex]::new($true, 'Local\RustTimeNoter.PerfMaster20260905.Benchmark', [ref] $created)
if (!$created) { $benchmarkMutex.Dispose(); throw 'Another resource benchmark is already running.' }
try {
    $scenarios = if ($Scenario -eq 'Both') { @('Default', 'Titles') } else { @($Scenario) }
    foreach ($case in $scenarios) {
        if (@($versions | Where-Object Mode -eq 'MasterLocalAppData').Count -gt 0) {
            $existingMaster = $null
            try { $existingMaster = [Threading.Mutex]::OpenExisting($masterMutexName) }
            catch [Threading.WaitHandleCannotBeOpenedException] { }
            if ($null -ne $existingMaster) {
                $existingMaster.Dispose()
                throw 'An isolated master already exists; refusing to stop or replace it.'
            }
        }
        $runId = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssfffZ') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 8)
        $logDirectory = Join-Path $outputRoot ($runId + '-' + $case.ToLowerInvariant())
        [System.IO.Directory]::CreateDirectory($logDirectory) | Out-Null
        $tempRoot = Join-Path ([System.IO.Path]::GetTempPath()) ('RustTimeNoterPerf\' + $runId)
        $rows = [System.Collections.Generic.List[object]]::new()
        $runs = [System.Collections.Generic.List[object]]::new()
        $stops = [System.Collections.Generic.List[object]]::new()
        $runError = $null
        $startUtc = [DateTime]::UtcNow
        $captureTitles = $case -eq 'Titles'
        $desktopAtStart = [pscustomobject]@{
            ForegroundAvailable = [RustTimeNoterPerf.Native]::ForegroundAvailable()
            ShellAvailable = [RustTimeNoterPerf.Native]::ShellAvailable()
        }
        if ($RequireDesktop -and (!$desktopAtStart.ForegroundAvailable -or !$desktopAtStart.ShellAvailable)) {
            throw 'A foreground window and interactive Shell are required; this environment cannot measure normal desktop tracking.'
        }
        $config = "afk_minutes = 5`ncapture_titles = $($captureTitles.ToString().ToLowerInvariant())`ntitle_blacklist = []`nflush_interval_secs = 30`nflush_block_records = 256`nidle_tick_secs = 30`ntitle_max_chars = 256`n"
        try {
            foreach ($version in $versions) {
                $dataParent = Join-Path $tempRoot $version.Label
                $dataRoot = if ($version.Mode -eq 'MasterLocalAppData') { Join-Path $dataParent 'RustTimeNoter' } else { $dataParent }
                [System.IO.Directory]::CreateDirectory($dataRoot) | Out-Null
                [System.IO.File]::WriteAllText((Join-Path $dataRoot 'config.toml'), $config, $utf8)
                $instance = 'perf-' + $version.Label + '-' + [Guid]::NewGuid().ToString('N')
                if ($instance -notmatch '^[A-Za-z0-9_-]{1,64}$') { throw 'Invalid isolated instance name.' }
                $daemon = [System.Diagnostics.Process]::new()
                $daemon.StartInfo = New-IsolatedProcessInfo $version 'run' $dataParent $instance
                if (!$daemon.Start()) { throw "Failed to start $($version.Label)." }
                $run = [pscustomobject]@{
                    Version = $version
                    Process = $daemon
                    DataParent = $dataParent
                    DataRoot = $dataRoot
                    Instance = if ($version.Mode -eq 'MasterLocalAppData') { 'PerfMaster20260905' } else { $instance }
                    StartedUtc = $daemon.StartTime.ToUniversalTime().ToString('o')
                    Stdout = $daemon.StandardOutput.ReadToEndAsync()
                    Stderr = $daemon.StandardError.ReadToEndAsync()
                }
                $runs.Add($run)
            }
            Write-Host ("{0}: warming {1} isolated daemons for {2}s; measuring {3}s." -f $case, $runs.Count, $WarmupSeconds, $Seconds)
            Start-Sleep -Seconds $WarmupSeconds
            $watch = [Diagnostics.Stopwatch]::StartNew()
            for ($index = 0; $index -le $Seconds; $index++) {
                $remaining = $index - $watch.Elapsed.TotalSeconds
                if ($remaining -gt 0) { Start-Sleep -Milliseconds ([int][math]::Ceiling($remaining * 1000)) }
                $idle = [RustTimeNoterPerf.Native]::DesktopIdleSeconds()
                foreach ($run in $runs) {
                    $rows.Add((Read-ResourceSample $run $index $watch.Elapsed.TotalSeconds $idle))
                }
            }
        } catch {
            $runError = $_.Exception.Message
        } finally {
            foreach ($run in $runs) {
                try { $stops.Add((Stop-OwnedDaemon $run $logDirectory)) }
                catch { $stops.Add([pscustomobject]@{ Label = $run.Version.Label; CleanupError = $_.Exception.Message }) }
            }
            $summaries = @()
            foreach ($version in $versions) {
                $series = @($rows | Where-Object Label -eq $version.Label)
                if ($series.Count -gt 1) { $summaries += Get-SeriesSummary $series ([Environment]::ProcessorCount) }
            }
            $result = [ordered]@{
                SchemaVersion = 2
                RunId = $runId
                Scenario = $case
                StartedUtc = $startUtc.ToString('o')
                FinishedUtc = [DateTime]::UtcNow.ToString('o')
                WarmupSeconds = $WarmupSeconds
                RequestedMeasurementSeconds = $Seconds
                SampleIntervalSeconds = 1
                LogicalProcessors = [Environment]::ProcessorCount
                OsVersion = [Environment]::OSVersion.VersionString
                PowerShellVersion = $PSVersionTable.PSVersion.ToString()
                MonitorPid = $PID
                DesktopAtStart = $desktopAtStart
                Workload = 'Unmodified live desktop; no simulated input, focus changes, synthetic WinEvents, or working-set trimming.'
                MeasurementNotes = @(
                    'Processes run concurrently on the same desktop; startup/warmup and shutdown are excluded from CPU/I/O deltas.',
                    'PrivateBytes is private committed memory; WorkingSetBytes includes shared resident pages.',
                    'I/O transfer counters describe process requests, not physical disk bytes or flush durability.',
                    'CPU cycles include user/kernel execution and depend on processor behavior; they are raw counters and are not converted to CPU time or percentages.',
                    'DesktopIdleSeconds identifies natural AFK periods; crossing a timer interval does not imply an active segment was written.',
                    'Master snapshot differs from 7c074ef only in the two Global Mutex/Stop constants, each suffixed .PerfMaster20260905.'
                )
                NativeApiSources = @('https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-getprocessiocounters', 'https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-io_counters', 'https://learn.microsoft.com/en-us/windows/win32/api/realtimeapiset/nf-realtimeapiset-queryprocesscycletime')
                ConfigToml = $config
                Binaries = $versions.ToArray()
                Instances = @($runs | ForEach-Object { [pscustomobject]@{ Label = $_.Version.Label; DataRoot = $_.DataRoot; Instance = $_.Instance; StartedUtc = $_.StartedUtc } })
                Error = $runError
                Shutdown = $stops.ToArray()
                Summary = $summaries
                Samples = $rows.ToArray()
            }
            $jsonPath = Join-Path $logDirectory 'resources.json'
            [System.IO.File]::WriteAllText($jsonPath, ($result | ConvertTo-Json -Depth 10), $utf8)
            Write-Host "Saved $jsonPath"
            $summaries | Format-Table Label, SampleCount, WorkingSetMedianBytes, PrivateMedianBytes, CpuMilliseconds, CpuCyclesDelta, WriteBytes -AutoSize | Out-Host
        }
        if ($null -ne $runError) { throw $runError }
        if (@($stops | Where-Object { $_.CleanupError -or ($_.PSObject.Properties.Name -contains 'ForcedTermination' -and $_.ForcedTermination) }).Count -gt 0) {
            throw 'At least one owned daemon required forced cleanup; inspect raw results.'
        }
    }
} finally {
    $benchmarkMutex.ReleaseMutex()
    $benchmarkMutex.Dispose()
}
