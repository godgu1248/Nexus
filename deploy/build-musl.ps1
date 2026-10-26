param(
  [string]$ZigDir = 'C:\zig',
  [string]$TargetDir = 'C:\nxtarget',
  [switch]$DebugBuild
)

# Build the Linux static binary for deployment (x86_64-unknown-linux-musl) from Windows.
# Pure ASCII file: PowerShell 5.1 reads scripts as ANSI, non-ASCII here would corrupt quoting.
#
# Requires: rustup target add x86_64-unknown-linux-musl ; zig extracted to an ASCII-only dir
# (default C:\zig, containing zig.exe and lib\). See deploy/README.md section 1.
#
# Why -TargetDir defaults to a short path like C:\nxtarget:
#   Windows caps a command line at 32767 characters, and the final link passes ~195 object
#   files. With the checkout at C:\Nexus... the total reached ~34k and the link died as
#   `spawnSync ... ENAMETOOLONG` / `error(link): unable to spawn ... NameTooLong` -- messages
#   that never mention argument length. A short target dir brings it under the limit.
#   The artifact is copied back to target\<triple>\<profile>\ so every other script keeps
#   using the same path.

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$zig = Join-Path $ZigDir 'zig.exe'
if (-not (Test-Path $zig)) { throw "zig.exe not found in $ZigDir" }

# cc-rs wants a compiler/archiver/linker it can call. Put the wrappers in an ASCII-only
# directory and drive them with node, so a non-ASCII checkout path cannot break exec.
$tmp = Join-Path $env:TEMP 'nexus-zig'
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
Copy-Item (Join-Path $PSScriptRoot 'zigcc.js') (Join-Path $tmp 'zigcc.js') -Force

$cc = Join-Path $tmp 'zigcc.bat'
$ccMusl = Join-Path $tmp 'x86_64-unknown-linux-musl-zigcc.bat'
$ar = Join-Path $tmp 'zigar.bat'
$node = (Get-Command node).Source
if (-not $node) { throw 'node.exe not found on PATH' }
$js = Join-Path $tmp 'zigcc.js'
Set-Content -Path $cc -Value ('@"' + $node + '" "' + $js + '" %*') -Encoding ASCII
Set-Content -Path $ccMusl -Value ('@"' + $node + '" "' + $js + '" %*') -Encoding ASCII
Set-Content -Path $ar -Value ('@"' + $zig + '" ar %*') -Encoding ASCII

$env:ZIG_EXE = $zig
$env:CC_x86_64_unknown_linux_musl = $ccMusl
$env:AR_x86_64_unknown_linux_musl = $ar
$env:CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER = $cc
$env:CARGO_TARGET_DIR = $TargetDir

Push-Location $repo
try {
  if ($DebugBuild) {
    & cargo build --target x86_64-unknown-linux-musl
  } else {
    & cargo build --release --target x86_64-unknown-linux-musl
  }
  if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
} finally {
  Pop-Location
}

$profileDir = if ($DebugBuild) { 'debug' } else { 'release' }
$built = Join-Path $TargetDir "x86_64-unknown-linux-musl\$profileDir\nexus"
if (-not (Test-Path $built)) { throw "build finished but $built is missing" }

# Copy back so deployers and packagers see one canonical location.
$canonical = Join-Path $repo "target\x86_64-unknown-linux-musl\$profileDir\nexus"
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $canonical) | Out-Null
Copy-Item $built $canonical -Force

$item = Get-Item $canonical
Write-Output ("artifact: {0} ({1:N2} MB)" -f $canonical, ($item.Length / 1MB))
Write-Output ("sha256:   " + (Get-FileHash $canonical -Algorithm SHA256).Hash.ToLower())
Write-Output 'Check static linkage with: node deploy/elfinfo.js <artifact>  -> PT_INTERP=none'
