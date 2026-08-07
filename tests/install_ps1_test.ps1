# Exercises install.ps1 -- the PowerShell counterpart to
# tests/install_script_test.sh, same reasoning: dot-source the real script so
# every test calls the actual functions, not a reimplementation of them that
# could drift away from what ships.
#
# Skipped by CI machinery that has no pwsh/powershell.exe on PATH (there is
# no way to run this at all without one), not failed -- this file is meant
# to be invoked directly with a real interpreter already selected, the same
# way `bash tests/install_script_test.sh` assumes bash is already the thing
# running it.

$ErrorActionPreference = 'Stop'
$failed = $false

function Test-Fail {
    param([string] $Message)
    Write-Host "FAIL: $Message" -ForegroundColor Red
    $script:failed = $true
}

function Test-Pass {
    param([string] $Message)
    Write-Host "PASS: $Message" -ForegroundColor Green
}

$repoRoot = Split-Path -Parent $PSScriptRoot
. (Join-Path $repoRoot 'install.ps1')

# --- Get-Arch -----------------------------------------------------------------
# Deterministic, not "returns something from a known set" -- that weaker
# check is exactly what let the real bug this guards against ship in the
# first place: on a real Windows machine (PowerShell 5.1, not the
# PowerShell 7 this was developed against), [RuntimeInformation]::
# OSArchitecture didn't resolve the way it did in testing, and Get-Arch
# silently returned "unsupported" on an ordinary 64-bit machine -- which
# *is* one of the "known set" values, so a test that only checked set
# membership would never have caught it. Env vars are saved and restored so
# this doesn't leak into any other test or the real environment.
$savedArchitecture = $env:PROCESSOR_ARCHITECTURE
$savedArchitew6432 = $env:PROCESSOR_ARCHITEW6432

function Test-Arch {
    param([string] $Architecture, [string] $Architew6432, [string] $Expected, [string] $Description)
    $env:PROCESSOR_ARCHITECTURE = $Architecture
    if ($Architew6432) { $env:PROCESSOR_ARCHITEW6432 = $Architew6432 } else { Remove-Item Env:\PROCESSOR_ARCHITEW6432 -ErrorAction SilentlyContinue }
    $got = Get-Arch
    if ($got -eq $Expected) {
        Test-Pass "Get-Arch: $Description"
    } else {
        Test-Fail "Get-Arch: $Description -- expected '$Expected', got '$got'"
    }
}

Test-Arch -Architecture 'AMD64' -Architew6432 $null -Expected 'x86_64' -Description '64-bit Intel/AMD reports AMD64'
Test-Arch -Architecture 'ARM64' -Architew6432 $null -Expected 'arm64' -Description '64-bit ARM reports ARM64'
Test-Arch -Architecture 'x86' -Architew6432 $null -Expected 'unsupported' -Description 'genuine 32-bit x86 is unsupported'
# The WOW64 case: a 32-bit PowerShell process running on a 64-bit OS reports
# PROCESSOR_ARCHITECTURE=x86 (its own bitness), but PROCESSOR_ARCHITEW6432
# carries the *actual* OS architecture -- must win when both are present.
Test-Arch -Architecture 'x86' -Architew6432 'AMD64' -Expected 'x86_64' -Description 'PROCESSOR_ARCHITEW6432 overrides a WOW64-reported x86'
Test-Arch -Architecture '' -Architew6432 $null -Expected 'unsupported' -Description 'neither variable set is unsupported, not a crash'

if ($null -ne $savedArchitecture) { $env:PROCESSOR_ARCHITECTURE = $savedArchitecture } else { Remove-Item Env:\PROCESSOR_ARCHITECTURE -ErrorAction SilentlyContinue }
if ($null -ne $savedArchitew6432) { $env:PROCESSOR_ARCHITEW6432 = $savedArchitew6432 } else { Remove-Item Env:\PROCESSOR_ARCHITEW6432 -ErrorAction SilentlyContinue }

