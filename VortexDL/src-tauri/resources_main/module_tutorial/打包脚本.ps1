# VortexDL 模块打包脚本
# 用法:
#   .\打包脚本.ps1 -ModuleDir .\example_simple
#   .\打包脚本.ps1 -ModuleDir .\example_advanced -OutFile my_advanced.zip

param(
    [Parameter(Mandatory=$true)]
    [string]$ModuleDir,
    [string]$OutFile
)

# 切换到模块源码目录
if (-not (Test-Path $ModuleDir)) {
    Write-Error "模块目录不存在: $ModuleDir"
    exit 1
}

$absDir = (Resolve-Path $ModuleDir).Path
$manifest = Join-Path $absDir "manifest.json"
if (-not (Test-Path $manifest)) {
    Write-Error "模块目录中找不到 manifest.json: $absDir"
    exit 1
}

# 读取 manifest 获取 id 作为默认输出名
$manifestContent = Get-Content $manifest -Raw | ConvertFrom-Json
if (-not $OutFile) {
    $OutFile = "$($manifestContent.id).zip"
}
$OutFile = [System.IO.Path]::GetFullPath($OutFile, (Get-Location).Path)

Write-Host "打包模块:" -ForegroundColor Cyan
Write-Host "  源码目录: $absDir"
Write-Host "  模块 ID:  $($manifestContent.id)"
Write-Host "  模块名:  $($manifestContent.name)"
Write-Host "  输出:    $OutFile"
Write-Host ""

# 收集要打包的文件（manifest.json + index.html + assets/）
$items = @("manifest.json")
if (Test-Path (Join-Path $absDir "index.html")) { $items += "index.html" }
if (Test-Path (Join-Path $absDir "assets")) { $items += "assets" }
if (Test-Path (Join-Path $absDir "entry.html")) { $items += "entry.html" }

Write-Host "打包文件: $($items -join ', ')" -ForegroundColor Gray

# 切换到模块目录后打包，确保 ZIP 根目录直接是文件
Push-Location $absDir
try {
    if (Test-Path $OutFile) { Remove-Item $OutFile -Force }
    Compress-Archive -Path $items -DestinationPath $OutFile -CompressionLevel Optimal
    $size = (Get-Item $OutFile).Length
    Write-Host ""
    Write-Host "打包成功! 文件大小: $([math]::Round($size/1KB, 2)) KB" -ForegroundColor Green
    Write-Host "将 $OutFile 拖拽到 VortexDL 工具页面即可安装" -ForegroundColor Green
} catch {
    Write-Error "打包失败: $_"
    exit 1
} finally {
    Pop-Location
}
