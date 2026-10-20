#Requires -Version 5.1
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$target = 'x86_64-pc-windows-msvc'

# These variables can replace Cargo's configured rustflags. Refuse them without
# changing their values, rather than silently dropping caller flags or remapping.
foreach ($name in @('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS', 'CARGO_BUILD_RUSTFLAGS')) {
    if (Test-Path -LiteralPath "Env:$name") {
        throw "$name is set. Review and remove it from this invocation before using the release build helper."
    }
}

$temporaryConfig = $null
$ownsConfig = $false
Push-Location -LiteralPath $projectRoot
try {
    $userProfilePath = [Environment]::GetFolderPath([Environment+SpecialFolder]::UserProfile)
    if ([string]::IsNullOrWhiteSpace($userProfilePath)) { throw 'Cannot locate the user profile.' }
    $cargoHomePath = $env:CARGO_HOME
    if ([string]::IsNullOrEmpty($cargoHomePath)) { $cargoHomePath = Join-Path $userProfilePath '.cargo' }
    if (-not [IO.Path]::IsPathRooted($cargoHomePath)) { $cargoHomePath = Join-Path $projectRoot $cargoHomePath }
    $sysrootOutput = @(& rustc --print sysroot)
    if ($LASTEXITCODE -ne 0 -or $sysrootOutput.Count -ne 1 -or [string]::IsNullOrWhiteSpace($sysrootOutput[0])) {
        throw 'Cannot locate the Rust sysroot.'
    }

    $prefixes = @(
        @{ From = $userProfilePath; To = '/home' },
        @{ From = $cargoHomePath; To = '/cargo-home' },
        @{ From = $projectRoot; To = '/workspace' },
        @{ From = $sysrootOutput[0].Trim(); To = '/rust-toolchain' }
    )
    foreach ($prefix in $prefixes) {
        $prefix.From = [IO.Path]::GetFullPath($prefix.From)
        if ($prefix.From -ne [IO.Path]::GetPathRoot($prefix.From)) {
            $prefix.From = $prefix.From.TrimEnd([char[]]'\/')
        }
    }
    # rustc uses the last matching textual prefix; cover both Windows spellings
    # and put more specific prefixes after their parents.
    $remapFlags = @(foreach ($prefix in ($prefixes | Sort-Object { $_.From.Length })) {
        foreach ($spelling in @($prefix.From, $prefix.From.Replace('\', '/')) | Select-Object -Unique) {
            "--remap-path-prefix=$spelling=$($prefix.To)"
        }
    })
    $flagsToml = ConvertTo-Json -InputObject $remapFlags -Compress
    # Cargo merges this array with the project's target flags, including crt-static.
    # Explicit --target applies these flags to the app and its target dependencies.
    $configText = "[target.$target]`nrustflags = $flagsToml`n"
    $temporaryConfig = Join-Path ([IO.Path]::GetTempPath()) ('hwinfo-flux-pro-' + [Guid]::NewGuid().ToString('N') + '.toml')
    $configStream = [IO.File]::Open($temporaryConfig, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
    $ownsConfig = $true
    try {
        $bytes = (New-Object System.Text.UTF8Encoding($false)).GetBytes($configText)
        $configStream.Write($bytes, 0, $bytes.Length)
    } finally {
        $configStream.Dispose()
    }
    & cargo --config $temporaryConfig build --release --locked --target $target
    if ($LASTEXITCODE -ne 0) { throw 'Release build failed.' }
} finally {
    try {
        if ($ownsConfig) { Remove-Item -LiteralPath $temporaryConfig -Force }
    } finally {
        Pop-Location
    }
}
