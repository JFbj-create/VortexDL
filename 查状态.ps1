# 查看仓库 / Pages / Release 的当前状态（只读）
[CmdletBinding()]
param([string]$Repo = 'JFbj-create/VortexDL')
$ErrorActionPreference = 'Continue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
[Net.ServicePointManager]::CheckCertificateRevocationList = $false

$fill = "protocol=https`nhost=github.com`n`n" | git credential fill 2>$null
$Token = ''
foreach ($l in $fill) { if ($l -like 'password=*') { $Token = $l.Substring(9) } }
$H = @{ Authorization = "Bearer $Token"; Accept = 'application/vnd.github+json'
        'X-GitHub-Api-Version' = '2022-11-28'; 'User-Agent' = 'VortexDL-Check' }
$api = "https://api.github.com/repos/$Repo"

Write-Host "=== 仓库 ===" -ForegroundColor Cyan
try {
    $r = Invoke-RestMethod -Uri $api -Headers $H -TimeoutSec 40
    Write-Host ("  " + $r.full_name + "  默认分支=" + $r.default_branch + "  大小=" + $r.size + "KB  私有=" + $r.private)
} catch { Write-Host "  失败: $($_.Exception.Message)" }

Write-Host "=== 最新提交 ===" -ForegroundColor Cyan
try {
    $c = Invoke-RestMethod -Uri "$api/commits?per_page=3" -Headers $H -TimeoutSec 40
    foreach ($x in $c) { Write-Host ("  " + $x.sha.Substring(0,7) + "  " + ($x.commit.message -split "`n")[0]) }
} catch { Write-Host "  失败: $($_.Exception.Message)" }

Write-Host "=== docs/ 是否在远端 ===" -ForegroundColor Cyan
try {
    $t = Invoke-RestMethod -Uri "$api/contents/docs" -Headers $H -TimeoutSec 40
    foreach ($f in $t) { Write-Host ("  " + $f.name + "  " + $f.size + " bytes") }
} catch { Write-Host "  失败: $($_.Exception.Message)" }

Write-Host "=== Pages ===" -ForegroundColor Cyan
try {
    $p = Invoke-RestMethod -Uri "$api/pages" -Headers $H -TimeoutSec 40
    Write-Host ("  url=" + $p.html_url + "  status=" + $p.status + "  " + $p.source.branch + $p.source.path)
} catch { Write-Host "  失败: $($_.Exception.Message)" }

Write-Host "=== Release ===" -ForegroundColor Cyan
try {
    $rel = Invoke-RestMethod -Uri "$api/releases" -Headers $H -TimeoutSec 40
    if (-not $rel) { Write-Host "  （还没有 release）" }
    foreach ($x in $rel) {
        Write-Host ("  " + $x.tag_name + "  " + $x.name)
        foreach ($a in $x.assets) {
            Write-Host ("     asset: " + $a.name + "  " + [math]::Round($a.size/1MB,1) + " MB  state=" + $a.state +
                        "  下载数=" + $a.download_count)
            Write-Host ("     " + $a.browser_download_url)
        }
    }
} catch { Write-Host "  失败: $($_.Exception.Message)" }
