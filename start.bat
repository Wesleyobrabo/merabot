@echo off
title Binance Futures Bot - MERA Scalp 5m (ETHUSDT Perpetuo USD-M)
echo Iniciando Binance Futures Bot - Motor MERA (ETHUSDT 5m)...
if exist target\release\binance-futures-bot.exe (
    target\release\binance-futures-bot.exe
) else (
    cargo run --bin binance-futures-bot
)
if %ERRORLEVEL% NEQ 0 (
    echo.
    echo O bot encerrou com erro (%ERRORLEVEL%).
    pause
)
