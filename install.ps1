# 1. Detect Architecture to match your Makefile naming convention
$arch = $env:PROCESSOR_ARCHITECTURE.ToLower()
if ($arch -eq "amd64") { $arch = "x86_64" }

$targetName = "cleandl-rs-windows-$arch"

# 2. Resolve the destination path (~/.local/bin equivalent)
$syncBinDir = Join-Path $HOME ".local\bin"
if (!( Test-Path $syncBinDir)) {
	New-Item -Path $syncBinDir -ItemType "Directory" -Force
}

Write-Host "Building cleandl in release mode..." -ForegroundColor Cyan
cargo build --release

if ($LASTEXITCODE -ne 0) {
    Write-Error "build failed."
    exit $LASTEXITCODE
}

# 3. Copy binary to the target location with architecture suffix
$sourcePath = "target\release\cleandl-rs.exe"
$destinationPath = Join-Path $syncBinDir "$targetName.exe"
Write-Host "Copying binary to $destinationPath..." -ForegroundColor Cyan
Move-Item -Path $sourcePath -Destination $destinationPath -Force

Write-Host "Installation successful!" -ForegroundColor Green
