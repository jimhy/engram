# engram plugin SessionEnd reviewer - thin glue (ASCII-only so PS 5.1 parses it).
#   - locates the bundled binary + prompt via $env:CLAUDE_PLUGIN_ROOT
#   - launches a headless reviewer via $env:ENGRAM_REVIEWER_CLI (default: claude)
#   - all paths handed to the reviewer's bash are forward-slashed (bash eats backslashes)
# Anti-recursion: the reviewer is itself a claude session; we mark it ENGRAM_REVIEWER=1 so its
# own SessionEnd bails out here.

$ErrorActionPreference = 'Stop'
if ($env:ENGRAM_REVIEWER -eq '1') { exit 0 }

$root = $env:CLAUDE_PLUGIN_ROOT
if (-not $root) { exit 0 }
$ENGRAM = Join-Path $root 'bin\engram-windows-x86_64.exe'
$PROMPT_TEMPLATE = Join-Path $root 'scripts\reviewer-prompt.md'
$CLAUDE = if ($env:ENGRAM_REVIEWER_CLI) { $env:ENGRAM_REVIEWER_CLI } else { 'claude' }

# read hook stdin JSON
$raw = [Console]::In.ReadToEnd()
$hook = $null
if ($raw) { try { $hook = $raw | ConvertFrom-Json } catch { $hook = $null } }
$transcript = if ($hook -and $hook.transcript_path) { $hook.transcript_path } else { '' }
$cwd = if ($hook -and $hook.cwd) { $hook.cwd } else { (Get-Location).Path }

# resolve db paths via the engine
$paths = & $ENGRAM resolve --project-dir $cwd --format json | ConvertFrom-Json

# forward-slash everything that goes into the reviewer's bash commands
$fs = { param($p) if ($p) { $p -replace '\\','/' } else { $p } }
$general    = & $fs $paths.general_db
$project    = & $fs $paths.project_db
$pname      = $paths.project_name
$transcript = & $fs $transcript
$engramFwd  = & $fs $ENGRAM

$tpl = Get-Content -Raw -Encoding UTF8 -LiteralPath $PROMPT_TEMPLATE
$prompt = $tpl
$prompt = $prompt.Replace('{{TRANSCRIPT}}', $transcript)
$prompt = $prompt.Replace('{{GENERAL_DB}}', $general)
$prompt = $prompt.Replace('{{PROJECT_DB}}', $project)
$prompt = $prompt.Replace('{{PROJECT_NAME}}', $pname)
$prompt = $prompt.Replace('{{ENGRAM}}', $engramFwd)

if ($env:ENGRAM_HOOK_DRYRUN -eq '1') {
    Write-Host "[dry-run] reviewer-cli = $CLAUDE"
    Write-Host "[dry-run] engram       = $engramFwd"
    Write-Host "[dry-run] transcript   = $transcript"
    Write-Host "[dry-run] general      = $general"
    Write-Host "[dry-run] project      = $project ($pname)"
    Write-Host "[dry-run] prompt $($prompt.Length) chars"
    exit 0
}

# launch headless reviewer in background; prompt via stdin (long multiline arg gets word-split)
$promptFile = Join-Path $env:TEMP ("engram-review-" + $PID + ".txt")
Set-Content -LiteralPath $promptFile -Value $prompt -Encoding UTF8
$env:ENGRAM_REVIEWER = '1'
Start-Process -FilePath $CLAUDE -ArgumentList '-p' -RedirectStandardInput $promptFile -RedirectStandardOutput (Join-Path $env:TEMP 'engram-review-out.txt') -RedirectStandardError (Join-Path $env:TEMP 'engram-review-err.txt') -NoNewWindow
exit 0
