# Builds the release exe and packages it into installers\Flask-<version>-<timestamp>-setup.exe.
# Every run produces a new, separately named installer; older ones are kept.
# Requires Rust (cargo) and Inno Setup 6 (winget install JRSoftware.InnoSetup).
$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot

$iscc = @(
    (Get-Command iscc -ErrorAction SilentlyContinue).Source,
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
if (-not $iscc) { throw 'Inno Setup 6 not found. Install it with: winget install JRSoftware.InnoSetup' }

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { $env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path" }

# The workspace version in Cargo.toml is the single source of truth.
$version = (Select-String -Path "$root\Cargo.toml" -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches.Groups[1].Value
if (-not $version) { throw 'Could not read the version from Cargo.toml' }
$name = "Flask-$version-$(Get-Date -Format 'yyyyMMdd-HHmmss')-setup"

Push-Location $root
try {
    cargo build --release -p flask
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }

    & $iscc /Qp "/DAppVersion=$version" "/DSourceExe=$root\target\release\flask.exe" "/DOutputName=$name" `
        "/O$root\installers" "$root\packaging\flask.iss"
    if ($LASTEXITCODE -ne 0) { throw 'Inno Setup compile failed' }
}
finally { Pop-Location }

Get-Item "$root\installers\$name.exe"
