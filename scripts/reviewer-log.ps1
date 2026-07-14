# engram - 复盘者收尾日志：把 headless 复盘者的退出码与 out/err 文件尾部若干行
# 追加进 ~/.engram/hook.log，让"复盘静默失败"变得可观测。
# 由 launch-reviewer.ps1 生成的一次性 .cmd 在复盘者进程退出后调用。
# 全程 best-effort：日志写不进也绝不影响任何主流程，恒 exit 0。
param(
    [string]$OutFile,
    [string]$ErrFile,
    [string]$ExitCode = '',
    [string]$LogFile
)
$ErrorActionPreference = 'SilentlyContinue'
try {
    if (-not $LogFile) { $LogFile = Join-Path $env:USERPROFILE '.engram\hook.log' }
    $dir = Split-Path -Parent $LogFile
    if ($dir -and -not (Test-Path -LiteralPath $dir)) {
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
    }
    $ts = Get-Date -Format 'yyyy-MM-dd HH:mm:ss'
    $buf = New-Object System.Collections.Generic.List[string]
    $buf.Add("[$ts] engram reviewer done exit=$ExitCode")
    $pairs = @()
    $pairs += , @('out', $OutFile)
    $pairs += , @('err', $ErrFile)
    $diag = $false
    if ($ExitCode -and $ExitCode -ne '0') { $diag = $true }
    foreach ($pair in $pairs) {
        $tag = $pair[0]
        $f = $pair[1]
        if ($f -and (Test-Path -LiteralPath $f)) {
            $tail = Get-Content -LiteralPath $f -Encoding UTF8 -Tail 5
            foreach ($ln in @($tail)) {
                if ($null -ne $ln -and $ln.Trim() -ne '') { $buf.Add("  " + $tag + "| " + $ln) }
            }
            # 认证/地区限制诊断：out/err 含 403 / not allowed / authenticate 触发提示。
            $raw = Get-Content -LiteralPath $f -Encoding UTF8 -Raw
            if ($raw -match '(?i)403|not allowed|authenticate') { $diag = $true }
        }
    }
    # 非零退出，或输出含认证失败标记时，补一句常见根因提示（接批2健康面）。
    if ($diag) {
        $buf.Add("  diag| 复盘者请求被拒(403)——常见原因：headless 子进程缺 HTTPS_PROXY 走了直连触发地区限制；请确认 HTTPS_PROXY 已设或用 ENGRAM_REVIEWER_PROXY 指定")
    }
    Add-Content -LiteralPath $LogFile -Value $buf -Encoding UTF8
} catch {}
exit 0
