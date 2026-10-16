#![allow(dead_code, unused_variables, unused_imports, unused_assignments)]
// language: Rust, file: src/bin/backtest_trap.rs, target: Windows 11, MSVC
// =========================================================================================
//  BACKTEST QUANTITATIVO: PHANTOM LIQUIDITY ENGINE (PLE) - AGOSTO COMPLETO (31 DIAS)
//  Objetivo: Explorar os 70% de Falsos Rompimentos (Fade Bull/Bear Traps) e os 30% de Retestes
// =========================================================================================

use anyhow::Result;
use binance_futures_bot::liquidity_trap::{LiquidityTrapConfig, PhantomLiquidityEngine};
use binance_futures_bot::types::{Candle, Position, PositionSide, SignalType};
use chrono::{DateTime, Utc};

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
    long_trades: usize,
    short_trades: usize,
    tp_partials: usize,
    tp_finals: usize,
    breakevens: usize,
    stop_losses: usize,
    trades: Vec<TradeLog>,
}

fn run_trap_backtest(
    candles: &[Candle],
    cfg: LiquidityTrapConfig,
    initial_eur: f64,
    leverage: u32,
    eur_to_usdt: f64,
    warmup_len: usize,
) -> BacktestResult {
    let mut engine = PhantomLiquidityEngine::new(cfg);
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

    let mut long_trades = 0;
    let mut short_trades = 0;
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
                        let notional_usdt = 800.0f64.min(balance_usdt * (leverage as f64) * 0.90);
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
                            long_trades += 1;
                        }
                    }
                }
                SignalType::OpenShort => {
                    if current_pos.side == PositionSide::None {
                        let fill_price = sig.price * (1.0 - slip_rate);
                        let balance_usdt = balance_eur * eur_to_usdt;
                        let notional_usdt = 800.0f64.min(balance_usdt * (leverage as f64) * 0.90);
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
                            short_trades += 1;
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
                                side: "LONG (PARCIAL 70%)".to_string(),
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
                                side: "SHORT (PARCIAL 70%)".to_string(),
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

                        let is_be = sig.reason.contains("Breakeven") || (fill_price >= current_pos.entry_price && !sig.reason.contains("Final"));
                        let is_tp_final = sig.reason.contains("Final");
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
                            reason: sig.reason,
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

                        let is_be = sig.reason.contains("Breakeven") || (fill_price <= current_pos.entry_price && !sig.reason.contains("Final"));
                        let is_tp_final = sig.reason.contains("Final");
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
                            reason: sig.reason,
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
        long_trades,
        short_trades,
        tp_partials,
        tp_finals,
        breakevens,
        stop_losses,
        trades,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("================================================================================");
    println!("   BACKTEST QUANTITATIVO: PHANTOM LIQUIDITY ENGINE (PLE)");
    println!("   CAÇADOR DE ARMADILHAS DE BANDA (FADE BULL/BEAR TRAP) & RETESTES (5m)");
    println!("================================================================================");

    // Carrega o cache verificado de Agosto
    let cache_file = "data/cache_ETHUSDT_1785369600000_1788220799999.json";
    let data = std::fs::read_to_string(cache_file)
        .expect("Cache de Agosto deve existir em data/");
    let candles: Vec<Candle> = serde_json::from_str(&data)?;

    println!("  Cache de Agosto carregado com sucesso: {} velas de 5m", candles.len());
    let warmup_candles = 576;
    let initial_eur = 200.0;
    let leverage = 5;
    let eur_to_usdt = 1.08;

    let mut cfg = LiquidityTrapConfig::default();
    cfg.sl_offset_pct = 0.10;
    cfg.max_sl_pct = 0.55;
    cfg.min_wick_ratio = 0.20;
    cfg.min_nw_spread = 0.0055;
    cfg.tp_partial_ratio = 0.70;

    let res = run_trap_backtest(&candles, cfg, initial_eur, leverage, eur_to_usdt, warmup_candles);

    println!("\n================================================================================");
    println!(" RESULTADO OFICIAL PHANTOM LIQUIDITY ENGINE — AGOSTO COMPLETO (31 DIAS)");
    println!("================================================================================");
    println!("  Banca Inicial: €{:.2}", res.initial_eur);
    println!("  Banca Final:   €{:.2} (${:.2} USDT)", res.final_eur, res.final_eur * eur_to_usdt);
    println!("  Lucro Líquido: €{:+.2} ({:+.2}%)", res.profit_eur, res.total_return_pct);
    println!("  Total Trades:  {} ({} Vitórias / {} Derrotas)", res.total_trades, res.wins, res.losses);
    println!("  Taxa Acerto:   {:.1}%", res.win_rate);
    println!("  Profit Factor: {:.2}", res.profit_factor);
    println!("  Max Drawdown:  {:.2}%", res.max_drawdown_pct);
    println!("  Desfechos:     {} TP Parcial (70%) | {} TP Final | {} Breakeven | {} Stop Loss\n",
        res.tp_partials, res.tp_finals, res.breakevens, res.stop_losses);

    println!("==========================================================================================================");
    println!(" EXTRATO TRADE A TRADE PHANTOM LIQUIDITY (Fade de Armadilhas e Retestes em 5m):");
    println!("==========================================================================================================");
    println!("  #  | DATA/HORA (UTC) | OPERAÇÃO / TIPO     | ENTRADA   -> SAÍDA    | PNL BRUTO (€) | TAXAS (€) | PNL LÍQ (€)  (%) | BANCA ATUAL | MOTIVO");
    println!("-----+-----------------+---------------------+-----------------------+---------------+-----------+------------------+-------------+---------------------------------------");
    for (i, t) in res.trades.iter().enumerate() {
        let dt = DateTime::<Utc>::from_timestamp_millis(t.timestamp)
            .map(|d| d.format("%d/%m %H:%M").to_string())
            .unwrap_or_else(|| "--/-- --:--".to_string());
        println!(
            " {:02}  | {}    | {:<19} | ${:<8.2} -> ${:<8.2} | {:+7.2}€    | {:5.2}€   | {:+7.2}€ ({:+6.2}%) | €{:<9.2} | {}",
            i + 1,
            dt,
            t.side,
            t.entry_price,
            t.exit_price,
            t.gross_pnl_eur,
            t.fee_eur,
            t.net_pnl_eur,
            t.return_pct,
            t.balance_after_eur,
            t.reason
        );
    }
    println!("==========================================================================================================\n");

    Ok(())
}
