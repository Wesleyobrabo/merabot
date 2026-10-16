// language: Rust, file: src/bin/test_slow_drift_vwap.rs, target: Windows 11, MSVC
use anyhow::Result;
use binance_futures_bot::config::AppConfig;
use binance_futures_bot::indicators::Ema;
use binance_futures_bot::mera::MeraEngine;
use binance_futures_bot::types::{Candle, Position, PositionSide, SignalType};

#[derive(Debug, Clone)]
struct TradeLog {
    net_pnl_eur: f64,
}

fn simulate_slow_drift_ema20(
    candles: &[Candle],
    warmup_len: usize,
    min_consecutive_1h_candles: usize,
    slope_threshold: f64,
    eur_to_usdt: f64,
    initial_eur: f64,
    leverage: u32,
) -> (f64, usize, usize, f64, usize, usize) {
    let mut config = AppConfig::load_or_create("config.toml").unwrap().mera;
    config.use_hybrid = true;
    config.enable_drift_filter = false;

    let mut engine = MeraEngine::new(config);
    engine.warmup(&candles[..warmup_len]);

    let mut ema20_1h = Ema::new(20);
    let mut chunk_1h: Vec<Candle> = Vec::new();
    let mut history_1h_ema20: Vec<f64> = Vec::new();
    let mut consecutive_1h_below_ema = 0;
    let mut consecutive_1h_above_ema = 0;

    // Warmup
    for c in &candles[..warmup_len] {
        chunk_1h.push(c.clone());
        if chunk_1h.len() == 12 {
            let close_1h = chunk_1h[11].close;
            chunk_1h.clear();
            if let Some(ema) = ema20_1h.update(close_1h) {
                history_1h_ema20.push(ema);
                if close_1h < ema {
                    consecutive_1h_below_ema += 1;
                    consecutive_1h_above_ema = 0;
                } else if close_1h > ema {
                    consecutive_1h_above_ema += 1;
                    consecutive_1h_below_ema = 0;
                } else {
                    consecutive_1h_below_ema = 0;
                    consecutive_1h_above_ema = 0;
                }
            }
        }
    }

    let mut balance_eur = initial_eur;
    let mut trades = Vec::new();
    let mut pos = Position::new("ETHUSDT".to_string(), leverage);
    let fee_rate = 0.0005;
    let slip_rate = 0.0005;

    let mut vetoed_longs = 0;
    let mut vetoed_shorts = 0;

    for candle in &candles[warmup_len..] {
        chunk_1h.push(candle.clone());
        if chunk_1h.len() == 12 {
            let close_1h = chunk_1h[11].close;
            chunk_1h.clear();
            if let Some(ema) = ema20_1h.update(close_1h) {
                history_1h_ema20.push(ema);
                if history_1h_ema20.len() > 100 {
                    history_1h_ema20.remove(0);
                }
                if close_1h < ema {
                    consecutive_1h_below_ema += 1;
                    consecutive_1h_above_ema = 0;
                } else if close_1h > ema {
                    consecutive_1h_above_ema += 1;
                    consecutive_1h_below_ema = 0;
                } else {
                    consecutive_1h_below_ema = 0;
                    consecutive_1h_above_ema = 0;
                }
            }
        }

        let n_ema = history_1h_ema20.len();
        let ema_slope_12h = if n_ema >= 12 {
            let past = history_1h_ema20[n_ema - 12];
            let curr = *history_1h_ema20.last().unwrap();
            if past > 0.0 { ((curr - past) / past) * 100.0 } else { 0.0 }
        } else if n_ema >= 4 {
            let past = history_1h_ema20[0];
            let curr = *history_1h_ema20.last().unwrap();
            if past > 0.0 { ((curr - past) / past) * 100.0 } else { 0.0 }
        } else {
            0.0
        };

        // Regra: se a EMA 20 de 1h estiver inclinada para baixo e o preço fechando abaixo dela por N horas consecutivas:
        let veto_long = ema_slope_12h <= -slope_threshold && consecutive_1h_below_ema >= min_consecutive_1h_candles;
        let veto_short = ema_slope_12h >= slope_threshold && consecutive_1h_above_ema >= min_consecutive_1h_candles;

        let (_telem, signal_opt) = engine.update(candle, &pos);

        if let Some(sig) = signal_opt {
            match sig.signal_type {
                SignalType::OpenLong => {
                    if veto_long {
                        vetoed_longs += 1;
                        continue;
                    }
                    if pos.side == PositionSide::None {
                        let fill_price = sig.price * (1.0 + slip_rate);
                        let balance_usdt = balance_eur * eur_to_usdt;
                        let risk_eur = balance_eur * 0.020;
                        let sl_dist_pct = sig.stop_loss.map(|sl| ((fill_price - sl) / fill_price).abs() * 100.0).unwrap_or(1.25);
                        let target_notional = ((risk_eur * eur_to_usdt) / (sl_dist_pct / 100.0))
                            .min(balance_usdt * (leverage as f64) * 0.90)
                            .min(800.0);
                        let mut qty = target_notional / fill_price;
                        qty = (qty * 1000.0).floor() / 1000.0;
                        if qty >= 0.001 {
                            balance_eur -= (qty * fill_price * fee_rate) / eur_to_usdt;
                            pos.side = PositionSide::Long;
                            pos.entry_price = fill_price;
                            pos.amount = qty;
                            pos.initial_amount = qty;
                            pos.stop_loss = sig.stop_loss;
                            pos.take_profit = sig.take_profit;
                            pos.tp_partial = sig.tp_partial;
                            pos.partial_taken = false;
                            pos.be_locked = false;
                        }
                    }
                }
                SignalType::OpenShort => {
                    if veto_short {
                        vetoed_shorts += 1;
                        continue;
                    }
                    if pos.side == PositionSide::None {
                        let fill_price = sig.price * (1.0 - slip_rate);
                        let balance_usdt = balance_eur * eur_to_usdt;
                        let risk_eur = balance_eur * 0.020;
                        let sl_dist_pct = sig.stop_loss.map(|sl| ((sl - fill_price) / fill_price).abs() * 100.0).unwrap_or(1.25);
                        let target_notional = ((risk_eur * eur_to_usdt) / (sl_dist_pct / 100.0))
                            .min(balance_usdt * (leverage as f64) * 0.90)
                            .min(800.0);
                        let mut qty = target_notional / fill_price;
                        qty = (qty * 1000.0).floor() / 1000.0;
                        if qty >= 0.001 {
                            balance_eur -= (qty * fill_price * fee_rate) / eur_to_usdt;
                            pos.side = PositionSide::Short;
                            pos.entry_price = fill_price;
                            pos.amount = qty;
                            pos.initial_amount = qty;
                            pos.stop_loss = sig.stop_loss;
                            pos.take_profit = sig.take_profit;
                            pos.tp_partial = sig.tp_partial;
                            pos.partial_taken = false;
                            pos.be_locked = false;
                        }
                    }
                }
                SignalType::PartialCloseLong => {
                    if pos.side == PositionSide::Long && !pos.partial_taken && pos.amount >= 0.001 {
                        let p_ratio = sig.partial_ratio.unwrap_or(0.70);
                        let mut close_qty = (pos.initial_amount * p_ratio * 1000.0).floor() / 1000.0;
                        close_qty = close_qty.min(pos.amount);
                        if close_qty >= 0.001 {
                            let exit_price = sig.price * (1.0 - slip_rate);
                            let gross_usdt = (exit_price - pos.entry_price) * close_qty;
                            let fee_usdt = (close_qty * exit_price) * fee_rate;
                            balance_eur += (gross_usdt - fee_usdt) / eur_to_usdt;
                            pos.amount = ((pos.amount - close_qty) * 1000.0).round() / 1000.0;
                            pos.partial_taken = true;
                            pos.be_locked = true;
                            pos.stop_loss = sig.stop_loss;
                        }
                    }
                }
                SignalType::PartialCloseShort => {
                    if pos.side == PositionSide::Short && !pos.partial_taken && pos.amount >= 0.001 {
                        let p_ratio = sig.partial_ratio.unwrap_or(0.70);
                        let mut close_qty = (pos.initial_amount * p_ratio * 1000.0).floor() / 1000.0;
                        close_qty = close_qty.min(pos.amount);
                        if close_qty >= 0.001 {
                            let exit_price = sig.price * (1.0 + slip_rate);
                            let gross_usdt = (pos.entry_price - exit_price) * close_qty;
                            let fee_usdt = (close_qty * exit_price) * fee_rate;
                            balance_eur += (gross_usdt - fee_usdt) / eur_to_usdt;
                            pos.amount = ((pos.amount - close_qty) * 1000.0).round() / 1000.0;
                            pos.partial_taken = true;
                            pos.be_locked = true;
                            pos.stop_loss = sig.stop_loss;
                        }
                    }
                }
                SignalType::CloseLong => {
                    if pos.side == PositionSide::Long && pos.amount >= 0.001 {
                        let exit_price = sig.price * (1.0 - slip_rate);
                        let gross_usdt = (exit_price - pos.entry_price) * pos.amount;
                        let fee_usdt = (pos.amount * exit_price) * fee_rate;
                        let net_eur = (gross_usdt - fee_usdt) / eur_to_usdt;
                        balance_eur += net_eur;
                        trades.push(TradeLog { net_pnl_eur: net_eur });
                        pos = Position::new("ETHUSDT".to_string(), leverage);
                    }
                }
                SignalType::CloseShort => {
                    if pos.side == PositionSide::Short && pos.amount >= 0.001 {
                        let exit_price = sig.price * (1.0 + slip_rate);
                        let gross_usdt = (pos.entry_price - exit_price) * pos.amount;
                        let fee_usdt = (pos.amount * exit_price) * fee_rate;
                        let net_eur = (gross_usdt - fee_usdt) / eur_to_usdt;
                        balance_eur += net_eur;
                        trades.push(TradeLog { net_pnl_eur: net_eur });
                        pos = Position::new("ETHUSDT".to_string(), leverage);
                    }
                }
                _ => {}
            }
        }
    }

    let profit_eur = balance_eur - initial_eur;
    let wins = trades.iter().filter(|t| t.net_pnl_eur > 0.0).count();
    let losses = trades.iter().filter(|t| t.net_pnl_eur <= 0.0).count();
    let win_rate = if !trades.is_empty() { (wins as f64 / trades.len() as f64) * 100.0 } else { 0.0 };

    (profit_eur, wins, losses, win_rate, vetoed_longs, vetoed_shorts)
}

