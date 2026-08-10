param(
  [string]$CodexHome = (Join-Path $env:USERPROFILE '.codex')
)

$ErrorActionPreference = 'Stop'
$runtimeCommit = 'cb6712f7e17f1c4082c0ad9a39ce225fb895d922'
$runtimeSha256 = '2000F589B4B2A01DC0C6E2FDFC1A0B7AEBF885CF2F4EFF9E37A85D7D4425ED9B'
$runtimeUrl = "https://github.com/chen0416ccc-cpu/codex-windows-fast-patch-skill/archive/$runtimeCommit.zip"
$runtimeRoot = Join-Path $CodexHome 'skills\codex-windows-fast-patch'
$runner = Join-Path $runtimeRoot 'scripts\repatch-codex-windows.ps1'
$runtimeVersionPath = Join-Path $runtimeRoot '.skill-version'
$stageRoot = Join-Path $env:ProgramData ("CC Switch\codex-repair-{0}" -f ([guid]::NewGuid().ToString('N')))
$stageArchivePath = Join-Path $stageRoot 'runtime.zip'
$stageExpandedRoot = Join-Path $stageRoot 'expanded'
$stateRoot = Join-Path $CodexHome 'state'
$logPath = Join-Path $stateRoot 'cc-switch-codex-repair.log'
$statusPath = Join-Path $stateRoot 'cc-switch-codex-repair-status.json'
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)

New-Item -ItemType Directory -Force -Path $stateRoot | Out-Null
function Write-RepairLog([string]$Message) {
  $line = "[{0}] {1}" -f (Get-Date -Format o), $Message
  Add-Content -LiteralPath $logPath -Value $line -Encoding UTF8
  Write-Host $line
}

function Write-RepairStatus([string]$State, [string]$Message) {
  $payload = [ordered]@{
    state = $State
    updatedAt = (Get-Date).ToUniversalTime().ToString('o')
    message = $Message
  }
  [System.IO.File]::WriteAllText(
    $statusPath,
    ($payload | ConvertTo-Json -Compress),
    $utf8NoBom
  )
}

try {
  Write-RepairLog 'CC Switch Codex Desktop repair started.'
  Write-RepairStatus 'running' 'Fast Patch runner is starting.'
  $configPath = Join-Path $CodexHome 'config.toml'
  $backupRoot = Join-Path $CodexHome 'backups\config'
  if (Test-Path -LiteralPath $configPath -PathType Leaf) {
    New-Item -ItemType Directory -Force -Path $backupRoot | Out-Null
    $backupPath = Join-Path $backupRoot ("config.toml.{0}.cc-switch.bak" -f (Get-Date -Format 'yyyyMMdd-HHmmss-fff'))
    Copy-Item -LiteralPath $configPath -Destination $backupPath -Force
    Write-RepairLog "Config backup created: $backupPath"
  }

  # Stage and execute the verified runtime outside the user-writable Codex skills tree.
  # The cached copy is only a convenience for detection and future repair display.
  New-Item -ItemType Directory -Force -Path $stageRoot | Out-Null
  & icacls.exe $stageRoot /inheritance:r /grant:r '*S-1-5-32-544:(OI)(CI)(F)' '*S-1-5-18:(OI)(CI)(F)' | Out-Null
  if ($LASTEXITCODE -ne 0) {
    throw "Unable to secure Fast Patch staging directory: $stageRoot"
  }

  $installedRuntimeCommit = if (Test-Path -LiteralPath $runtimeVersionPath -PathType Leaf) {
    (Get-Content -LiteralPath $runtimeVersionPath -Raw).Trim()
  } else {
    ''
  }
  Invoke-WebRequest -Uri $runtimeUrl -OutFile $stageArchivePath
  $actualSha256 = (Get-FileHash -LiteralPath $stageArchivePath -Algorithm SHA256).Hash
  if ($actualSha256 -ne $runtimeSha256) {
    throw "Fast Patch runtime checksum mismatch: $actualSha256"
  }
  Expand-Archive -LiteralPath $stageArchivePath -DestinationPath $stageExpandedRoot -Force
  $sourceRoot = Get-ChildItem -LiteralPath $stageExpandedRoot -Directory | Select-Object -First 1
  if ($null -eq $sourceRoot) {
    throw 'Fast Patch runtime archive has no root directory.'
  }
  $verifiedRunner = Join-Path $sourceRoot.FullName 'scripts\repatch-codex-windows.ps1'
  if (-not (Test-Path -LiteralPath $verifiedRunner -PathType Leaf)) {
    throw "Fast Patch runner is missing from the verified archive: $verifiedRunner"
  }
  $verifiedRunnerHash = (Get-FileHash -LiteralPath $verifiedRunner -Algorithm SHA256).Hash
  $installedRunnerHash = if (Test-Path -LiteralPath $runner -PathType Leaf) {
    (Get-FileHash -LiteralPath $runner -Algorithm SHA256).Hash
  } else {
    ''
  }
  $needsInstall = (-not (Test-Path -LiteralPath $runner -PathType Leaf)) -or
    $installedRuntimeCommit -ne $runtimeCommit -or
    $installedRunnerHash -ne $verifiedRunnerHash
  if ($needsInstall) {
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $runtimeRoot) | Out-Null
    if (Test-Path -LiteralPath $runtimeRoot) {
      $runtimeBackupRoot = Join-Path $CodexHome 'backups\codex-repair-runtime'
      New-Item -ItemType Directory -Force -Path $runtimeBackupRoot | Out-Null
      $runtimeBackup = Join-Path $runtimeBackupRoot ("codex-windows-fast-patch.{0}" -f (Get-Date -Format 'yyyyMMdd-HHmmss-fff'))
      Move-Item -LiteralPath $runtimeRoot -Destination $runtimeBackup
      Write-RepairLog "Existing incomplete runtime moved to: $runtimeBackup"
    }
    Copy-Item -LiteralPath $sourceRoot.FullName -Destination $runtimeRoot -Recurse -Force
    [System.IO.File]::WriteAllText($runtimeVersionPath, $runtimeCommit, $utf8NoBom)
    Write-RepairLog "Fast Patch runtime installed: $runtimeCommit"
  }

  Write-RepairLog "Launching verified Fast Patch runner: $verifiedRunner"
  & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $verifiedRunner 2>&1 | ForEach-Object {
    $line = [string]$_
    Add-Content -LiteralPath $logPath -Value $line -Encoding UTF8
    Write-Host $line
  }
  $runnerExitCode = $LASTEXITCODE
  if ($runnerExitCode -ne 0) {
    throw "Fast Patch runner exited with code $runnerExitCode"
  }
  Write-RepairStatus 'succeeded' 'Fast Patch runner completed successfully.'
  Write-RepairLog 'CC Switch Codex Desktop repair completed.'
} catch {
  $errorMessage = $_.Exception.Message
  Write-RepairStatus 'failed' $errorMessage
  Write-RepairLog "Repair failed: $errorMessage"
  exit 1
} finally {
  if (Test-Path -LiteralPath $stageRoot) {
    del -LiteralPath $stageRoot -Recurse -Force
  }
}
