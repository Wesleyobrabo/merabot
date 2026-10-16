#![allow(dead_code, unused_variables, unused_imports, unused_assignments)]
// language: Rust, file: src/bin/backtest_hybrid_year.rs, target: Windows 11, MSVC
// =========================================================================================
//  BACKTEST OFICIAL DO MOTOR MERA HÍBRIDO (2026 COMPLETO - 10 MESES)
//  Arquitetura: Motor MERA Oficial (src/mera.rs)
//  Regime 1: MTF-1 Trend Bimodal (Alvos Longos: TP1 2.80% / TP2 6.50% / SL 1.25% / BE +0.25%)
//  Regime 2: LCP-Harvester Lateral (Micro-Scalp: TP1 1.05% / TP2 1.75% / SL 1.15% / BE +0.20%)
// =========================================================================================

use anyhow::Result;
use binance_futures_bot::config::{AppConfig, MeraConfig};
use binance_futures_bot::mera::MeraEngine;
use binance_futures_bot::types::{Candle, Position, PositionSide, SignalType};
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
                    return Ok(cached);
                }
            }
        }
    }

    let mut current_start = start_time;
    let mut candles = Vec::new();

    print!("  Baixando candles de 5m da Binance Futures");
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
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
    println!(" Concluído ({} candles)!", candles.len());

    let _ = std::fs::create_dir_all(&cache_dir);
    if let Ok(serialized) = serde_json::to_string(&candles) {
        let _ = std::fs::write(&cache_file, serialized);
    }

    Ok(candles)
}

