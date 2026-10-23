<#
.SYNOPSIS
    Flattens a source folder structure (including SMB shares) into a single destination folder.
.DESCRIPTION
    Recursively finds all files in a source path and moves them to a destination folder.
    Handles filename collisions automatically. Defaults to Dry Run mode for safety.
.PARAMETER SourcePath
    The path to the source folder or SMB network share.
.PARAMETER DestinationPath
    The path to the target destination folder.
.PARAMETER DryRun
    If true (default), simulates the operation without moving files. Pass -DryRun:$false to execute.
.EXAMPLE
    .\MoveBack.ps1 -SourcePath "\\server\source_share" -DestinationPath "C:\Dest"
.EXAMPLE
    .\MoveBack.ps1 -SourcePath "C:\Source" -DestinationPath "C:\Dest" -DryRun:$false
#>

[CmdletBinding()]
param(
    [Parameter(Mandatory=$true, HelpMessage="Enter the source folder path or SMB share.")]
    [string]$SourcePath,

    [Parameter(Mandatory=$true, HelpMessage="Enter the destination folder path.")]
    [string]$DestinationPath,

    [Parameter(Mandatory=$false)]
    [bool]$DryRun = $true
)

Write-Host "--- Configuration ---" -ForegroundColor Cyan
Write-Host "Source      : $SourcePath"
Write-Host "Destination : $DestinationPath"
Write-Host "Dry Run     : $DryRun"
Write-Host "---------------------" -ForegroundColor Cyan

if ($DryRun) {
    Write-Host "[INFO] Running in DRY RUN mode. No files will be moved.`n" -ForegroundColor Yellow
}

# Verify source path exists
if (-not (Test-Path -LiteralPath $SourcePath)) {
    Write-Error "Source path does not exist or is not accessible: $SourcePath"
    return
}

# Ensure destination folder exists (or simulate in dry run)
if (-not (Test-Path -LiteralPath $DestinationPath)) {
    if ($DryRun) {
        Write-Host "[DRY RUN] Would create destination folder: $DestinationPath" -ForegroundColor DarkCyan
    } else {
        New-Item -ItemType Directory -Path $DestinationPath -Force | Out-Null
        Write-Host "Created destination folder: $DestinationPath" -ForegroundColor Green
    }
}

# Get all files recursively from the source
Write-Host "Scanning source folder..." -ForegroundColor Cyan
$files = Get-ChildItem -LiteralPath $SourcePath -Recurse -File

if (-not $files) {
    Write-Host "No files found in the source path." -ForegroundColor Yellow
    return
}

Write-Host "Found $($files.Count) file(s) to process.`n" -ForegroundColor Cyan
$processedCount = 0

# Track simulated destination filenames during dry run to handle collisions accurately
$simulatedDestinations = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
if ($DryRun -and (Test-Path -LiteralPath $DestinationPath)) {
    Get-ChildItem -LiteralPath $DestinationPath -ErrorAction SilentlyContinue | ForEach-Object { 
        [void]$simulatedDestinations.Add($_.Name) 
    }
}

foreach ($file in $files) {
    $targetFileName = $file.Name
    $destFile = Join-Path $DestinationPath $targetFileName

    if ($DryRun) {
        # Check against simulated destination set or actual file system if it already exists
        while ($simulatedDestinations.Contains($targetFileName) -or (Test-Path (Join-Path $DestinationPath $targetFileName))) {
            $baseName = [System.IO.Path]::GetFileNameWithoutExtension($file.Name)
            $extension = $file.Extension
            $counter = 1
            do {
                $targetFileName = "${baseName}_$counter${extension}"
                $counter++
            } while ($simulatedDestinations.Contains($targetFileName) -or (Test-Path (Join-Path $DestinationPath $targetFileName)))
        }
        [void]$simulatedDestinations.Add($targetFileName)
        $destFile = Join-Path $DestinationPath $targetFileName
        
        Write-Host "[DRY RUN] Would move: '$($file.FullName)' -> '$destFile'" -ForegroundColor DarkYellow
    } else {
        # Live run collision handling
        if (Test-Path -LiteralPath $destFile) {
            $baseName = [System.IO.Path]::GetFileNameWithoutExtension($file.Name)
            $extension = $file.Extension
            $counter = 1
            
            do {
                $targetFileName = "${baseName}_$counter${extension}"
                $destFile = Join-Path $DestinationPath $targetFileName
                $counter++
            } while (Test-Path -LiteralPath $destFile)
            
            Write-Host "Collision detected for '$($file.Name)'. Renaming to: $targetFileName" -ForegroundColor Yellow
        }

        Move-Item -LiteralPath $file.FullName -Destination $destFile -Force
        Write-Host "Moved: $($file.Name) -> $targetFileName" -ForegroundColor Green
    }
    $processedCount++
}

if ($DryRun) {
    Write-Host "`nDry run complete. Simulated processing of $processedCount file(s)." -ForegroundColor Cyan
    Write-Host "To execute the move for real, run the command again with: -DryRun:`$false" -ForegroundColor Yellow
} else {
    Write-Host "`nSuccessfully moved $processedCount file(s) to $DestinationPath." -ForegroundColor Green
}