# --- Get-AssetDownloadUrl ------------------------------------------------------
# Pure and fixture-driven, like asset_download_url's own bash tests -- no
# network needed to prove the lookup logic itself is right.
$fixtureRelease = [PSCustomObject]@{
    tag_name = 'v0.9.0'
    assets   = @(
        [PSCustomObject]@{ name = 'tuisample-code-linux-x86_64'; browser_download_url = 'https://example.com/linux-x86_64' }
        [PSCustomObject]@{ name = 'tuisample-code-windows-x86_64.exe'; browser_download_url = 'https://example.com/windows-x86_64.exe' }
        [PSCustomObject]@{ name = 'SHA256SUMS.txt'; browser_download_url = 'https://example.com/sums' }
    )
}

$url = Get-AssetDownloadUrl -Release $fixtureRelease -AssetName 'tuisample-code-windows-x86_64.exe'
if ($url -eq 'https://example.com/windows-x86_64.exe') {
    Test-Pass 'Get-AssetDownloadUrl finds the right asset URL'
} else {
    Test-Fail "Get-AssetDownloadUrl returned wrong URL: $url"
}

$missing = Get-AssetDownloadUrl -Release $fixtureRelease -AssetName 'tuisample-code-windows-arm64.exe'
if ($null -eq $missing) {
    Test-Pass 'Get-AssetDownloadUrl returns $null for an asset that is not in the release'
} else {
    Test-Fail "Get-AssetDownloadUrl should have returned `$null, got: $missing"
}

# --- Get-PrebuiltBinary: failure path -------------------------------------------
# A bad API base must fail fast and leave nothing behind, the exact shape of
# "no release has been published yet" that Main must recover from with a
# clear error rather than a stack trace or a half-written file.
$env:TUISAMPLE_RELEASE_API_BASE = 'http://127.0.0.1:1'
$badDest = Join-Path ([System.IO.Path]::GetTempPath()) "tuisample-ps1-test-should-not-exist-$PID.exe"
Remove-Item -Force $badDest -ErrorAction SilentlyContinue
$start = Get-Date
try {
    Get-PrebuiltBinary -AssetName 'tuisample-code-linux-x86_64' -Dest $badDest
    Test-Fail 'Get-PrebuiltBinary should have thrown when the API is unreachable'
} catch {
    Test-Pass 'Get-PrebuiltBinary throws when the API is unreachable'
}
$elapsed = ((Get-Date) - $start).TotalSeconds
if (Test-Path $badDest) {
    Test-Fail 'a failed fetch must not leave a partial file behind'
} else {
    Test-Pass 'a failed fetch leaves nothing behind'
}
if ($elapsed -lt 5) {
    Test-Pass "a refused connection fails fast (${elapsed}s), not the full timeout"
} else {
    Test-Fail "a refused connection should fail fast, took ${elapsed}s"
}
Remove-Item Env:\TUISAMPLE_RELEASE_API_BASE -ErrorAction SilentlyContinue

# --- Get-PrebuiltBinary: the real thing -----------------------------------------
# Against the actual published release, same convention as tools.rs's and
# install.sh's own "real thing, skipped if unreachable" tests.
$realDest = Join-Path ([System.IO.Path]::GetTempPath()) "tuisample-ps1-test-real-$PID.exe"
Remove-Item -Force $realDest -ErrorAction SilentlyContinue
try {
    Get-PrebuiltBinary -AssetName 'tuisample-code-windows-x86_64.exe' -Dest $realDest
    if ((Get-Item $realDest).Length -gt 0) {
        Test-Pass 'Get-PrebuiltBinary downloads and checksum-verifies the real released binary'
    } else {
        Test-Fail 'downloaded file is empty'
    }
} catch {
    Write-Host "SKIP: Get-PrebuiltBinary real-network test ($_)"
}
Remove-Item -Force $realDest -ErrorAction SilentlyContinue

