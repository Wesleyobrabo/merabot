use anyhow::Result;
use binance_futures_bot::indicators::IndicatorEngine;
use binance_futures_bot::types::{Candle, PositionSide};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterType {
    Ema200Pura,
    Ema70Pura,
    Ema70PlusVolume,
    Ema200PlusVolume,
}

struct BacktestResult {
    name: String,
    total_trades: usize,
    wins: usize,
    losses: usize,
    win_rate: f64,
    net_pnl: f64,
    profit_factor: f64,
}

async fn fetch_candles(client: &reqwest::Client, symbol: &str, limit: usize) -> Result<Vec<Candle>> {
    let url = format!(
        "https://fapi.binance.com/fapi/v1/klines?symbol={}&interval=15m&limit={}",
        symbol, limit
    );
    let resp: Vec<Vec<Value>> = client.get(&url).send().await?.json().await?;

    let mut candles: Vec<Candle> = Vec::with_capacity(resp.len());
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
    Ok(candles)
}

fn run_backtest(candles: &[Candle], filter: FilterType, tp_pct: f64, sl_pct: f64) -> BacktestResult {
    let mut engine = IndicatorEngine::new(10, 2.2, 9, 21, 14, 20);
    let ema_period = match filter {
        FilterType::Ema70Pura | FilterType::Ema70PlusVolume => 70,
        _ => 200,
    };
    let mut dedicated_ema = binance_futures_bot::indicators::Ema::new(ema_period);

    // Aquecimento inicial com os primeiros 200 candles
    let warmup_len = 200.min(candles.len() / 3);
    for c in &candles[..warmup_len] {
        engine.update(c);
        dedicated_ema.update(c.close);
    }

    let mut in_pos = false;
    let mut pos_side = PositionSide::None;
    let mut entry_price = 0.0;
    let mut sl_price = 0.0;
    let mut tp_price = 0.0;

    let mut trade_pnls = Vec::new();
    let mut trade_timestamps: Vec<i64> = Vec::new();

    for candle in &candles[warmup_len..] {
        // Remove trades mais velhos que 24h
        trade_timestamps.retain(|&ts| candle.close_time - ts < 86_400_000);
        let limit_reached = trade_timestamps.len() >= 3;

        // 1. Checagem intrabar de saída
        if in_pos {
            let mut exited = false;
            let mut exit_p = 0.0;

            if pos_side == PositionSide::Long {
                if candle.low <= sl_price {
                    exited = true; exit_p = sl_price;
                } else if candle.high >= tp_price {
                    exited = true; exit_p = tp_price;
                }
            } else if pos_side == PositionSide::Short {
                if candle.high >= sl_price {
                    exited = true; exit_p = sl_price;
                } else if candle.low <= tp_price {
                    exited = true; exit_p = tp_price;
                }
            }

            if exited {
                let pnl = match pos_side {
                    PositionSide::Long => (exit_p - entry_price) / entry_price * 100.0,
                    PositionSide::Short => (entry_price - exit_p) / entry_price * 100.0,
                    _ => 0.0,
                };
                trade_pnls.push(pnl);
                in_pos = false;
                pos_side = PositionSide::None;
            }
        }

        // 2. Atualiza indicadores da vela
        let ind = engine.update(candle);
        let current_ema = dedicated_ema.update(candle.close).unwrap_or(candle.close);

        // Calcula VWAP dos últimos 96 candles
        let n_c = engine.recent_candles.len();
        let rolling_vwap = if n_c >= 10 {
            let start = if n_c > 96 { n_c - 96 } else { 0 };
            let mut sum_pv = 0.0;
            let mut sum_v = 0.0;
            for c in &engine.recent_candles[start..] {
                let typical_price = (c.high + c.low + c.close) / 3.0;
                sum_pv += typical_price * c.volume;
                sum_v += c.volume;
            }
            if sum_v > 0.0 { sum_pv / sum_v } else { candle.close }
        } else {
            candle.close
        };

        // 3. Avaliação de nova entrada no fechamento: Range Móvel de 12 Horas (48 candles de 15m)
        let (r_high, r_low) = if n_c >= 49 {
            let start = n_c - 49;
            let end = n_c - 1;
            let mut h = f64::MIN;
            let mut l = f64::MAX;
            for c in &engine.recent_candles[start..end] {
                if c.high > h { h = c.high; }
                if c.low < l { l = c.low; }
            }
            (h, l)
        } else {
            (0.0, 0.0)
        };

        if !in_pos && !limit_reached {
            let range_ready = r_high > 0.0 && r_low > 0.0 && r_high > r_low;

            if range_ready {
                let long_breakout = candle.close > r_high;
                let short_breakout = candle.close < r_low;

                let is_vol_confirmed = candle.volume >= ind.volume_sma;
                let is_range_wide = ind.rolling_range_pct >= 3.0;

                let (allow_long, allow_short) = match filter {
                    FilterType::Ema200Pura => (
                        candle.close >= current_ema,
                        candle.close < current_ema,
                    ),
                    FilterType::Ema70Pura => (
                        candle.close >= current_ema,
                        candle.close < current_ema,
                    ),
                    FilterType::Ema70PlusVolume => (
                        candle.close >= current_ema && is_vol_confirmed,
                        candle.close < current_ema && is_vol_confirmed,
                    ),
                    FilterType::Ema200PlusVolume => (
                        candle.close >= current_ema && is_vol_confirmed,
                        candle.close < current_ema && is_vol_confirmed,
                    ),
                };

                if long_breakout && !short_breakout && allow_long {
                    in_pos = true;
                    pos_side = PositionSide::Long;
                    entry_price = candle.close * 1.0005; // 0.05% slippage
                    sl_price = entry_price * (1.0 - (sl_pct / 100.0));
                    tp_price = entry_price * (1.0 + (tp_pct / 100.0));
                    trade_timestamps.push(candle.close_time);
                } else if short_breakout && !long_breakout && allow_short {
                    in_pos = true;
                    pos_side = PositionSide::Short;
                    entry_price = candle.close * 0.9995; // 0.05% slippage
                    sl_price = entry_price * (1.0 + (sl_pct / 100.0));
                    tp_price = entry_price * (1.0 - (tp_pct / 100.0));
                    trade_timestamps.push(candle.close_time);
                }
            }
        }
    }

    let total_trades = trade_pnls.len();
    let wins = trade_pnls.iter().filter(|&&p| p > 0.0).count();
    let losses = trade_pnls.iter().filter(|&&p| p <= 0.0).count();
    let win_rate = if total_trades > 0 { (wins as f64 / total_trades as f64) * 100.0 } else { 0.0 };
    let net_pnl: f64 = trade_pnls.iter().sum();

    let gross_profit: f64 = trade_pnls.iter().filter(|&&p| p > 0.0).sum();
    let gross_loss: f64 = trade_pnls.iter().filter(|&&p| p < 0.0).map(|p| p.abs()).sum();
    let profit_factor = if gross_loss > 0.0 { gross_profit / gross_loss } else { gross_profit };

    let name = match filter {
        FilterType::Ema200Pura => "Baseline: EMA 200 Pura".to_string(),
        FilterType::Ema70Pura => "Teste 1: EMA 70 Pura".to_string(),
        FilterType::Ema70PlusVolume => "Teste 2: EMA 70 + Volume SMA 20".to_string(),
        FilterType::Ema200PlusVolume => "Referência: EMA 200 + Volume SMA 20".to_string(),
    };

    BacktestResult {
        name,
        total_trades,
        wins,
        losses,
        win_rate,
        net_pnl,
        profit_factor,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let client = reqwest::Client::new();
    let symbols = vec!["SOLUSDT", "ETHUSDT"];
    let tp_pct = 2.5;
    let sl_pct = 1.2;

    println!("==========================================================================================");
    println!("BACKTEST COMPARATIVO: RANGE MÓVEL DE 12 HORAS (48 CANDLES DE 15M)");
    println!("Base de dados: Binance Futures (1.500 candles de 15m = ~15.6 dias contínuos de mercado)");
    println!("Configuração: Take Profit: +{:.1}% | Stop Loss: -{:.1}% | Max Trades 24h: 3", tp_pct, sl_pct);
    println!("==========================================================================================\n");

    for symbol in symbols {
        println!(">>> Avaliando {} no gráfico de 15 minutos...", symbol);
        let candles = fetch_candles(&client, symbol, 1500).await?;
        println!("Candles carregados: {}\n", candles.len());

        let filters = vec![
            FilterType::Ema200Pura,
            FilterType::Ema70Pura,
            FilterType::Ema70PlusVolume,
            FilterType::Ema200PlusVolume,
        ];

        println!("{:<32} | {:<7} | {:<5} | {:<6} | {:<9} | {:<12} | {:<12} | {:<8}",
            "Configuração do Filtro", "Trades", "Wins", "Losses", "Win Rate", "Net PnL (1x)", "Net PnL (5x)", "Prof.Fact"
        );
        println!("{:-<105}", "");

        for f in filters {
            let res = run_backtest(&candles, f, tp_pct, sl_pct);
            println!("{:<32} | {:<7} | {:<5} | {:<6} | {:>6.1}%  | {:>+10.2}%  | {:>+10.2}%  | {:>8.2}",
                res.name,
                res.total_trades,
                res.wins,
                res.losses,
                res.win_rate,
                res.net_pnl,
                res.net_pnl * 5.0,
                res.profit_factor
            );
        }
        println!("\n");
    }

    Ok(())
}
