# Bootstrap the native Fai installer from one immutable GitHub release.
param(
    [string]$Version = '@VERSION@',
    [string]$HomeDirectory = '',
    [switch]$NoModifyPath
)

function Assert-FaiInstallerArchive {
    param([Parameter(Mandatory = $true)][string]$ArchivePath)

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $Zip = [IO.Compression.ZipFile]::OpenRead($ArchivePath)
    try {
        [string[]]$Names = @($Zip.Entries | ForEach-Object { $_.FullName })
        [Array]::Sort($Names, [StringComparer]::Ordinal)
        if ($Names.Count -ne 3 -or ($Names -join "`n") -cne "LICENSE`nbin/fai.exe`nmetadata.json") {
            throw ('Invalid installer archive members: ' + ($Names -join ', '))
        }
        foreach ($Entry in $Zip.Entries) {
            $Kind = ($Entry.ExternalAttributes -shr 16) -band 61440
            if ($Kind -ne 0 -and $Kind -ne 32768) { throw 'Installer archive contains a non-regular file' }
        }
    } finally { $Zip.Dispose() }
}

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$ReleaseVersion = '@VERSION@'
if ($HomeDirectory) { $env:FAI_HOME = $HomeDirectory }
if ($env:FAI_HOME) { $env:FAI_HOME = [IO.Path]::GetFullPath($env:FAI_HOME) }
if ($env:FAI_TOOLCHAIN_DIR) { $env:FAI_TOOLCHAIN_DIR = [IO.Path]::GetFullPath($env:FAI_TOOLCHAIN_DIR) }
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw 'Use install.sh on Linux and macOS'
}
try {
    $Architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLowerInvariant()
} catch {
    $Architecture = $env:PROCESSOR_ARCHITEW6432
    if (-not $Architecture) { $Architecture = $env:PROCESSOR_ARCHITECTURE }
    $Architecture = $Architecture.ToLowerInvariant()
}
switch ($Architecture) {
    { $_ -in 'x64', 'amd64', 'x86_64' } { $Target = 'x86_64-pc-windows-msvc'; break }
    { $_ -in 'arm64', 'aarch64' } { $Target = 'aarch64-pc-windows-msvc'; break }
    default { throw "Unsupported architecture: $Architecture" }
}
$Build = [int](Get-ItemPropertyValue -LiteralPath 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion' -Name CurrentBuildNumber)
if ($Build -lt 20348 -or ($Target -eq 'aarch64-pc-windows-msvc' -and $Build -lt 22000)) {
    throw 'This release requires Windows 11 or Windows Server 2022 (Windows 11 on ARM64)'
}
$Base = $env:FAI_RELEASE_BASE_URL
if (-not $Base) { $Base = 'https://github.com/Krzysztof-Cieslak/fai/releases' }
$Uri = [Uri]$Base
if ($Uri.Scheme -ne 'https' -and -not ($Uri.Scheme -eq 'http' -and $Uri.IsLoopback)) {
    throw 'Release URLs require HTTPS'
}
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$Temporary = Join-Path ([IO.Path]::GetTempPath()) ('fai-install-' + [Guid]::NewGuid().ToString('N'))
[void](New-Item -ItemType Directory -Path $Temporary)
try {
    $Asset = "fai-installer-$ReleaseVersion-$Target.zip"
    $Archive = Join-Path $Temporary $Asset
    $Sums = Join-Path $Temporary 'SHA256SUMS'
    Invoke-WebRequest -UseBasicParsing "$Base/download/v$ReleaseVersion/SHA256SUMS" -OutFile $Sums
    Invoke-WebRequest -UseBasicParsing "$Base/download/v$ReleaseVersion/$Asset" -OutFile $Archive
    $Matches = @(Get-Content -LiteralPath $Sums | Where-Object { $_ -match ('^[0-9a-f]{64}  ' + [Regex]::Escape($Asset) + '$') })
    if ($Matches.Count -ne 1) { throw 'Missing or ambiguous installer checksum' }
    $Expected = $Matches[0].Substring(0, 64)
    if ((Get-FileHash -LiteralPath $Archive -Algorithm SHA256).Hash.ToLowerInvariant() -ne $Expected) {
        throw 'Installer checksum mismatch'
    }
    Assert-FaiInstallerArchive -ArchivePath $Archive
    Expand-Archive -LiteralPath $Archive -DestinationPath $Temporary
    $Launcher = Join-Path $Temporary 'bin/fai.exe'
    $Arguments = @('installer', 'setup', '--version', $Version)
    if ($NoModifyPath) { $Arguments += '--no-modify-path' }
    & $Launcher @Arguments
    if ($LASTEXITCODE -ne 0) { throw "Fai installation failed with exit code $LASTEXITCODE" }
    $InstallHome = $env:FAI_HOME
    if (-not $InstallHome) {
        $UserHome = $env:HOME
        if (-not $UserHome) { $UserHome = $env:USERPROFILE }
        $InstallHome = Join-Path $UserHome '.fai'
    }
    $Bin = Join-Path $InstallHome 'bin'
    if (-not $NoModifyPath -and ($env:Path -split ';') -notcontains $Bin) { $env:Path = "$Bin;$env:Path" }
} finally {
    Remove-Item -LiteralPath $Temporary -Recurse -Force
}
