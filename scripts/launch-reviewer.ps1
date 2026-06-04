# engram - launch one headless reviewer for a single consolidation task.
# Shared by SessionEnd review and SessionStart catch-up. ASCII-only so PS 5.1 parses it.
# All paths handed to the reviewer's bash are forward-slashed (bash eats backslashes).
param(
    [Parameter(Mandatory = $true)][string]$Root,
    [Parameter(Mandatory = $true)][string]$Engram,
    [Parameter(Mandatory = $true)][string]$Slice,
    [Parameter(Mandatory = $true)][string]$GeneralDb,
    [Parameter(Mandatory = $true)][string]$ProjectDb,
    [Parameter(Mandatory = $true)][string]$ProjectName,
    [Parameter(Mandatory = $true)][string]$Pending,
    [Parameter(Mandatory = $true)][string]$Watermark,
    [string]$Cli = 'claude'
)
$ErrorActionPreference = 'Stop'

# forward-slash everything that goes into the reviewer's bash commands
$fs = { param($p) if ($p) { $p -replace '\\', '/' } else { $p } }
$slice     = & $fs $Slice
$engramFwd = & $fs $Engram
$general   = & $fs $GeneralDb
$project   = & $fs $ProjectDb
$pending   = & $fs $Pending
$watermark = & $fs $Watermark
$skill     = & $fs (Join-Path $Root 'skills\engram\SKILL.md')

$tpl = Get-Content -Raw -Encoding UTF8 -LiteralPath (Join-Path $Root 'scripts\reviewer-prompt.md')
$prompt = $tpl
$prompt = $prompt.Replace('{{TRANSCRIPT}}', $slice)
$prompt = $prompt.Replace('{{ENGRAM}}', $engramFwd)
$prompt = $prompt.Replace('{{GENERAL_DB}}', $general)
$prompt = $prompt.Replace('{{PROJECT_DB}}', $project)
$prompt = $prompt.Replace('{{PROJECT_NAME}}', $ProjectName)
$prompt = $prompt.Replace('{{PENDING}}', $pending)
$prompt = $prompt.Replace('{{WATERMARK}}', $watermark)
$prompt = $prompt.Replace('{{SKILL}}', $skill)

if ($env:ENGRAM_HOOK_DRYRUN -eq '1') {
    Write-Host "[dry-run] reviewer-cli = $Cli"
    Write-Host "[dry-run] slice        = $slice"
    Write-Host "[dry-run] general      = $general"
    Write-Host "[dry-run] project      = $project ($ProjectName)"
    Write-Host "[dry-run] pending      = $pending"
    Write-Host "[dry-run] watermark    = $watermark"
    Write-Host "[dry-run] skill        = $skill"
    Write-Host "[dry-run] prompt $($prompt.Length) chars"
    return
}

# launch headless reviewer in background; prompt via stdin (long multiline arg gets word-split)
$promptFile = Join-Path $env:TEMP ("engram-review-" + $PID + "-" + (Get-Random) + ".txt")
Set-Content -LiteralPath $promptFile -Value $prompt -Encoding UTF8
$env:ENGRAM_REVIEWER = '1'
$outFile = Join-Path $env:TEMP ("engram-review-out-" + $PID + ".txt")
$errFile = Join-Path $env:TEMP ("engram-review-err-" + $PID + ".txt")
Start-Process -FilePath $Cli -ArgumentList '-p' -RedirectStandardInput $promptFile -RedirectStandardOutput $outFile -RedirectStandardError $errFile -NoNewWindow
