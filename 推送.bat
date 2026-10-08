@echo off
setlocal
cd /d "%~dp0"

echo ============================================================
echo  把 VortexDL 公开源码 + 官网 推到 GitHub
echo  仓库: https://github.com/JFbj-create/VortexDL
echo ============================================================
echo.
echo 说明: 第一次推会弹出 GitHub 登录窗口, 点 "Sign in with your browser"
echo       授权一次即可, 之后不用再登.
echo.
echo 如果那个窗口点了没反应 / 报 401, 就用"令牌"方式:
echo   1) 打开 https://github.com/settings/tokens 生成 classic token (勾 repo 权限)
echo   2) 重新运行本脚本, 弹出的账号密码框里:
echo        用户名 = JFbj-create
echo        密码   = 粘贴 ghp_ 开头的令牌 (不是账号密码)
echo.

git remote get-url origin >nul 2>&1
if errorlevel 1 git remote add origin https://github.com/JFbj-create/VortexDL.git

echo [1/3] 检查本地改动...
git status -s

echo.
echo [2/3] 提交本地改动 (如果没有改动会跳过)...
git add -A
git -c user.email="noreply@github.com" -c user.name="JFbj-create" commit -m "update: 同步最新改动" >nul 2>&1

echo.
echo [3/3] 推送到 GitHub...
git push -u origin main
if errorlevel 1 goto failed

echo.
echo ============================================================
echo  推送成功!
echo  仓库: https://github.com/JFbj-create/VortexDL
echo.
echo  接下来两步 (在网页上点两下, 一次性的):
echo    1) 开官网: Settings - Pages - Source 选 "Deploy from a branch"
echo       Branch 选 main, 文件夹选 /docs, 保存
echo       过一两分钟访问: https://jfbj-create.github.io/VortexDL/
echo    2) 传安装包: 直接双击 发版.bat (走 API 上传, 比网页拖 200MB 稳)
echo       或者手动: Releases - Draft a new release, Tag 填 v1.0.0,
echo       把 F:\vdgame\VortexDL-Setup.exe 拖进附件区, 发布
echo ============================================================
goto end

:failed
echo.
echo !! 推送失败. 常见原因:
echo    1) 没登录: 会弹出 "Connect to GitHub" 窗口, 点 "Sign in with your browser"
echo       窗口没反应就按上面说的用 ghp_ 令牌当密码
echo    2) 网络: 国内直连 github.com 不稳定, 可多试几次
echo    3) 权限: 确认这个账号对仓库有写权限
echo.

:end
pause
