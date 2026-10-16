#![allow(dead_code, unused_variables, unused_imports, unused_assignments)]
// src/bin/optimize.rs - Testes Avançados para Solana 15M: Sessão Multi-Trade vs 24/7 Rolling Breakout
use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Timelike};
use serde_json::Value;

#[derive(Debug, Clone)]
struct Candle {
    open_time: i64,
    close_time: i64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PositionSide {
    Long,
    Short,
}

#[derive(Debug, Clone)]
struct ParamSet {
    name: String,
    tz_offset_hours: i32,
    range_start_hour: u32,
    range_end_hour: u32,
    close_hour: u32,
    sl_pct: f64,
    sl_mode_opposite: bool,
    tp_mode: TpMode,
    tp_pct: f64,
    use_breakeven: bool,
    be_trigger_pct: f64,
    breakout_touch: bool,
    trend_filter_ema200: bool,
    max_trades_per_day: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum TpMode {
    None,       // Time exit only
    FixedPct,   // eg +2.5%
    RiskReward, // eg 1.5R, 2.0R
}

#[derive(Debug, Clone)]
struct SimResult {
    name: String,
    net_pnl_pct: f64,
    final_balance: f64,
    trades_count: usize,
    wins: usize,
    losses: usize,
    win_rate: f64,
    profit_factor: f64,
    max_drawdown_pct: f64,
    total_fees_paid: f64,
    avg_trade_pnl_pct: f64,
}

const TAKER_FEE_PCT: f64 = 0.0005; // 0.05% taker

async fn fetch_historical_candles(
    client: &reqwest::Client,
    symbol: &str,
    interval: &str,
    target_candles: usize,
) -> Result<Vec<Candle>> {
    let mut all_candles = Vec::new();
    let mut end_time: Option<i64> = None;

    while all_candles.len() < target_candles {
        let mut url = format!(
            "https://fapi.binance.com/fapi/v1/klines?symbol={}&interval={}&limit=1000",
            symbol, interval
        );
        if let Some(et) = end_time {
            url.push_str(&format!("&endTime={}", et));
        }

        let resp = client.get(&url).send().await?.json::<Value>().await?;
        let arr = match resp.as_array() {
            Some(a) if !a.is_empty() => a,
            _ => break,
        };

        let mut batch = Vec::new();
        for row in arr {
            let r = row.as_array().context("Array esperado")?;
            batch.push(Candle {
                open_time: r[0].as_i64().unwrap(),
                close_time: r[6].as_i64().unwrap(),
                open: r[1].as_str().unwrap().parse().unwrap(),
                high: r[2].as_str().unwrap().parse().unwrap(),
                low: r[3].as_str().unwrap().parse().unwrap(),
                close: r[4].as_str().unwrap().parse().unwrap(),
                volume: r[5].as_str().unwrap().parse().unwrap(),
            });
        }

        let first_open = batch.first().unwrap().open_time;
        end_time = Some(first_open - 1);

        batch.extend(all_candles);
        all_candles = batch;

        if all_candles.len() >= target_candles || arr.len() < 1000 {
            break;
        }
    }

    all_candles.sort_by_key(|c| c.open_time);
    all_candles.dedup_by_key(|c| c.open_time);

    Ok(all_candles)
}

fn calculate_ema(candles: &[Candle], period: usize) -> Vec<Option<f64>> {
    let mut ema_vec = vec![None; candles.len()];
    if candles.len() < period {
        return ema_vec;
    }

    let alpha = 2.0 / (period as f64 + 1.0);
    let mut sum = 0.0;
    for i in 0..period {
        sum += candles[i].close;
    }
    let mut prev_ema = sum / period as f64;
    ema_vec[period - 1] = Some(prev_ema);

    for i in period..candles.len() {
        let current_ema = alpha * candles[i].close + (1.0 - alpha) * prev_ema;
        ema_vec[i] = Some(current_ema);
        prev_ema = current_ema;
    }

    ema_vec
}

fn simulate_session(candles: &[Candle], ema200: &[Option<f64>], p: &ParamSet) -> SimResult {
    let initial_balance = 100.0;
    let leverage = 5.0;
    let risk_pct = 0.03;

    let mut balance = initial_balance;
    let mut peak_balance = balance;
    let mut max_drawdown_pct = 0.0;
    let mut total_fees = 0.0;

    let mut in_position = false;
    let mut pos_side = PositionSide::Long;
    let mut pos_entry_price = 0.0;
    let mut pos_sl = 0.0;
    let mut pos_tp: Option<f64> = None;
    let mut pos_be_active = false;
    let mut pos_qty = 0.0;
    let mut pos_notional = 0.0;

    let mut current_day_key = -1;
    let mut range_high: Option<f64> = None;
    let mut range_low: Option<f64> = None;
    let mut trades_today = 0;

    let mut wins = 0;
    let mut losses = 0;
    let mut gross_profit = 0.0;
    let mut gross_loss = 0.0;
    let mut trade_pnl_pcts = Vec::new();

    for (idx, c) in candles.iter().enumerate() {
        let total_seconds = (c.open_time / 1000) + (p.tz_offset_hours as i64 * 3600);
        let dt = DateTime::from_timestamp(total_seconds, 0)
            .map(|d| d.naive_utc())
            .unwrap_or_default();

        let bar_hour = dt.hour();
        let day_key = dt.year() * 10000 + dt.month() as i32 * 100 + dt.day() as i32;

        if day_key != current_day_key {
            current_day_key = day_key;
            range_high = None;
            range_low = None;
            trades_today = 0;
        }

        let in_range = if p.range_start_hour <= p.range_end_hour {
            bar_hour >= p.range_start_hour && bar_hour < p.range_end_hour
        } else {
            bar_hour >= p.range_start_hour || bar_hour < p.range_end_hour
        };

        if in_range {
            range_high = Some(range_high.map_or(c.high, |h| h.max(c.high)));
            range_low = Some(range_low.map_or(c.low, |l| l.min(c.low)));
        }

        let range_ready = range_high.is_some() && range_low.is_some() && range_high.unwrap() > range_low.unwrap();
        let in_trading_window = bar_hour >= p.range_end_hour && bar_hour < p.close_hour;
        let past_close_hour = bar_hour >= p.close_hour;

        if in_position {
            let mut closed = false;
            let mut exit_price = 0.0;

            if p.use_breakeven && !pos_be_active {
                match pos_side {
                    PositionSide::Long => {
                        if (c.high - pos_entry_price) / pos_entry_price >= p.be_trigger_pct {
                            pos_sl = pos_entry_price * 1.0005;
                            pos_be_active = true;
                        }
                    }
                    PositionSide::Short => {
                        if (pos_entry_price - c.low) / pos_entry_price >= p.be_trigger_pct {
                            pos_sl = pos_entry_price * 0.9995;
                            pos_be_active = true;
                        }
                    }
                }
            }

            match pos_side {
                PositionSide::Long => {
                    if c.low <= pos_sl {
                        exit_price = pos_sl;
                        closed = true;
                    } else if let Some(tp) = pos_tp {
                        if c.high >= tp {
                            exit_price = tp;
                            closed = true;
                        }
                    }
                }
                PositionSide::Short => {
                    if c.high >= pos_sl {
                        exit_price = pos_sl;
                        closed = true;
                    } else if let Some(tp) = pos_tp {
                        if c.low <= tp {
                            exit_price = tp;
                            closed = true;
                        }
                    }
                }
            }

            if !closed && past_close_hour {
                exit_price = c.close;
                closed = true;
            }

            if closed {
                let pnl_raw = match pos_side {
                    PositionSide::Long => (exit_price - pos_entry_price) / pos_entry_price,
                    PositionSide::Short => (pos_entry_price - exit_price) / pos_entry_price,
                };
                let gross_pnl = pnl_raw * pos_notional;
                let fees = (pos_notional + (exit_price * pos_qty)) * TAKER_FEE_PCT;
                total_fees += fees;
                let net_pnl = gross_pnl - fees;

                balance += net_pnl;
                if balance > peak_balance {
                    peak_balance = balance;
                }
                let dd = (peak_balance - balance) / peak_balance * 100.0;
                if dd > max_drawdown_pct {
                    max_drawdown_pct = dd;
                }

                if net_pnl > 0.0 {
                    wins += 1;
                    gross_profit += net_pnl;
                } else {
                    losses += 1;
                    gross_loss += net_pnl.abs();
                }
                trade_pnl_pcts.push(pnl_raw * 100.0);

                in_position = false;
            }
        }

        // Nova Entrada
        if !in_position && in_trading_window && range_ready && trades_today < p.max_trades_per_day {
            let r_high = range_high.unwrap();
            let r_low = range_low.unwrap();

            let long_breakout = if p.breakout_touch {
                c.high >= r_high
            } else {
                c.close > r_high
            };

            let short_breakout = if p.breakout_touch {
                c.low <= r_low
            } else {
                c.close < r_low
            };

            let (allow_long, allow_short) = if p.trend_filter_ema200 {
                if let Some(ema) = ema200[idx] {
                    (c.close > ema, c.close < ema)
                } else {
                    (true, true)
                }
            } else {
                (true, true)
            };

            if long_breakout && allow_long {
                in_position = true;
                pos_side = PositionSide::Long;
                pos_entry_price = if p.breakout_touch {
                    c.open.max(r_high)
                } else {
                    c.close
                };
                pos_be_active = false;

                pos_sl = if p.sl_mode_opposite {
                    r_low
                } else {
                    pos_entry_price * (1.0 - p.sl_pct)
                };

                let sl_dist_pct = (pos_entry_price - pos_sl) / pos_entry_price;
                if sl_dist_pct > 0.001 {
                    let risk_usdt = balance * risk_pct;
                    let notional = (risk_usdt / sl_dist_pct).min(balance * leverage);
                    pos_notional = notional;
                    pos_qty = notional / pos_entry_price;

                    pos_tp = match p.tp_mode {
                        TpMode::None => None,
                        TpMode::FixedPct => Some(pos_entry_price * (1.0 + p.tp_pct)),
                        TpMode::RiskReward => Some(pos_entry_price * (1.0 + sl_dist_pct * p.tp_pct)),
                    };

                    trades_today += 1;
                } else {
                    in_position = false;
                }
            } else if short_breakout && allow_short {
                in_position = true;
                pos_side = PositionSide::Short;
                pos_entry_price = if p.breakout_touch {
                    c.open.min(r_low)
                } else {
                    c.close
                };
                pos_be_active = false;

                pos_sl = if p.sl_mode_opposite {
                    r_high
                } else {
                    pos_entry_price * (1.0 + p.sl_pct)
                };

                let sl_dist_pct = (pos_sl - pos_entry_price) / pos_entry_price;
                if sl_dist_pct > 0.001 {
                    let risk_usdt = balance * risk_pct;
                    let notional = (risk_usdt / sl_dist_pct).min(balance * leverage);
                    pos_notional = notional;
                    pos_qty = notional / pos_entry_price;

                    pos_tp = match p.tp_mode {
                        TpMode::None => None,
                        TpMode::FixedPct => Some(pos_entry_price * (1.0 - p.tp_pct)),
                        TpMode::RiskReward => Some(pos_entry_price * (1.0 - sl_dist_pct * p.tp_pct)),
                    };

                    trades_today += 1;
                } else {
                    in_position = false;
                }
            }
        }
    }

    let total_trades = wins + losses;
    let win_rate = if total_trades > 0 {
        wins as f64 / total_trades as f64 * 100.0
    } else {
        0.0
    };
    let profit_factor = if gross_loss > 0.0 {
        gross_profit / gross_loss
    } else if gross_profit > 0.0 {
        99.99
    } else {
        0.0
    };
    let avg_trade = if !trade_pnl_pcts.is_empty() {
        trade_pnl_pcts.iter().sum::<f64>() / trade_pnl_pcts.len() as f64
    } else {
        0.0
    };

    SimResult {
        name: p.name.clone(),
        net_pnl_pct: ((balance - initial_balance) / initial_balance) * 100.0,
        final_balance: balance,
        trades_count: total_trades,
        wins,
        losses,
        win_rate,
        profit_factor,
        max_drawdown_pct,
        total_fees_paid: total_fees,
        avg_trade_pnl_pct: avg_trade,
    }
}

// ----------------------------------------------------------------------------
// SIMULADOR 24/7 ROLLING DONCHIAN BREAKOUT (SEM NENHUM FUSO HORÁRIO)
// ----------------------------------------------------------------------------
#[derive(Debug, Clone)]
struct RollingParamSet {
    name: String,
    period: usize, // Ex: 16 (4h), 20 (5h), 28 (7h), 32 (8h), 48 (12h), 96 (24h)
    sl_pct: f64,
    tp_pct: Option<f64>,
    trend_filter_ema200: bool,
    max_trades_24h: usize,
}

fn simulate_rolling_donchian(candles: &[Candle], ema200: &[Option<f64>], p: &RollingParamSet) -> SimResult {
    let initial_balance = 100.0;
    let leverage = 5.0;
    let risk_pct = 0.03;

    let mut balance = initial_balance;
    let mut peak_balance = balance;
    let mut max_drawdown_pct = 0.0;
    let mut total_fees = 0.0;

    let mut in_position = false;
    let mut pos_side = PositionSide::Long;
    let mut pos_entry_price = 0.0;
    let mut pos_sl = 0.0;
    let mut pos_tp: Option<f64> = None;
    let mut pos_qty = 0.0;
    let mut pos_notional = 0.0;

    let mut wins = 0;
    let mut losses = 0;
    let mut gross_profit = 0.0;
    let mut gross_loss = 0.0;
    let mut trade_pnl_pcts = Vec::new();
    let mut trades_timestamps: Vec<i64> = Vec::new();

    if candles.len() < p.period + 10 {
        return SimResult {
            name: p.name.clone(),
            net_pnl_pct: 0.0,
            final_balance: balance,
            trades_count: 0,
            wins: 0,
            losses: 0,
            win_rate: 0.0,
            profit_factor: 0.0,
            max_drawdown_pct: 0.0,
            total_fees_paid: 0.0,
            avg_trade_pnl_pct: 0.0,
        };
    }

    for i in p.period..candles.len() {
        let c = &candles[i];

        // Máxima e mínima dos últimos N candles anteriores
        let mut hh = f64::MIN;
        let mut ll = f64::MAX;
        for prev in &candles[(i - p.period)..i] {
            if prev.high > hh {
                hh = prev.high;
            }
            if prev.low < ll {
                ll = prev.low;
            }
        }

        // Posição aberta
        if in_position {
            let mut closed = false;
            let mut exit_price = 0.0;

            match pos_side {
                PositionSide::Long => {
                    if c.low <= pos_sl {
                        exit_price = pos_sl;
                        closed = true;
                    } else if let Some(tp) = pos_tp {
                        if c.high >= tp {
                            exit_price = tp;
                            closed = true;
                        }
                    } else if c.close < ll {
                        // Saída no rompimento contrário
                        exit_price = c.close;
                        closed = true;
                    }
                }
                PositionSide::Short => {
                    if c.high >= pos_sl {
                        exit_price = pos_sl;
                        closed = true;
                    } else if let Some(tp) = pos_tp {
                        if c.low <= tp {
                            exit_price = tp;
                            closed = true;
                        }
                    } else if c.close > hh {
                        // Saída no rompimento contrário
                        exit_price = c.close;
                        closed = true;
                    }
                }
            }

            if closed {
                let pnl_raw = match pos_side {
                    PositionSide::Long => (exit_price - pos_entry_price) / pos_entry_price,
                    PositionSide::Short => (pos_entry_price - exit_price) / pos_entry_price,
                };
                let gross_pnl = pnl_raw * pos_notional;
                let fee = (pos_notional + (exit_price * pos_qty)) * TAKER_FEE_PCT;
                total_fees += fee;
                let net_pnl = gross_pnl - fee;

                balance += net_pnl;
                if balance > peak_balance {
                    peak_balance = balance;
                }
                let dd = (peak_balance - balance) / peak_balance * 100.0;
                if dd > max_drawdown_pct {
                    max_drawdown_pct = dd;
                }

                if net_pnl > 0.0 {
                    wins += 1;
                    gross_profit += net_pnl;
                } else {
                    losses += 1;
                    gross_loss += net_pnl.abs();
                }
                trade_pnl_pcts.push(pnl_raw * 100.0);
                in_position = false;
            }
        }

        // Nova entrada 24/7
        if !in_position {
            let trades_in_24h = trades_timestamps
                .iter()
                .filter(|&&ts| c.open_time - ts < 86_400_000)
                .count();

            if p.max_trades_24h > 0 && trades_in_24h >= p.max_trades_24h {
                continue;
            }

            let (allow_long, allow_short) = if p.trend_filter_ema200 {
                if let Some(ema) = ema200[i] {
                    (c.close > ema, c.close < ema)
                } else {
                    (true, true)
                }
            } else {
                (true, true)
            };

            if c.close > hh && allow_long {
                in_position = true;
                pos_side = PositionSide::Long;
                pos_entry_price = c.close;
                pos_sl = pos_entry_price * (1.0 - p.sl_pct);
                let sl_dist = (pos_entry_price - pos_sl) / pos_entry_price;
                if sl_dist > 0.001 {
                    let notional = ((balance * risk_pct) / sl_dist).min(balance * leverage);
                    pos_notional = notional;
                    pos_qty = notional / pos_entry_price;
                    pos_tp = p.tp_pct.map(|tp| pos_entry_price * (1.0 + tp));
                    trades_timestamps.push(c.open_time);
                } else {
                    in_position = false;
                }
            } else if c.close < ll && allow_short {
                in_position = true;
                pos_side = PositionSide::Short;
                pos_entry_price = c.close;
                pos_sl = pos_entry_price * (1.0 + p.sl_pct);
                let sl_dist = (pos_sl - pos_entry_price) / pos_entry_price;
                if sl_dist > 0.001 {
                    let notional = ((balance * risk_pct) / sl_dist).min(balance * leverage);
                    pos_notional = notional;
                    pos_qty = notional / pos_entry_price;
                    pos_tp = p.tp_pct.map(|tp| pos_entry_price * (1.0 - tp));
                    trades_timestamps.push(c.open_time);
                } else {
                    in_position = false;
                }
            }
        }
    }

    let total_trades = wins + losses;
    let win_rate = if total_trades > 0 {
        wins as f64 / total_trades as f64 * 100.0
    } else {
        0.0
    };
    let profit_factor = if gross_loss > 0.0 {
        gross_profit / gross_loss
    } else if gross_profit > 0.0 {
        99.99
    } else {
        0.0
    };
    let avg_trade = if !trade_pnl_pcts.is_empty() {
        trade_pnl_pcts.iter().sum::<f64>() / trade_pnl_pcts.len() as f64
    } else {
        0.0
    };

    SimResult {
        name: p.name.clone(),
        net_pnl_pct: ((balance - initial_balance) / initial_balance) * 100.0,
        final_balance: balance,
        trades_count: total_trades,
        wins,
        losses,
        win_rate,
        profit_factor,
        max_drawdown_pct,
        total_fees_paid: total_fees,
        avg_trade_pnl_pct: avg_trade,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()?;
    let symbol = "SOLUSDT";
    let interval = "15m";

    println!("==========================================================================================");
    println!("   EXPERIMENTO SOLUSDT 15M: SESSÃO MULTI-TRADE vs BREAKOUT 24/7 SEM FUSO HORÁRIO           ");
    println!("==========================================================================================");
    print!("Carregando 5.000 velas de 15m da Binance Futures (~52 dias)... ");
    let candles = fetch_historical_candles(&client, symbol, interval, 5000).await?;
    println!("Concluído! Total: {} velas.", candles.len());

    let ema200 = calculate_ema(&candles, 200);

    // ========================================================================
    // PARTE 1: EFEITO DO LIMITE DE TRADES POR DIA NA SESSÃO ASIÁTICA
    // ========================================================================
    println!("\n==========================================================================================");
    println!("   TESTE 1: QUANTIDADE DE OPERAÇÕES POR DIA (NA SESSÃO ASIÁTICA 00h-07h UTC)              ");
    println!("   Parâmetros: Range 00h-07h UTC | Trading 07h-21h UTC | SL 1.0% | Gatilho Close         ");
    println!("==========================================================================================");

    let trade_limits = vec![1, 2, 3, 5, 999];
    let mut session_limit_results = Vec::new();

    for limit in trade_limits {
        let label = if limit == 999 { "Ilimitado".to_string() } else { format!("{} trades/dia", limit) };
        let p = ParamSet {
            name: format!("Asia_00_07_Close_{}", label),
            tz_offset_hours: 0,
            range_start_hour: 0,
            range_end_hour: 7,
            close_hour: 21,
            sl_pct: 0.010,
            sl_mode_opposite: false,
            tp_mode: TpMode::None,
            tp_pct: 0.0,
            use_breakeven: false,
            be_trigger_pct: 0.0,
            breakout_touch: false,
            trend_filter_ema200: false,
            max_trades_per_day: limit,
        };
        session_limit_results.push(simulate_session(&candles, &ema200, &p));
    }

    println!("{:<24} | {:<8} | {:<7} | {:<7} | {:<6} | {:<8} | {:<9}",
        "Limite Diário", "Lucro %", "WinRate", "Trades", "P.F.", "Max DD", "Taxas Pagas");
    println!("------------------------------------------------------------------------------------------");
    for r in &session_limit_results {
        println!("{:<24} | {:>+7.2}% | {:>6.1}% | {:>7} | {:>6.2} | {:>7.1}% | ${:>7.2}",
            r.name, r.net_pnl_pct, r.win_rate, r.trades_count, r.profit_factor, r.max_drawdown_pct, r.total_fees_paid
        );
    }

    // ========================================================================
    // PARTE 2: GRID SEARCH COMPLETO FIXADO EM ATÉ 3 TRADES POR DIA (5.000 CANDLES)
    // ========================================================================
    println!("\n==========================================================================================");
    println!("   TESTE 2: GRID SEARCH COMPLETO OTIMIZADO PARA ATÉ 3 TRADES POR DIA (5.000 CANDLES)      ");
    println!("   Buscando a melhor combinação de SL, TP, Breakeven e Filtro para 3 operações/dia...     ");
    println!("==========================================================================================");

    let sessions_3t = vec![
        ("Asia_00_07_UTC", 0, 0, 7, 21),
        ("AsiaShort_00_06_UTC", 0, 0, 6, 20),
        ("Asia_00_08_UTC", 0, 0, 8, 21),
    ];
    let sls_3t = vec![0.010, 0.012, 0.015, 0.020, 0.025];
    let tps_3t = vec![
        (TpMode::None, 0.0, "TimeExit"),
        (TpMode::FixedPct, 0.020, "TP_2.0%"),
        (TpMode::FixedPct, 0.030, "TP_3.0%"),
        (TpMode::FixedPct, 0.040, "TP_4.0%"),
        (TpMode::RiskReward, 1.5, "RR_1.5"),
        (TpMode::RiskReward, 2.0, "RR_2.0"),
    ];
    let bes_3t = vec![
        (false, 0.0, "NoBE"),
        (true, 0.015, "BE_+1.5%"),
    ];
    let trigs_3t = vec![
        (false, "Close"),
        (true, "Touch"),
    ];
    let emas_3t = vec![
        (false, "NoFilter"),
        (true, "EMA200"),
    ];

    let mut results_3t = Vec::new();

    for s in &sessions_3t {
        for sl in &sls_3t {
            for tp in &tps_3t {
                for be in &bes_3t {
                    for trig in &trigs_3t {
                        for ema in &emas_3t {
                            let name = format!("{}_{}_SL{:.1}%_{}_{}_{}_3T",
                                s.0, trig.1, sl * 100.0, tp.2, be.2, ema.1);

                            let p = ParamSet {
                                name,
                                tz_offset_hours: s.1,
                                range_start_hour: s.2,
                                range_end_hour: s.3,
                                close_hour: s.4,
                                sl_pct: *sl,
                                sl_mode_opposite: false,
                                tp_mode: tp.0,
                                tp_pct: tp.1,
                                use_breakeven: be.0,
                                be_trigger_pct: be.1,
                                breakout_touch: trig.0,
                                trend_filter_ema200: ema.0,
                                max_trades_per_day: 3,
                            };
                            results_3t.push(simulate_session(&candles, &ema200, &p));
                        }
                    }
                }
            }
        }
    }

    results_3t.sort_by(|a, b| b.net_pnl_pct.partial_cmp(&a.net_pnl_pct).unwrap());

    println!("Total de combinações avaliadas para 3 trades/dia: {}", results_3t.len());
    println!("\n--- TOP 10 MELHORES CONFIGURAÇÕES COM ATÉ 3 TRADES POR DIA ---");
    println!("{:<4} | {:<46} | {:<8} | {:<7} | {:<7} | {:<6} | {:<8} | {:<9}",
        "#", "Estratégia / Parâmetros", "Lucro %", "WinRate", "Trades", "P.F.", "Max DD", "Taxas Pagas");
    println!("-------------------------------------------------------------------------------------------------------------");
    // ========================================================================
    // PARTE 3: COMPARAÇÃO DIRETA: SESSÃO ASIÁTICA vs 24/7 ROLLING RANGE (ATÉ 3 TRADES/24H)
    // ========================================================================
    println!("\n==========================================================================================");
    println!("   TESTE 3: SESSÃO ASIÁTICA vs 24/7 ROLLING RANGE (MÁXIMO 3 TRADES POR 24 HORAS)          ");
    println!("   Parâmetros: TP +2.0% | SL 1.2% | Filtro EMA 200 | Máximo 3 trades em janela de 24h     ");
    println!("==========================================================================================");

    let mut direct_comparison = Vec::new();

    // 1. Nossa configuração atual da Sessão Asiática
    let asia_current = ParamSet {
        name: "Sessao_Asia_3T_TP2.0%_SL1.2%_EMA200".to_string(),
        tz_offset_hours: 0,
        range_start_hour: 0,
        range_end_hour: 7,
        close_hour: 21,
        sl_pct: 0.012,
        sl_mode_opposite: false,
        tp_mode: TpMode::FixedPct,
        tp_pct: 0.020,
        use_breakeven: false,
        be_trigger_pct: 0.0,
        breakout_touch: false,
        trend_filter_ema200: true,
        max_trades_per_day: 3,
    };
    direct_comparison.push(simulate_session(&candles, &ema200, &asia_current));

    // 2. Rolling Range 24/7 com diferentes janelas de formação de range
    let periods = vec![
        (16, "Rolling_16bars_4h"),
        (20, "Rolling_20bars_5h"),
        (24, "Rolling_24bars_6h"),
        (28, "Rolling_28bars_7h"),
        (32, "Rolling_32bars_8h"),
        (48, "Rolling_48bars_12h"),
        (96, "Rolling_96bars_24h"),
    ];

    for (per, label) in &periods {
        for tp in &[0.020, 0.025, 0.030] {
            let p_rolling = RollingParamSet {
                name: format!("{}_3T_TP{:.1}%_SL1.2%_EMA200", label, tp * 100.0),
                period: *per,
                sl_pct: 0.012,
                tp_pct: Some(*tp),
                trend_filter_ema200: true,
                max_trades_24h: 3,
            };
            direct_comparison.push(simulate_rolling_donchian(&candles, &ema200, &p_rolling));
        }
    }

    direct_comparison.sort_by(|a, b| b.net_pnl_pct.partial_cmp(&a.net_pnl_pct).unwrap());

    println!("{:<4} | {:<42} | {:<8} | {:<7} | {:<7} | {:<6} | {:<8} | {:<9}",
        "#", "Estratégia / Modelo", "Lucro %", "WinRate", "Trades", "P.F.", "Max DD", "Taxas Pagas");
    println!("---------------------------------------------------------------------------------------------------------");
    for (i, r) in direct_comparison.iter().enumerate() {
        println!("{:<4} | {:<42} | {:>+7.2}% | {:>6.1}% | {:>7} | {:>6.2} | {:>7.1}% | ${:>7.2}",
            i + 1, r.name, r.net_pnl_pct, r.win_rate, r.trades_count, r.profit_factor, r.max_drawdown_pct, r.total_fees_paid
        );
    }

    Ok(())
}
