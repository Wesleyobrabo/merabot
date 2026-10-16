#![allow(dead_code, unused_variables, unused_imports, unused_assignments)]
// language: Rust, file: src/bin/optimize_mkt_ple.rs, target: Windows 11, MSVC

use anyhow::Result;
use binance_futures_bot::config::MeraConfig;
use binance_futures_bot::liquidity_trap::{LiquidityTrapConfig, PhantomLiquidityEngine};
use binance_futures_bot::types::{Candle, Position, PositionSide, SignalType};

#[derive(Debug, Clone)]
struct SimResult {
    total_trades: usize,
    wins: usize,
    losses: usize,
    win_rate: f64,
    profit_eur: f64,
    return_pct: f64,
    profit_factor: f64,
    max_dd_pct: f64,
}

fn simulate(
    candles: &[Candle],
    sl_pct: f64,
    tp1_pct: f64,
    tp2_pct: f64,
    min_spread: f64,
    min_wick: f64,
    fade_only_macro_aligned: bool,
    retest_only_macro_aligned: bool,
    be_early_trigger_pct: f64,
) -> SimResult {
    let mut ple_cfg = LiquidityTrapConfig::default();
    ple_cfg.min_nw_spread = min_spread;
    ple_cfg.min_wick_ratio = min_wick;
    ple_cfg.sl_offset_pct = sl_pct;
    ple_cfg.max_sl_pct = sl_pct * 1.5;
    ple_cfg.enable_fade_traps = true;
    ple_cfg.enable_retest_trend = true;
    ple_cfg.use_trend_filter = fade_only_macro_aligned;

    let mut engine = PhantomLiquidityEngine::new(ple_cfg);
    let warmup = 576.min(candles.len() / 4);
    if warmup > 0 {
        engine.warmup(&candles[..warmup]);
    }

    let initial_eur = 200.0;
    let leverage = 5.0;
    let eur_to_usdt = 1.08;
    let fee_rate = 0.0005; // 0.05%
    let slip_rate = 0.0005; // 0.05%

    let mut balance_eur = initial_eur;
    let mut peak_eur = initial_eur;
    let mut max_dd = 0.0;
    let mut wins = 0;
    let mut losses = 0;
    let mut gross_wins = 0.0;
    let mut gross_losses = 0.0;
    let mut total_trades = 0;

    let mut pos_side = PositionSide::None;
    let mut pos_entry = 0.0;
    let mut pos_qty = 0.0;
    let mut pos_initial_qty = 0.0;
    let mut pos_sl = 0.0;
    let mut pos_tp1 = 0.0;
    let mut pos_tp2 = 0.0;
    let mut pos_partial_taken = false;
    let mut pos_be_locked = false;

    // Moving EMA 200 for macro alignment
    let mut ema_200 = candles[0].close;
    let alpha_200 = 2.0 / 201.0;

    for candle in &candles[warmup..] {
        ema_200 = alpha_200 * candle.close + (1.0 - alpha_200) * ema_200;

        let mut cur_pos = Position::new("ETHUSDT".to_string(), 5);
        cur_pos.side = pos_side;
        cur_pos.entry_price = pos_entry;
        cur_pos.amount = pos_qty;
        cur_pos.initial_amount = pos_initial_qty;
        cur_pos.stop_loss = Some(pos_sl);
        cur_pos.take_profit = Some(pos_tp2);
        cur_pos.tp_partial = Some(pos_tp1);
        cur_pos.partial_taken = pos_partial_taken;
        cur_pos.be_locked = pos_be_locked;

        // 1. Gerenciamento de Posição Aberta
        if pos_side == PositionSide::Long {
            // A. Early Breakeven Lock
            if !pos_be_locked && be_early_trigger_pct > 0.0 {
                let gain_pct = (candle.high - pos_entry) / pos_entry;
                if gain_pct >= be_early_trigger_pct / 100.0 {
                    pos_be_locked = true;
                    pos_sl = pos_sl.max(pos_entry * 1.0005);
                }
            }

            // B. Take Profit 1 Parcial
            if !pos_partial_taken && candle.high >= pos_tp1 {
                let fill = pos_tp1 * (1.0 - slip_rate);
                let close_qty = (pos_initial_qty * 0.70 * 1000.0).floor() / 1000.0;
                let gross_usdt = (fill - pos_entry) * close_qty;
                let fee_usdt = close_qty * fill * fee_rate;
                let net_eur = (gross_usdt - fee_usdt) / eur_to_usdt;
                balance_eur += net_eur;
                if net_eur > 0.0 { gross_wins += net_eur; } else { gross_losses += net_eur.abs(); }

                pos_qty -= close_qty;
                pos_partial_taken = true;
                pos_be_locked = true;
                pos_sl = pos_sl.max(pos_entry * 1.0005);
            }

            // C. Take Profit 2 Final
            if candle.high >= pos_tp2 {
                let fill = pos_tp2 * (1.0 - slip_rate);
                let gross_usdt = (fill - pos_entry) * pos_qty;
                let fee_usdt = pos_qty * fill * fee_rate;
                let net_eur = (gross_usdt - fee_usdt) / eur_to_usdt;
                balance_eur += net_eur;
                if net_eur > 0.0 { gross_wins += net_eur; wins += 1; } else { gross_losses += net_eur.abs(); losses += 1; }
                total_trades += 1;
                pos_side = PositionSide::None;
            } else if candle.low <= pos_sl {
                // D. Stop Loss / Breakeven Stop
                let fill = pos_sl * (1.0 - slip_rate);
                let gross_usdt = (fill - pos_entry) * pos_qty;
                let fee_usdt = pos_qty * fill * fee_rate;
                let net_eur = (gross_usdt - fee_usdt) / eur_to_usdt;
                balance_eur += net_eur;
                if net_eur > 0.0 { gross_wins += net_eur; wins += 1; } else { gross_losses += net_eur.abs(); losses += 1; }
                total_trades += 1;
                pos_side = PositionSide::None;
            }
        } else if pos_side == PositionSide::Short {
            // A. Early Breakeven Lock
            if !pos_be_locked && be_early_trigger_pct > 0.0 {
                let gain_pct = (pos_entry - candle.low) / pos_entry;
                if gain_pct >= be_early_trigger_pct / 100.0 {
                    pos_be_locked = true;
                    pos_sl = pos_sl.min(pos_entry * 0.9995);
                }
            }

            // B. Take Profit 1 Parcial
            if !pos_partial_taken && candle.low <= pos_tp1 {
                let fill = pos_tp1 * (1.0 + slip_rate);
                let close_qty = (pos_initial_qty * 0.70 * 1000.0).floor() / 1000.0;
                let gross_usdt = (pos_entry - fill) * close_qty;
                let fee_usdt = close_qty * fill * fee_rate;
                let net_eur = (gross_usdt - fee_usdt) / eur_to_usdt;
                balance_eur += net_eur;
                if net_eur > 0.0 { gross_wins += net_eur; } else { gross_losses += net_eur.abs(); }

                pos_qty -= close_qty;
                pos_partial_taken = true;
                pos_be_locked = true;
                pos_sl = pos_sl.min(pos_entry * 0.9995);
            }

            // C. Take Profit 2 Final
            if candle.low <= pos_tp2 {
                let fill = pos_tp2 * (1.0 + slip_rate);
                let gross_usdt = (pos_entry - fill) * pos_qty;
                let fee_usdt = pos_qty * fill * fee_rate;
                let net_eur = (gross_usdt - fee_usdt) / eur_to_usdt;
                balance_eur += net_eur;
                if net_eur > 0.0 { gross_wins += net_eur; wins += 1; } else { gross_losses += net_eur.abs(); losses += 1; }
                total_trades += 1;
                pos_side = PositionSide::None;
            } else if candle.high >= pos_sl {
                // D. Stop Loss / Breakeven Stop
                let fill = pos_sl * (1.0 + slip_rate);
                let gross_usdt = (pos_entry - fill) * pos_qty;
                let fee_usdt = pos_qty * fill * fee_rate;
                let net_eur = (gross_usdt - fee_usdt) / eur_to_usdt;
                balance_eur += net_eur;
                if net_eur > 0.0 { gross_wins += net_eur; wins += 1; } else { gross_losses += net_eur.abs(); losses += 1; }
                total_trades += 1;
                pos_side = PositionSide::None;
            }
        }

        // Drawdown Tracking
        if balance_eur > peak_eur { peak_eur = balance_eur; }
        let dd = if peak_eur > 0.0 { ((peak_eur - balance_eur) / peak_eur) * 100.0 } else { 0.0 };
        if dd > max_dd { max_dd = dd; }

        // 2. Novos Gatilhos do PLE
        if pos_side == PositionSide::None && candle.is_closed {
            let (_telem, sig_opt) = engine.update(candle, &cur_pos);
            if let Some(sig) = sig_opt {
                let is_fade = sig.reason.contains("Fade");
                let is_retest = sig.reason.contains("Reteste");

                let macro_bull = candle.close >= ema_200;
                let macro_bear = candle.close <= ema_200;

                let allowed = match sig.signal_type {
                    SignalType::OpenLong => {
                        if is_fade && fade_only_macro_aligned && !macro_bull { false }
                        else if is_retest && retest_only_macro_aligned && !macro_bull { false }
                        else { true }
                    }
                    SignalType::OpenShort => {
                        if is_fade && fade_only_macro_aligned && !macro_bear { false }
                        else if is_retest && retest_only_macro_aligned && !macro_bear { false }
                        else { true }
                    }
                    _ => false,
                };

                if allowed {
                    match sig.signal_type {
                        SignalType::OpenLong => {
                            let fill = sig.price * (1.0 + slip_rate);
                            let notional = 800.0f64.min(balance_eur * eur_to_usdt * leverage * 0.90);
                            let qty = ((notional / fill) * 1000.0).floor() / 1000.0;
                            if qty >= 0.001 {
                                let open_fee = qty * fill * fee_rate;
                                balance_eur -= open_fee / eur_to_usdt;

                                pos_side = PositionSide::Long;
                                pos_entry = fill;
                                pos_qty = qty;
                                pos_initial_qty = qty;
                                pos_sl = fill * (1.0 - sl_pct / 100.0);
                                pos_tp1 = fill * (1.0 + tp1_pct / 100.0);
                                pos_tp2 = fill * (1.0 + tp2_pct / 100.0);
                                pos_partial_taken = false;
                                pos_be_locked = false;
                            }
                        }
                        SignalType::OpenShort => {
                            let fill = sig.price * (1.0 - slip_rate);
                            let notional = 800.0f64.min(balance_eur * eur_to_usdt * leverage * 0.90);
                            let qty = ((notional / fill) * 1000.0).floor() / 1000.0;
                            if qty >= 0.001 {
                                let open_fee = qty * fill * fee_rate;
                                balance_eur -= open_fee / eur_to_usdt;

                                pos_side = PositionSide::Short;
                                pos_entry = fill;
                                pos_qty = qty;
                                pos_initial_qty = qty;
                                pos_sl = fill * (1.0 + sl_pct / 100.0);
                                pos_tp1 = fill * (1.0 - tp1_pct / 100.0);
                                pos_tp2 = fill * (1.0 - tp2_pct / 100.0);
                                pos_partial_taken = false;
                                pos_be_locked = false;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    let profit_eur = balance_eur - initial_eur;
    let win_rate = if total_trades > 0 { (wins as f64 / total_trades as f64) * 100.0 } else { 0.0 };
    let pf = if gross_losses > 0.0 { gross_wins / gross_losses } else { 99.0 };

    SimResult {
        total_trades,
        wins,
        losses,
        win_rate,
        profit_eur,
        return_pct: (profit_eur / initial_eur) * 100.0,
        profit_factor: pf,
        max_dd_pct: max_dd,
    }
}

fn main() -> Result<()> {
    // Carrega os 3 meses cacheados
    let f_aug = "data/cache_ETHUSDT_1785369600000_1788220799999.json";
    let f_sep = "data/cache_ETHUSDT_1788048000000_1790812799999.json";
    let f_oct = "data/cache_ETHUSDT_1790640000000_1792176684688.json";

    let mut all_candles = Vec::new();
    for f in &[f_aug, f_sep, f_oct] {
        if let Ok(data) = std::fs::read_to_string(f) {
            if let Ok(mut c) = serde_json::from_str::<Vec<Candle>>(&data) {
                all_candles.append(&mut c);
            }
        }
    }
    // Remove velas duplicadas
    all_candles.sort_by_key(|c| c.open_time);
    all_candles.dedup_by_key(|c| c.open_time);

    println!("Total de Velas Carregadas no Trimestre: {}", all_candles.len());

    println!("\n{:<6} | {:<7} | {:<7} | {:<8} | {:<7} | {:<6} | {:<12} | {:<8} | {:<6} | {:<7}",
        "SL %", "TP1 %", "TP2 %", "SPREAD", "BE_LOCK", "MACRO", "TRIMESTRE PNL", "RETORNO", "WR %", "PF");
    println!("----------------------------------------------------------------------------------------------------------");

    // Agregação de 5m para 15m (3 velas de 5m por vela de 15m)
    let mut candles_15m = Vec::new();
    let mut chunk: Vec<Candle> = Vec::new();
    for c in &all_candles {
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
    println!("Total de Velas de 15m Geradas no Trimestre: {}", candles_15m.len());

    println!("\n=== TESTE EM TIMEFRAME 15M (MKT + PLE COM VELAS DE 15M) ===");
    println!("{:<6} | {:<7} | {:<7} | {:<8} | {:<6} | {:<14} | {:<8} | {:<6} | {:<7}",
        "SL %", "TP1 %", "TP2 %", "SPREAD", "MACRO", "TRIMESTRE PNL", "RETORNO", "WR %", "PF");
    println!("---------------------------------------------------------------------------------------------------");

    let sl_15m = [0.80, 1.20, 1.50, 1.80];
    let tp1_15m = [1.20, 1.60, 2.20, 3.00];
    let sp_15m = [0.0080, 0.0120, 0.0150];

    for &sl in &sl_15m {
        for &tp1 in &tp1_15m {
            let tp2 = tp1 * 1.8;
            for &sp in &sp_15m {
                for &macro_f in &[false, true] {
                    let res = simulate(&candles_15m, sl, tp1, tp2, sp, 0.20, macro_f, macro_f, 0.0);
                    if res.total_trades >= 5 {
                        println!("{:<6.2} | {:<7.2} | {:<7.2} | {:<8.4} | {:<6} | {:+7.2}€ ({:2}T)   | {:+6.2}%  | {:4.1}% | {:4.2}",
                            sl, tp1, tp2, sp, if macro_f { "SIM" } else { "NAO" },
                            res.profit_eur, res.total_trades, res.return_pct, res.win_rate, res.profit_factor
                        );
                    }
                }
            }
        }
    }

    Ok(())
}
