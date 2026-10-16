#![allow(dead_code, unused_variables, unused_imports, unused_assignments)]
// language: Rust, file: src/bin/backtest_quarter.rs, target: Windows 11, MSVC
// =========================================================================================
//  BACKTEST OFICIAL CONSOLIDADO 3 MESES: AGOSTO, SETEMBRO E OUTUBRO (2026)
//  Motor Híbrido Unificado: MERA Scalp + MKT 2.2 + Phantom Liquidity Engine (PLE)
// =========================================================================================

use anyhow::Result;
use binance_futures_bot::config::MeraConfig;
use binance_futures_bot::mera::MeraEngine;
use binance_futures_bot::types::{Candle, Position, PositionSide, SignalType};
use chrono::{DateTime, Utc};
use serde_json::Value;

#[derive(Debug, Clone)]
struct TradeLog {
    timestamp: i64,
    side: String,
    entry_price: f64,
    exit_price: f64,
    quantity: f64,
    gross_pnl_eur: f64,
    fee_eur: f64,
    net_pnl_eur: f64,
    return_pct: f64,
    balance_after_eur: f64,
    reason: String,
}

#[derive(Debug, Clone)]
struct BacktestResult {
    period_name: String,
    leverage: u32,
    initial_eur: f64,
    final_eur: f64,
    profit_eur: f64,
    total_return_pct: f64,
    total_trades: usize,
    wins: usize,
    losses: usize,
    win_rate: f64,
    max_drawdown_pct: f64,
    profit_factor: f64,
    tp_partials: usize,
    tp_finals: usize,
    breakevens: usize,
    stop_losses: usize,
    trades: Vec<TradeLog>,
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
                    println!("  [CACHE LOCAL] {} candles carregados de {:?}", cached.len(), cache_file);
                    return Ok(cached);
                }
            }
        }
    }

    let mut current_start = start_time;
    let mut candles = Vec::new();

    print!("  Baixando candles de 5m da Binance Futures ({} a {})", start_time, end_time);
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

        print!(".");
        use std::io::Write;
        let _ = std::io::stdout().flush();

        if last_close <= current_start {
            break;
        }
        current_start = last_close + 1;
        tokio::time::sleep(tokio::time::Duration::from_millis(80)).await;
    }
    println!(" Concluído!");

    let _ = std::fs::create_dir_all(&cache_dir);
    if let Ok(serialized) = serde_json::to_string(&candles) {
        let _ = std::fs::write(&cache_file, serialized);
    }

    Ok(candles)
}

fn aggregate_to_15m(candles_5m: &[Candle]) -> Vec<Candle> {
    let mut candles_15m = Vec::new();
    let mut chunk: Vec<Candle> = Vec::new();
    for c in candles_5m {
        chunk.push(c.clone());
        if chunk.len() == 3 {
            let open = chunk[0].open;
            let high = chunk.iter().map(|k| k.high).fold(f64::NEG_INFINITY, f64::max);
            let low = chunk.iter().map(|k| k.low).fold(f64::INFINITY, f64::min);
            let close = chunk[2].close;
            let volume = chunk.iter().map(|k| k.volume).sum();
            candles_15m.push(Candle {
                open_time: chunk[0].open_time,
                open,
                high,
                low,
                close,
                volume,
                close_time: chunk[2].close_time,
                is_closed: true,
            });
            chunk.clear();
        }
    }
    candles_15m
}

