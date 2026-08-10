# boxcode installer (Windows PowerShell)
#
# The PowerShell counterpart to install.sh: `curl | bash` doesn't work in
# native PowerShell (no bash, and `|` pipes objects, not text), so Windows
# users need their own entry point --
#   irm https://raw.githubusercontent.com/HolboxAI/boxcode/main/install.ps1 | iex
# is that platform's equivalent one-liner (Invoke-RestMethod | Invoke-Expression).
#
# Unlike install.sh, there is no source-build fallback here: building Rust on
# Windows needs the MSVC Build Tools, a much bigger ask than `rustup` alone,
# so this only ever fetches the prebuilt binary release.yml already produces.
# If that's ever missing (no release published, or this platform genuinely
# isn't built), this fails with a clear message rather than trying to set up
# a C++ toolchain unasked.
#
# Functions only below `Main` is never called automatically when this file is
# dot-sourced (`. .\install.ps1`) rather than run directly -- that's what
# lets tests call individual functions in isolation, the same way
# tests/install_script_test.sh does with install.sh's own functions.

$ErrorActionPreference = 'Stop'

# Where release assets and checksums are published. Overridable so a fork or
# an internal mirror can serve its own builds -- mirrors install.sh's own
# BOXCODE_RELEASE_API_BASE.
function Get-ReleaseApiBase {
    if ($env:BOXCODE_RELEASE_API_BASE) {
        return $env:BOXCODE_RELEASE_API_BASE
    }
    return 'https://api.github.com/repos/HolboxAI/boxcode'
}

# Only `x86_64` is actually built by release.yml today; this still reports
# `arm64` distinctly (rather than folding it into "unsupported") so the
# no-prebuilt-binary error message can say which architecture it looked for.
function Get-Arch {
    # Deliberately not [System.Runtime.InteropServices.RuntimeInformation]::
    # OSArchitecture -- confirmed on a real Windows machine (Windows
    # PowerShell 5.1, not PowerShell 7, where this was actually tested
    # before shipping) that it does not reliably resolve there, silently
    # reporting an architecture this switch didn't recognise. Environment
    # variables are the standard, battle-tested way to detect this that
    # works identically across every PowerShell version back to 2.0 --
    # rustup's own install script uses the same approach for the same
    # reason.
    #
    # PROCESSOR_ARCHITEW6432 is set (and reports the *true* OS
    # architecture) only when this process itself is running under WOW64 --
    # a 32-bit process on a 64-bit OS, where PROCESSOR_ARCHITECTURE alone
    # would misreport "x86" -- so it takes precedence when present.
    $raw = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    switch ($raw) {
        'AMD64' { return 'x86_64' }
        'ARM64' { return 'arm64' }
        default { return 'unsupported' }
    }
}

# Pulls the download URL for one named asset out of a GitHub "get the latest
# release" API response. `Invoke-RestMethod` already parses the JSON into
# objects, so unlike install.sh's grep/sed approach this is just a filter --
# no hand-rolled parsing to keep in sync with GitHub's response shape.
function Get-AssetDownloadUrl {
    param(
        [Parameter(Mandatory)] $Release,
        [Parameter(Mandatory)] [string] $AssetName
    )
    $asset = $Release.assets | Where-Object { $_.name -eq $AssetName } | Select-Object -First 1
    if ($asset) {
        return $asset.browser_download_url
    }
    return $null
}

# Downloads the release asset matching `$AssetName` to `$Dest`, verifying it
# against SHA256SUMS.txt when the release publishes one (older releases, from
# before that file existed, will not -- a missed check, not a reason to
# refuse an otherwise-good binary). Throws on anything short of a verified
# (or unverifiable-but-present) binary landing at `$Dest` -- the caller is
# expected to catch that and report it as "no prebuilt binary available",
# since every failure mode here (no release yet, no asset for this platform,
# a network failure, a checksum mismatch) is exactly that.
function Get-PrebuiltBinary {
    param(
        [Parameter(Mandatory)] [string] $AssetName,
        [Parameter(Mandatory)] [string] $Dest
    )

    $release = Invoke-RestMethod -Uri "$(Get-ReleaseApiBase)/releases/latest" -TimeoutSec 15
    $downloadUrl = Get-AssetDownloadUrl -Release $release -AssetName $AssetName
    if (-not $downloadUrl) {
        throw "no '$AssetName' asset in the latest release"
    }

    Invoke-WebRequest -Uri $downloadUrl -OutFile $Dest -TimeoutSec 60

    $sumsUrl = Get-AssetDownloadUrl -Release $release -AssetName 'SHA256SUMS.txt'
    if ($sumsUrl) {
        $sums = Invoke-RestMethod -Uri $sumsUrl -TimeoutSec 15
        $expectedLine = ($sums -split "`n") | Where-Object { $_ -match "\s$([regex]::Escape($AssetName))\s*$" } | Select-Object -First 1
        if ($expectedLine) {
            $expected = ($expectedLine -split '\s+')[0].Trim().ToLowerInvariant()
            $actual = (Get-FileHash -Algorithm SHA256 -Path $Dest).Hash.ToLowerInvariant()
            if ($expected -ne $actual) {
                Remove-Item -Force $Dest -ErrorAction SilentlyContinue
                throw "checksum mismatch for $AssetName -- refusing to install a corrupted download"
            }
        }
    }
}

