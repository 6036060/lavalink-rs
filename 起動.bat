@echo off
setlocal
chcp 65001 >nul
cd /d "%~dp0"

REM ================= 設定 =================
REM シークレットは環境変数で上書き可能 (未設定ならデフォルト値)
if not defined SERVER_SECRET_KEY set "SERVER_SECRET_KEY=0123456789abcdef"
set "YT_COMPANION_URL=http://127.0.0.1:8282"
set "YT_COMPANION_SECRET=%SERVER_SECRET_KEY%"
set "CMAKE_POLICY_VERSION_MINIMUM=3.5"
set "COMPANION_DIR=%~dp0potoken\invidious-companion"
set "STARTED_COMPANION=0"

REM ============ companion 起動 ============
REM 既に応答があればそのまま使う (HTTP 応答さえあれば OK)
curl -s -o nul --max-time 2 "%YT_COMPANION_URL%/"
if %errorlevel%==0 (
    echo [start] companion は既に起動しています
    goto run_server
)

where deno >nul 2>nul
if errorlevel 1 (
    echo [start] deno が見つかりません。https://deno.land からインストールしてください。
    goto end
)

echo [start] invidious-companion を起動します...
start "invidious-companion" /D "%COMPANION_DIR%" cmd /c "deno task dev"
set "STARTED_COMPANION=1"

set /a tries=0
:wait_companion
set /a tries+=1
if %tries% gtr 60 (
    echo [start] companion の起動を 60 秒待ちましたが応答がありません。中止します。
    goto cleanup
)
timeout /t 1 /nobreak >nul
curl -s -o nul --max-time 2 "%YT_COMPANION_URL%/"
if not %errorlevel%==0 goto wait_companion
echo [start] companion の起動を確認しました: %YT_COMPANION_URL%

REM ============ lavalink サーバー起動 ============
:run_server
echo [start] lavalink-rs-server を起動します...
cargo run -p lavalink-server --features dave --bin lavalink-rs
echo [start] サーバーが終了しました。

:cleanup
REM このスクリプトが起動した companion だけ後片付けする
if "%STARTED_COMPANION%"=="1" (
    echo [start] companion を停止します...
    taskkill /F /T /FI "WINDOWTITLE eq invidious-companion*" >nul 2>nul
)

:end
endlocal
pause
