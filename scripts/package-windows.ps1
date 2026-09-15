# Build a portable x64 Windows directory and ZIP. Never publishes a release.
[CmdletBinding()]
param([string]$Target = 'x86_64-pc-windows-msvc', [switch]$SkipBuild)
$ErrorActionPreference = 'Stop'
if ($Target -ne 'x86_64-pc-windows-msvc') { throw 'Only native x64 MSVC packages are supported.' }
$Root = Split-Path -Parent $PSScriptRoot
Push-Location $Root
try {
    if (-not $SkipBuild) {
        cargo build --locked --release --target $Target --target-dir target
        if ($LASTEXITCODE -ne 0) { throw 'Cargo build failed' }
    }
    $Version = (Select-String -Path Cargo.toml -Pattern '^version = "([^"]+)"').Matches[0].Groups[1].Value
    $Name = "Binwave-$Version-windows-x64"
    $Package = Join-Path $Root "dist/$Name"
    if (Test-Path $Package) { Remove-Item -Recurse -Force $Package }
    New-Item -ItemType Directory -Path $Package -Force | Out-Null
    Copy-Item "target/$Target/release/binwave.exe" "$Package/Binwave.exe"
    Copy-Item README.md $Package
    New-Item -ItemType Directory -Path "$Package/docs" -Force | Out-Null
    Copy-Item docs/platforms.md,docs/platform-validation.md "$Package/docs"
    # Whitelist public fixtures; never package generated/private samples/data files.
    New-Item -ItemType Directory -Path "$Package/samples/data" -Force | Out-Null
    Copy-Item samples/data/sample.csv,samples/data/sensors.bin "$Package/samples/data"
    Copy-Item samples/decoders,samples/configs,samples/generators,samples/quickstart -Destination "$Package/samples" -Recurse
    Copy-Item samples/*.md "$Package/samples"
    New-Item -ItemType Directory -Path "$Package/python" -Force | Out-Null
    Copy-Item python/README.md,python/runner.py "$Package/python"
    Copy-Item scripts/Launch-Binwave.cmd $Package
    $Zip = Join-Path $Root "dist/$Name.zip"
    Compress-Archive -Path $Package -DestinationPath $Zip -Force
    $Hash = (Get-FileHash $Zip -Algorithm SHA256).Hash.ToLowerInvariant()
    "$Hash  $Name.zip" | Set-Content "$Zip.sha256" -Encoding ascii
    Write-Host "Created $Zip"
} finally { Pop-Location }
