# ============================================================================
#  把 VortexDL-Setup.exe 作为 Release 附件发到 GitHub
#  ---------------------------------------------------------------------------
#  为什么要有这个脚本：
#    GitHub 单个文件超过 100MB 就不能直接 commit（安装包 200MB+），
#    必须走 Release 附件。网页上手动拖 200MB 很容易失败，用 API 上传更稳。
#
#  怎么用（在 _publish2 目录里）：
#     powershell -ExecutionPolicy Bypass -File .\发版.ps1
#   或者直接双击 发版.bat
#
#  令牌从哪来：
#     先跑一次 推送.bat 用浏览器登录，凭据会存进 Windows 凭据管理器；
#     本脚本用 `git credential fill` 把它读出来复用，不用你再粘一次。
#     如果没登录过，脚本会提示你用 -Token 参数手动传。
#
#  只做两件事（都是可重复执行的：已存在的 release 会复用，同名附件会先删）：
#     1) POST /repos/{repo}/releases        建 release（tag v1.0.0）
#     2) POST uploads.github.com/.../assets 传 VortexDL-Setup.exe
# ============================================================================
[CmdletBinding()]
param(
    [string]$Repo    = 'JFbj-create/VortexDL',
    [string]$Tag     = 'v1.0.0',
    [string]$Title   = 'VortexDL v1.0 正式版',
    [string]$Installer = '',
    [string]$Token   = '',
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'

# --- TLS：这台机器上 curl/Schannel 的吊销检查会失败（CRL 站点不通），
#     表现为 SSL connect error。关掉吊销检查、强制 TLS1.2 即可。
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
[Net.ServicePointManager]::CheckCertificateRevocationList = $false

function Say([string]$m, [string]$c = 'Gray') { Write-Host $m -ForegroundColor $c }
function Fail([string]$m) { Say $m 'Red'; exit 1 }

# ---------------------------------------------------------------- 找安装包
if (-not $Installer) {
    $cands = @(
        (Join-Path (Split-Path -Parent $PSScriptRoot) 'VortexDL-Setup.exe'),
        (Join-Path $PSScriptRoot 'VortexDL-Setup.exe')
    )
    $Installer = $cands | Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $Installer -or -not (Test-Path $Installer)) {
    Fail "找不到安装包。用 -Installer 指定路径，例如：`n  -Installer F:\vdgame\VortexDL-Setup.exe"
}
$Installer = (Resolve-Path $Installer).Path
$sizeMB = [math]::Round((Get-Item $Installer).Length / 1MB, 1)
Say "安装包: $Installer  ($sizeMB MB)" 'Cyan'

# ---------------------------------------------------------------- 取令牌
if (-not $Token) {
    Say '正在从 Windows 凭据管理器读取 GitHub 凭据（就是 推送.bat 登录时存的那份）...'
    try {
        $fill = "protocol=https`nhost=github.com`n`n" | git credential fill 2>$null
        foreach ($line in $fill) {
            if ($line -like 'password=*') { $Token = $line.Substring(9) }
        }
    } catch { }
}
if (-not $Token) {
    Fail @"
没有拿到 GitHub 令牌。两个办法二选一：
  A) 先运行一次 推送.bat，在弹出窗口里点 "Sign in with your browser" 登录；再跑本脚本。
  B) 到 https://github.com/settings/tokens 生成一个 classic token（勾 repo 权限），
     然后这样跑：  .\发版.ps1 -Token ghp_你的令牌
"@
}
Say ("令牌: " + $Token.Substring(0, [Math]::Min(7, $Token.Length)) + '...（已隐藏）') 'DarkGray'

$headers = @{
    Authorization          = "Bearer $Token"
    Accept                 = 'application/vnd.github+json'
    'X-GitHub-Api-Version' = '2022-11-28'
    'User-Agent'           = 'VortexDL-Release'
}
$apiBase = "https://api.github.com/repos/$Repo"

# ---------------------------------------------------------------- 0) 连通性 + 权限
Say "`n[0/3] 检查仓库与权限 ..."
try {
    $repoInfo = Invoke-RestMethod -Uri $apiBase -Headers $headers -Method Get -TimeoutSec 40
} catch {
    Fail "读不到仓库 $Repo ：$($_.Exception.Message)"
}
Say ("      仓库: " + $repoInfo.full_name + "   默认分支: " + $repoInfo.default_branch)
if (-not $repoInfo.permissions.push) {
    Fail "这个令牌对 $Repo 没有写权限（permissions.push=false）。`n      确认 token 勾了 repo 权限，且账号对仓库有写权限。"
}
Say '      写权限: 有' 'Green'