#[tokio::main]
async fn main() -> Result<()> {
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

    let warmup_candles_5m = 576;
    let warmup_offset_ms = (warmup_candles_5m as i64) * 5 * 60 * 1000;

    println!("==========================================================================================================");
    println!(" TESTANDO SLOW DRIFT: EMA 20 1h SLOPE 12h + CONSECUTIVAS VELAS 1h ABAIXO DA EMA 20");
    println!("==========================================================================================================");

    for hours in [3, 4, 6, 8, 12] {
        for slope in [0.00, 0.02, 0.04] {
            let mut total_pnl = 0.0;
            let mut total_wins = 0;
            let mut total_losses = 0;
            let mut total_vl = 0;
            let mut total_vs = 0;

            for (_name, start, end) in &months {
                let fetch_start = start - warmup_offset_ms;
                let cache_file = format!("data/cache_ETHUSDT_{}_{}.json", fetch_start, end);
                let data = std::fs::read_to_string(&cache_file)?;
                let candles: Vec<Candle> = serde_json::from_str(&data)?;

                let (pnl, wins, losses, _, vl, vs) = simulate_slow_drift_ema20(&candles, warmup_candles_5m, hours, slope, 1.08, 200.0, 5);
                total_pnl += pnl;
                total_wins += wins;
                total_losses += losses;
                total_vl += vl;
                total_vs += vs;
            }

            let total_ops = total_wins + total_losses;
            let avg_wr = if total_ops > 0 { (total_wins as f64 / total_ops as f64) * 100.0 } else { 0.0 };
            println!("Velas 1h Abaixo EMA20: {:2}h | Slope Min: {:4.2}% | PnL Total: {:+7.2}€ ({:+5.2}%) | WR: {:4.1}% ({}W/{}L) | Vetos: L={} / S={}",
                hours, slope, total_pnl, (total_pnl / 200.0) * 100.0, avg_wr, total_wins, total_losses, total_vl, total_vs
            );
        }
    }

    Ok(())
}
