# engram plugin SessionStart catch-up. ASCII-only so PS 5.1 parses it.
#   If a previous session ended abnormally (Ctrl+C hard-kill / window closed / power loss)
#   the reviewer never finished and left a pending marker behind. On the next session start
#   we scan for the most recent leftover and re-run consolidation on it (the slice is the
#   un-consolidated increment, so nothing is lost and nothing is re-done).
# Anti-recursion: the reviewer is itself a claude session whose own SessionStart fires this;
# ENGRAM_REVIEWER=1 makes it bail out here.
$ErrorActionPreference = 'Stop'
if ($env:ENGRAM_REVIEWER -eq '1') { exit 0 }

$root = $env:CLAUDE_PLUGIN_ROOT
if (-not $root) { exit 0 }
$ENGRAM = Join-Path $root 'bin\engram-windows-x86_64.exe'
$CLI = if ($env:ENGRAM_REVIEWER_CLI) { $env:ENGRAM_REVIEWER_CLI } else { 'claude' }

$work = Join-Path $env:USERPROFILE '.engram\pending'
$wm = Join-Path $env:USERPROFILE '.engram\watermark.json'

# ask the engine whether there is a leftover pending to catch up on (it also clears older ones)
$planRaw = & $ENGRAM catchup-scan --work-dir $work
$plan = $null
if ($planRaw) { try { $plan = $planRaw | ConvertFrom-Json } catch { $plan = $null } }
if (-not $plan -or $plan.action -ne 'review') { exit 0 }   # none / parse error -> nothing to do

& (Join-Path $root 'scripts\launch-reviewer.ps1') `
    -Root $root -Engram $ENGRAM `
    -Slice $plan.slice -GeneralDb $plan.general_db -ProjectDb $plan.project_db -ProjectName $plan.project_name `
    -Pending $plan.pending -Watermark $wm -Cli $CLI
exit 0
