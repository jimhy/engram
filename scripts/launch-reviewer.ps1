# engram - 为单次巩固任务起一个 headless 复盘者（Windows 版；macOS/Linux 走 launch-reviewer.sh）。
# 由 SessionEnd 复盘与 SessionStart 补跑共用。本文件为带 BOM 的 UTF-8（含中文注释，
# BOM 保证 Windows PowerShell 5.1 正确解码）。
# 交给复盘者 bash 的所有路径统一正斜杠（bash 会吃掉反斜杠）。
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

# scope kind: the engine puts a 'workspace' marker next to a management-directory db
# (<dir>/.engram/workspace). Detect it here so BOTH callers (session-end + catch-up,
# whose pending plan has no kind field) get the right reviewer rules.
$kind = 'project'
try {
    $marker = Join-Path (Split-Path -Parent $ProjectDb) 'workspace'
    if (Test-Path -LiteralPath $marker) { $kind = 'workspace' }
} catch {}

$tpl = Get-Content -Raw -Encoding UTF8 -LiteralPath (Join-Path $Root 'scripts\reviewer-prompt.md')
$prompt = $tpl
$prompt = $prompt.Replace('{{TRANSCRIPT}}', $slice)
$prompt = $prompt.Replace('{{ENGRAM}}', $engramFwd)
$prompt = $prompt.Replace('{{GENERAL_DB}}', $general)
$prompt = $prompt.Replace('{{PROJECT_DB}}', $project)
$prompt = $prompt.Replace('{{PROJECT_NAME}}', $ProjectName)
$prompt = $prompt.Replace('{{KIND}}', $kind)
$prompt = $prompt.Replace('{{PENDING}}', $pending)
$prompt = $prompt.Replace('{{WATERMARK}}', $watermark)
$prompt = $prompt.Replace('{{SKILL}}', $skill)

# 最小显式授权：headless（claude -p）下未被 allowlist 覆盖的工具调用会被直接拒绝且无人应答，
# 导致复盘静默失败。这里只放行两样：Read（读转录切片 / SKILL.md，均在 cwd 之外）与
# engram 引擎二进制的 Bash 调用。规则前缀用与提示词一致的正斜杠路径；再加一条带引号变体，
# 覆盖模型给路径加引号的写法（路径含空格时必然如此）。cmd 批处理里引号按 "" 转义。
$allowedArgs = '--allowedTools "Read" "Bash(' + $engramFwd + ':*)" "Bash(""' + $engramFwd + '"":*)"'

# ---- 代理派生（堵 403）：headless 的 `claude -p` 复盘子进程若缺 HTTPS_PROXY 会走直连，
# 被 Anthropic 以「403 Request not allowed」按地区限制拒绝。启动前确保 HTTPS_PROXY/HTTP_PROXY
# 有值，优先级：① ENGRAM_REVIEWER_PROXY 显式指定；② 已有 HTTPS_PROXY/https_proxy → 继承不动；
# ③ ALL_PROXY/all_proxy → 据以设两者；④（仅 Windows）系统代理注册表 ProxyEnable=1 且 ProxyServer
# 非空 → 据以设；⑤ 都没有 → 不设（直连，可能失败，留给下面的 403 告警）。ProxyServer 可能是
# host:port（无 scheme），统一补 http://。派生结果经 Start-Process 的 ShellExecute 随父环境继承给复盘者。
function Get-NormalizedProxy {
    param([string]$Value)
    $v = $Value.Trim()
    if ($v -notmatch '^[a-zA-Z][a-zA-Z0-9+.-]*://') { $v = 'http://' + $v }
    return $v
}
function Set-ReviewerProxy {
    if ($env:ENGRAM_REVIEWER_PROXY) {
        $p = Get-NormalizedProxy $env:ENGRAM_REVIEWER_PROXY
        $env:HTTPS_PROXY = $p; $env:HTTP_PROXY = $p
        return
    }
    if ($env:HTTPS_PROXY -or $env:https_proxy) { return }  # 继承环境里已有的代理
    $allp = if ($env:ALL_PROXY) { $env:ALL_PROXY } elseif ($env:all_proxy) { $env:all_proxy } else { '' }
    if ($allp) {
        $p = Get-NormalizedProxy $allp
        $env:HTTPS_PROXY = $p; $env:HTTP_PROXY = $p
        return
    }
    try {
        $reg = Get-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Internet Settings' -ErrorAction Stop
        if ($reg.ProxyEnable -eq 1 -and $reg.ProxyServer) {
            $ps = [string]$reg.ProxyServer
            # ProxyServer 可能是 "host:port"（全协议）或 "http=h:p;https=h:p;..."（分协议），优先取 https。
            if ($ps -match 'https=([^;]+)') { $ps = $matches[1] }
            elseif ($ps -match 'http=([^;]+)') { $ps = $matches[1] }
            if ($ps.Trim()) {
                $p = Get-NormalizedProxy $ps
                $env:HTTPS_PROXY = $p; $env:HTTP_PROXY = $p
            }
        }
    } catch {}
}
Set-ReviewerProxy

