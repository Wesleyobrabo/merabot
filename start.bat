@echo off
title Binance Futures Bot - Rolling 24/7 Breakout 15M
echo Iniciando Binance Futures Bot...
target\release\binance-futures-bot.exe
if %ERRORLEVEL% NEQ 0 (
    echo.
    echo O bot encerrou com erro (%ERRORLEVEL%).
    pause
)