# ---------------------------------------------------------------- 1) 建 / 找 release
Say "`n[1/3] 处理 release（tag $Tag）..."
$release = $null
try {
    $release = Invoke-RestMethod -Uri "$apiBase/releases/tags/$Tag" -Headers $headers -Method Get -TimeoutSec 40
    Say "      已存在同名 release（id=$($release.id)），直接复用" 'Yellow'
} catch {
    $body = @{
        tag_name         = $Tag
        name             = $Title
        target_commitish = $repoInfo.default_branch
        draft            = $false
        prerelease       = $false
        body             = @"
## VortexDL v1.0 正式版

一体化游戏资源下载与管理工具。安装包单文件，双击即可安装。

- 多源资源索引（本地索引 5.4 万+ 条，离线可搜）
- 自研多线程下载引擎 SwiftFetch（实测峰值 110 MB/s）
- 下载完自动解压 + 创建桌面快捷方式（名字可改）
- 动漫追番：多源聚合 + 实时超分超帧 + 观看进度记忆
- 书库：Gutenberg / 国学经典 / 古籍，在线阅读 + 下载到本地
- 77 套界面主题，低配机器自动降级

> 完整功能与使用说明见 [README](https://github.com/$Repo#readme)
"@
    }
    $json = $body | ConvertTo-Json -Depth 5
    $release = Invoke-RestMethod -Uri "$apiBase/releases" -Headers $headers -Method Post `
                                 -Body ([Text.Encoding]::UTF8.GetBytes($json)) -ContentType 'application/json' `
                                 -TimeoutSec 60
    Say "      已创建 release（id=$($release.id)）" 'Green'
}
$uploadUrl = $release.upload_url -replace '\{.*$', ''
Say "      附件上传地址: $uploadUrl"

# ---------------------------------------------------------------- 2) 传安装包
$assetName = Split-Path $Installer -Leaf
Say "`n[2/3] 上传附件 $assetName （$sizeMB MB，请耐心等）..."

# 同名附件先删掉（GitHub 不允许重名，重复执行会 422）
try {
    $existing = Invoke-RestMethod -Uri "$apiBase/releases/$($release.id)/assets" -Headers $headers -Method Get -TimeoutSec 40
    foreach ($a in $existing) {
        if ($a.name -eq $assetName) {
            Say "      发现同名旧附件（id=$($a.id)），先删除 ..." 'Yellow'
            Invoke-RestMethod -Uri "$apiBase/releases/assets/$($a.id)" -Headers $headers -Method Delete -TimeoutSec 40 | Out-Null
        }
    }
} catch { Say "      （列旧附件失败，继续：$($_.Exception.Message)）" 'DarkGray' }

if ($DryRun) {
    Say "`n[3/3] -DryRun：跳过真实上传。" 'Yellow'
    Say "DRYRUN_OK"
    exit 0
}

$sw = [Diagnostics.Stopwatch]::StartNew()
try {
    $resp = Invoke-RestMethod -Uri "$uploadUrl`?name=$assetName" -Headers $headers -Method Post `
                              -InFile $Installer -ContentType 'application/octet-stream' -TimeoutSec 3600
} catch {
    Fail "上传失败：$($_.Exception.Message)"
}
$sw.Stop()
$mbps = if ($sw.Elapsed.TotalSeconds -gt 0) { [math]::Round($sizeMB / $sw.Elapsed.TotalSeconds, 2) } else { 0 }
Say ("      上传完成：$([math]::Round($resp.size/1MB,1)) MB，用时 $([math]::Round($sw.Elapsed.TotalSeconds,1)) 秒（约 $mbps MB/s）") 'Green'

# ---------------------------------------------------------------- 完事
$dl = $resp.browser_download_url
Say "`n[3/3] 完成！" 'Green'
Say "  下载直链: $dl"
Say "  Release 页: https://github.com/$Repo/releases/tag/$Tag"
Say "  官网下载按钮指向 releases/latest，已经能用了。"
Say "RELEASE_OK"