if ($env:ENGRAM_HOOK_DRYRUN -eq '1') {
    Write-Host "[dry-run] reviewer-cli = $Cli"
    Write-Host "[dry-run] allowed      = $allowedArgs"
    Write-Host "[dry-run] proxy        = $($env:HTTPS_PROXY)"
    Write-Host "[dry-run] slice        = $slice"
    Write-Host "[dry-run] general      = $general"
    Write-Host "[dry-run] project      = $project ($ProjectName, kind=$kind)"
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
# 启动前清扫：%TEMP% 下超过 7 天的 engram-review-*（prompt/out/err/cmd 残留）一律删掉，
# 防止历史输出无限堆积。best-effort，失败不影响启动。
try {
    $cutoff = (Get-Date).AddDays(-7)
    Get-ChildItem -LiteralPath $env:TEMP -Filter 'engram-review-*' -File -ErrorAction Stop |
        Where-Object { $_.LastWriteTime -lt $cutoff } |
        Remove-Item -Force -ErrorAction SilentlyContinue
} catch {}

$promptFile = Join-Path $env:TEMP ("engram-review-" + $PID + "-" + (Get-Random) + ".txt")
Set-Content -LiteralPath $promptFile -Value $prompt -Encoding UTF8
$outFile = Join-Path $env:TEMP ("engram-review-out-" + $PID + "-" + (Get-Random) + ".txt")
$errFile = Join-Path $env:TEMP ("engram-review-err-" + $PID + "-" + (Get-Random) + ".txt")
$cmdFile = Join-Path $env:TEMP ("engram-review-" + $PID + "-" + (Get-Random) + ".cmd")
# 一次性批处理，三步：
#   1. call "<cli>" -p --allowedTools ... < "<prompt>" > "<out>" 2> "<err>"  （所有路径加引号防空格；
#      必须 call——npm 的 claude 实为 claude.cmd，批处理里不 call 直接调另一个批处理会把控制权
#      整个移交过去，后面的日志与自删行永远不会执行）
#   2. 复盘者退出后调 reviewer-log.ps1，把退出码 + out/err 尾部追进 ~/.engram/hook.log（可观测）
#   3. del "%~f0" 自删本 .cmd（prompt/out/err 留给上面的 7 天清扫，便于排查）
$logScript = Join-Path $Root 'scripts\reviewer-log.ps1'
$hookLog = Join-Path $env:USERPROFILE '.engram\hook.log'
$cmdLines = @(
    '@echo off',
    ('call "' + $Cli + '" -p ' + $allowedArgs + ' < "' + $promptFile + '" > "' + $outFile + '" 2> "' + $errFile + '"'),
    ('powershell -NoProfile -ExecutionPolicy Bypass -File "' + $logScript + '" -OutFile "' + $outFile + '" -ErrFile "' + $errFile + '" -ExitCode %ERRORLEVEL% -LogFile "' + $hookLog + '"'),
    'del "%~f0"'
)
# ANSI (system codepage) so a non-ASCII TEMP path (e.g. a Chinese Windows username) survives
# when cmd reads the batch file.
Set-Content -LiteralPath $cmdFile -Value $cmdLines -Encoding Default
# ENGRAM_REVIEWER=1 is inherited by the launched process (ShellExecute passes the parent env),
# so the reviewer's own SessionStart/SessionEnd hooks bail out -> no recursion.
$env:ENGRAM_REVIEWER = '1'
try {
    Start-Process -FilePath $cmdFile -WindowStyle Hidden -ErrorAction Stop
} catch {
    Write-Host ("engram: failed to launch reviewer: " + $_.Exception.Message)
}
