# engram plugin SessionEnd reviewer - thin glue (ASCII-only so PS 5.1 parses it).
#   - reads the hook stdin (transcript_path / cwd / session_id)
#   - resolves db paths via the engine
#   - review-prepare: computes the incremental slice (since the last watermark) + a pending
#     marker, so the reviewer only consolidates NEW transcript lines (saves tokens on long
#     / resumed sessions) and an abnormal exit can be caught up next start.
#   - launches a headless reviewer on that slice via launch-reviewer.ps1
# Anti-recursion: the reviewer is itself a claude session; ENGRAM_REVIEWER=1 makes its own
# SessionEnd bail out here.
# Best-effort throughout: a failure here must not disrupt anything, so every error is
# swallowed and we always exit 0.
if ($env:ENGRAM_REVIEWER -eq '1') { exit 0 }
$ErrorActionPreference = 'Stop'
try {
    $root = $env:CLAUDE_PLUGIN_ROOT
    if (-not $root) { exit 0 }
    $ENGRAM = Join-Path $root 'bin\engram-windows-x86_64.exe'
    $CLI = if ($env:ENGRAM_REVIEWER_CLI) { $env:ENGRAM_REVIEWER_CLI } else { 'claude' }

    # read hook stdin JSON
    $raw = [Console]::In.ReadToEnd()
    $hook = $null
    if ($raw) { try { $hook = $raw | ConvertFrom-Json } catch { $hook = $null } }
    $transcript = if ($hook -and $hook.transcript_path) { $hook.transcript_path } else { '' }
    $cwd = if ($hook -and $hook.cwd) { $hook.cwd } else { (Get-Location).Path }
    $sid = if ($hook -and $hook.session_id) { $hook.session_id } else { [System.IO.Path]::GetFileNameWithoutExtension($transcript) }
    if (-not $transcript) { exit 0 }
    if (-not $sid) { $sid = 'session' }

    # resolve db paths via the engine
    $paths = & $ENGRAM resolve --project-dir $cwd --format json | ConvertFrom-Json
    $work = Join-Path $env:USERPROFILE '.engram\pending'
    $wm = Join-Path $env:USERPROFILE '.engram\watermark.json'

    # compute the incremental slice + drop a pending marker
    $planRaw = & $ENGRAM review-prepare --transcript $transcript --session-id $sid --watermark $wm --work-dir $work --general-db $paths.general_db --project-db $paths.project_db --project-name $paths.project_name
    $plan = $null
    if ($planRaw) { try { $plan = $planRaw | ConvertFrom-Json } catch { $plan = $null } }
    if ($plan -and $plan.action -eq 'review') {
        & (Join-Path $root 'scripts\launch-reviewer.ps1') `
            -Root $root -Engram $ENGRAM `
            -Slice $plan.slice -GeneralDb $plan.general_db -ProjectDb $plan.project_db -ProjectName $plan.project_name `
            -Pending $plan.pending -Watermark $wm -Cli $CLI
    }
}
catch {
    Write-Host ("engram session-end skipped: " + $_.Exception.Message)
}
exit 0
