// language: Rust, file: src/bin/backtest_chop_scalper.rs, target: Windows 11, MSVC
// =========================================================================================
//  BACKTEST EXCLUSIVO: MOTOR DE HARVESTING EM COMPRESSÃO LATERAL & FALSO ROMPIMENTO
// =========================================================================================

use anyhow::Result;
use binance_futures_bot::lateral_scalper::{LateralScalpConfig, LateralScalpEngine};
use binance_futures_bot::types::{Candle, PositionSide};
use serde_json::Value;

#[derive(Debug, Clone)]
struct TradeLog {
    timestamp: i64,
    side: String,
    entry_price: f64,
    exit_price: f64,
    quantity: f64,
    net_pnl_eur: f64,
    return_pct: f64,
    balance_after_eur: f64,
    reason: String,
}

#[derive(Debug, Clone)]
struct MonthBacktestResult {
    month_name: String,
    initial_eur: f64,
    final_eur: f64,
    profit_eur: f64,
    return_pct: f64,
    total_trades: usize,
    wins: usize,
    losses: usize,
    win_rate: f64,
    profit_factor: f64,
    max_drawdown_pct: f64,
    tp_partials: usize,
    tp_finals: usize,
    breakevens: usize,
    stop_losses: usize,
    time_stops: usize,
}

#[derive(Debug, Clone)]
struct ActivePosition {
    side: PositionSide,
    entry_price: f64,
    amount: f64,
    initial_amount: f64,
    stop_loss: f64,
    tp_partial: f64,
    tp_final: f64,
    partial_taken: bool,
    be_locked: bool,
    entry_index: usize,
    max_hold: usize,
    entry_reason: String,
}

