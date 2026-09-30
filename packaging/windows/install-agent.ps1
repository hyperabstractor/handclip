# Installs handclip-agent.exe (placed next to this script) for the current user and
# starts it at every sign-in. On first launch Handclip opens Settings to finish setup.
$ErrorActionPreference = "Stop"

$root = Join-Path $env:LOCALAPPDATA "Handclip"
$binary = Join-Path $root "bin\handclip-agent.exe"
$source = Join-Path $PSScriptRoot "handclip-agent.exe"
$runKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"

# Upgrade from pre-release "Connet" installs: stop the old agent and move its data.
Get-Process -Name "connet-agent" -ErrorAction SilentlyContinue | Stop-Process -Force
Remove-ItemProperty -Path $runKey -Name "ConnetAgent" -ErrorAction SilentlyContinue
$legacyRoot = Join-Path $env:LOCALAPPDATA "Connet"
if ((Test-Path -LiteralPath $legacyRoot) -and -not (Test-Path -LiteralPath $root)) {
    Start-Sleep -Milliseconds 500
    Move-Item -LiteralPath $legacyRoot -Destination $root
    Remove-Item -LiteralPath (Join-Path $root "bin\connet-agent.exe") -ErrorAction SilentlyContinue
    $config = Join-Path $root "config.json"
    if (Test-Path -LiteralPath $config) {
        $json = (Get-Content -LiteralPath $config -Raw) -replace '\\\\Connet\\\\', '\\Handclip\\'
        [System.IO.File]::WriteAllText($config, $json, (New-Object System.Text.UTF8Encoding($false)))
    }
}

if (Test-Path -LiteralPath $source) {
    New-Item -ItemType Directory -Force -Path (Split-Path $binary) | Out-Null
    Get-Process -Name "handclip-agent" -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 500
    Copy-Item -LiteralPath $source -Destination $binary -Force
    # Clear the downloaded-file mark so Windows starts it without a SmartScreen prompt.
    Unblock-File -LiteralPath $binary
}
if (-not (Test-Path -LiteralPath $binary)) {
    throw "Put handclip-agent.exe next to this script: $source"
}

$legacyTask = Get-ScheduledTask -TaskName "Connet Agent" -ErrorAction SilentlyContinue
if ($null -ne $legacyTask) {
    Unregister-ScheduledTask -TaskName "Connet Agent" -Confirm:$false
}

Set-ItemProperty -Path $runKey -Name "HandclipAgent" -Value "`"$binary`""
Start-Process -FilePath $binary

Write-Output "Handclip is running. On a new install, finish setup in its Settings window."
