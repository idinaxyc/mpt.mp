param(
    [string]$Repository = $env:MPT_GITHUB_REPO
)
$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($Repository)) {
    $Repository = Read-Host 'GitHub repository (owner/name)'
}
if ($Repository -notmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$') {
    throw 'Invalid repository. Expected owner/name.'
}
if (-not [Environment]::Is64BitOperatingSystem) {
    throw 'The published Windows build currently targets 64-bit Windows.'
}

$asset = 'mpt-mp-windows-x86_64.exe'
$url = "https://github.com/$Repository/releases/latest/download/$asset"
$installDir = Join-Path $env:LOCALAPPDATA 'Programs\mpt.mp'
$destination = Join-Path $installDir 'mpt-mp.exe'
$tempFile = Join-Path ([IO.Path]::GetTempPath()) ([IO.Path]::GetRandomFileName())

try {
    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    Invoke-WebRequest -Uri $url -OutFile $tempFile
    Move-Item -Force $tempFile $destination
} finally {
    if (Test-Path $tempFile) { Remove-Item -Force $tempFile }
}

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$pathEntries = @($userPath -split ';' | Where-Object { $_ })
if ($pathEntries -notcontains $installDir) {
    $newPath = (@($pathEntries) + $installDir) -join ';'
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    Write-Host 'Added the installation directory to your user PATH. Open a new terminal to use it.'
}
Write-Host "mpt.mp installed to: $destination"
Write-Host 'You can launch it from the Start menu search or run: mpt-mp'