fn simulate_engine_month(
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
    let mut trades: Vec<TradeLog> = Vec::new();
    let mut current_pos = Position::new("ETHUSDT".to_string(), leverage);

    let fee_rate = 0.0005; // 0.05% Taker
    let slip_rate = 0.0005; // 0.05% Slippage

    let mut tp_partials = 0;
    let mut tp_finals = 0;
    let mut breakevens = 0;
    let mut stop_losses = 0;

    for candle in &candles[warmup_len..] {
        let (_telem, signal_opt) = engine.update(candle, &current_pos);

        if let Some(sig) = signal_opt {
            match sig.signal_type {
                SignalType::OpenLong => {
                    if current_pos.side == PositionSide::None {
                        let fill_price = sig.price * (1.0 + slip_rate);
                        let balance_usdt = balance_eur * eur_to_usdt;
                        let risk_eur = balance_eur * 0.020;
                        let sl_dist_pct = if let Some(sl) = sig.stop_loss {
                            ((fill_price - sl) / fill_price).abs() * 100.0
                        } else {
                            1.25
                        };
                        let target_notional = ((risk_eur * eur_to_usdt) / (sl_dist_pct / 100.0))
                            .min(balance_usdt * (leverage as f64) * 0.90)
                            .min(800.0);
                        let mut qty = target_notional / fill_price;
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
                        let fill_price = sig.price * (1.0 - slip_rate);
                        let balance_usdt = balance_eur * eur_to_usdt;
                        let risk_eur = balance_eur * 0.020;
                        let sl_dist_pct = if let Some(sl) = sig.stop_loss {
                            ((sl - fill_price) / fill_price).abs() * 100.0
                        } else {
                            1.25
                        };
                        let target_notional = ((risk_eur * eur_to_usdt) / (sl_dist_pct / 100.0))
                            .min(balance_usdt * (leverage as f64) * 0.90)
                            .min(800.0);
                        let mut qty = target_notional / fill_price;
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
                    if current_pos.side == PositionSide::Long && !current_pos.partial_taken && current_pos.amount >= 0.001 {
                        let p_ratio = sig.partial_ratio.unwrap_or(0.70);
                        let mut close_qty = (current_pos.initial_amount * p_ratio * 1000.0).floor() / 1000.0;
                        close_qty = close_qty.min(current_pos.amount);
                        if close_qty >= 0.001 {
                            let exit_price = sig.price * (1.0 - slip_rate);
                            let gross_usdt = (exit_price - current_pos.entry_price) * close_qty;
                            let fee_usdt = (close_qty * exit_price) * fee_rate;
                            let net_usdt = gross_usdt - fee_usdt;
                            let net_eur = net_usdt / eur_to_usdt;

                            balance_eur += net_eur;
                            current_pos.amount = ((current_pos.amount - close_qty) * 1000.0).round() / 1000.0;
                            current_pos.partial_taken = true;
                            current_pos.be_locked = true;
                            current_pos.stop_loss = sig.stop_loss;
                            tp_partials += 1;

                            trades.push(TradeLog {
                                timestamp: candle.close_time,
                                side: format!("LONG (PARCIAL {:.0}%)", p_ratio * 100.0),
                                entry_price: current_pos.entry_price,
                                exit_price,
                                quantity: close_qty,
                                gross_pnl_eur: gross_usdt / eur_to_usdt,
                                fee_eur: fee_usdt / eur_to_usdt,
                                net_pnl_eur: net_eur,
                                return_pct: ((exit_price - current_pos.entry_price) / current_pos.entry_price) * 100.0 * (leverage as f64),
                                balance_after_eur: balance_eur,
                                reason: sig.reason,
                            });
                        }
                    }
                }
                SignalType::PartialCloseShort => {
                    if current_pos.side == PositionSide::Short && !current_pos.partial_taken && current_pos.amount >= 0.001 {
                        let p_ratio = sig.partial_ratio.unwrap_or(0.70);
                        let mut close_qty = (current_pos.initial_amount * p_ratio * 1000.0).floor() / 1000.0;
                        close_qty = close_qty.min(current_pos.amount);
                        if close_qty >= 0.001 {
                            let exit_price = sig.price * (1.0 + slip_rate);
                            let gross_usdt = (current_pos.entry_price - exit_price) * close_qty;
                            let fee_usdt = (close_qty * exit_price) * fee_rate;
                            let net_usdt = gross_usdt - fee_usdt;
                            let net_eur = net_usdt / eur_to_usdt;

                            balance_eur += net_eur;
                            current_pos.amount = ((current_pos.amount - close_qty) * 1000.0).round() / 1000.0;
                            current_pos.partial_taken = true;
                            current_pos.be_locked = true;
                            current_pos.stop_loss = sig.stop_loss;
                            tp_partials += 1;

                            trades.push(TradeLog {
                                timestamp: candle.close_time,
                                side: format!("SHORT (PARCIAL {:.0}%)", p_ratio * 100.0),
                                entry_price: current_pos.entry_price,
                                exit_price,
                                quantity: close_qty,
                                gross_pnl_eur: gross_usdt / eur_to_usdt,
                                fee_eur: fee_usdt / eur_to_usdt,
                                net_pnl_eur: net_eur,
                                return_pct: ((current_pos.entry_price - exit_price) / current_pos.entry_price) * 100.0 * (leverage as f64),
                                balance_after_eur: balance_eur,
                                reason: sig.reason,
                            });
                        }
                    }
                }
                SignalType::CloseLong => {
                    if current_pos.side == PositionSide::Long && current_pos.amount >= 0.001 {
                        let exit_price = sig.price * (1.0 - slip_rate);
                        let gross_usdt = (exit_price - current_pos.entry_price) * current_pos.amount;
                        let fee_usdt = (current_pos.amount * exit_price) * fee_rate;
                        let net_usdt = gross_usdt - fee_usdt;
                        let net_eur = net_usdt / eur_to_usdt;

                        balance_eur += net_eur;

                        let is_be = sig.reason.contains("Breakeven");
                        let is_tp = sig.reason.contains("TP Final");
                        if is_tp {
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
                            exit_price,
                            quantity: current_pos.amount,
                            gross_pnl_eur: gross_usdt / eur_to_usdt,
                            fee_eur: fee_usdt / eur_to_usdt,
                            net_pnl_eur: net_eur,
                            return_pct: ((exit_price - current_pos.entry_price) / current_pos.entry_price) * 100.0 * (leverage as f64),
                            balance_after_eur: balance_eur,
                            reason: sig.reason,
                        });

                        current_pos = Position::new("ETHUSDT".to_string(), leverage);
                    }
                }
                SignalType::CloseShort => {
                    if current_pos.side == PositionSide::Short && current_pos.amount >= 0.001 {
                        let exit_price = sig.price * (1.0 + slip_rate);
                        let gross_usdt = (current_pos.entry_price - exit_price) * current_pos.amount;
                        let fee_usdt = (current_pos.amount * exit_price) * fee_rate;
                        let net_usdt = gross_usdt - fee_usdt;
                        let net_eur = net_usdt / eur_to_usdt;

                        balance_eur += net_eur;

                        let is_be = sig.reason.contains("Breakeven");
                        let is_tp = sig.reason.contains("TP Final");
                        if is_tp {
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
                            exit_price,
                            quantity: current_pos.amount,
                            gross_pnl_eur: gross_usdt / eur_to_usdt,
                            fee_eur: fee_usdt / eur_to_usdt,
                            net_pnl_eur: net_eur,
                            return_pct: ((current_pos.entry_price - exit_price) / current_pos.entry_price) * 100.0 * (leverage as f64),
                            balance_after_eur: balance_eur,
                            reason: sig.reason,
                        });

                        current_pos = Position::new("ETHUSDT".to_string(), leverage);
                    }
                }
                _ => {}
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
        .timeout(std::time::Duration::from_secs(20))
        .danger_accept_invalid_certs(true)
        .build()?;

    let config = AppConfig::load_or_create("config.toml")?;
    let mut mera_cfg = config.mera.clone();
    mera_cfg.use_hybrid = true;

    let symbol = "ETHUSDT";
    let initial_eur = 200.00;
    let leverage = 5;
    let eur_to_usdt = 1.08;
    let warmup_candles_5m = 576; // 48 horas de velas de 5m
    let warmup_offset_ms = (warmup_candles_5m as i64) * 5 * 60 * 1000;

    println!("================================================================================");
    println!("   BACKTEST OFICIAL MOTOR MERA HÍBRIDO (2026: JANEIRO A OUTUBRO)");
    println!("   MTF-1 Trend Bimodal (2.80%/6.50%) + LCP-Harvester Lateral (1.05%/1.75%)");
    println!("================================================================================");
    println!("  PAR:              ETHUSDT Perpétuo (5m)");
    println!("  BANCA BASE:       €{:.2} (${:.2} USDT) em {}x Alavancagem", initial_eur, initial_eur * eur_to_usdt, leverage);
    println!("  RISCO POR TRADE:  2.0% da banca");
    println!("  CUSTOS REAIS:     Taker Fee 0.05% + Slippage 0.05% por ponta de execução\n");

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
        let res = simulate_engine_month(name, &candles_5m, mera_cfg.clone(), initial_eur, leverage, eur_to_usdt, warmup_candles_5m);

        total_trades_year += res.total_trades;
        total_wins_year += res.wins;
        total_losses_year += res.losses;
        total_pnl_year += res.profit_eur;
        running_balance += res.profit_eur;

        month_results.push(res);
    }

    println!("\n==========================================================================================================");
    println!(" TABELA COMPARATIVA MÊS A MÊS - NOVO MOTOR MERA HÍBRIDO (2026):");
    println!("==========================================================================================================");
    println!("{:<20} | {:<16} | {:<14} | {:<14} | {:<8} | {:<10}", "MÊS", "PNL LÍQUIDO", "BANCA FINAL", "WIN RATE", "PF", "TRADES");
    println!("---------------------+------------------+----------------+----------------+----------+------------");

    for r in &month_results {
        println!("{:<20} | {:+8.2}€ ({:+5.2}%) | €{:<6.2} (${:<6.2}) | {:4.1}% ({:2}W/{:2}L) | {:4.2}     | {:2} ops",
            r.period_name, r.profit_eur, r.total_return_pct, r.final_eur, r.final_eur * eur_to_usdt,
            r.win_rate, r.wins, r.losses, r.profit_factor, r.total_trades
        );
    }

    let avg_wr = if total_trades_year > 0 { (total_wins_year as f64 / total_trades_year as f64) * 100.0 } else { 0.0 };

    println!("==========================================================================================================");
    println!(" BALANÇO CONSOLIDADO DO NOVO MOTOR HÍBRIDO (2026 COMPLETO):");
    println!("  * Lucro Líquido Acumulado: {:+.2}€ ({:+.2}% da banca base)", total_pnl_year, (total_pnl_year / initial_eur) * 100.0);
    println!("  * Banca Final Estimada:    €{:.2} (${:.2} USDT)", running_balance, running_balance * eur_to_usdt);
    println!("  * Total de Operações:      {} ({} Vitórias / {} Derrotas)", total_trades_year, total_wins_year, total_losses_year);
    println!("  * Taxa Geral de Acerto:    {:.1}%", avg_wr);
    println!("==========================================================================================================\n");

    Ok(())
}
