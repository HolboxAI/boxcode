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

# --- Get-PythonStandaloneTarget -------------------------------------------------
# Deterministic and pure -- no reason for this one to depend on the network
# or the machine's real architecture.
if ((Get-PythonStandaloneTarget -Arch 'x86_64') -eq 'x86_64-pc-windows-msvc') {
    Test-Pass 'Get-PythonStandaloneTarget maps x86_64 to the msvc target'
} else {
    Test-Fail "Get-PythonStandaloneTarget(x86_64) returned $(Get-PythonStandaloneTarget -Arch 'x86_64')"
}
if ($null -eq (Get-PythonStandaloneTarget -Arch 'arm64')) {
    Test-Pass 'Get-PythonStandaloneTarget has no arm64 target, same as release.yml builds none'
} else {
    Test-Fail 'Get-PythonStandaloneTarget should not have an arm64 target'
}

# --- Install-EmbeddedPython: failure path ---------------------------------------
# A bad base URL must fail fast and leave nothing behind, the same
# "no release reachable" shape Get-PrebuiltBinary's own failure test covers.
$badPythonHome = Join-Path ([System.IO.Path]::GetTempPath()) "tuisample-ps1-badpython-$PID"
Remove-Item -Recurse -Force $badPythonHome -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $badPythonHome | Out-Null
$env:USERPROFILE = $badPythonHome
$env:PROCESSOR_ARCHITECTURE = 'AMD64'
Remove-Item Env:\PROCESSOR_ARCHITEW6432 -ErrorAction SilentlyContinue
$env:TUISAMPLE_PYTHON_STANDALONE_URL = 'http://127.0.0.1:1'
$start = Get-Date
if (Install-EmbeddedPython) {
    Test-Fail 'Install-EmbeddedPython should have failed against an unreachable URL'
} else {
    Test-Pass 'Install-EmbeddedPython fails when the download is unreachable'
}
$elapsed = ((Get-Date) - $start).TotalSeconds
if ($elapsed -lt 10) {
    Test-Pass "a refused connection fails fast (${elapsed}s), not the full timeout"
} else {
    Test-Fail "took too long to fail: ${elapsed}s"
}
if (Test-Path (Get-EmbeddedPythonDir)) {
    Test-Fail 'a failed download must not leave a partial embedded Python behind'
} else {
    Test-Pass 'a failed download leaves nothing behind'
}
Remove-Item Env:\TUISAMPLE_PYTHON_STANDALONE_URL -ErrorAction SilentlyContinue
Remove-Item -Recurse -Force $badPythonHome -ErrorAction SilentlyContinue

# --- Install-EmbeddedPython: the real thing --------------------------------------
# Against the actual published python-build-standalone release -- skipped
# rather than failed when unreachable, same convention as this file's other
# "the real thing" tests. Can't execute the resulting python.exe from here
# (it's a real Windows PE binary, this is running on Linux), so this proves
# the download/extraction pipeline for real and stops there; Install-Ddgs's
# own logic once a Python is found is exercised separately below with a
# stand-in that this OS can actually run.
$realPythonHome = Join-Path ([System.IO.Path]::GetTempPath()) "tuisample-ps1-realpython-$PID"
Remove-Item -Recurse -Force $realPythonHome -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $realPythonHome | Out-Null
$env:USERPROFILE = $realPythonHome
$env:PROCESSOR_ARCHITECTURE = 'AMD64'
Remove-Item Env:\PROCESSOR_ARCHITEW6432 -ErrorAction SilentlyContinue
try {
    if (Install-EmbeddedPython) {
        $exe = Join-Path (Get-EmbeddedPythonDir) 'python.exe'
        if ((Test-Path $exe) -and (Get-Item $exe).Length -gt 0) {
            Test-Pass 'Install-EmbeddedPython downloads and extracts a real, non-empty python.exe'
        } else {
            Test-Fail 'Install-EmbeddedPython reported success but python.exe is missing or empty'
        }
    } else {
        Write-Host 'SKIP: Install-EmbeddedPython real-network test (returned false)'
    }
} catch {
    Write-Host "SKIP: Install-EmbeddedPython real-network test ($_)"
}
Remove-Item -Recurse -Force $realPythonHome -ErrorAction SilentlyContinue

