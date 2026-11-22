# Builds target\release\copyshot.exe and wraps it in dist\Copyshot-Setup-<version>.exe.
$projectRoot = Split-Path -Parent $PSScriptRoot
$isccCandidates = @(
    (Join-Path ${env:ProgramFiles(x86)} "Inno Setup 6\ISCC.exe"),
    (Join-Path $env:ProgramFiles "Inno Setup 6\ISCC.exe"),
    (Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 6\ISCC.exe")
)
$iscc = $isccCandidates | Where-Object { $_ -and (Test-Path -LiteralPath $_) } | Select-Object -First 1

if (-not $iscc) {
    throw "Inno Setup 6 was not found. Install it with: winget install --id JRSoftware.InnoSetup -e"
}

Push-Location $projectRoot
try {
    # Not using $ErrorActionPreference = "Stop": Windows PowerShell treats cargo's progress
    # output on stderr as an error. Exit codes are checked instead.
    & cargo build --release
    if ($LASTEXITCODE) {
        exit $LASTEXITCODE
    }

    & $iscc "installer\copyshot.iss"
    if ($LASTEXITCODE) {
        exit $LASTEXITCODE
    }
}
finally {
    Pop-Location
}

Write-Host "Installer created in $projectRoot\dist"
