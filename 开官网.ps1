# ============================================================================
#  开启 GitHub Pages（main 分支 /docs 目录），并等待首次构建完成
#  凭据复用 推送.bat 登录时存进 Windows 凭据管理器的那份。
# ============================================================================
[CmdletBinding()]
param(
    [string]$Repo = 'JFbj-create/VortexDL',
    [string]$Branch = 'main',
    [string]$Path = '/docs'
)
$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
[Net.ServicePointManager]::CheckCertificateRevocationList = $false

function Say([string]$m, [string]$c = 'Gray') { Write-Host $m -ForegroundColor $c }

$fill = "protocol=https`nhost=github.com`n`n" | git credential fill 2>$null
$Token = ''
foreach ($l in $fill) { if ($l -like 'password=*') { $Token = $l.Substring(9) } }
if (-not $Token) { Say '没读到凭据，先跑 推送.bat 登录一次。' 'Red'; exit 1 }

$H = @{
    Authorization          = "Bearer $Token"
    Accept                 = 'application/vnd.github+json'
    'X-GitHub-Api-Version' = '2022-11-28'
    'User-Agent'           = 'VortexDL-Pages'
}
$api = "https://api.github.com/repos/$Repo"

Say "[1/3] 当前 Pages 状态 ..."
$exists = $false
try {
    $cur = Invoke-RestMethod -Uri "$api/pages" -Headers $H -Method Get -TimeoutSec 40
    $exists = $true
    Say ("      已存在: " + $cur.html_url + "  status=" + $cur.status + "  branch=" + $cur.source.branch + " path=" + $cur.source.path) 'Yellow'
} catch {
    Say "      还没有 Pages（$($_.Exception.Message)）" 'DarkGray'
}

if (-not $exists) {
    Say "[2/3] 创建 Pages（$Branch $Path）..."
    $body = @{ source = @{ branch = $Branch; path = $Path } } | ConvertTo-Json -Depth 4
    try {
        $r = Invoke-RestMethod -Uri "$api/pages" -Headers $H -Method Post `
              -Body ([Text.Encoding]::UTF8.GetBytes($body)) -ContentType 'application/json' -TimeoutSec 60
        Say ("      已创建: " + $r.html_url) 'Green'
    } catch {
        $msg = $_.Exception.Message
        # 409 = 已存在；422 常见于"分支未推送"或"目录不存在"
        Say "      创建失败: $msg" 'Red'
        exit 1
    }
} else {
    Say "[2/3] 已开启，跳过创建。"
}

Say "[3/3] 等待构建（最多 3 分钟）..."
$url = "https://$($Repo.Split('/')[0]).github.io/$($Repo.Split('/')[1])/"
for ($i = 1; $i -le 18; $i++) {
    Start-Sleep -Seconds 10
    try {
        $s = Invoke-RestMethod -Uri "$api/pages/builds/latest" -Headers $H -Method Get -TimeoutSec 30
        Say ("      [$i] status=" + $s.status + "  " + $s.created_at)
        if ($s.status -eq 'built') { break }
        if ($s.status -eq 'errored') { Say '      构建报错，去 Actions 看日志。' 'Red'; break }
    } catch { Say "      [$i] 查询失败：$($_.Exception.Message)" 'DarkGray' }
}

Say ""
Say "官网地址: $url" 'Cyan'
try {
    $code = (Invoke-WebRequest -Uri $url -Method Head -TimeoutSec 30 -UseBasicParsing).StatusCode
    Say "访问检查: HTTP $code" 'Green'
} catch {
    Say "访问检查失败（可能还在生效，等 1-2 分钟再刷新）：$($_.Exception.Message)" 'Yellow'
}
Say 'PAGES_DONE'
