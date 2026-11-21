# Fetch the Microsoft console host pair used by the Windows release.
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet('x64', 'x86_64', 'arm64')]
    [string] $Architecture,

    [Parameter(Mandatory)]
    [string] $OutputDirectory
)

$ErrorActionPreference = 'Stop'

$version = '1.25.261109001'
$sha256 = '3d77f535df848e44225c2a8912ac844fecb23f3057732b639f3034b587496f09'
$package = 'microsoft.windows.console.conpty'
$arch = if ($Architecture -eq 'arm64') { 'arm64' } else { 'x64' }
$url = "https://api.nuget.org/v3-flatcontainer/$package/$version/$package.$version.nupkg"
$scratch = New-Item -ItemType Directory -Path (Join-Path ([IO.Path]::GetTempPath()) ("muxel-conpty-" + [guid]::NewGuid()))
$archivePath = Join-Path $scratch.FullName 'conpty.nupkg'
$dll = Join-Path $scratch.FullName 'conpty.dll'
$hostExe = Join-Path $scratch.FullName 'OpenConsole.exe'

try {
    Invoke-WebRequest -Uri $url -OutFile $archivePath -TimeoutSec 120
    $digest = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash
    if ($digest -ne $sha256) {
        throw "ConPTY $version SHA-256 mismatch (expected $sha256, got $digest)"
    }

    $archive = [IO.Compression.ZipFile]::OpenRead($archivePath)
    try {
        $entries = [ordered]@{
            "runtimes/win-$arch/native/conpty.dll" = $dll
            "build/native/runtimes/$arch/OpenConsole.exe" = $hostExe
        }
        foreach ($name in $entries.Keys) {
            $entry = $archive.GetEntry($name)
            if ($null -eq $entry) {
                throw "ConPTY $version is missing $name"
            }
            $file = $entries[$name]
            [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $file)
            $signature = Get-AuthenticodeSignature -FilePath $file
            if ($signature.Status -ne 'Valid' -or
                $signature.SignerCertificate.Subject -notmatch '(?:^|,\s*)CN=Microsoft Corporation(?:,|$)') {
                throw "ConPTY $version has an unverified Microsoft signature on $($entry.Name) ($($signature.Status))"
            }
        }
    } finally {
        $archive.Dispose()
    }

    $destination = New-Item -ItemType Directory -Path $OutputDirectory -Force
    Copy-Item -LiteralPath $dll, $hostExe -Destination $destination.FullName
    Write-Output "Fetched and verified Microsoft ConPTY $version ($arch)"
} finally {
    Remove-Item -LiteralPath $archivePath, $dll, $hostExe -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $scratch.FullName -Force
}
