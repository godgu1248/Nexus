# Repo-tracked copy of the packaging script.
#
# deploy/package.ps1  -- run this from anywhere: it derives the repo from its own
#                        location and writes <repo>/dist (git-ignored).
# <workspace>/package.ps1 -- the same script kept at the workspace root, which is
#                        outside git; this copy exists so the build recipe survives.
#
# ASCII-only on purpose: Windows PowerShell 5.1 reads a BOM-less .ps1 as ANSI and
# any non-ASCII byte turns the following lines into a parse error.
$ErrorActionPreference = 'Stop'

# Build the three release archives. ASCII-only on purpose: this file has no BOM and
# Windows PowerShell 5.1 would decode Chinese comments as ANSI and swallow later lines.
$repo = Split-Path $PSScriptRoot -Parent
$dist = Join-Path (Split-Path $PSScriptRoot -Parent) 'dist'
$stage = Join-Path $env:TEMP ('nexus-pack-' + [guid]::NewGuid().ToString('N'))
# Version comes from Cargo.toml, not a literal: a hardcoded copy silently ships
# archives named after the previous release (that is exactly what happened when
# 0.1.2 was bumped to 0.1.3).
$cargoToml = Join-Path $repo 'Cargo.toml'
$verMatch = Select-String -Path $cargoToml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
if (-not $verMatch) { throw "no version in $cargoToml" }
$ver = $verMatch.Matches[0].Groups[1].Value
Write-Host "packaging version $ver"

if (Test-Path $dist) { Remove-Item $dist -Recurse -Force }
New-Item -ItemType Directory -Path $dist | Out-Null
New-Item -ItemType Directory -Path $stage | Out-Null

$common = @('static', 'deploy', 'migrations', 'README.md', 'DESIGN.md', '.env.example')
$exe = Join-Path $repo 'target\release\nexus.exe'
$elf = Join-Path $repo 'target\x86_64-unknown-linux-musl\release\nexus'

if (-not (Test-Path $exe)) { throw "missing Windows binary: $exe" }
if (-not (Test-Path $elf)) { throw "missing Linux binary: $elf" }

function Copy-Common([string]$target) {
    foreach ($item in $common) {
        $src = Join-Path $repo $item
        if (Test-Path $src) { Copy-Item $src -Destination $target -Recurse -Force }
    }
}

# 1) source archive -- straight from git, so nothing untracked leaks in
$srcZip = Join-Path $dist "nexus-$ver-source.zip"
git -C $repo archive --format=zip --prefix="nexus-$ver/" -o $srcZip HEAD
if ($LASTEXITCODE -ne 0) { throw "git archive failed" }

# 2) Windows package
$winDir = Join-Path $stage "nexus-$ver-windows-x86_64"
New-Item -ItemType Directory -Path $winDir | Out-Null
Copy-Item $exe (Join-Path $winDir 'nexus.exe')
Copy-Common $winDir
$winZip = Join-Path $dist "nexus-$ver-windows-x86_64.zip"
# bsdtar, not Compress-Archive: PowerShell 5.1 writes backslash separators into the
# zip, so a Linux user extracts one flat file named "pkg\.env.example" instead of a
# tree. bsdtar writes forward slashes and keeps the entries in a stable order.
tar.exe -a -c -f $winZip -C $stage "nexus-$ver-windows-x86_64"
if ($LASTEXITCODE -ne 0) { throw "tar failed for windows package" }

# 3) Linux package
$linDir = Join-Path $stage "nexus-$ver-linux-x86_64"
New-Item -ItemType Directory -Path $linDir | Out-Null
Copy-Item $elf (Join-Path $linDir 'nexus')
Copy-Common $linDir
$linZip = Join-Path $dist "nexus-$ver-linux-x86_64.zip"
tar.exe -a -c -f $linZip -C $stage "nexus-$ver-linux-x86_64"
if ($LASTEXITCODE -ne 0) { throw "tar failed for linux package" }

# The same tree again as .tar.gz, because that is what the CI workflow publishes
# and what most Linux users expect. Without it the local-build mirror is missing
# a form the top level has, and anyone following "download the tar.gz" from the
# release page finds nothing under local-build/.
$linTar = Join-Path $dist "nexus-$ver-linux-x86_64.tar.gz"
tar.exe -czf $linTar -C $stage "nexus-$ver-linux-x86_64"
if ($LASTEXITCODE -ne 0) { throw "tar failed for linux tarball" }

# 4) checksums + inventory
#
# Standard two-column format only: "<hash>  <name>". `sha256sum -c` accepts this
# (and the "hash *name" variant). The previous three-column form ("hash size name")
# made `sha256sum -c` treat "size + name" as one filename and fail with
# "FAILED open or read"; Get-FileHash on Windows kept working, so it went unnoticed.
# Keep this file ASCII-only: PowerShell 5.1 reads .ps1 as ANSI when there is no BOM,
# and any non-ASCII byte here turns the following lines into a parse error.
$lines = foreach ($f in Get-ChildItem $dist -File | Where-Object { $_.Name -match '\.(zip|tar\.gz)$' } | Sort-Object Name) {
    $h = (Get-FileHash $f.FullName -Algorithm SHA256).Hash.ToLower()
    "{0}  {1}" -f $h, $f.Name
}
$lines | Set-Content -Path (Join-Path $dist 'SHA256SUMS.txt') -Encoding ASCII

Remove-Item $stage -Recurse -Force
Get-ChildItem $dist | Select-Object Name, Length | Format-Table -AutoSize
Get-Content (Join-Path $dist 'SHA256SUMS.txt')