# TEMPORARY, see tools.rs's own doc comment: python-build-standalone is a
# stop-gap for machines with no Python at all, not a permanent architecture
# decision -- the Unix counterpart of this whole section is install.sh's
# install_embedded_python, which has the fuller reasoning (pinned release,
# why not "latest", etc.). Kept in sync with it by hand for now.
$PythonStandaloneRelease = '20260807'
$PythonStandaloneVersion = '3.12.13'
function Get-PythonStandaloneBaseUrl {
    # A function re-evaluated on every call, not a variable fixed once at
    # dot-source time -- same reason Get-ReleaseApiBase above is one: a
    # test (or a fork) that sets $env:BOXCODE_PYTHON_STANDALONE_URL after
    # this file has already been dot-sourced needs that to actually take
    # effect.
    if ($env:BOXCODE_PYTHON_STANDALONE_URL) {
        return $env:BOXCODE_PYTHON_STANDALONE_URL
    }
    return 'https://github.com/astral-sh/python-build-standalone/releases/download'
}

# Only x86_64 -- release.yml does not build a Windows arm64 boxcode
# either, so there is no reason to promise a Windows arm64 Python here.
function Get-PythonStandaloneTarget {
    param([string] $Arch)
    if ($Arch -eq 'x86_64') { return 'x86_64-pc-windows-msvc' }
    return $null
}

function Get-EmbeddedPythonDir {
    Join-Path $env:USERPROFILE '.boxcode\python'
}

# Downloads and extracts a self-contained Python for machines with no system
# Python at all. Idempotent (does nothing if one is already there from a
# previous run) and never throws -- every failure is a return, one option
# among several Install-Ddgs tries, not something that should ever be
# allowed to fail the install itself.
function Install-EmbeddedPython {
    $dir = Get-EmbeddedPythonDir
    if (Test-Path (Join-Path $dir 'python.exe')) {
        return $true
    }

    $target = Get-PythonStandaloneTarget -Arch (Get-Arch)
    if (-not $target) {
        return $false
    }

    $url = "$(Get-PythonStandaloneBaseUrl)/$PythonStandaloneRelease/cpython-$PythonStandaloneVersion+$PythonStandaloneRelease-$target-install_only_stripped.tar.gz"
    $tmpDir = Join-Path ([System.IO.Path]::GetTempPath()) "boxcode-python-$PID"
    Remove-Item -Recurse -Force $tmpDir -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force -Path $tmpDir | Out-Null
    $archive = Join-Path $tmpDir 'python.tar.gz'

    Write-Host "Python not found -- downloading a self-contained one for web_search..."
    try {
        Invoke-WebRequest -Uri $url -OutFile $archive -TimeoutSec 120
    } catch {
        Remove-Item -Recurse -Force $tmpDir -ErrorAction SilentlyContinue
        return $false
    }

    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $dir) | Out-Null
    Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
    # tar, not Expand-Archive: the asset is a .tar.gz on every platform
    # release.yml/python-build-standalone publish, Windows included -- one
    # archive format everywhere rather than a Windows-only special case.
    # Bundled with Windows itself since the 1803 update, so this needs no
    # extra tooling beyond what a modern Windows already has.
    & tar xzf $archive -C $tmpDir 2>$null
    if ($LASTEXITCODE -ne 0) {
        Remove-Item -Recurse -Force $tmpDir -ErrorAction SilentlyContinue
        return $false
    }
    # Every python-build-standalone release extracts to a top-level "python"
    # directory regardless of platform or version -- confirmed against the
    # real release this is pinned to, not assumed.
    Move-Item -Force -Path (Join-Path $tmpDir 'python') -Destination $dir
    Remove-Item -Recurse -Force $tmpDir -ErrorAction SilentlyContinue

    return Test-Path (Join-Path $dir 'python.exe')
}

