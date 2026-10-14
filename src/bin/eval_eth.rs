use anyhow::Result;
use binance_futures_bot::config::AppConfig;
use binance_futures_bot::strategy::PineSupertrendRsiStrategy;
use binance_futures_bot::types::{Candle, Position, PositionSide, SignalType};
use chrono::{DateTime, Utc};
use serde_json::Value;

#[tokio::main]
async fn main() -> Result<()> {
    let client = reqwest::Client::new();
    let symbol = std::env::args().nth(1).unwrap_or_else(|| "SOLUSDT".to_string());
    let interval = "15m";

    // 1500 candles max per request, vamos pegar 500 candles (cobre ~5 dias)
    let url = format!(
        "https://fapi.binance.com/fapi/v1/klines?symbol={}&interval={}&limit=500",
        symbol, interval
    );

    println!("Baixando candles de 15m para {} da Binance Futures...", symbol);
    let resp: Vec<Vec<Value>> = client.get(&url).send().await?.json().await?;
    
    let mut candles: Vec<Candle> = Vec::new();
    for row in resp {
        let open_time = row[0].as_i64().unwrap_or(0);
        let open: f64 = row[1].as_str().unwrap().parse().unwrap();
        let high: f64 = row[2].as_str().unwrap().parse().unwrap();
        let low: f64 = row[3].as_str().unwrap().parse().unwrap();
        let close: f64 = row[4].as_str().unwrap().parse().unwrap();
        let volume: f64 = row[5].as_str().unwrap().parse().unwrap();
        let close_time = row[6].as_i64().unwrap_or(0);

        candles.push(Candle {
            open_time,
            close_time,
            open,
            high,
            low,
            close,
            volume,
            is_closed: true,
        });
    }

    println!("Total de candles carregados: {}", candles.len());
    if candles.len() < 120 {
        println!("Candles insuficientes.");
        return Ok(());
    }

    // Carrega config base e ajusta para ETH
    let mut config = AppConfig::load_or_create("config.toml")?;
    config.trading.symbol = symbol.to_string();
    config.trading.interval = interval.to_string();

    let mut strategy = PineSupertrendRsiStrategy::new(config.strategy.clone(), config.risk.clone());

    // As últimas 24 horas correspondem aos últimos 96 candles de 15m
    let total = candles.len();
    let split_idx = total - 96;
    let warmup_candles = &candles[..split_idx];
    let test_candles = &candles[split_idx..];

    // Aquece até a fronteira das últimas 24 horas
    strategy.warmup(warmup_candles);

    let start_time = DateTime::from_timestamp_millis(test_candles[0].open_time).unwrap_or_default();
    let end_time = DateTime::from_timestamp_millis(test_candles.last().unwrap().close_time).unwrap_or_default();

    let min_price = test_candles.iter().map(|c| c.low).fold(f64::MAX, f64::min);
    let max_price = test_candles.iter().map(|c| c.high).fold(f64::MIN, f64::max);
    let first_close = test_candles[0].open;
    let last_close = test_candles.last().unwrap().close;
    let range_pct = ((max_price - min_price) / min_price) * 100.0;
    let change_24h = ((last_close - first_close) / first_close) * 100.0;

    println!("========================================================");
    println!("AVALIAÇÃO ESTRATÉGIA 24/7 ROLLING BREAKOUT - ETHUSDT 15M");
    println!("Janela avaliada (últimas 24 horas):");
    println!("Início: {} UTC", start_time.format("%Y-%m-%d %H:%M:%S"));
    println!("Fim:    {} UTC", end_time.format("%Y-%m-%d %H:%M:%S"));
    println!("Preço Inicial: {:.2} | Preço Final: {:.2} (Variação: {:+.2}%)", first_close, last_close, change_24h);
    println!("Mínima 24h: {:.2} | Máxima 24h: {:.2} | Range Total: {:.2}%", min_price, max_price, range_pct);
    println!("Configuração: TP: +{:.1}% | SL: -{:.1}% | Max Trades 24h: {}", config.strategy.tp_percent, config.strategy.sl_percent, config.strategy.max_trades_24h);
    println!("========================================================");

    let mut current_position = Position::new(symbol.to_string(), config.trading.leverage);
    let mut trades = Vec::new();

    struct CompletedTrade {
        side: PositionSide,
        entry_price: f64,
        exit_price: f64,
        pnl_pct: f64,
        is_win: bool,
        entry_time: i64,
        exit_time: i64,
        exit_reason: String,
    }

    for (i, candle) in test_candles.iter().enumerate() {
        let candle_time = DateTime::from_timestamp_millis(candle.close_time).unwrap_or_default();

        // 1. Checa saída intrabar se houver posição aberta
        if current_position.side != PositionSide::None {
            let mut exited = false;
            let mut exit_price = 0.0;
            let mut reason = String::new();

            if current_position.side == PositionSide::Long {
                let sl = current_position.stop_loss.unwrap_or(0.0);
                let tp = current_position.take_profit.unwrap_or(f64::MAX);

                if candle.low <= sl {
                    exited = true;
                    exit_price = sl;
                    reason = "Stop Loss (-1.2%)".to_string();
                } else if candle.high >= tp {
                    exited = true;
                    exit_price = tp;
                    reason = "Take Profit (+2.5%)".to_string();
                }
            } else if current_position.side == PositionSide::Short {
                let sl = current_position.stop_loss.unwrap_or(f64::MAX);
                let tp = current_position.take_profit.unwrap_or(0.0);

                if candle.high >= sl {
                    exited = true;
                    exit_price = sl;
                    reason = "Stop Loss (-1.2%)".to_string();
                } else if candle.low <= tp {
                    exited = true;
                    exit_price = tp;
                    reason = "Take Profit (+2.5%)".to_string();
                }
            }

            if exited {
                let pnl_pct = match current_position.side {
                    PositionSide::Long => (exit_price - current_position.entry_price) / current_position.entry_price * 100.0,
                    PositionSide::Short => (current_position.entry_price - exit_price) / current_position.entry_price * 100.0,
                    _ => 0.0,
                };
                let is_win = pnl_pct > 0.0;

                println!(
                    "[{}] FECHAMENTO {:?} @ {:.2} | PnL: {:.2}% | Motivo: {}",
                    candle_time.format("%H:%M"),
                    current_position.side,
                    exit_price,
                    pnl_pct,
                    reason
                );

                trades.push(CompletedTrade {
                    side: current_position.side,
                    entry_price: current_position.entry_price,
                    exit_price,
                    pnl_pct,
                    is_win,
                    entry_time: candle.open_time,
                    exit_time: candle.close_time,
                    exit_reason: reason,
                });

                current_position = Position::new(symbol.to_string(), config.trading.leverage);
            }
        }

        // 2. Avalia sinal de fechamento de vela
        let signal_opt = strategy.on_candle(candle, &current_position);
        if let Some(sig) = signal_opt {
            match sig.signal_type {
                SignalType::OpenLong => {
                    current_position.side = PositionSide::Long;
                    current_position.entry_price = sig.price;
                    current_position.stop_loss = sig.stop_loss;
                    current_position.take_profit = sig.take_profit;
                    println!(
                        "[{}] ABERTURA LONG #{} @ {:.2} | SL: {:.2} | TP: {:.2} | Motivo: {}",
                        candle_time.format("%H:%M"),
                        trades.len() + 1,
                        sig.price,
                        sig.stop_loss.unwrap_or(0.0),
                        sig.take_profit.unwrap_or(0.0),
                        sig.reason
                    );
                }
                SignalType::OpenShort => {
                    current_position.side = PositionSide::Short;
                    current_position.entry_price = sig.price;
                    current_position.stop_loss = sig.stop_loss;
                    current_position.take_profit = sig.take_profit;
                    println!(
                        "[{}] ABERTURA SHORT #{} @ {:.2} | SL: {:.2} | TP: {:.2} | Motivo: {}",
                        candle_time.format("%H:%M"),
                        trades.len() + 1,
                        sig.price,
                        sig.stop_loss.unwrap_or(0.0),
                        sig.take_profit.unwrap_or(0.0),
                        sig.reason
                    );
                }
                _ => {}
            }
        }
    }

    // Se houver posição aberta no final
    if current_position.side != PositionSide::None {
        let last_c = test_candles.last().unwrap();
        let pnl_pct = match current_position.side {
            PositionSide::Long => (last_c.close - current_position.entry_price) / current_position.entry_price * 100.0,
            PositionSide::Short => (current_position.entry_price - last_c.close) / current_position.entry_price * 100.0,
            _ => 0.0,
        };
        println!(
            "\n[POSIÇÃO AINDA EM ABERTO NO MOMENTO] {:?} aberta em {:.2} | Preço atual: {:.2} | PnL atual: {:.2}%",
            current_position.side, current_position.entry_price, last_c.close, pnl_pct
        );
    }

    println!("\n========================================================");
    println!("RESUMO DAS ÚLTIMAS 24 HORAS (ETHUSDT 15M)");
    println!("========================================================");
    let total_trades = trades.len();
    let wins = trades.iter().filter(|t| t.is_win).count();
    let losses = trades.iter().filter(|t| !t.is_win).count();
    let win_rate = if total_trades > 0 {
        (wins as f64 / total_trades as f64) * 100.0
    } else {
        0.0
    };

    let total_pnl: f64 = trades.iter().map(|t| t.pnl_pct).sum();

    println!("Total de Trades Finalizados: {}", total_trades);
    println!("Vitórias (Wins):            {}", wins);
    println!("Derrotas (Losses):          {}", losses);
    println!("Taxa de Acerto (Win Rate):   {:.1}%", win_rate);
    println!("PnL Acumulado (sem alavancagem): {:.2}%", total_pnl);
    println!("PnL Acumulado (com 5x):          {:.2}%", total_pnl * 5.0);
    println!("========================================================");

    // Avaliação expandida dos últimos 4 dias (~400 candles) para apurar os trades mais recentes
    println!("\n========================================================");
    println!("AVALIAÇÃO RECENTE EXPANDIDA (ÚLTIMOS 4 DIAS DE ETHUSDT)");
    println!("========================================================");
    let mut strat_multi = PineSupertrendRsiStrategy::new(config.strategy.clone(), config.risk.clone());
    let warmup_len = 100;
    strat_multi.warmup(&candles[..warmup_len]);

    let mut multi_pos = Position::new(symbol.to_string(), config.trading.leverage);
    let mut multi_trades = Vec::new();

    for candle in &candles[warmup_len..] {
        let candle_time = DateTime::from_timestamp_millis(candle.close_time).unwrap_or_default();
        if multi_pos.side != PositionSide::None {
            let mut exited = false;
            let mut exit_price = 0.0;
            let mut reason = String::new();

            if multi_pos.side == PositionSide::Long {
                let sl = multi_pos.stop_loss.unwrap_or(0.0);
                let tp = multi_pos.take_profit.unwrap_or(f64::MAX);
                if candle.low <= sl {
                    exited = true; exit_price = sl; reason = "Stop Loss (-1.2%)".to_string();
                } else if candle.high >= tp {
                    exited = true; exit_price = tp; reason = "Take Profit (+2.5%)".to_string();
                }
            } else if multi_pos.side == PositionSide::Short {
                let sl = multi_pos.stop_loss.unwrap_or(f64::MAX);
                let tp = multi_pos.take_profit.unwrap_or(0.0);
                if candle.high >= sl {
                    exited = true; exit_price = sl; reason = "Stop Loss (-1.2%)".to_string();
                } else if candle.low <= tp {
                    exited = true; exit_price = tp; reason = "Take Profit (+2.5%)".to_string();
                }
            }

            if exited {
                let pnl_pct = match multi_pos.side {
                    PositionSide::Long => (exit_price - multi_pos.entry_price) / multi_pos.entry_price * 100.0,
                    PositionSide::Short => (multi_pos.entry_price - exit_price) / multi_pos.entry_price * 100.0,
                    _ => 0.0,
                };
                let is_win = pnl_pct > 0.0;
                println!(
                    "[{}] Fechamento {:?} @ {:.2} | PnL: {:+.2}% | {}",
                    candle_time.format("%d/%m %H:%M"), multi_pos.side, exit_price, pnl_pct, reason
                );
                multi_trades.push(is_win);
                multi_pos = Position::new(symbol.to_string(), config.trading.leverage);
            }
        }

        let sig_opt = strat_multi.on_candle(candle, &multi_pos);
        if let Some(sig) = sig_opt {
            match sig.signal_type {
                SignalType::OpenLong => {
                    multi_pos.side = PositionSide::Long;
                    multi_pos.entry_price = sig.price;
                    multi_pos.stop_loss = sig.stop_loss;
                    multi_pos.take_profit = sig.take_profit;
                    println!("[{}] Entrada LONG @ {:.2}", candle_time.format("%d/%m %H:%M"), sig.price);
                }
                SignalType::OpenShort => {
                    multi_pos.side = PositionSide::Short;
                    multi_pos.entry_price = sig.price;
                    multi_pos.stop_loss = sig.stop_loss;
                    multi_pos.take_profit = sig.take_profit;
                    println!("[{}] Entrada SHORT @ {:.2}", candle_time.format("%d/%m %H:%M"), sig.price);
                }
                _ => {}
            }
        }
    }

    let m_total = multi_trades.len();
    let m_wins = multi_trades.iter().filter(|&&w| w).count();
    let m_wr = if m_total > 0 { (m_wins as f64 / m_total as f64) * 100.0 } else { 0.0 };
    println!("Total de Trades: {} | Wins: {} | Losses: {} | Win Rate: {:.1}%", m_total, m_wins, m_total - m_wins, m_wr);
    println!("========================================================");

    Ok(())
}
