#Requires -Version 5.1
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $OutputPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$utf8 = New-Object System.Text.UTF8Encoding($false)
Push-Location -LiteralPath $projectRoot
try {
    $metadataJson = & cargo metadata --locked --offline --format-version 1 --filter-platform x86_64-pc-windows-msvc
    if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed; run the locked build first.' }
    $metadata = ($metadataJson -join "`n") | ConvertFrom-Json
    $packages = @{}
    $nodes = @{}
    foreach ($package in $metadata.packages) { $packages[$package.id] = $package }
    foreach ($node in $metadata.resolve.nodes) { $nodes[$node.id] = $node }
    $pending = New-Object 'System.Collections.Generic.Stack[string]'
    $included = New-Object 'System.Collections.Generic.HashSet[string]'
    $pending.Push($metadata.resolve.root)
    while ($pending.Count -gt 0) {
        $id = $pending.Pop()
        if (-not $included.Add($id)) { continue }
        foreach ($dependency in $nodes[$id].deps) {
            $productionKinds = @($dependency.dep_kinds | Where-Object { $_.kind -ne 'dev' })
            if ($productionKinds.Count -gt 0) { $pending.Push($dependency.pkg) }
        }
    }

    $text = New-Object System.Text.StringBuilder
    [void] $text.AppendLine(@'
THIRD-PARTY NOTICES - hwinfo-flux-pro

This file covers the locked Windows x86_64 normal and build dependency graph.
Build-only dependencies are included conservatively; this list does not claim
that every listed package contributes machine code to the executable.
MIT is selected where offered as an alternative. Unicode terms also apply
where indicated. Bundled native HIDAPI is used under its BSD-style option.
The application itself is MIT-licensed; see LICENSE.
Rust standard-library notices accompany this file as RUST-THIRD-PARTY-NOTICES.html.
HWiNFO, iUnity, and FanControl binaries or proprietary source are not bundled.

Public FLUX PRO protocol reference
https://github.com/renanmt/linux-flux-pro
Revision: 9c6d713607b5e6cfcab95c024c88568f7559f711
Source: docs/PROTOCOL.md; license: LICENSE at the same revision.

MIT License

Copyright (c) 2026 Renan Tonheiro

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:
The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.
THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
'@)

    $reviewedExpressions = @(
        'MIT', 'MIT OR Apache-2.0', 'Apache-2.0 OR MIT', 'MIT/Apache-2.0',
        'Unlicense OR MIT', '(MIT OR Apache-2.0) AND Unicode-3.0'
    )
    # These published crate archives declare MIT but omit their license file.
    # Keep exact upstream texts and provenance offline, limited to reviewed versions.
    $reviewedMissingLicenses = @{
        'native-windows-gui@1.0.13' = @{
            Source = 'https://github.com/gabdube/native-windows-gui/blob/a6c96e8de5d01fe7bb566d737622dfead3cd1aed/LICENSE'
            Note = 'The 1.0.13 archive Cargo.toml/readme declare MIT. Its recorded VCS revision was unavailable; this is the official repository license, not a claimed exact-source match.'
            Text = @'
MIT License

Copyright (c) 2019 Gabriel Dube

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
'@
        }
        'winapi-build@0.1.1' = @{
            Source = 'https://github.com/retep998/winapi-rs/blob/77588710a683e1a4ec085b0f3d6c8939ecc7f030/LICENSE.md'
            Note = 'The archive src/lib.rs matches upstream build/src/lib.rs at this revision and explicitly refers to the MIT LICENSE.md omitted from the archive.'
            Text = @'
The MIT License (MIT)

Copyright (c) 2015 Peter Atashian

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
'@
        }
    }
    $dependencies = @($included | Where-Object { $_ -ne $metadata.resolve.root } |
        ForEach-Object { $packages[$_] } | Sort-Object name, version)
    foreach ($package in $dependencies) {
        if ($reviewedExpressions -cnotcontains $package.license) {
            throw "Review new license expression for $($package.name): $($package.license)"
        }
        $packageRoot = Split-Path -Parent $package.manifest_path
        $licenseFiles = @(Get-ChildItem -LiteralPath $packageRoot -File | Where-Object {
            $_.Name -imatch '^license(?:[-._]mit)?(?:\.(?:txt|md))?$' -or
            $_.Name -imatch '^(?:notice|copyright)(?:\.(?:txt|md))?$'
        })
        if ($package.license -eq '(MIT OR Apache-2.0) AND Unicode-3.0') {
            $unicode = Join-Path $packageRoot 'LICENSE-UNICODE'
            if (-not (Test-Path -LiteralPath $unicode -PathType Leaf)) { throw "Missing Unicode terms for $($package.name)." }
            $licenseFiles += Get-Item -LiteralPath $unicode
        }
        $missingLicense = $null
        if ($licenseFiles.Count -eq 0) {
            $missingLicense = $reviewedMissingLicenses["$($package.name)@$($package.version)"]
            if ($null -eq $missingLicense) { throw "Missing MIT license text for $($package.name)." }
        }
        [void] $text.AppendLine("`n================================================================")
        [void] $text.AppendLine("$($package.name) $($package.version)")
        [void] $text.AppendLine("Declared license: $($package.license)")
        [void] $text.AppendLine("Repository: $($package.repository)")
        if ($null -ne $missingLicense) {
            [void] $text.AppendLine("`n--- License omitted from crate archive; reviewed upstream copy ---")
            [void] $text.AppendLine("Source: $($missingLicense.Source)")
            [void] $text.AppendLine($missingLicense.Note)
            [void] $text.AppendLine($missingLicense.Text)
        }
        foreach ($file in ($licenseFiles | Sort-Object Name)) {
            [void] $text.AppendLine("`n--- $($file.Name) ---")
            [void] $text.AppendLine([IO.File]::ReadAllText($file.FullName))
        }
        if ($package.name -eq 'hidapi') {
            if ($package.version -ne '2.6.7') { throw 'Review bundled HIDAPI license paths after upgrading hidapi.' }
            foreach ($relativePath in @('etc/hidapi/LICENSE.txt', 'etc/hidapi/LICENSE-bsd.txt')) {
                [void] $text.AppendLine("`n--- Bundled native HIDAPI: $relativePath (BSD option selected) ---")
                [void] $text.AppendLine([IO.File]::ReadAllText((Join-Path $packageRoot $relativePath)))
            }
        }
    }
    $destination = [IO.Path]::GetFullPath($OutputPath)
    [void] [IO.Directory]::CreateDirectory((Split-Path -Parent $destination))
    [IO.File]::WriteAllText($destination, $text.ToString().Replace("`r`n", "`n"), $utf8)
    Write-Host "Generated notices for $($dependencies.Count) locked dependencies: $destination"
} finally {
    Pop-Location
}