fn simulate_chop_scalp(
    month_name: &str,
    candles: &[Candle],
    cfg: LateralScalpConfig,
    initial_eur: f64,
    leverage: u32,
    eur_to_usdt: f64,
    warmup_candles: usize,
) -> MonthBacktestResult {
    let mut balance_eur = initial_eur;
    let mut peak_eur = initial_eur;
    let mut max_drawdown_pct = 0.0;

    let fee_rate = 0.0005; // 0.05% taker
    let slip_rate = 0.0005; // 0.05% slippage

    let mut engine = LateralScalpEngine::new(cfg.clone());
    let mut current_pos: Option<ActivePosition> = None;
    let mut trades: Vec<TradeLog> = Vec::new();

    let mut tp_partials = 0;
    let mut tp_finals = 0;
    let mut breakevens = 0;
    let mut stop_losses = 0;
    let mut time_stops = 0;

    for (idx, candle) in candles.iter().enumerate() {
        // 1. Gerenciamento de Posição
        if let Some(mut pos) = current_pos.take() {
            let mut closed = false;
            let bars_held = idx.saturating_sub(pos.entry_index);

            match pos.side {
                PositionSide::Long => {
                    // A. Take Profit 1 Parcial (70%)
                    if !pos.partial_taken && candle.high >= pos.tp_partial {
                        let fill_price = pos.tp_partial * (1.0 - slip_rate);
                        let close_qty = ((pos.amount * cfg.partial_ratio) * 1000.0).floor() / 1000.0;
                        if close_qty >= 0.001 {
                            let gross_usdt = (fill_price - pos.entry_price) * close_qty;
                            let fee_usdt = (close_qty * fill_price) * fee_rate;
                            let net_eur = (gross_usdt - fee_usdt) / eur_to_usdt;
                            balance_eur += net_eur;

                            pos.amount = ((pos.amount - close_qty) * 1000.0).round() / 1000.0;
                            pos.partial_taken = true;
                            pos.be_locked = true;
                            // Trava Breakeven acima da entrada cobrindo taxas
                            pos.stop_loss = pos.entry_price * (1.0 + cfg.be_offset_pct / 100.0);
                            tp_partials += 1;

                            trades.push(TradeLog {
                                timestamp: candle.close_time,
                                side: "LONG (PARCIAL 70%)".to_string(),
                                entry_price: pos.entry_price,
                                exit_price: fill_price,
                                quantity: close_qty,
                                net_pnl_eur: net_eur,
                                return_pct: (gross_usdt / (close_qty * pos.entry_price)) * 100.0 * (leverage as f64),
                                balance_after_eur: balance_eur,
                                reason: format!("Scalp TP1 (+{:.2}%)", cfg.tp_partial_pct),
                            });
                        }
                    }

                    // B. Take Profit 2 Runner (30%)
                    if candle.high >= pos.tp_final && pos.amount >= 0.001 {
                        let fill_price = pos.tp_final * (1.0 - slip_rate);
                        let gross_usdt = (fill_price - pos.entry_price) * pos.amount;
                        let fee_usdt = (pos.amount * fill_price) * fee_rate;
                        let net_eur = (gross_usdt - fee_usdt) / eur_to_usdt;
                        balance_eur += net_eur;
                        tp_finals += 1;
                        closed = true;

                        trades.push(TradeLog {
                            timestamp: candle.close_time,
                            side: "LONG (TP2 FINAL)".to_string(),
                            entry_price: pos.entry_price,
                            exit_price: fill_price,
                            quantity: pos.amount,
                            net_pnl_eur: net_eur,
                            return_pct: (gross_usdt / (pos.amount * pos.entry_price)) * 100.0 * (leverage as f64),
                            balance_after_eur: balance_eur,
                            reason: format!("Scalp TP2 Runner (+{:.2}%)", cfg.tp_final_pct),
                        });
                    }

                    // C. Stop Loss / Breakeven
                    if !closed && pos.amount >= 0.001 && candle.low <= pos.stop_loss {
                        let fill_price = pos.stop_loss * (1.0 - slip_rate);
                        let gross_usdt = (fill_price - pos.entry_price) * pos.amount;
                        let fee_usdt = (pos.amount * fill_price) * fee_rate;
                        let net_eur = (gross_usdt - fee_usdt) / eur_to_usdt;
                        balance_eur += net_eur;
                        closed = true;

                        let is_be = pos.be_locked || pos.partial_taken;
                        if is_be {
                            breakevens += 1;
                        } else {
                            stop_losses += 1;
                        }

                        trades.push(TradeLog {
                            timestamp: candle.close_time,
                            side: if is_be { "LONG (BREAKEVEN)".to_string() } else { "LONG (STOP LOSS)".to_string() },
                            entry_price: pos.entry_price,
                            exit_price: fill_price,
                            quantity: pos.amount,
                            net_pnl_eur: net_eur,
                            return_pct: (gross_usdt / (pos.amount * pos.entry_price)) * 100.0 * (leverage as f64),
                            balance_after_eur: balance_eur,
                            reason: if is_be { "Breakeven Protegido".to_string() } else { format!("Stop Loss (-{:.2}%)", cfg.sl_pct) },
                        });
                    }

                    // D. Time-Stop por Estagnação
                    if !closed && pos.amount >= 0.001 && bars_held >= pos.max_hold {
                        let cur_diff_pct = ((candle.close - pos.entry_price) / pos.entry_price).abs() * 100.0;
                        if cur_diff_pct < 0.25 {
                            let fill_price = candle.close * (1.0 - slip_rate);
                            let gross_usdt = (fill_price - pos.entry_price) * pos.amount;
                            let fee_usdt = (pos.amount * fill_price) * fee_rate;
                            let net_eur = (gross_usdt - fee_usdt) / eur_to_usdt;
                            balance_eur += net_eur;
                            closed = true;
                            time_stops += 1;

                            trades.push(TradeLog {
                                timestamp: candle.close_time,
                                side: "LONG (TIME-STOP)".to_string(),
                                entry_price: pos.entry_price,
                                exit_price: fill_price,
                                quantity: pos.amount,
                                net_pnl_eur: net_eur,
                                return_pct: (gross_usdt / (pos.amount * pos.entry_price)) * 100.0 * (leverage as f64),
                                balance_after_eur: balance_eur,
                                reason: format!("Time-Stop Estagnação ({} velas)", bars_held),
                            });
                        }
                    }
                }
                PositionSide::Short => {
                    // A. Take Profit 1 Parcial (70%)
                    if !pos.partial_taken && candle.low <= pos.tp_partial {
                        let fill_price = pos.tp_partial * (1.0 + slip_rate);
                        let close_qty = ((pos.amount * cfg.partial_ratio) * 1000.0).floor() / 1000.0;
                        if close_qty >= 0.001 {
                            let gross_usdt = (pos.entry_price - fill_price) * close_qty;
                            let fee_usdt = (close_qty * fill_price) * fee_rate;
                            let net_eur = (gross_usdt - fee_usdt) / eur_to_usdt;
                            balance_eur += net_eur;

                            pos.amount = ((pos.amount - close_qty) * 1000.0).round() / 1000.0;
                            pos.partial_taken = true;
                            pos.be_locked = true;
                            // Trava Breakeven abaixo da entrada cobrindo taxas
                            pos.stop_loss = pos.entry_price * (1.0 - cfg.be_offset_pct / 100.0);
                            tp_partials += 1;

                            trades.push(TradeLog {
                                timestamp: candle.close_time,
                                side: "SHORT (PARCIAL 70%)".to_string(),
                                entry_price: pos.entry_price,
                                exit_price: fill_price,
                                quantity: close_qty,
                                net_pnl_eur: net_eur,
                                return_pct: (gross_usdt / (close_qty * pos.entry_price)) * 100.0 * (leverage as f64),
                                balance_after_eur: balance_eur,
                                reason: format!("Scalp TP1 (+{:.2}%)", cfg.tp_partial_pct),
                            });
                        }
                    }

                    // B. Take Profit 2 Runner (30%)
                    if candle.low <= pos.tp_final && pos.amount >= 0.001 {
                        let fill_price = pos.tp_final * (1.0 + slip_rate);
                        let gross_usdt = (pos.entry_price - fill_price) * pos.amount;
                        let fee_usdt = (pos.amount * fill_price) * fee_rate;
                        let net_eur = (gross_usdt - fee_usdt) / eur_to_usdt;
                        balance_eur += net_eur;
                        tp_finals += 1;
                        closed = true;

                        trades.push(TradeLog {
                            timestamp: candle.close_time,
                            side: "SHORT (TP2 FINAL)".to_string(),
                            entry_price: pos.entry_price,
                            exit_price: fill_price,
                            quantity: pos.amount,
                            net_pnl_eur: net_eur,
                            return_pct: (gross_usdt / (pos.amount * pos.entry_price)) * 100.0 * (leverage as f64),
                            balance_after_eur: balance_eur,
                            reason: format!("Scalp TP2 Runner (+{:.2}%)", cfg.tp_final_pct),
                        });
                    }

                    // C. Stop Loss / Breakeven
                    if !closed && pos.amount >= 0.001 && candle.high >= pos.stop_loss {
                        let fill_price = pos.stop_loss * (1.0 + slip_rate);
                        let gross_usdt = (pos.entry_price - fill_price) * pos.amount;
                        let fee_usdt = (pos.amount * fill_price) * fee_rate;
                        let net_eur = (gross_usdt - fee_usdt) / eur_to_usdt;
                        balance_eur += net_eur;
                        closed = true;

                        let is_be = pos.be_locked || pos.partial_taken;
                        if is_be {
                            breakevens += 1;
                        } else {
                            stop_losses += 1;
                        }

                        trades.push(TradeLog {
                            timestamp: candle.close_time,
                            side: if is_be { "SHORT (BREAKEVEN)".to_string() } else { "SHORT (STOP LOSS)".to_string() },
                            entry_price: pos.entry_price,
                            exit_price: fill_price,
                            quantity: pos.amount,
                            net_pnl_eur: net_eur,
                            return_pct: (gross_usdt / (pos.amount * pos.entry_price)) * 100.0 * (leverage as f64),
                            balance_after_eur: balance_eur,
                            reason: if is_be { "Breakeven Protegido".to_string() } else { format!("Stop Loss (-{:.2}%)", cfg.sl_pct) },
                        });
                    }

                    // D. Time-Stop por Estagnação
                    if !closed && pos.amount >= 0.001 && bars_held >= pos.max_hold {
                        let cur_diff_pct = ((candle.close - pos.entry_price) / pos.entry_price).abs() * 100.0;
                        if cur_diff_pct < 0.25 {
                            let fill_price = candle.close * (1.0 + slip_rate);
                            let gross_usdt = (pos.entry_price - fill_price) * pos.amount;
                            let fee_usdt = (pos.amount * fill_price) * fee_rate;
                            let net_eur = (gross_usdt - fee_usdt) / eur_to_usdt;
                            balance_eur += net_eur;
                            closed = true;
                            time_stops += 1;

                            trades.push(TradeLog {
                                timestamp: candle.close_time,
                                side: "SHORT (TIME-STOP)".to_string(),
                                entry_price: pos.entry_price,
                                exit_price: fill_price,
                                quantity: pos.amount,
                                net_pnl_eur: net_eur,
                                return_pct: (gross_usdt / (pos.amount * pos.entry_price)) * 100.0 * (leverage as f64),
                                balance_after_eur: balance_eur,
                                reason: format!("Time-Stop Estagnação ({} velas)", bars_held),
                            });
                        }
                    }
                }
                _ => {}
            }

            if !closed {
                current_pos = Some(pos);
            }
        }

        // 2. Processa vela no motor de cálculo
        let signal_opt = engine.process_candle(candle);

        if idx < warmup_candles {
            continue;
        }

        // 3. Abertura de Nova Posição
        if current_pos.is_none() {
            if let Some(sig) = signal_opt {
                let fill_price = match sig.side {
                    PositionSide::Long => sig.entry_price * (1.0 + slip_rate),
                    PositionSide::Short => sig.entry_price * (1.0 - slip_rate),
                    _ => sig.entry_price,
                };

                let balance_usdt = balance_eur * eur_to_usdt;
                let risk_eur = balance_eur * (cfg.risk_per_trade_pct / 100.0);
                let sl_dist_pct = cfg.sl_pct / 100.0;

                let target_notional = ((risk_eur * eur_to_usdt) / sl_dist_pct)
                    .min(balance_usdt * (leverage as f64) * 0.90)
                    .min(800.0);

                let mut qty = target_notional / fill_price;
                qty = (qty * 1000.0).floor() / 1000.0;

                if qty >= 0.001 {
                    let open_fee_usdt = (qty * fill_price) * fee_rate;
                    balance_eur -= open_fee_usdt / eur_to_usdt;

                    current_pos = Some(ActivePosition {
                        side: sig.side,
                        entry_price: fill_price,
                        amount: qty,
                        initial_amount: qty,
                        stop_loss: match sig.side {
                            PositionSide::Long => fill_price * (1.0 - cfg.sl_pct / 100.0),
                            PositionSide::Short => fill_price * (1.0 + cfg.sl_pct / 100.0),
                            _ => 0.0,
                        },
                        tp_partial: match sig.side {
                            PositionSide::Long => fill_price * (1.0 + cfg.tp_partial_pct / 100.0),
                            PositionSide::Short => fill_price * (1.0 - cfg.tp_partial_pct / 100.0),
                            _ => 0.0,
                        },
                        tp_final: match sig.side {
                            PositionSide::Long => fill_price * (1.0 + cfg.tp_final_pct / 100.0),
                            PositionSide::Short => fill_price * (1.0 - cfg.tp_final_pct / 100.0),
                            _ => 0.0,
                        },
                        partial_taken: false,
                        be_locked: false,
                        entry_index: idx,
                        max_hold: sig.max_hold_candles,
                        entry_reason: sig.reason,
                    });
                }
            }
        }

        // 4. Medição de Drawdown
        if balance_eur > peak_eur {
            peak_eur = balance_eur;
        }
        let dd = if peak_eur > 0.0 {
            ((peak_eur - balance_eur) / peak_eur) * 100.0
        } else {
            0.0
        };
        if dd > max_drawdown_pct {
            max_drawdown_pct = dd;
        }
    }

    let total_trades = trades.len();
    let wins = trades.iter().filter(|t| t.net_pnl_eur > 0.0).count();
    let losses = trades.iter().filter(|t| t.net_pnl_eur <= 0.0).count();
    let win_rate = if total_trades > 0 {
        (wins as f64 / total_trades as f64) * 100.0
    } else {
        0.0
    };

    let gross_wins: f64 = trades.iter().filter(|t| t.net_pnl_eur > 0.0).map(|t| t.net_pnl_eur).sum();
    let gross_losses: f64 = trades.iter().filter(|t| t.net_pnl_eur < 0.0).map(|t| t.net_pnl_eur.abs()).sum();
    let profit_factor = if gross_losses > 0.0 {
        gross_wins / gross_losses
    } else if gross_wins > 0.0 {
        99.9
    } else {
        0.0
    };

    let profit_eur = balance_eur - initial_eur;
    let return_pct = (profit_eur / initial_eur) * 100.0;

    MonthBacktestResult {
        month_name: month_name.to_string(),
        initial_eur,
        final_eur: balance_eur,
        profit_eur,
        return_pct,
        total_trades,
        wins,
        losses,
        win_rate,
        profit_factor,
        max_drawdown_pct,
        tp_partials,
        tp_finals,
        breakevens,
        stop_losses,
        time_stops,
    }
}