# --- Send-InstallPing -----------------------------------------------------------
$fakeHome = Join-Path ([System.IO.Path]::GetTempPath()) "tuisample-ps1-test-home-$PID"
Remove-Item -Recurse -Force $fakeHome -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $fakeHome | Out-Null
$env:USERPROFILE = $fakeHome
$idFile = Join-Path $fakeHome '.tuisample-code\device_id'

$env:TUISAMPLE_TELEMETRY_URL = 'off'
Send-InstallPing -Version '9.9.9'
if (Test-Path $idFile) {
    Test-Fail 'the "off" sentinel should disable sending'
} else {
    Test-Pass 'the "off" sentinel disables sending'
}

$env:TUISAMPLE_TELEMETRY_URL = 'http://127.0.0.1:1/nowhere'
$start = Get-Date
Send-InstallPing -Version ''
$elapsed = ((Get-Date) - $start).TotalSeconds
if ((Test-Path $idFile) -and $elapsed -lt 5) {
    Test-Pass "Send-InstallPing with an unreachable endpoint and an empty version does not throw or block (${elapsed}s)"
} else {
    Test-Fail "Send-InstallPing should have created a device id quickly even with an empty version, elapsed=${elapsed}s exists=$(Test-Path $idFile)"
}

$firstId = (Get-Content $idFile -Raw).Trim()
Send-InstallPing -Version '9.9.9'
$secondId = (Get-Content $idFile -Raw).Trim()
if ($firstId -eq $secondId) {
    Test-Pass 'device id is stable across calls, not regenerated'
} else {
    Test-Fail 'device id should not have changed between calls'
}

Remove-Item -Recurse -Force $fakeHome -ErrorAction SilentlyContinue
Remove-Item Env:\TUISAMPLE_TELEMETRY_URL -ErrorAction SilentlyContinue

# --- Main: full end-to-end run, sandboxed ---------------------------------------
$sandboxHome = Join-Path ([System.IO.Path]::GetTempPath()) "tuisample-ps1-test-sandbox-$PID"
$sandboxAppData = Join-Path $sandboxHome 'LocalAppData'
Remove-Item -Recurse -Force $sandboxHome -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $sandboxAppData | Out-Null

$env:USERPROFILE = $sandboxHome
$env:LOCALAPPDATA = $sandboxAppData
$env:TUISAMPLE_TELEMETRY_URL = 'off'
# A normal 64-bit Windows machine reports AMD64 -- without pinning this,
# Get-Arch reads whatever this machine's own PROCESSOR_ARCHITECTURE
# actually is, which is unset entirely on Linux/macOS. That would make this
# test silently skip forever in CI (ubuntu-latest has no such variable),
# exactly the kind of quiet coverage loss that let the real Get-Arch bug
# through in the first place.
$env:PROCESSOR_ARCHITECTURE = 'AMD64'
Remove-Item Env:\PROCESSOR_ARCHITEW6432 -ErrorAction SilentlyContinue
try {
    Main
    $installedExe = Join-Path $sandboxAppData 'Programs\tuisample-code\tuisample-code.exe'
    if ((Test-Path $installedExe) -and (Get-Item $installedExe).Length -gt 0) {
        Test-Pass 'Main runs end-to-end and installs a real, non-empty binary'
    } else {
        Test-Fail 'Main completed but no binary was installed'
    }
} catch {
    Write-Host "SKIP: Main end-to-end test ($_)"
}
Remove-Item -Recurse -Force $sandboxHome -ErrorAction SilentlyContinue
Remove-Item Env:\TUISAMPLE_TELEMETRY_URL -ErrorAction SilentlyContinue
Remove-Item Env:\PROCESSOR_ARCHITECTURE -ErrorAction SilentlyContinue

if ($failed) {
    Write-Host ''
    Write-Host 'Some install.ps1 tests FAILED.' -ForegroundColor Red
    exit 1
} else {
    Write-Host ''
    Write-Host 'All install.ps1 tests passed.' -ForegroundColor Green
    exit 0
}
