$ErrorActionPreference = "Stop"

$Root = Split-Path -Parent $PSScriptRoot
& (Join-Path $Root "bin\herdr-updater.ps1") version | Out-Null
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$DevBinary = Join-Path $Root "target\release\herdr-updater.exe"
$CachedBinary = Join-Path $Root "bin\.cache\0.3.0\herdr-updater.exe"
$SourceBinary = if (Test-Path -LiteralPath $DevBinary) { $DevBinary } else { $CachedBinary }
$DestinationDir = Join-Path $env:LOCALAPPDATA "Microsoft\WindowsApps"
New-Item -ItemType Directory -Path $DestinationDir -Force | Out-Null
$Destination = Join-Path $DestinationDir "herdr-updater.exe"
Copy-Item -LiteralPath $SourceBinary -Destination "$Destination.tmp" -Force
Move-Item -LiteralPath "$Destination.tmp" -Destination $Destination -Force
Write-Output "installed $Destination"
