$ErrorActionPreference = 'Stop'

$repo = 'idinaxyc/mpt.mp'
$asset = 'mpt-mp-windows-x86_64.zip'
$url = "https://github.com/$repo/releases/latest/download/$asset"
$tempDir = Join-Path ([IO.Path]::GetTempPath()) ([IO.Path]::GetRandomFileName())
$installDir = Join-Path $env:LOCALAPPDATA 'Programs\mpt.mp'

try {
    New-Item -ItemType Directory -Force -Path $tempDir | Out-Null
    $archive = Join-Path $tempDir 'release.zip'
    Invoke-WebRequest -Uri $url -OutFile $archive
    Expand-Archive -LiteralPath $archive -DestinationPath $tempDir -Force

    $binary = Join-Path $tempDir 'mpt-mp.exe'
    if (-not (Test-Path $binary)) {
        throw 'The release archive does not contain mpt-mp.exe.'
    }

    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    Copy-Item -Force $binary (Join-Path $installDir 'mpt-mp.exe')
    $plugins = Join-Path $tempDir 'plugins'
    $installedPlugins = Join-Path $installDir 'plugins'
    if (Test-Path $plugins) {
        if (Test-Path $installedPlugins) { Remove-Item -Recurse -Force $installedPlugins }
        Copy-Item -Recurse $plugins $installedPlugins
    }

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $entries = @($userPath -split ';' | Where-Object { $_ })
    if ($entries -notcontains $installDir) {
        [Environment]::SetEnvironmentVariable('Path', (($entries + $installDir) -join ';'), 'User')
        Write-Host 'Added mpt.mp to your user PATH. Open a new terminal.'
    }
    Write-Host "mpt.mp installed to $installDir"
}
finally {
    if (Test-Path $tempDir) { Remove-Item -Recurse -Force $tempDir }
}
