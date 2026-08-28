#requires -version 5.1

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$repo = 'ref42/armup'
$releaseBase = "https://github.com/$repo/releases/latest/download"
$installDir = Join-Path $env:LOCALAPPDATA 'Programs\armup'
$tempRoot = Join-Path -Path ([System.IO.Path]::GetTempPath()) -ChildPath ("armup-" + [System.IO.Path]::GetRandomFileName())
$archivePath = Join-Path $tempRoot 'armup.zip'
$hashPath = Join-Path $tempRoot 'armup.zip.sha256'

function Download-File {
    param(
        [Parameter(Mandatory)]
        [string]$Uri,
        [Parameter(Mandatory)]
        [string]$OutFile
    )

    Invoke-WebRequest -Uri $Uri -OutFile $OutFile -Headers @{ 'User-Agent' = 'armup-install' }
}

function Add-ToUserPath {
    param(
        [Parameter(Mandatory)]
        [string]$PathValue
    )

    $normalized = $PathValue.TrimEnd('\')
    $comparison = [System.StringComparison]::OrdinalIgnoreCase

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $userEntries = @()
    if ($userPath) {
        $userEntries = $userPath -split ';' | ForEach-Object { $_.Trim() } | Where-Object { $_ }
    }
    if (-not ($userEntries | Where-Object { $_.TrimEnd('\').Equals($normalized, $comparison) })) {
        $userEntries += $normalized
        [Environment]::SetEnvironmentVariable('Path', ($userEntries -join ';'), 'User')
    }

    $sessionEntries = @()
    if ($env:Path) {
        $sessionEntries = $env:Path -split ';' | ForEach-Object { $_.Trim() } | Where-Object { $_ }
    }
    if (-not ($sessionEntries | Where-Object { $_.TrimEnd('\').Equals($normalized, $comparison) })) {
        $sessionEntries += $normalized
        $env:Path = $sessionEntries -join ';'
    }
}

try {
    New-Item -ItemType Directory -Force -Path $tempRoot | Out-Null

    Write-Host "Downloading armup..."
    Download-File -Uri "$releaseBase/armup.zip" -OutFile $archivePath
    Download-File -Uri "$releaseBase/armup.zip.sha256" -OutFile $hashPath

    $expectedHash = [regex]::Match((Get-Content $hashPath -Raw), '^[0-9a-fA-F]{64}').Value.ToLowerInvariant()
    if (-not $expectedHash) {
        throw "failed to read the expected SHA-256 hash from $hashPath"
    }

    $actualHash = (Get-FileHash -Algorithm SHA256 -Path $archivePath).Hash.ToLowerInvariant()
    if ($actualHash -ne $expectedHash) {
        throw "hash mismatch for armup.zip: expected $expectedHash, got $actualHash"
    }

    if (Test-Path $installDir) {
        Remove-Item $installDir -Recurse -Force
    }
    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    Expand-Archive -Path $archivePath -DestinationPath $installDir -Force

    Add-ToUserPath -PathValue $installDir

    Write-Host "Installed armup to $installDir"
    Write-Host "Open a new terminal to use armup."
}
finally {
    if (Test-Path $tempRoot) {
        Remove-Item $tempRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}
