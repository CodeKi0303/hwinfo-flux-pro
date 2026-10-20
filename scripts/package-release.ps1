#Requires -Version 5.1
[CmdletBinding()]
param([string] $ExpectedVersion)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$target = 'x86_64-pc-windows-msvc'
$utf8 = New-Object System.Text.UTF8Encoding($false)
$readmeImages = @(
    'docs/images/flux-pro-settings.png',
    'docs/images/hwinfo-gadget-cpu.png',
    'docs/images/hwinfo-gadget-gpu.png'
)
Push-Location -LiteralPath $projectRoot
try {
    $metadataJson = & cargo metadata --locked --no-deps --format-version 1
    if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed.' }
    $metadata = ($metadataJson -join "`n") | ConvertFrom-Json
    $package = @($metadata.packages | Where-Object { $_.name -eq 'hwinfo-flux-pro' })
    if ($package.Count -ne 1) { throw 'Expected exactly one hwinfo-flux-pro package.' }
    $version = $package[0].version
    if ($version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?$') { throw 'Unsupported package version.' }
    if ($ExpectedVersion -and $ExpectedVersion -cne $version) {
        throw "Tag version $ExpectedVersion does not match Cargo version $version."
    }
    $stem = "hwinfo-flux-pro-$version-$target"
    $distRoot = [IO.Path]::GetFullPath((Join-Path $projectRoot 'dist'))
    $destination = [IO.Path]::GetFullPath((Join-Path $distRoot $stem))
    if (Test-Path -LiteralPath $destination) {
        throw "Release output already exists and will be preserved: $destination"
    }
    if (Test-Path -LiteralPath $distRoot) {
        $distItem = Get-Item -LiteralPath $distRoot -Force
        if (-not $distItem.PSIsContainer -or ($distItem.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw 'dist must be a normal directory inside the project.'
        }
    }
    & (Join-Path $PSScriptRoot 'build-release.ps1')

    [void] [IO.Directory]::CreateDirectory($distRoot)
    $staging = [IO.Path]::GetFullPath((Join-Path $distRoot ('.staging-' + [Guid]::NewGuid().ToString('N'))))
    if (Test-Path -LiteralPath $staging) { throw 'Unique staging directory already exists.' }
    [void] [IO.Directory]::CreateDirectory($staging)
    $exeName = "$stem.exe"
    $binary = Join-Path $metadata.target_directory "$target/release/hwinfo-flux-pro.exe"
    Copy-Item -LiteralPath $binary -Destination (Join-Path $staging $exeName)
    foreach ($name in (@('LICENSE', 'README.md', 'config.example.json') + $readmeImages)) {
        $stagedFile = Join-Path $staging $name
        [void] [IO.Directory]::CreateDirectory((Split-Path -Parent $stagedFile))
        Copy-Item -LiteralPath (Join-Path $projectRoot $name) -Destination $stagedFile
    }
    & (Join-Path $PSScriptRoot 'generate-notices.ps1') -OutputPath (Join-Path $staging 'THIRD-PARTY-NOTICES.txt')
    $sysroot = & rustc --print sysroot
    if ($LASTEXITCODE -ne 0) { throw 'Cannot locate Rust standard-library notices.' }
    Copy-Item -LiteralPath (Join-Path $sysroot.Trim() 'share/doc/rust/COPYRIGHT-library.html') `
        -Destination (Join-Path $staging 'RUST-THIRD-PARTY-NOTICES.html')

    $versionOutput = & rustc --version
    if ($LASTEXITCODE -ne 0) { throw 'Cannot identify rustc version.' }
    $commit = & git rev-parse --verify --quiet HEAD
    if ($LASTEXITCODE -ne 0) { $commit = 'uncommitted-initial-project' }
    $gitChanges = & git status --porcelain=v1 --untracked-files=normal
    if ($LASTEXITCODE -ne 0) { throw 'Cannot identify source working-tree state.' }
    $dirty = [bool] $gitChanges
    $buildInfo = "Package: hwinfo-flux-pro $version`nTarget: $target`nCompiler: $versionOutput`nCommit: $commit`nUncommitted changes: $dirty`n"
    [IO.File]::WriteAllText((Join-Path $staging 'BUILD-INFO.txt'), $buildInfo, $utf8)

    # Explicit entries avoid accidentally shipping local settings, diagnostics, or old releases.
    # ZIP names and timestamps are stable; compiler output itself is not claimed reproducible.
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zipPath = Join-Path $staging "$stem.zip"
    $zipStream = [IO.File]::Open($zipPath, [IO.FileMode]::Create)
    $archive = $null
    try {
        $archive = [IO.Compression.ZipArchive]::new($zipStream, [IO.Compression.ZipArchiveMode]::Create)
        $entries = [ordered]@{ 'hwinfo-flux-pro.exe' = $exeName }
        foreach ($name in (@('BUILD-INFO.txt', 'LICENSE', 'README.md', 'config.example.json', 'THIRD-PARTY-NOTICES.txt', 'RUST-THIRD-PARTY-NOTICES.html') + $readmeImages)) {
            $entries[$name] = $name
        }
        foreach ($entryName in $entries.Keys) {
            $entry = $archive.CreateEntry($entryName, [IO.Compression.CompressionLevel]::Optimal)
            $entry.LastWriteTime = [DateTimeOffset]::new(2000, 1, 1, 0, 0, 0, [TimeSpan]::Zero)
            $sourceStream = [IO.File]::OpenRead((Join-Path $staging $entries[$entryName]))
            try {
                $destinationStream = $entry.Open()
                try { $sourceStream.CopyTo($destinationStream) } finally { $destinationStream.Dispose() }
            } finally {
                $sourceStream.Dispose()
            }
        }
    } finally {
        try { if ($null -ne $archive) { $archive.Dispose() } } finally { $zipStream.Dispose() }
    }
    $assetNames = @($exeName, "$stem.zip")
    $hashes = foreach ($name in ($assetNames | Sort-Object)) {
        $digest = (Get-FileHash -LiteralPath (Join-Path $staging $name) -Algorithm SHA256).Hash.ToLowerInvariant()
        "$digest  $name"
    }
    [IO.File]::WriteAllText((Join-Path $staging 'SHA256SUMS'), (($hashes -join "`n") + "`n"), $utf8)

    # Only complete packages receive the versioned directory name used by CI.
    # Keep failed staging directories for inspection; never delete previous output.
    $resolvedDist = (Resolve-Path -LiteralPath $distRoot).ProviderPath
    $resolvedStaging = (Resolve-Path -LiteralPath $staging).ProviderPath
    if ((Split-Path -Parent $resolvedDist) -ine [IO.Path]::GetFullPath($projectRoot) -or
        (Split-Path -Parent $resolvedStaging) -ine $resolvedDist -or
        (Split-Path -Parent $destination) -ine $resolvedDist) {
        throw 'Refusing to move package output outside the project dist directory.'
    }
    foreach ($directory in @($resolvedDist, $resolvedStaging)) {
        if ((Get-Item -LiteralPath $directory -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw 'Refusing to publish through a directory link.'
        }
    }
    # Directory.Move fails if the destination appeared concurrently; it never nests
    # staging inside an existing release directory or overwrites its contents.
    [IO.Directory]::Move($resolvedStaging, $destination)
    Write-Host "Packaged $stem in $destination"
} finally {
    Pop-Location
}