async fn fetch_cached_candles(
    client: &reqwest::Client,
    symbol: &str,
    start_time: i64,
    end_time: i64,
) -> Result<Vec<Candle>> {
    let cache_dir = std::path::Path::new("data");
    let cache_file = cache_dir.join(format!("cache_{}_{}_{}.json", symbol, start_time, end_time));
    if cache_file.exists() {
        if let Ok(data) = std::fs::read_to_string(&cache_file) {
            if let Ok(cached) = serde_json::from_str::<Vec<Candle>>(&data) {
                if !cached.is_empty() {
                    return Ok(cached);
                }
            }
        }
    }

    let mut current_start = start_time;
    let mut candles = Vec::new();

    print!("  Baixando {} a {}...", start_time, end_time);
    use std::io::Write;
    let _ = std::io::stdout().flush();

    while current_start < end_time {
        let url = format!(
            "https://fapi.binance.com/fapi/v1/klines?symbol={}&interval=5m&startTime={}&endTime={}&limit=1000",
            symbol, current_start, end_time
        );
        let resp = client
            .get(&url)
            .header("User-Agent", "Mozilla/5.0")
            .send()
            .await?;
        let raw: Vec<Vec<Value>> = resp.json().await?;
        if raw.is_empty() {
            break;
        }

        let mut last_close = current_start;
        for item in raw {
            if item.len() < 7 {
                continue;
            }
            let open_time = item[0].as_i64().unwrap_or(0);
            let open = item[1].as_str().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
            let high = item[2].as_str().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
            let low = item[3].as_str().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
            let close = item[4].as_str().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
            let volume = item[5].as_str().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
            let close_time = item[6].as_i64().unwrap_or(0);
            last_close = close_time;

            candles.push(Candle {
                open_time,
                open,
                high,
                low,
                close,
                volume,
                close_time,
                is_closed: true,
            });
        }

        if last_close <= current_start {
            break;
        }
        current_start = last_close + 1;
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
    println!(" Concluído ({} candles)!", candles.len());

    let _ = std::fs::create_dir_all(&cache_dir);
    if let Ok(serialized) = serde_json::to_string(&candles) {
        let _ = std::fs::write(&cache_file, serialized);
    }

    Ok(candles)
}

#[tokio::main]
async fn main() -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .danger_accept_invalid_certs(true)
        .build()?;

    let symbol = "ETHUSDT";
    let initial_eur = 200.00;
    let leverage = 5;
    let eur_to_usdt = 1.08;
    let warmup_candles_5m = 576; // 48 horas
    let warmup_offset_ms = (warmup_candles_5m as i64) * 5 * 60 * 1000;

    println!("================================================================================");
    println!("   BACKTEST SISTEMA LATERAL & FALSOS ROMPIMENTOS (LCP-HARVESTER 2026)");
    println!("   Micro-Alvos Calibrados + Sweep de Falso Rompimento + RSI + Breakeven Lock");
    println!("================================================================================");
    println!("  PAR:              ETHUSDT Perpétuo (5m)");
    println!("  BANCA BASE:       €{:.2} (${:.2} USDT) em {}x Alavancagem", initial_eur, initial_eur * eur_to_usdt, leverage);
    println!("  TAXAS REAIS:      Taker Fee 0.05% + Slippage 0.05% por ponta de execução\n");

    let cfg = LateralScalpConfig::default();

    let months = vec![
        ("01. Janeiro 2026",   1767225600000i64, 1769903999999i64),
        ("02. Fevereiro 2026", 1769904000000i64, 1772323199999i64),
        ("03. Março 2026",     1772323200000i64, 1775001599999i64),
        ("04. Abril 2026",     1775001600000i64, 1777593599999i64),
        ("05. Maio 2026",      1777593600000i64, 1780271999999i64),
        ("06. Junho 2026",     1780272000000i64, 1782863999999i64),
        ("07. Julho 2026",     1782864000000i64, 1785542399999i64),
        ("08. Agosto 2026",    1785542400000i64, 1788220799999i64),
        ("09. Setembro 2026",  1788220800000i64, 1790812799999i64),
        ("10. Outubro 2026",   1790812800000i64, 1792188000000i64),
    ];

    let mut month_results = Vec::new();
    let mut total_trades_year = 0;
    let mut total_wins_year = 0;
    let mut total_losses_year = 0;
    let mut total_pnl_year = 0.0;
    let mut running_balance = initial_eur;

    for (name, start, end) in &months {
        let fetch_start = start - warmup_offset_ms;
        let candles_5m = fetch_cached_candles(&client, symbol, fetch_start, *end).await?;
        let res = simulate_chop_scalp(name, &candles_5m, cfg.clone(), initial_eur, leverage, eur_to_usdt, warmup_candles_5m);

        total_trades_year += res.total_trades;
        total_wins_year += res.wins;
        total_losses_year += res.losses;
        total_pnl_year += res.profit_eur;
        running_balance += res.profit_eur;

        month_results.push(res);
    }

    println!("\n==========================================================================================================");
    println!(" TABELA COMPARATIVA MÊS A MÊS - MOTOR LATERAL & FALSO ROMPIMENTO (2026):");
    println!("==========================================================================================================");
    println!("{:<20} | {:<16} | {:<14} | {:<14} | {:<8} | {:<10}", "MÊS", "PNL LÍQUIDO", "BANCA FINAL", "WIN RATE", "PF", "TRADES");
    println!("---------------------+------------------+----------------+----------------+----------+------------");

    for r in &month_results {
        println!("{:<20} | {:+8.2}€ ({:+5.2}%) | €{:<6.2} (${:<6.2}) | {:4.1}% ({:2}W/{:2}L) | {:4.2}     | {:2} ops",
            r.month_name, r.profit_eur, r.return_pct, r.final_eur, r.final_eur * eur_to_usdt,
            r.win_rate, r.wins, r.losses, r.profit_factor, r.total_trades
        );
    }

    let avg_wr = if total_trades_year > 0 { (total_wins_year as f64 / total_trades_year as f64) * 100.0 } else { 0.0 };

    println!("==========================================================================================================");
    println!(" BALANÇO CONSOLIDADO DO MOTOR DE HARVESTING LATERAL (2026):");
    println!("  * Lucro Líquido Acumulado: {:+.2}€ ({:+.2}% da banca base)", total_pnl_year, (total_pnl_year / initial_eur) * 100.0);
    println!("  * Banca Final Estimada:    €{:.2} (${:.2} USDT)", running_balance, running_balance * eur_to_usdt);
    println!("  * Total de Operações:      {} ({} Vitórias / {} Derrotas)", total_trades_year, total_wins_year, total_losses_year);
    println!("  * Taxa Geral de Acerto:    {:.1}%", avg_wr);
    println!("==========================================================================================================\n");

    Ok(())
}
