# wishlang installer for Windows (x86_64).
#
#   irm https://raw.githubusercontent.com/Xunrise/wishlang/master/install.ps1 | iex
#
# Parameters (when saved and run as a file): -Version 0.2.0, -InstallDir C:\tools\wishlang
# Environment: WISHLANG_INSTALL_DIR, WISHLANG_VERSION, WISHLANG_BASE_URL (mirror/testing).
# Re-running upgrades in place.
param(
  [string]$Version = $env:WISHLANG_VERSION,
  [string]$InstallDir = $env:WISHLANG_INSTALL_DIR
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$repo = 'Xunrise/wishlang'

if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64' -and $env:PROCESSOR_ARCHITEW6432 -ne 'AMD64') {
  throw "Unsupported architecture $env:PROCESSOR_ARCHITECTURE (only x86_64 Windows builds exist)"
}
$target = 'x86_64-pc-windows-msvc'

if (-not $Version) {
  $resp = Invoke-WebRequest "https://github.com/$repo/releases/latest" -MaximumRedirection 0 -ErrorAction SilentlyContinue
  $loc = [string]$resp.Headers.Location
  if (-not $loc) { throw 'Could not resolve the latest release; pass -Version.' }
  $Version = $loc.Substring($loc.LastIndexOf('/v') + 2)
}
$Version = $Version.TrimStart('v')
$base = if ($env:WISHLANG_BASE_URL) { $env:WISHLANG_BASE_URL } else { "https://github.com/$repo/releases/download/v$Version" }
$name = "wishlang-$Version-$target"
if (-not $InstallDir) { $InstallDir = Join-Path $env:LOCALAPPDATA 'wishlang\bin' }

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("wishlang-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
  Write-Host "Downloading wishlang $Version for $target"
  Invoke-WebRequest "$base/$name.zip" -OutFile "$tmp\$name.zip"
  Invoke-WebRequest "$base/wishlang-$Version-checksums.txt" -OutFile "$tmp\checksums.txt"

  $line = Get-Content "$tmp\checksums.txt" | Where-Object { ($_ -split '\s+')[1] -in @("$name.zip", "*$name.zip") } | Select-Object -First 1
  if (-not $line) { throw "No checksum listed for $name.zip" }
  $want = ($line -split '\s+')[0].ToLower()
  $got = (Get-FileHash "$tmp\$name.zip" -Algorithm SHA256).Hash.ToLower()
  if ($want -ne $got) { throw "Checksum mismatch for $name.zip (expected $want, got $got)" }

  Expand-Archive "$tmp\$name.zip" -DestinationPath $tmp
  $exe = Join-Path $tmp "$name\wishlang.exe"
  if (-not (Test-Path $exe)) { throw 'Archive has no wishlang.exe' }

  New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
  $dest = Join-Path $InstallDir 'wishlang.exe'
  $old = $null
  if (Test-Path $dest) { try { $old = (& $dest --version) -replace '^.*\s', '' } catch {} }
  # A running wishlang.exe can't be overwritten but can be renamed away.
  if (Test-Path $dest) { Move-Item -Force $dest "$dest.old" }
  Copy-Item $exe $dest
  Remove-Item -Force "$dest.old" -ErrorAction SilentlyContinue

  if (-not $old) { Write-Host "Installed wishlang $Version to $dest" }
  elseif ($old -eq $Version) { Write-Host "wishlang $Version was already installed; reinstalled in $dest" }
  else { Write-Host "Upgraded wishlang $old -> $Version in $dest" }

  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  $entries = @($userPath -split ';' | Where-Object { $_ })
  if ($entries -notcontains $InstallDir) {
    [Environment]::SetEnvironmentVariable('Path', (($entries + $InstallDir) -join ';'), 'User')
    Write-Host "Added $InstallDir to your user PATH. Restart your terminal."
  }
} finally {
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
Write-Host 'Run: wishlang --version'
