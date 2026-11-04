# BuildRelease.ps1
# Builds the next version, packs it with Velopack and publishes it as a GitHub release.
#   .\BuildRelease.ps1             build, pack and upload
#   .\BuildRelease.ps1 -LocalOnly  build and pack into .\Releases only
#
# Requires the vpk tool, matching the velopack crate version in Cargo.toml:
#   dotnet tool install -g vpk --version 1.2.161
# and, unless -LocalOnly, the GitHub CLI logged in with write access to the repo (gh auth login).
param([switch]$LocalOnly)

Set-Location -Path $PSScriptRoot
$ErrorActionPreference = 'Stop'

$AppName = "WeekNumber"
$ExeName = "weeknumber.exe"
$RepoUrl = "https://github.com/kreaton/WeekNumber"
$FirstVersion = "1.0.0"
$PublishDir = ".\publish"
$ReleasesDir = ".\Releases"

if (-not $LocalOnly) {
    # Fetch the latest published release so the version continues from it and vpk can build a delta
    $token = gh auth token
    if ($LASTEXITCODE -ne 0) { throw "Not logged in to GitHub CLI (run: gh auth login)" }
    vpk download github --repoUrl $RepoUrl --token $token -o $ReleasesDir
    if ($LASTEXITCODE -ne 0) { throw "vpk download failed" }
}

# Find the highest version already packed. It is read from the full package's file name because
# vpk download only fetches the package: a clean Releases folder has no releases.win.json.
$largestVersion = Get-ChildItem -Path $ReleasesDir -Filter "$AppName-*-full.nupkg" -ErrorAction SilentlyContinue
    | ForEach-Object { [version]($_.Name -replace "^$AppName-|-full\.nupkg$") }
    | Sort-Object -Descending
    | Select-Object -First 1
if ($largestVersion) {
    # Increment the patch number by 1
    $newVersion = "{0}.{1}.{2}" -f $largestVersion.Major, $largestVersion.Minor, ($largestVersion.Build + 1)
}
else {
    $newVersion = $FirstVersion
}

Write-Host "Building $AppName version $newVersion (win-x64, static C runtime)"
& .\build.ps1
if (Test-Path $PublishDir) { Remove-Item $PublishDir -Recurse -Force }
New-Item -ItemType Directory -Path $PublishDir | Out-Null
Copy-Item ".\target\release\$ExeName" $PublishDir

Write-Host "Packing $AppName version $newVersion with Velopack"
vpk pack -u $AppName -v $newVersion -p $PublishDir -e $ExeName -o $ReleasesDir --runtime win-x64
if ($LASTEXITCODE -ne 0) { throw "vpk pack failed" }

if (-not $LocalOnly) {
    Write-Host "Uploading $AppName version $newVersion to GitHub"
    vpk upload github --repoUrl $RepoUrl --token $token -o $ReleasesDir --publish --tag "v$newVersion" --releaseName "$AppName $newVersion"
    if ($LASTEXITCODE -ne 0) { throw "vpk upload failed" }
}

Write-Host "Built $AppName version $newVersion"