# Windows ships `python.exe`/`python3.exe` "App Execution Alias" stubs in
# WindowsApps that sit on PATH by default -- even on a machine with no real
# Python installed. Get-Command finds them like any real binary (they are
# real files, just useless ones), so without this check Install-Ddgs below
# would think a real Python was found, skip Install-EmbeddedPython
# entirely, and then fail confusingly trying to pip-install into the stub.
# See tools.rs's matching looks_like_windows_app_execution_alias_stub for
# the runtime side of this same problem. Matched by path rather than by
# running it, since running it can be slow or pop the Store depending on
# Windows version/settings.
function Test-IsAppExecutionAliasStub {
    param([string] $Path)
    return ($Path -like '*\WindowsApps\python.exe') -or ($Path -like '*\WindowsApps\python3.exe')
}

# `web_search` needs Python's `ddgs` package -- see tools.rs's own doc
# comment for why it shells out to Python rather than a pure-Rust HTTP call,
# and install.sh's ensure_ddgs_available for the Unix counterpart of this
# same step. Best-effort in every direction: a python that genuinely can't
# be gotten (see Install-EmbeddedPython) means web_search simply won't work
# (it already explains that clearly when actually used), and a failed pip
# install is reported but never fatal to the install itself.
function Install-Ddgs {
    $pythonPath = $null
    $embedded = $false

    $found = Get-Command python -ErrorAction SilentlyContinue
    if ($found -and (Test-IsAppExecutionAliasStub $found.Source)) {
        $found = $null
    }
    if (-not $found) {
        $found = Get-Command python3 -ErrorAction SilentlyContinue
        if ($found -and (Test-IsAppExecutionAliasStub $found.Source)) {
            $found = $null
        }
    }
    if ($found) {
        $pythonPath = $found.Source
    } elseif (Install-EmbeddedPython) {
        $pythonPath = Join-Path (Get-EmbeddedPythonDir) 'python.exe'
        $embedded = $true
    } else {
        # Install-EmbeddedPython already said it was trying -- leaving it at
        # that would look like this silently hung or half-worked rather
        # than plainly failed.
        Write-Host "  Could not install 'ddgs' automatically. web_search will explain how"
        Write-Host "  to install it yourself (pip install ddgs) if you end up using it."
        return
    }

    & $pythonPath -c 'import ddgs' 2>$null
    if ($LASTEXITCODE -eq 0) {
        return
    }

    Write-Host "Installing the 'ddgs' Python package (needed for web_search)..."
    if ($embedded) {
        # Not a system install at all -- there is no reason to scope this to
        # a "user" site when the whole directory is already ours alone.
        & $pythonPath -m pip install ddgs *> $null
    } else {
        & $pythonPath -m pip install --user ddgs *> $null
    }

    & $pythonPath -c 'import ddgs' 2>$null
    if ($LASTEXITCODE -eq 0) {
        Write-Host "  ddgs installed"
    } else {
        Write-Host "  Could not install 'ddgs' automatically. web_search will explain how"
        Write-Host "  to install it yourself (pip install ddgs) if you end up using it."
    }
}

# Anonymous "an install happened" ping -- the PowerShell counterpart to
# install.sh's ping_install and telemetry.rs's own `active` ping, which this
# binary hasn't run yet to send. A random id in
# $env:USERPROFILE\.boxcode\device_id labels this machine, not the
# person running it, and is the same file/format telemetry.rs itself reads
# and reuses later rather than generating a second, conflicting id.
#
# Synchronous with a short timeout rather than backgrounded like
# install.sh's -- true fire-and-forget needs a job or a runspace, and a few
# seconds' delay at the very end of the install, after everything that
# actually matters has already happened, is an acceptable simplification.
# Every failure mode here is swallowed; this must never fail the install.
function Send-InstallPing {
    # Deliberately not Mandatory: a mandatory parameter's binding is checked
    # *before* the function body runs, so an empty version string (the
    # binary failing to report one, for whatever reason) would throw right
    # at the call site under $ErrorActionPreference = 'Stop' -- skipping
    # every bit of this function's own best-effort error handling and
    # crashing the install on its very last, least important step.
    param([string] $Version)
    if (-not $Version) {
        $Version = 'unknown'
    }

    $defaultUrl = 'https://tui-telemetry.dhruvm307.workers.dev'
    # install.sh distinguishes "unset" (use the default) from "explicitly
    # set to empty" (disable, even though the default is non-blank) via
    # `${VAR-default}`. That distinction is not available here: PowerShell's
    # `$env:X = ''` does not store an empty value, it deletes the variable
    # outright (confirmed directly -- `Test-Path env:X` is $false
    # afterwards), so by the time this function runs, "explicitly blank" and
    # "never touched" are already indistinguishable. `off` is therefore the
    # one reliable way to disable sending on this platform.
    $override = $env:BOXCODE_TELEMETRY_URL
    if ($override -and $override.Trim().ToLowerInvariant() -eq 'off') {
        return
    }
    $url = if ($override) { $override } else { $defaultUrl }
    if (-not $url) {
        return
    }

    try {
        $stateDir = Join-Path $env:USERPROFILE '.boxcode'
        New-Item -ItemType Directory -Force -Path $stateDir | Out-Null
        $idFile = Join-Path $stateDir 'device_id'
        if (-not (Test-Path $idFile) -or -not (Get-Content $idFile -Raw -ErrorAction SilentlyContinue)) {
            [guid]::NewGuid().ToString() | Set-Content -Path $idFile -NoNewline
        }
        $deviceId = (Get-Content $idFile -Raw).Trim()
        if (-not $deviceId) {
            return
        }

        $payload = @{
            anon_id = $deviceId
            event   = 'install'
            version = $Version
            os      = 'Windows'
        } | ConvertTo-Json -Compress

        Invoke-RestMethod -Uri $url -Method Post -Body $payload -ContentType 'application/json' -TimeoutSec 3 | Out-Null
    } catch {
        # Best-effort: network down, endpoint unreachable, anything -- never
        # lets a telemetry failure surface as an install failure.
    }
}

