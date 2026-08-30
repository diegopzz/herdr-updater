$ErrorActionPreference = "Stop"

$Version = "0.1.4"
$Repository = "diegopzz/herdr-updater"
$Root = Split-Path -Parent $PSScriptRoot
$DevBinary = Join-Path $Root "target\release\herdr-updater.exe"
if (Test-Path -LiteralPath $DevBinary) {
    & $DevBinary @args
    exit $LASTEXITCODE
}

if ([Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne [Runtime.InteropServices.Architecture]::X64) {
    [Console]::Error.WriteLine("herdr-updater: no prebuilt Windows binary for this architecture")
    exit 2
}

$Target = "x86_64-pc-windows-msvc"
$Asset = "herdr-updater-$Version-$Target.tar.gz"
$Checksums = "checksums-$Version.txt"
$Cache = Join-Path $PSScriptRoot ".cache\$Version"
$Binary = Join-Path $Cache "herdr-updater.exe"
if (-not (Test-Path -LiteralPath $Binary)) {
    $Temp = Join-Path ([IO.Path]::GetTempPath()) ("herdr-updater-" + [Guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Path $Temp | Out-Null
    try {
        $AssetPath = Join-Path $Temp $Asset
        $ChecksumPath = Join-Path $Temp $Checksums
        $Downloaded = $false
        if (Get-Command gh -ErrorAction SilentlyContinue) {
            & gh release download "v$Version" --repo $Repository --pattern $Asset --pattern $Checksums --dir $Temp 2>$null
            $Downloaded = $LASTEXITCODE -eq 0 -and
                (Test-Path -LiteralPath $AssetPath) -and
                (Test-Path -LiteralPath $ChecksumPath)
            if (-not $Downloaded) {
                Remove-Item -LiteralPath $AssetPath, $ChecksumPath -Force -ErrorAction SilentlyContinue
            }
        }
        if (-not $Downloaded) {
            $Base = "https://github.com/$Repository/releases/download/v$Version"
            Invoke-WebRequest -UseBasicParsing -Uri "$Base/$Asset" -OutFile $AssetPath
            Invoke-WebRequest -UseBasicParsing -Uri "$Base/$Checksums" -OutFile $ChecksumPath
            $Downloaded = $true
        }

        $Expected = $null
        foreach ($Line in Get-Content -LiteralPath $ChecksumPath) {
            $Parts = $Line.Trim() -split "\s+", 2
            if ($Parts.Count -eq 2 -and $Parts[1] -eq $Asset) {
                $Expected = $Parts[0].ToLowerInvariant()
                break
            }
        }
        if (-not $Expected) { throw "release checksum does not list $Asset" }
        $Actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $AssetPath).Hash.ToLowerInvariant()
        if ($Actual -ne $Expected) { throw "checksum verification failed for $Asset" }

        & tar -xzf $AssetPath -C $Temp
        if ($LASTEXITCODE -ne 0) { throw "release archive extraction failed" }
        $Extracted = Join-Path $Temp "herdr-updater.exe"
        if (-not (Test-Path -LiteralPath $Extracted)) { throw "release archive is missing the executable" }
        New-Item -ItemType Directory -Path $Cache -Force | Out-Null
        Copy-Item -LiteralPath $Extracted -Destination "$Binary.tmp" -Force
        Move-Item -LiteralPath "$Binary.tmp" -Destination $Binary -Force
    } catch {
        [Console]::Error.WriteLine("herdr-updater: $($_.Exception.Message)")
        exit 2
    } finally {
        if (Test-Path -LiteralPath $Temp) { Remove-Item -LiteralPath $Temp -Recurse -Force }
    }
}

& $Binary @args
exit $LASTEXITCODE