fn simulate_engine(
    period_name: &str,
    candles: &[Candle],
    cfg: MeraConfig,
    initial_eur: f64,
    leverage: u32,
    eur_to_usdt: f64,
    warmup_len: usize,
) -> BacktestResult {
    let mut engine = MeraEngine::new(cfg);
    if warmup_len > 0 && warmup_len < candles.len() {
        engine.warmup(&candles[..warmup_len]);
    }

    let mut balance_eur = initial_eur;
    let mut peak_eur = initial_eur;
    let mut max_drawdown_pct = 0.0;
    let mut trades = Vec::new();
    let mut current_pos = Position::new("ETHUSDT".to_string(), leverage);

    let fee_rate = 0.0005; // 0.05% Taker fee
    let slip_rate = 0.0005; // 0.05% Slippage

    let mut tp_partials = 0;
    let mut tp_finals = 0;
    let mut breakevens = 0;
    let mut stop_losses = 0;
    let mut entry_reason = String::new();

    for candle in &candles[warmup_len..] {
        let (_telem, signal_opt) = engine.update(candle, &current_pos);

        if let Some(sig) = signal_opt {
            match sig.signal_type {
                SignalType::OpenLong => {
                    if current_pos.side == PositionSide::None {
                        entry_reason = sig.reason.clone();
                        let fill_price = sig.price * (1.0 + slip_rate);
                        let balance_usdt = balance_eur * eur_to_usdt;
                        let size_mult = sig.size_factor.unwrap_or(1.0);
                        let notional_usdt = (800.0f64.min(balance_usdt * (leverage as f64) * 0.90)) * size_mult;
                        let mut qty = notional_usdt / fill_price;
                        qty = (qty * 1000.0).floor() / 1000.0;
                        if qty >= 0.001 {
                            let open_fee_usdt = (qty * fill_price) * fee_rate;
                            balance_eur -= open_fee_usdt / eur_to_usdt;

                            current_pos.side = PositionSide::Long;
                            current_pos.entry_price = fill_price;
                            current_pos.amount = qty;
                            current_pos.initial_amount = qty;
                            current_pos.stop_loss = sig.stop_loss;
                            current_pos.take_profit = sig.take_profit;
                            current_pos.tp_partial = sig.tp_partial;
                            current_pos.partial_taken = false;
                            current_pos.be_locked = false;
                        }
                    }
                }
                SignalType::OpenShort => {
                    if current_pos.side == PositionSide::None {
                        entry_reason = sig.reason.clone();
                        let fill_price = sig.price * (1.0 - slip_rate);
                        let balance_usdt = balance_eur * eur_to_usdt;
                        let size_mult = sig.size_factor.unwrap_or(1.0);
                        let notional_usdt = (800.0f64.min(balance_usdt * (leverage as f64) * 0.90)) * size_mult;
                        let mut qty = notional_usdt / fill_price;
                        qty = (qty * 1000.0).floor() / 1000.0;
                        if qty >= 0.001 {
                            let open_fee_usdt = (qty * fill_price) * fee_rate;
                            balance_eur -= open_fee_usdt / eur_to_usdt;

                            current_pos.side = PositionSide::Short;
                            current_pos.entry_price = fill_price;
                            current_pos.amount = qty;
                            current_pos.initial_amount = qty;
                            current_pos.stop_loss = sig.stop_loss;
                            current_pos.take_profit = sig.take_profit;
                            current_pos.tp_partial = sig.tp_partial;
                            current_pos.partial_taken = false;
                            current_pos.be_locked = false;
                        }
                    }
                }
                SignalType::PartialCloseLong => {
                    if current_pos.side == PositionSide::Long && !current_pos.partial_taken {
                        let fill_price = sig.price * (1.0 - slip_rate);
                        let ratio = sig.partial_ratio.unwrap_or(0.70);
                        let mut close_qty = (current_pos.initial_amount * ratio * 1000.0).floor() / 1000.0;
                        close_qty = close_qty.min(current_pos.amount);
                        if close_qty >= 0.001 {
                            let gross_pnl_usdt = (fill_price - current_pos.entry_price) * close_qty;
                            let close_fee_usdt = (close_qty * fill_price) * fee_rate;
                            let net_pnl_usdt = gross_pnl_usdt - close_fee_usdt;
                            let net_pnl_eur = net_pnl_usdt / eur_to_usdt;
                            balance_eur += net_pnl_eur;

                            current_pos.amount = ((current_pos.amount - close_qty) * 1000.0).round() / 1000.0;
                            current_pos.partial_taken = true;
                            current_pos.be_locked = true;
                            if let Some(be_stop) = sig.stop_loss {
                                current_pos.stop_loss = Some(be_stop);
                            }
                            tp_partials += 1;

                            trades.push(TradeLog {
                                timestamp: candle.close_time,
                                side: format!("LONG (PARCIAL {:.0}%)", ratio * 100.0),
                                entry_price: current_pos.entry_price,
                                exit_price: fill_price,
                                quantity: close_qty,
                                gross_pnl_eur: gross_pnl_usdt / eur_to_usdt,
                                fee_eur: close_fee_usdt / eur_to_usdt,
                                net_pnl_eur,
                                return_pct: (gross_pnl_usdt / (close_qty * current_pos.entry_price)) * 100.0 * (leverage as f64),
                                balance_after_eur: balance_eur,
                                reason: sig.reason,
                            });
                        }
                    }
                }
                SignalType::PartialCloseShort => {
                    if current_pos.side == PositionSide::Short && !current_pos.partial_taken {
                        let fill_price = sig.price * (1.0 + slip_rate);
                        let ratio = sig.partial_ratio.unwrap_or(0.70);
                        let mut close_qty = (current_pos.initial_amount * ratio * 1000.0).floor() / 1000.0;
                        close_qty = close_qty.min(current_pos.amount);
                        if close_qty >= 0.001 {
                            let gross_pnl_usdt = (current_pos.entry_price - fill_price) * close_qty;
                            let close_fee_usdt = (close_qty * fill_price) * fee_rate;
                            let net_pnl_usdt = gross_pnl_usdt - close_fee_usdt;
                            let net_pnl_eur = net_pnl_usdt / eur_to_usdt;
                            balance_eur += net_pnl_eur;

                            current_pos.amount = ((current_pos.amount - close_qty) * 1000.0).round() / 1000.0;
                            current_pos.partial_taken = true;
                            current_pos.be_locked = true;
                            if let Some(be_stop) = sig.stop_loss {
                                current_pos.stop_loss = Some(be_stop);
                            }
                            tp_partials += 1;

                            trades.push(TradeLog {
                                timestamp: candle.close_time,
                                side: format!("SHORT (PARCIAL {:.0}%)", ratio * 100.0),
                                entry_price: current_pos.entry_price,
                                exit_price: fill_price,
                                quantity: close_qty,
                                gross_pnl_eur: gross_pnl_usdt / eur_to_usdt,
                                fee_eur: close_fee_usdt / eur_to_usdt,
                                net_pnl_eur,
                                return_pct: (gross_pnl_usdt / (close_qty * current_pos.entry_price)) * 100.0 * (leverage as f64),
                                balance_after_eur: balance_eur,
                                reason: sig.reason,
                            });
                        }
                    }
                }
                SignalType::CloseLong => {
                    if current_pos.side == PositionSide::Long && current_pos.amount > 0.0 {
                        let fill_price = sig.price * (1.0 - slip_rate);
                        let gross_pnl_usdt = (fill_price - current_pos.entry_price) * current_pos.amount;
                        let close_fee_usdt = (current_pos.amount * fill_price) * fee_rate;
                        let net_pnl_usdt = gross_pnl_usdt - close_fee_usdt;
                        let net_pnl_eur = net_pnl_usdt / eur_to_usdt;
                        balance_eur += net_pnl_eur;

                        let is_be = sig.reason.contains("Breakeven") || (fill_price >= current_pos.entry_price && !sig.reason.contains("Final") && !sig.reason.contains("TP"));
                        let is_tp_final = sig.reason.contains("Final") || sig.reason.contains("TP Final");
                        if is_tp_final {
                            tp_finals += 1;
                        } else if is_be {
                            breakevens += 1;
                        } else {
                            stop_losses += 1;
                        }

                        trades.push(TradeLog {
                            timestamp: candle.close_time,
                            side: "LONG (FECHAMENTO)".to_string(),
                            entry_price: current_pos.entry_price,
                            exit_price: fill_price,
                            quantity: current_pos.amount,
                            gross_pnl_eur: gross_pnl_usdt / eur_to_usdt,
                            fee_eur: close_fee_usdt / eur_to_usdt,
                            net_pnl_eur,
                            return_pct: (gross_pnl_usdt / (current_pos.amount * current_pos.entry_price)) * 100.0 * (leverage as f64),
                            balance_after_eur: balance_eur,
                            reason: format!("[{}] -> [{}]", entry_reason, sig.reason),
                        });

                        current_pos = Position::new("ETHUSDT".to_string(), leverage);
                    }
                }
                SignalType::CloseShort => {
                    if current_pos.side == PositionSide::Short && current_pos.amount > 0.0 {
                        let fill_price = sig.price * (1.0 + slip_rate);
                        let gross_pnl_usdt = (current_pos.entry_price - fill_price) * current_pos.amount;
                        let close_fee_usdt = (current_pos.amount * fill_price) * fee_rate;
                        let net_pnl_usdt = gross_pnl_usdt - close_fee_usdt;
                        let net_pnl_eur = net_pnl_usdt / eur_to_usdt;
                        balance_eur += net_pnl_eur;

                        let is_be = sig.reason.contains("Breakeven") || (fill_price <= current_pos.entry_price && !sig.reason.contains("Final") && !sig.reason.contains("TP"));
                        let is_tp_final = sig.reason.contains("Final") || sig.reason.contains("TP Final");
                        if is_tp_final {
                            tp_finals += 1;
                        } else if is_be {
                            breakevens += 1;
                        } else {
                            stop_losses += 1;
                        }

                        trades.push(TradeLog {
                            timestamp: candle.close_time,
                            side: "SHORT (FECHAMENTO)".to_string(),
                            entry_price: current_pos.entry_price,
                            exit_price: fill_price,
                            quantity: current_pos.amount,
                            gross_pnl_eur: gross_pnl_usdt / eur_to_usdt,
                            fee_eur: close_fee_usdt / eur_to_usdt,
                            net_pnl_eur,
                            return_pct: (gross_pnl_usdt / (current_pos.amount * current_pos.entry_price)) * 100.0 * (leverage as f64),
                            balance_after_eur: balance_eur,
                            reason: format!("[{}] -> [{}]", entry_reason, sig.reason),
                        });

                        current_pos = Position::new("ETHUSDT".to_string(), leverage);
                    }
                }
                SignalType::Hold => {}
            }
        }

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
    let total_return_pct = (profit_eur / initial_eur) * 100.0;

    BacktestResult {
        period_name: period_name.to_string(),
        leverage,
        initial_eur,
        final_eur: balance_eur,
        profit_eur,
        total_return_pct,
        total_trades,
        wins,
        losses,
        win_rate,
        max_drawdown_pct,
        profit_factor,
        tp_partials,
        tp_finals,
        breakevens,
        stop_losses,
        trades,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .danger_accept_invalid_certs(true)
        .build()?;

    let symbol = "ETHUSDT";
    let initial_eur = 200.00;
    let leverage = 5;
    let eur_to_usdt = 1.08;
    let warmup_candles_count = 576;
    let warmup_offset_ms = (warmup_candles_count as i64) * 5 * 60 * 1000;
    let warmup_5m = 576; // 48 horas de velas de 5m

    println!("================================================================================");
    println!("   BACKTEST OFICIAL TRIMESTRAL — ÚLTIMOS 3 MESES (AGOSTO, SETEMBRO, OUTUBRO)");
    println!("   Motor MERA Adaptativo Conectado (Nadaraya + MKT 3.0 + PLE) — TIMEFRAME 5M");
    println!("================================================================================");
    println!("  PAR:              ETHUSDT Perpétuo (USDⓈ-M 5m)");
    println!("  BANCA BASE:       €{:.2} (${:.2} USDT) em {}x Alavancagem", initial_eur, initial_eur * eur_to_usdt, leverage);
    println!("  CALIBRAÇÃO 5M:    SL 0.85% | TP1 1.30% (70%) | TP2 2.40% (Runner) | Spread >= 0.35%");
    println!("  CUSTOS REAIS:     Taker Fee 0.05% + Slippage 0.05% por ponta de execução\n");

    let cfg = MeraConfig::default();

    // 1. Mês de Agosto (01/08 a 31/08/2026)
    let aug_start = 1785542400000i64;
    let aug_end = 1788220799999i64;
    let aug_fetch_start = aug_start - warmup_offset_ms;
    println!("-> Carregando dados de Agosto 2026...");
    let aug_candles_5m = fetch_cached_candles(&client, symbol, aug_fetch_start, aug_end).await?;
    let res_aug = simulate_engine("Agosto 2026 (5m)", &aug_candles_5m, cfg.clone(), initial_eur, leverage, eur_to_usdt, warmup_5m);

    // 2. Mês de Setembro (01/09 a 30/09/2026)
    let sep_start = 1788220800000i64;
    let sep_end = 1790812799999i64;
    let sep_fetch_start = sep_start - warmup_offset_ms;
    println!("-> Carregando dados de Setembro 2026...");
    let sep_candles_5m = fetch_cached_candles(&client, symbol, sep_fetch_start, sep_end).await?;
    let res_sep = simulate_engine("Setembro 2026 (5m)", &sep_candles_5m, cfg.clone(), initial_eur, leverage, eur_to_usdt, warmup_5m);

    // 3. Mês de Outubro (01/10/2026 até Agora)
    let oct_start = 1790812800000i64;
    let oct_end = 1792176684688i64;
    let oct_fetch_start = oct_start - warmup_offset_ms;
    println!("-> Carregando dados de Outubro 2026 (Até o momento)...");
    let oct_candles_5m = fetch_cached_candles(&client, symbol, oct_fetch_start, oct_end).await?;
    let res_oct = simulate_engine("Outubro 2026 (5m)", &oct_candles_5m, cfg.clone(), initial_eur, leverage, eur_to_usdt, warmup_5m);

    // Resumo Comparativo Mês a Mês
    println!("\n================================================================================");
    println!(" TABELA COMPARATIVA MÊS A MÊS (ÚLTIMOS 3 MESES):");
    println!("================================================================================");
    println!("{:<28} | {:<12} | {:<14} | {:<10} | {:<8} | {:<10}", "PERÍODO", "PNL LÍQUIDO", "BANCA FINAL", "WIN RATE", "PF", "DRAWDOWN");
    println!("-----------------------------+--------------+----------------+------------+----------+-----------");

    for r in &[&res_aug, &res_sep, &res_oct] {
        println!("{:<28} | {:+7.2}€ ({:+5.2}%) | €{:<6.2} (${:<6.2}) | {:4.1}% ({:2}W/{:2}L)| {:4.2}     | {:4.2}%",
            r.period_name, r.profit_eur, r.total_return_pct, r.final_eur, r.final_eur * eur_to_usdt,
            r.win_rate, r.wins, r.losses, r.profit_factor, r.max_drawdown_pct
        );
    }

    for r in &[&res_aug, &res_sep, &res_oct] {
        println!("\n--- Extrato Detalhado: {} ---", r.period_name);
        for (i, t) in r.trades.iter().enumerate() {
            println!("  #{:02}: {:<20} | ${:<7.2} -> ${:<7.2} | {:+6.2}€ | {}",
                i+1, t.side, t.entry_price, t.exit_price, t.net_pnl_eur, t.reason
            );
        }
    }

    // Consolidação Acumulada
    let total_trades_all = res_aug.total_trades + res_sep.total_trades + res_oct.total_trades;
    let total_wins_all = res_aug.wins + res_sep.wins + res_oct.wins;
    let total_losses_all = res_aug.losses + res_sep.losses + res_oct.losses;
    let total_pnl_all = res_aug.profit_eur + res_sep.profit_eur + res_oct.profit_eur;
    let avg_wr = if total_trades_all > 0 { (total_wins_all as f64 / total_trades_all as f64) * 100.0 } else { 0.0 };

    println!("================================================================================");
    println!(" BALANÇO CONSOLIDADO DO TRIMESTRE:");
    println!("  * Lucro Líquido Acumulado: {:+.2}€ ({:+.2}% da banca base)", total_pnl_all, (total_pnl_all / initial_eur) * 100.0);
    println!("  * Total de Operações:      {} ({} Vitórias / {} Derrotas)", total_trades_all, total_wins_all, total_losses_all);
    println!("  * Taxa Média de Acerto:    {:.1}%", avg_wr);
    println!("================================================================================\n");

    Ok(())
}