function Main {
    Write-Host 'Installing boxcode...'
    Write-Host ''

    $arch = Get-Arch
    $assetName = "boxcode-windows-$arch.exe"
    $installDir = Join-Path $env:LOCALAPPDATA 'Programs\boxcode'
    $installedAt = Join-Path $installDir 'boxcode.exe'

    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    $tempDest = Join-Path $installDir 'boxcode.exe.new'

    Write-Host "Looking for a prebuilt $arch binary..."
    try {
        Get-PrebuiltBinary -AssetName $assetName -Dest $tempDest
    } catch {
        Remove-Item -Force $tempDest -ErrorAction SilentlyContinue
        Write-Host ''
        Write-Host "No prebuilt binary is available for windows-$arch right now ($($_.Exception.Message))."
        Write-Host ''
        Write-Host 'There is no automatic source-build fallback on Windows (it needs the MSVC'
        Write-Host 'Build Tools, which this installer will not set up unasked). Options:'
        Write-Host '  - Install Rust (https://rustup.rs) and run: cargo build --release'
        Write-Host '  - Use WSL and the regular install.sh instead'
        throw 'no prebuilt binary available'
    }

    # Move-Item -Force replaces the destination even while boxcode.exe
    # (the very binary being replaced, under --upgrade) is running -- Windows
    # allows renaming an in-use executable's directory entry even though it
    # blocks overwriting its content directly, the same reason install.sh's
    # install_binary renames into place rather than writing over the target.
    Move-Item -Force -Path $tempDest -Destination $installedAt
    Write-Host "Installed to $installedAt"

    $userPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
    $pathEntries = @()
    if ($userPath) {
        $pathEntries = $userPath -split ';' | Where-Object { $_ }
    }
    if ($pathEntries -notcontains $installDir) {
        $newPath = if ($userPath) { "$userPath;$installDir" } else { $installDir }
        [Environment]::SetEnvironmentVariable('PATH', $newPath, 'User')
        Write-Host "Added $installDir to your PATH (open a new shell for it to take effect)."
    }

    $resolved = Get-Command boxcode -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($resolved -and $resolved.Source -ne $installedAt) {
        Write-Host ''
        Write-Host "WARNING: 'boxcode' currently resolves to $($resolved.Source),"
        Write-Host "  but this build was installed to $installedAt."
        Write-Host '  Remove the other copy, or fix your PATH order, or you will keep'
        Write-Host '  running the old version.'
    }

    Install-Ddgs

    $version = (& $installedAt --version 2>$null)
    Send-InstallPing -Version $version

    Write-Host ''
    Write-Host 'Installation complete!'
    Write-Host ''
    Write-Host 'Next steps:'
    Write-Host '1. Configure your LLM endpoint:'
    Write-Host '   $env:BOXCODE_ENDPOINT = "https://api.openai.com"'
    Write-Host '   $env:BOXCODE_MODEL = "gpt-4"'
    Write-Host '   $env:BOXCODE_API_KEY = "sk-..."'
    Write-Host ''
    Write-Host '2. Open a new shell (so the updated PATH takes effect), then run:'
    Write-Host '   boxcode'
    Write-Host ''
    Write-Host 'For more info: https://github.com/HolboxAI/boxcode'
}

if ($MyInvocation.InvocationName -ne '.') {
    Main
}
