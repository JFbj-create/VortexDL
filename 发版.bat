@echo off
setlocal
cd /d "%~dp0"

echo ============================================================
echo  把 VortexDL-Setup.exe 作为 Release 附件发到 GitHub
echo  仓库: https://github.com/JFbj-create/VortexDL
echo ============================================================
echo.
echo 前置条件: 先跑过一次 推送.bat (登录过 GitHub), 本脚本自动复用那份凭据.
echo.

powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0发版.ps1" %*
set RC=%ERRORLEVEL%

echo.
if "%RC%"=="0" goto ok
echo !! 失败了 (退出码 %RC%). 看上面的红字.
echo    没登录过就先跑 推送.bat
echo    也可以手动传令牌: 发版.bat -Token ghp_xxxx
goto end

:ok
echo 完成.

:end
pause
exit /b %RC%
