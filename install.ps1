# Resolve the destination path (~/.local/bin equivalent)
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

# Copy the binary using the public command name
$sourcePath = "target\release\cleandl.exe"
$destinationPath = Join-Path $syncBinDir "cleandl.exe"
Write-Host "Copying binary to $destinationPath..." -ForegroundColor Cyan
Copy-Item -Path $sourcePath -Destination $destinationPath -Force

Write-Host "Installation successful!" -ForegroundColor Green
