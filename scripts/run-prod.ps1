param(
    [string]$Port = $env:PORT,
    [string]$UiPort = $env:NINEROUTER_UI_PORT,
    [string]$HostAddress = $env:NINEROUTER_HOST,
    [string]$DataDir = $env:NINEROUTER_DATA_DIR
)

$ErrorActionPreference = 'Stop'
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$runMjs = Join-Path $scriptDir "run.mjs"

$argsList = @($runMjs, "prod")
if ($Port) { $argsList += @("--port", $Port) }
if ($UiPort) { $argsList += @("--ui-port", $UiPort) }
if ($HostAddress) { $argsList += @("--host", $HostAddress) }
if ($DataDir) { $argsList += @("--data-dir", $DataDir) }

& (Get-Command node.exe).Source $argsList
exit $LASTEXITCODE
