# idlepack installer for Windows, in the style of rtk's.
#
#   From a checkout:   powershell -ExecutionPolicy Bypass -File install.ps1
#   From GitHub:       $env:IDLEPACK_REPO='OWNER/idlepack'; irm https://raw.githubusercontent.com/OWNER/idlepack/main/install.ps1 | iex
#
# Installs the prebuilt release binary when the repository publishes one
# (see .github/workflows/release.yml), otherwise builds with cargo, then runs
# `idlepack init`. Variables: IDLEPACK_REPO, IDLEPACK_BIN_DIR (default
# %LOCALAPPDATA%\idlepack\bin), IDLEPACK_FROM_SOURCE=1, IDLEPACK_NO_INIT=1,
# IDLEPACK_DOWNLOAD_BASE.
$ErrorActionPreference = 'Stop'

$repo = $env:IDLEPACK_REPO
$binDir = if ($env:IDLEPACK_BIN_DIR) { $env:IDLEPACK_BIN_DIR } else { Join-Path $env:LOCALAPPDATA 'idlepack\bin' }
$checkout = $null
if ($PSScriptRoot -and (Test-Path (Join-Path $PSScriptRoot 'Cargo.toml'))) {
    if (Select-String -Path (Join-Path $PSScriptRoot 'Cargo.toml') -Pattern '^name = "idlepack"' -Quiet) { $checkout = $PSScriptRoot }
}
$installed = $null

function Install-Prebuilt {
    if ($env:IDLEPACK_FROM_SOURCE -eq '1') { return $false }
    $arch = $env:PROCESSOR_ARCHITECTURE
    if ($arch -ne 'AMD64') { return $false }
    $base = $env:IDLEPACK_DOWNLOAD_BASE
    if (-not $base) {
        if (-not $repo) { return $false }
        $base = "https://github.com/$repo/releases/latest/download"
    }
    $asset = 'idlepack-x86_64-pc-windows-msvc.zip'
    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("idlepack-" + [Guid]::NewGuid())
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        Write-Host "Downloading $asset ..."
        Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile (Join-Path $tmp $asset)
    } catch {
        Write-Host "No prebuilt binary; building from source instead."
        Remove-Item -Recurse -Force $tmp
        return $false
    }
    $sumFile = Join-Path $tmp "$asset.sha256"
    try { Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset.sha256" -OutFile $sumFile } catch { $sumFile = $null }
    if ($sumFile) {
        $want = (Get-Content $sumFile -Raw).Split(' ')[0].Trim().ToLower()
        $got = (Get-FileHash -Algorithm SHA256 (Join-Path $tmp $asset)).Hash.ToLower()
        if ($want -ne $got) { Remove-Item -Recurse -Force $tmp; throw "checksum mismatch for $asset" }
    }
    Expand-Archive -Path (Join-Path $tmp $asset) -DestinationPath $tmp -Force
    New-Item -ItemType Directory -Force -Path $binDir | Out-Null
    Copy-Item -Force (Join-Path $tmp 'idlepack.exe') (Join-Path $binDir 'idlepack.exe')
    Remove-Item -Recurse -Force $tmp
    $script:installed = Join-Path $binDir 'idlepack.exe'
    return $true
}

function Install-FromSource {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw 'cargo not found. Install Rust from https://rustup.rs (choose the MSVC toolchain), then run this again.'
    }
    if ($checkout) {
        Write-Host "Building idlepack from $checkout ..."
        cargo install --path $checkout --locked --force
    } elseif ($repo) {
        Write-Host "Building idlepack from github.com/$repo ..."
        cargo install --git "https://github.com/$repo" --locked --force idlepack
    } else {
        throw 'run this from a checkout, or set $env:IDLEPACK_REPO = "owner/name"'
    }
    if ($LASTEXITCODE -ne 0) { throw 'cargo install failed' }
    $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
    $script:installed = Join-Path $cargoHome 'bin\idlepack.exe'
}

if ($checkout -and -not $env:IDLEPACK_DOWNLOAD_BASE) {
    Install-FromSource
} elseif (-not (Install-Prebuilt)) {
    Install-FromSource
}

if (-not (Test-Path $installed)) { throw "the install did not produce $installed" }
Write-Host "Installed $(& $installed --version) at $installed"
$dir = Split-Path $installed
if (-not (($env:PATH -split ';') -contains $dir)) {
    $userPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
    if (-not (($userPath -split ';') -contains $dir)) {
        [Environment]::SetEnvironmentVariable('PATH', ($userPath.TrimEnd(';') + ';' + $dir).TrimStart(';'), 'User')
        Write-Host "Added $dir to your user PATH (open a new terminal to use ``idlepack`` by name)."
    }
}

if ($env:IDLEPACK_NO_INIT -ne '1') {
    Write-Host ''
    & $installed init
}