# --- Install-Ddgs: falls back to an embedded Python when none is on PATH --------
# A fake "python" -- a tiny script standing in for python.exe, the same
# trick tools.rs's own tests use for python_bin -- pre-seeded directly at
# Get-EmbeddedPythonDir's python.exe path. Install-EmbeddedPython's own
# idempotency check (`if python.exe already exists, done`) then treats it
# as already installed and never touches the network, so this exercises
# Install-Ddgs's real fallback *logic* deterministically, without needing a
# real Windows python.exe this OS could never execute anyway.
$fallbackHome = Join-Path ([System.IO.Path]::GetTempPath()) "tuisample-ps1-fallback-$PID"
Remove-Item -Recurse -Force $fallbackHome -ErrorAction SilentlyContinue
$fakeEmbeddedDir = Join-Path $fallbackHome '.tuisample-code\python'
New-Item -ItemType Directory -Force -Path $fakeEmbeddedDir | Out-Null
$fakePythonExe = Join-Path $fakeEmbeddedDir 'python.exe'
# On Unix (this test's actual host, whatever the platform under test), a
# shebang script works the same way through `&` invocation as a real
# executable would -- there is nothing Windows-specific about the *logic*
# Install-Ddgs runs once it has a python path in hand, only about which
# path that is, which Get-EmbeddedPythonDir already abstracts.
Set-Content -Path $fakePythonExe -Value @'
#!/bin/sh
if [ "$1" = "-c" ]; then
  [ -f "$(dirname "$0")/ddgs-marker" ]
  exit $?
fi
if [ "$1" = "-m" ] && [ "$2" = "pip" ]; then
  touch "$(dirname "$0")/ddgs-marker"
  exit 0
fi
exit 1
'@
chmod +x $fakePythonExe

$env:USERPROFILE = $fallbackHome
$env:PROCESSOR_ARCHITECTURE = 'AMD64'

# A curated allowlist PATH, not a denylist filter over the real one: an
# earlier version of this test filtered out any PATH directory containing
# python/python3, which on a real machine also strips *every other* tool
# that happens to live alongside them (touch, dirname, ...) -- exactly the
# tools the fake python.exe stand-in above needs once its own shebang hands
# control to /bin/sh. Same fix tests/install_script_test.sh's own
# ensure_ddgs_available tests already needed for the identical reason.
$curatedPathDir = Join-Path ([System.IO.Path]::GetTempPath()) "tuisample-ps1-curated-path-$PID"
Remove-Item -Recurse -Force $curatedPathDir -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $curatedPathDir | Out-Null
foreach ($tool in @('touch', 'dirname')) {
    $toolCmd = Get-Command $tool -ErrorAction SilentlyContinue
    if (-not $toolCmd) {
        Test-Fail "this test needs a real '$tool' on the machine running it"
        continue
    }
    New-Item -ItemType SymbolicLink -Path (Join-Path $curatedPathDir $tool) -Target $toolCmd.Source -Force | Out-Null
}

$oldPath = $env:PATH
$env:PATH = $curatedPathDir
Install-Ddgs
$env:PATH = $oldPath
Remove-Item -Recurse -Force $curatedPathDir -ErrorAction SilentlyContinue

if (Test-Path (Join-Path $fakeEmbeddedDir 'ddgs-marker')) {
    Test-Pass 'Install-Ddgs falls back to the embedded Python and installs ddgs into it when nothing is on PATH'
} else {
    Test-Fail 'Install-Ddgs should have used the pre-seeded embedded Python fallback'
}
Remove-Item -Recurse -Force $fallbackHome -ErrorAction SilentlyContinue
Remove-Item Env:\PROCESSOR_ARCHITECTURE -ErrorAction SilentlyContinue
Remove-Item Env:\PROCESSOR_ARCHITEW6432 -ErrorAction SilentlyContinue

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
