# Build a release zip "RustTimeNoter-vX.Y.Z.zip" containing:
#   tracker.exe
#   install.bat
#   uninstall.bat
#   view.bat
#   README.txt

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

# cargo writes "Finished ..." to stderr; with $ErrorActionPreference=Stop that
# would abort the script even on a successful build. Run cargo under a relaxed
# preference and check the exit code explicitly.
$prev = $ErrorActionPreference
$ErrorActionPreference = "Continue"
& cargo build --release 2>&1 | Out-Host
$cargoExit = $LASTEXITCODE
$ErrorActionPreference = $prev
if ($cargoExit -ne 0) { throw "cargo build failed (exit $cargoExit)" }

$exe = Join-Path $repoRoot "target\release\tracker.exe"
if (-not (Test-Path $exe)) { throw "tracker.exe not built" }

$ver = (cargo pkgid 2>$null) -replace '.*[#@]', ''
if (-not $ver) { $ver = "0.1.0" }

$staging = Join-Path $repoRoot "target\dist-staging"
if (Test-Path $staging) { Remove-Item $staging -Recurse -Force }
New-Item -ItemType Directory -Path $staging -Force | Out-Null

Copy-Item $exe                                $staging
Copy-Item (Join-Path $PSScriptRoot "..\dist\install.bat")   $staging
Copy-Item (Join-Path $PSScriptRoot "..\dist\uninstall.bat") $staging
Copy-Item (Join-Path $PSScriptRoot "..\dist\view.bat")      $staging
Copy-Item (Join-Path $PSScriptRoot "..\dist\README.txt")    $staging

$zipPath = Join-Path $repoRoot ("target\RustTimeNoter-v{0}.zip" -f $ver)
if (Test-Path $zipPath) { Remove-Item $zipPath -Force }
Compress-Archive -Path (Join-Path $staging "*") -DestinationPath $zipPath -CompressionLevel Optimal

$exeSize = (Get-Item $exe).Length
$zipSize = (Get-Item $zipPath).Length
"`nBuilt:"
"  tracker.exe : {0:N0} bytes ({1:N1} KB)" -f $exeSize, ($exeSize/1KB)
"  zip         : {0:N0} bytes ({1:N1} KB)" -f $zipSize, ($zipSize/1KB)
"  path        : $zipPath"
