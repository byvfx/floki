<#
.SYNOPSIS
    Windows dev-convenience wrapper: assemble the native build environment, then run a
    cargo task for floki.

.DESCRIPTION
    floki compiles OCIO unconditionally, and even the default `system-ocio` backend builds
    shaderc from source via the floki-ocio crate. That needs a C++ toolchain plus
    cmake + ninja + python on PATH, and (for system-ocio) a prebuilt OpenColorIO to link.

    On a typical Windows dev box those tools exist but are NOT on PATH:
      - cmake + ninja are bundled inside Visual Studio 2022
        (Common7\IDE\CommonExtensions\Microsoft\CMake\{CMake\bin,Ninja})
      - python ships with Anaconda (the bare `python` on PATH is usually the Store shim)
      - OpenColorIO / OpenEXR / Imath come from a vcpkg tree

    This script imports the MSVC environment from vcvars64.bat, prepends the bundled tools
    ahead of any Store shims, points system-ocio at the vcpkg OCIO, then invokes cargo.

    This is a LOCAL developer convenience only. CI does NOT use it (see the Test & Lint
    job in .github/workflows). Paths default to this machine's layout but are overridable
    via the environment variables noted below.

.PARAMETER Task
    run   (default) -> cargo run --release            (launch the GUI)
    test           -> cargo fmt --check, clippy -D warnings, cargo test --all-targets
    build          -> cargo build --release
    clippy         -> cargo clippy --all-targets -- -D warnings
    soak           -> cargo run --release with RUST_LOG=floki::playback=debug, teeing
                      the 1 Hz playback trace to soak-logs\soak-<timestamp>.log (#100)
    inspect        -> cargo run --release --bin inspect_exr (parts / compression /
                      channel counts for the soak manifest, #100 Phase 0)
    Any trailing args after the task are passed through, e.g.:
      scripts\run-windows.ps1 run -- "C:\path\to\image.exr"
    For run / soak / inspect they go to the *program*; for build / test, to cargo.

.ENVIRONMENT
    FLOKI_VCPKG     vcpkg root         (default: G:\__projects\_programming\vcpkg)
    FLOKI_ANACONDA  Anaconda root      (default: C:\ProgramData\anaconda3)
    FLOKI_VSPATH    VS install path    (default: auto-detected via vswhere, else VS 2022 Community)

.EXAMPLE
    scripts\run-windows.ps1 test
    scripts\run-windows.ps1            # same as: run
    scripts\run-windows.ps1 run -- "D:\shots\comp_v003.exr"
    scripts\run-windows.ps1 soak -- "X:\seq\shot.0001.exr"
    scripts\run-windows.ps1 inspect -- "X:\seq\shot.0001.exr"
#>
[CmdletBinding()]
param(
    [ValidateSet('run', 'test', 'build', 'clippy', 'soak', 'inspect')]
    [string]$Task = 'run',

    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$CargoArgs
)

$ErrorActionPreference = 'Stop'

function Die($msg) { Write-Error $msg; exit 1 }

# --- Resolve Visual Studio (for vcvars64 + bundled cmake/ninja) -----------------------
$vsPath = $env:FLOKI_VSPATH
if (-not $vsPath) {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path $vswhere) {
        $vsPath = (& $vswhere -latest -property installationPath 2>$null | Select-Object -First 1)
    }
}
if (-not $vsPath -or -not (Test-Path $vsPath)) {
    $vsPath = 'C:\Program Files\Microsoft Visual Studio\2022\Community'  # fallback
}
if (-not (Test-Path $vsPath)) {
    Die "Could not find Visual Studio. Set FLOKI_VSPATH to your VS install path. Looked at: $vsPath"
}

$vcvars    = Join-Path $vsPath 'VC\Auxiliary\Build\vcvars64.bat'
$cmakeBin  = Join-Path $vsPath 'Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin'
$ninjaDir  = Join-Path $vsPath 'Common7\IDE\CommonExtensions\Microsoft\CMake\Ninja'
if (-not (Test-Path $vcvars)) { Die "vcvars64.bat not found at $vcvars" }

# --- Resolve vcpkg (OpenColorIO/OpenEXR/Imath) and Anaconda (python) ------------------
$vcpkg    = if ($env:FLOKI_VCPKG)    { $env:FLOKI_VCPKG }    else { 'G:\__projects\_programming\vcpkg' }
$anaconda = if ($env:FLOKI_ANACONDA) { $env:FLOKI_ANACONDA } else { 'C:\ProgramData\anaconda3' }

$ocioRoot = Join-Path $vcpkg 'installed\x64-windows'
$vcpkgBin = Join-Path $ocioRoot 'bin'   # OpenColorIO_2_4.dll lives here (needed at runtime)

if (-not (Test-Path (Join-Path $ocioRoot 'lib\OpenColorIO.lib'))) {
    Die "OpenColorIO.lib not found under $ocioRoot. Set FLOKI_VCPKG to a vcpkg root with opencolorio:x64-windows installed."
}

# --- Import the MSVC environment (cl.exe, Windows SDK, etc.) from vcvars64 -------------
Write-Host "==> Importing MSVC env from vcvars64..." -ForegroundColor Cyan
cmd /c "`"$vcvars`" && set" | ForEach-Object {
    if ($_ -match '^([^=]+)=(.*)$') {
        Set-Item -Path "env:$($matches[1])" -Value $matches[2]
    }
}

# --- Prepend our tools ahead of any Store shims / system entries ----------------------
$prepend = @($cmakeBin, $ninjaDir, $anaconda, "$anaconda\Scripts", $vcpkgBin) |
    Where-Object { Test-Path $_ }
$env:PATH = ($prepend -join ';') + ';' + $env:PATH

# --- OCIO backend roots for system-ocio -----------------------------------------------
$env:OPENCOLORIO_ROOT = $ocioRoot
$env:IMATH_ROOT       = $ocioRoot

# --- Sanity: fail loudly here rather than deep inside a native build ------------------
Write-Host "==> Toolchain:" -ForegroundColor Cyan
$tools = @{ 'cmake' = 'cmake --version'; 'ninja' = 'ninja --version'; 'python' = 'python --version'; 'cl' = 'cl' }
foreach ($t in 'cmake', 'ninja', 'python') {
    $cmd = Get-Command $t -ErrorAction SilentlyContinue
    if (-not $cmd) { Die "'$t' not found on PATH after env setup. Check the VS / Anaconda paths." }
    Write-Host ("    {0,-7} {1}" -f $t, $cmd.Source)
}
if (-not (Get-Command cl -ErrorAction SilentlyContinue)) { Die "cl.exe (MSVC) not on PATH; vcvars import failed." }
Write-Host "    OPENCOLORIO_ROOT $env:OPENCOLORIO_ROOT"

# --- Dispatch -------------------------------------------------------------------------
Push-Location (Join-Path $PSScriptRoot '..')
try {
    function Invoke-Cargo([string[]]$argv) {
        Write-Host "==> cargo $($argv -join ' ')" -ForegroundColor Green
        & cargo @argv
        if ($LASTEXITCODE -ne 0) { Die "cargo $($argv -join ' ') failed (exit $LASTEXITCODE)" }
    }

    # Args destined for the *program* rather than for cargo. PowerShell consumes the
    # `--` in `run-windows.ps1 run -- foo.exr` itself, so $CargoArgs arrives without
    # it and `cargo run --release foo.exr` would make cargo reject `foo.exr` as an
    # unknown cargo argument. Re-insert the separator.
    function ProgramArgs([string[]]$argv) {
        if ($CargoArgs -and $CargoArgs.Count) { return $argv + @('--') + $CargoArgs }
        return $argv
    }

    switch ($Task) {
        'run'    { Invoke-Cargo (ProgramArgs @('run', '--release')) }
        'build'  { Invoke-Cargo (@('build', '--release') + $CargoArgs) }
        'inspect' {
            # Phase 0 of the #100 soak: dump parts / compression / channel counts for
            # each frame path given, so the soak numbers are interpretable.
            Invoke-Cargo (ProgramArgs @('run', '--release', '--bin', 'inspect_exr'))
        }
        'clippy' { Invoke-Cargo (@('clippy', '--all-targets', '--') + @('-D', 'warnings')) }
        'test'   {
            Invoke-Cargo @('fmt', '--all', '--', '--check')
            Invoke-Cargo @('clippy', '--all-targets', '--', '-D', 'warnings')
            Invoke-Cargo (@('test', '--all-targets') + $CargoArgs)
        }
        'soak'   {
            # #100 capture: the 1 Hz `trace_playback_state` line goes to stderr via
            # env_logger, so scope RUST_LOG to that target (wgpu/eframe are far too
            # chatty at debug) and tee the whole run to a timestamped log.
            $logDir = Join-Path (Get-Location) 'soak-logs'
            New-Item -ItemType Directory -Force -Path $logDir | Out-Null
            $log = Join-Path $logDir ("soak-{0}.log" -f (Get-Date -Format 'yyyyMMdd-HHmmss'))
            $env:RUST_LOG = 'floki::playback=debug'
            Write-Host "==> RUST_LOG=$env:RUST_LOG" -ForegroundColor Cyan
            Write-Host "==> log: $log" -ForegroundColor Cyan
            # Native stderr surfaces as ErrorRecords, which the script-wide 'Stop'
            # preference would turn into a fatal on the very first log line. Relax
            # it for the capture only.
            $prev = $ErrorActionPreference
            $ErrorActionPreference = 'Continue'
            $runArgs = ProgramArgs @('run', '--release')
            Write-Host "==> cargo $($runArgs -join ' ')" -ForegroundColor Green
            try {
                & cargo @runArgs 2>&1 | Tee-Object -FilePath $log
            }
            finally {
                $ErrorActionPreference = $prev
            }
            if ($LASTEXITCODE -ne 0) { Die "soak run failed (exit $LASTEXITCODE); log: $log" }
            Write-Host "==> captured: $log" -ForegroundColor Cyan
        }
    }
    Write-Host "==> Done." -ForegroundColor Green
}
finally {
    Pop-Location
}
