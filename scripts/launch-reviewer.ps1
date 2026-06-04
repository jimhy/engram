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

# Launch the reviewer FULLY DETACHED so it never blocks the session. Two hard requirements,
# both learned the hard way:
#  1. npm ships `claude` as claude / claude.cmd / claude.ps1 side by side; Start-Process
#     -FilePath 'claude' grabs the extension-less bash shim and dies ("%1 is not a valid
#     Win32 application"). => go through cmd, whose own resolution finds claude.cmd.
#  2. Start-Process -NoNewWindow (UseShellExecute=false) makes the child INHERIT this hook's
#     stdout pipe handle, so Claude Code -- which captures the hook's stdout -- blocks until
#     the reviewer EXITS (observed: a multi-minute hang on session start). Fix verified by
#     experiment (10s -> 0.2s): write a one-shot .cmd that does its OWN stdio redirection,
#     then Start-Process it with -WindowStyle Hidden (UseShellExecute=true), which does NOT
#     inherit handles -> the hook returns instantly while the reviewer runs on, detached.
$promptFile = Join-Path $env:TEMP ("engram-review-" + $PID + "-" + (Get-Random) + ".txt")
Set-Content -LiteralPath $promptFile -Value $prompt -Encoding UTF8
$outFile = Join-Path $env:TEMP ("engram-review-out-" + $PID + "-" + (Get-Random) + ".txt")
$errFile = Join-Path $env:TEMP ("engram-review-err-" + $PID + "-" + (Get-Random) + ".txt")
$cmdFile = Join-Path $env:TEMP ("engram-review-" + $PID + "-" + (Get-Random) + ".cmd")
# one-shot batch: "<cli>" -p < "<prompt>" > "<out>" 2> "<err>"  (every path quoted for spaces)
$line = '"' + $Cli + '" -p < "' + $promptFile + '" > "' + $outFile + '" 2> "' + $errFile + '"'
# ANSI (system codepage) so a non-ASCII TEMP path (e.g. a Chinese Windows username) survives
# when cmd reads the batch file.
Set-Content -LiteralPath $cmdFile -Value $line -Encoding Default
# ENGRAM_REVIEWER=1 is inherited by the launched process (ShellExecute passes the parent env),
# so the reviewer's own SessionStart/SessionEnd hooks bail out -> no recursion.
$env:ENGRAM_REVIEWER = '1'
try {
    Start-Process -FilePath $cmdFile -WindowStyle Hidden -ErrorAction Stop
} catch {
    Write-Host ("engram: failed to launch reviewer: " + $_.Exception.Message)
}
