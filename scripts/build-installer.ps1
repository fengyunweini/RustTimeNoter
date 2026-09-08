# Build a release zip "RustTimeNoter-vX.Y.Z.zip" containing:
#   tracker.exe
#   install.bat
#   uninstall.bat
#   view.bat
#   README.txt
# Also writes SHA256SUMS.txt for tracker.exe and the zip.

param(
    [ValidateNotNullOrEmpty()]
    [string]$TargetDirectory = "target"
)

$ErrorActionPreference = "Stop"
$repoRoot = [System.IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
Set-Location $repoRoot

function Invoke-CheckedCommand {
    param(
        [string]$Command,
        [string[]]$Arguments
    )

    # Native tools use stderr for successful progress messages too. Preserve
    # stdout separately (metadata is JSON) and always check the native exit.
    $commandPath = (Get-Command -Name $Command -CommandType Application -ErrorAction Stop).Source
    $previousPreference = $ErrorActionPreference
    try {
        $ErrorActionPreference = "Continue"
        & $commandPath @Arguments
        $commandSucceeded = $?
        $commandExit = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $previousPreference
    }
    if (-not $commandSucceeded -or $commandExit -ne 0) {
        throw "$Command failed (exit $commandExit)"
    }
}

if ([System.IO.Path]::IsPathRooted($TargetDirectory)) {
    $targetRoot = [System.IO.Path]::GetFullPath($TargetDirectory)
}
else {
    $targetRoot = [System.IO.Path]::GetFullPath((Join-Path $repoRoot $TargetDirectory))
}

$metadataText = Invoke-CheckedCommand -Command "cargo" -Arguments @(
    "metadata", "--locked", "--no-deps", "--format-version", "1"
)
$metadata = ($metadataText -join "`n") | ConvertFrom-Json
$rootManifest = [System.IO.Path]::GetFullPath((Join-Path $repoRoot "Cargo.toml"))
$rootPackages = @($metadata.packages | Where-Object {
    $_.name -eq "tracker" -and
    [System.IO.Path]::GetFullPath($_.manifest_path) -eq $rootManifest
})
if ($rootPackages.Count -ne 1 -or -not $rootPackages[0].version) {
    throw "cargo metadata did not identify the tracker root package version"
}
$ver = [string]$rootPackages[0].version

Invoke-CheckedCommand -Command "cargo" -Arguments @(
    "build", "--locked", "--release", "--bin", "tracker", "--target-dir", $targetRoot
) | Out-Host

$exe = Join-Path $targetRoot "release\tracker.exe"
if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) {
    throw "tracker.exe not built at $exe"
}
$exeVersion = (Invoke-CheckedCommand -Command $exe -Arguments @("--version") | Out-String).Trim()
if ($exeVersion -cne "tracker $ver") {
    throw "built executable version '$exeVersion' does not match tracker $ver"
}

$archiveFiles = @(
    $exe
    (Join-Path $repoRoot "dist\install.bat")
    (Join-Path $repoRoot "dist\uninstall.bat")
    (Join-Path $repoRoot "dist\view.bat")
    (Join-Path $repoRoot "dist\README.txt")
)
foreach ($file in $archiveFiles) {
    if (-not (Test-Path -LiteralPath $file -PathType Leaf)) {
        throw "missing release file: $file"
    }
}

$zipPath = Join-Path $targetRoot ("RustTimeNoter-v{0}.zip" -f $ver)
Compress-Archive -LiteralPath $archiveFiles -DestinationPath $zipPath -CompressionLevel Optimal -Force

$checksumsPath = Join-Path $targetRoot "SHA256SUMS.txt"
$checksums = foreach ($file in @($exe, $zipPath)) {
    $hash = (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash.ToLowerInvariant()
    "{0} *{1}" -f $hash, [System.IO.Path]::GetFileName($file)
}
[System.IO.File]::WriteAllText(
    $checksumsPath,
    (($checksums -join "`n") + "`n"),
    [System.Text.Encoding]::ASCII
)

$exeSize = (Get-Item -LiteralPath $exe).Length
$zipSize = (Get-Item -LiteralPath $zipPath).Length
"`nBuilt:"
"  version     : {0}" -f $ver
"  tracker.exe : {0:N0} bytes ({1:N1} KB)" -f $exeSize, ($exeSize/1KB)
"  zip         : {0:N0} bytes ({1:N1} KB)" -f $zipSize, ($zipSize/1KB)
"  path        : $zipPath"
"  checksums   : $checksumsPath"
