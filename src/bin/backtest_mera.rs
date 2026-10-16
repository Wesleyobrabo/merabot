#![allow(dead_code, unused_variables, unused_imports, unused_assignments)]
// language: Rust, file: src/bin/backtest_mera.rs, target: Windows 11, MSVC
// Backtest Real Binance Futures USD-M: Motor MERA (Session VWAP + Nadaraya-Watson Envelope 5m)
// Período: Mês de Abril Inteiro (01/04 a 30/04 - 30 Dias Contínuos de Mercado)
// Banca Inicial: €20,00 (~$21,60 USDT) | Par: ETHUSDT Perpétuo | Alavancagem: 1x, 3x e 5x

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

async fn fetch_month_candles(
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
        tokio::time::sleep(tokio::time::Duration::from_millis(80)).await;
    }
    println!(" Concluído!");

    let _ = std::fs::create_dir_all(&cache_dir);
    if let Ok(serialized) = serde_json::to_string(&candles) {
        let _ = std::fs::write(&cache_file, serialized);
    }

    Ok(candles)
}

fn run_backtest(
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
                        let size_mult = sig.size_factor.unwrap_or(1.0);
                        let notional_usdt = if leverage == 5 && initial_eur >= 100.0 {
                            800.0f64.min(balance_usdt * (leverage as f64) * 0.90) * size_mult
                        } else {
                            balance_usdt.max(5.50) * (leverage as f64) * size_mult
                        };
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
                        let size_mult = sig.size_factor.unwrap_or(1.0);
                        let notional_usdt = if leverage == 5 && initial_eur >= 100.0 {
                            800.0f64.min(balance_usdt * (leverage as f64) * 0.90) * size_mult
                        } else {
                            balance_usdt.max(5.50) * (leverage as f64) * size_mult
                        };
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
                        let p_ratio = sig.partial_ratio.unwrap_or(0.70);
                        let close_qty = (current_pos.amount * p_ratio * 1000.0).floor() / 1000.0;
                        let exit_price = sig.price * (1.0 - slip_rate);
                        let gross_pnl_usdt = (exit_price - current_pos.entry_price) * close_qty;
                        let close_fee_usdt = (close_qty * exit_price) * fee_rate;
                        let net_pnl_usdt = gross_pnl_usdt - close_fee_usdt;
                        let net_pnl_eur = net_pnl_usdt / eur_to_usdt;

                        balance_eur += net_pnl_eur;
                        current_pos.amount -= close_qty;
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
                            gross_pnl_eur: gross_pnl_usdt / eur_to_usdt,
                            fee_eur: close_fee_usdt / eur_to_usdt,
                            net_pnl_eur,
                            return_pct: ((exit_price - current_pos.entry_price) / current_pos.entry_price) * 100.0 * (leverage as f64),
                            balance_after_eur: balance_eur,
                            reason: sig.reason,
                        });
                    }
                }
                SignalType::PartialCloseShort => {
                    if current_pos.side == PositionSide::Short && !current_pos.partial_taken {
                        let p_ratio = sig.partial_ratio.unwrap_or(0.70);
                        let close_qty = (current_pos.amount * p_ratio * 1000.0).floor() / 1000.0;
                        let exit_price = sig.price * (1.0 + slip_rate);
                        let gross_pnl_usdt = (current_pos.entry_price - exit_price) * close_qty;
                        let close_fee_usdt = (close_qty * exit_price) * fee_rate;
                        let net_pnl_usdt = gross_pnl_usdt - close_fee_usdt;
                        let net_pnl_eur = net_pnl_usdt / eur_to_usdt;

                        balance_eur += net_pnl_eur;
                        current_pos.amount -= close_qty;
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
                            gross_pnl_eur: gross_pnl_usdt / eur_to_usdt,
                            fee_eur: close_fee_usdt / eur_to_usdt,
                            net_pnl_eur,
                            return_pct: ((current_pos.entry_price - exit_price) / current_pos.entry_price) * 100.0 * (leverage as f64),
                            balance_after_eur: balance_eur,
                            reason: sig.reason,
                        });
                    }
                }
                SignalType::CloseLong => {
                    if current_pos.side == PositionSide::Long {
                        let exit_price = sig.price * (1.0 - slip_rate);
                        let gross_pnl_usdt = (exit_price - current_pos.entry_price) * current_pos.amount;
                        let close_fee_usdt = (current_pos.amount * exit_price) * fee_rate;
                        let net_pnl_usdt = gross_pnl_usdt - close_fee_usdt;
                        let net_pnl_eur = net_pnl_usdt / eur_to_usdt;

                        balance_eur += net_pnl_eur;
                        if sig.reason.contains("TP Final") {
                            tp_finals += 1;
                        } else if sig.reason.contains("Breakeven") {
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
                            gross_pnl_eur: gross_pnl_usdt / eur_to_usdt,
                            fee_eur: close_fee_usdt / eur_to_usdt,
                            net_pnl_eur,
                            return_pct: ((exit_price - current_pos.entry_price) / current_pos.entry_price) * 100.0 * (leverage as f64),
                            balance_after_eur: balance_eur,
                            reason: sig.reason,
                        });

                        current_pos = Position::new("ETHUSDT".to_string(), leverage);
                    }
                }
                SignalType::CloseShort => {
                    if current_pos.side == PositionSide::Short {
                        let exit_price = sig.price * (1.0 + slip_rate);
                        let gross_pnl_usdt = (current_pos.entry_price - exit_price) * current_pos.amount;
                        let close_fee_usdt = (current_pos.amount * exit_price) * fee_rate;
                        let net_pnl_usdt = gross_pnl_usdt - close_fee_usdt;
                        let net_pnl_eur = net_pnl_usdt / eur_to_usdt;

                        balance_eur += net_pnl_eur;
                        if sig.reason.contains("TP Final") {
                            tp_finals += 1;
                        } else if sig.reason.contains("Breakeven") {
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
                            gross_pnl_eur: gross_pnl_usdt / eur_to_usdt,
                            fee_eur: close_fee_usdt / eur_to_usdt,
                            net_pnl_eur,
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

    // Fechamento contábil no último candle do mês caso haja posição aberta
    if current_pos.side != PositionSide::None {
        let last_c = candles.last().unwrap();
        let exit_price = last_c.close;
        let is_long = current_pos.side == PositionSide::Long;
        let gross_pnl_usdt = if is_long {
            (exit_price - current_pos.entry_price) * current_pos.amount
        } else {
            (current_pos.entry_price - exit_price) * current_pos.amount
        };
        let close_fee_usdt = (current_pos.amount * exit_price) * fee_rate;
        let net_pnl_usdt = gross_pnl_usdt - close_fee_usdt;
        let net_pnl_eur = net_pnl_usdt / eur_to_usdt;
        balance_eur += net_pnl_eur;
        trades.push(TradeLog {
            timestamp: last_c.close_time,
            side: format!("{} (FIM DO MÊS)", if is_long { "LONG" } else { "SHORT" }),
            entry_price: current_pos.entry_price,
            exit_price,
            quantity: current_pos.amount,
            gross_pnl_eur: gross_pnl_usdt / eur_to_usdt,
            fee_eur: close_fee_usdt / eur_to_usdt,
            net_pnl_eur,
            return_pct: (gross_pnl_usdt / (current_pos.amount * current_pos.entry_price)) * 100.0 * (leverage as f64),
            balance_after_eur: balance_eur,
            reason: "Fechamento contábil 30/04 23:59 UTC".to_string(),
        });
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
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    let symbol = "ETHUSDT";
    let initial_eur = 20.00;
    let initial_eur_5x = 200.00;
    let eur_to_usdt = 1.08;

    let args: Vec<String> = std::env::args().collect();
    let is_august = args.iter().any(|a| a == "--august" || a == "august" || a == "agosto" || a == "-aug");
    let is_october = args.iter().any(|a| a == "--october" || a == "october" || a == "oct" || a == "outubro");
    let is_april = args.iter().any(|a| a == "--april" || a == "april" || a == "abril" || a == "-a");

    let (month_name, start_sep_ms, end_sep_ms) = if is_august {
        (
            "AGOSTO COMPLETO (31 DIAS)",
            1785542400000i64, // 01/08/2026 00:00:00 UTC
            1788220799999i64, // 31/08/2026 23:59:59 UTC
        )
    } else if is_october {
        (
            "OUTUBRO (01/10 até Agora em Tempo Real)",
            1790812800000i64, // 01/10/2026 00:00:00 UTC
            chrono::Utc::now().timestamp_millis(),
        )
    } else if is_april {
        (
            "ABRIL COMPLETO (30 DIAS)",
            1775001600000i64, // 01/04/2026 00:00:00 UTC
            1777593599000i64, // 30/04/2026 23:59:59 UTC
        )
    } else {
        (
            "SETEMBRO COMPLETO (30 DIAS)",
            1788220800000i64,
            1790812799000i64,
        )
    };

    // 576 candles de aquecimento (48h antes) para que o período comece com o tensor macro calibrado
    let warmup_candles_count = 576;
    let warmup_offset_ms = (warmup_candles_count as i64) * 5 * 60 * 1000;
    let fetch_start_ms = start_sep_ms - warmup_offset_ms;

    println!("================================================================================");
    println!("   BACKTEST OFICIAL MOTOR MERA — MÊS DE {}", month_name);
    println!("================================================================================");
    println!("  PAR:              ETHUSDT Perpétuo (USDⓈ-M)");
    println!("  TIMEFRAME:        5m (Alta Frequência / Scalping Institucional)");
    println!("  PERÍODO:          {}", month_name);
    println!("  BANCA DE TESTE:   €{:.2} (~${:.2} USDT) em 5x (Alvo €5/win) | Ref €20.00", initial_eur_5x, initial_eur_5x * eur_to_usdt);
    println!("  ESTRATÉGIA:       MERA Scalp Blindado (Session VWAP + Nadaraya + MKT 2.0)");
    println!("  PROTEÇÃO:         Spread Mínimo >= 0.55% + ATR >= 0.22% + Quarentena 1h pós-stop");
    println!("  CUSTOS REAIS:     Taker Fee 0.05% + Slippage 0.05% por ponta de execução");
    println!("================================================================================\n");

    let candles = fetch_month_candles(&client, symbol, fetch_start_ms, end_sep_ms).await?;
    let period_candles_count = candles.len().saturating_sub(warmup_candles_count);

    println!(
        "\nTotal de velas carregadas: {} ({} de aquecimento + {} velas oficiais do período)\n",
        candles.len(),
        warmup_candles_count,
        period_candles_count
    );

    println!("--- Avaliando Parâmetros MERA no Mês de {} ---", month_name);
    let mut candidates: Vec<(MeraConfig, BacktestResult)> = Vec::new();

    let tp_list = [0.85, 1.00, 1.20, 1.40];
    let sl_list = [0.50, 0.60, 0.70, 0.85];
    let wick_list = [0.30, 0.38];
    let partial_ratio_list = [0.50, 0.70, 1.00];
    let cooldown_list = [2];
    let spread_list = [0.0045, 0.0055, 0.0065];
    let ema_p_list = [200, 288];

    for &tp in &tp_list {
        for &sl in &sl_list {
            for &wick in &wick_list {
                for &pr in &partial_ratio_list {
                    for &cd in &cooldown_list {
                        for &sp in &spread_list {
                            for &ep in &ema_p_list {
                                let mut test_cfg = MeraConfig::default();
                                test_cfg.tp_partial_percent = tp;
                                test_cfg.sl_percent = sl;
                                test_cfg.min_wick_ratio = wick;
                                test_cfg.partial_ratio = pr;
                                test_cfg.cooldown_candles = cd;
                                test_cfg.min_nw_spread = sp;
                                test_cfg.use_htf_trend = true;
                                test_cfg.ema_trend_period = ep;

                                let res = run_backtest(&candles, test_cfg.clone(), initial_eur, 1, eur_to_usdt, warmup_candles_count);
                                if res.total_trades >= 5 {
                                    candidates.push((test_cfg, res));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    candidates.sort_by(|a, b| b.1.profit_eur.partial_cmp(&a.1.profit_eur).unwrap_or(std::cmp::Ordering::Equal));

    println!("Top 5 Combinações Avaliadas no Período:");
    for (i, (c, r)) in candidates.iter().take(5).enumerate() {
        println!("  #{}: PnL: {:+6.2}€ ({:+.2}%) | WR: {:4.1}% ({}W/{}L) | PF: {:.2} | Trades: {} | TP1: {:.2}% | SL: {:.2}% | Wick: {:.2} | Parcial: {:.0}% | CD: {}",
            i + 1, r.profit_eur, r.total_return_pct, r.win_rate, r.wins, r.losses, r.profit_factor, r.total_trades,
            c.tp_partial_percent, c.sl_percent, c.min_wick_ratio, c.partial_ratio * 100.0, c.cooldown_candles
        );
    }
    println!();

    let cfg = MeraConfig::default();

    let res_1x = run_backtest(&candles, cfg.clone(), initial_eur, 1, eur_to_usdt, warmup_candles_count);
    let res_3x = run_backtest(&candles, cfg.clone(), initial_eur, 3, eur_to_usdt, warmup_candles_count);
    let res_5x = run_backtest(&candles, cfg.clone(), initial_eur, 5, eur_to_usdt, warmup_candles_count);
    let res_5x_200 = run_backtest(&candles, cfg.clone(), initial_eur_5x, 5, eur_to_usdt, warmup_candles_count);

    println!("================================================================================");
    println!(" 1. CONFIGURAÇÃO OFICIAL: 5x ALAVANCAGEM COM BANCA DE €200 (ALVO €5/TRADE)");
    println!("================================================================================");
    println!("  Banca Inicial: €{:.2}", res_5x_200.initial_eur);
    println!("  Banca Final:   €{:.2} (${:.2} USDT)", res_5x_200.final_eur, res_5x_200.final_eur * eur_to_usdt);
    println!("  Lucro Líquido: €{:+.2} ({:+.2}%)", res_5x_200.profit_eur, res_5x_200.total_return_pct);
    println!("  Total Trades:  {} ({} Vitórias / {} Derrotas)", res_5x_200.total_trades, res_5x_200.wins, res_5x_200.losses);
    println!("  Taxa Acerto:   {:.1}%", res_5x_200.win_rate);
    println!("  Profit Factor: {:.2}", res_5x_200.profit_factor);
    println!("  Max Drawdown:  {:.2}%", res_5x_200.max_drawdown_pct);
    println!("  Entradas:      {} Longs | {} Shorts", res_5x_200.long_trades, res_5x_200.short_trades);
    println!("  Desfechos:     {} TP Parcial (70%) | {} TP Final | {} Breakeven | {} Stop Loss\n",
        res_5x_200.tp_partials, res_5x_200.tp_finals, res_5x_200.breakevens, res_5x_200.stop_losses);

    println!("================================================================================");
    println!(" 2. REFERÊNCIA COMPARATIVA - BANCA €20 (1x, 3x, 5x)");
    println!("================================================================================");
    println!("  1x Sem Alavancagem: Final €{:.2} | Lucro {:+.2}€ ({:+.2}%) | WR {:.1}% | DD {:.2}%",
        res_1x.final_eur, res_1x.profit_eur, res_1x.total_return_pct, res_1x.win_rate, res_1x.max_drawdown_pct);
    println!("  3x Moderado:        Final €{:.2} | Lucro {:+.2}€ ({:+.2}%) | WR {:.1}% | DD {:.2}%",
        res_3x.final_eur, res_3x.profit_eur, res_3x.total_return_pct, res_3x.win_rate, res_3x.max_drawdown_pct);
    println!("  5x Agressivo:       Final €{:.2} | Lucro {:+.2}€ ({:+.2}%) | WR {:.1}% | DD {:.2}%\n",
        res_5x.final_eur, res_5x.profit_eur, res_5x.total_return_pct, res_5x.win_rate, res_5x.max_drawdown_pct);

    // Distribuição semanal dos lucros no período
    println!("================================================================================");
    println!(" PROGRESSÃO SEMANAL — {} (Modo 5x / Banca €200)", month_name);
    println!("================================================================================");

    let weeks: Vec<(&str, i64, i64)> = if is_august {
        vec![
            ("Semana 1 (01/08 a 07/08)", start_sep_ms, start_sep_ms + 7 * 86400 * 1000),
            ("Semana 2 (08/08 a 14/08)", start_sep_ms + 7 * 86400 * 1000, start_sep_ms + 14 * 86400 * 1000),
            ("Semana 3 (15/08 a 21/08)", start_sep_ms + 14 * 86400 * 1000, start_sep_ms + 21 * 86400 * 1000),
            ("Semana 4 (22/08 a 28/08)", start_sep_ms + 21 * 86400 * 1000, start_sep_ms + 28 * 86400 * 1000),
            ("Semana 5 (29/08 a 31/08)", start_sep_ms + 28 * 86400 * 1000, end_sep_ms + 1000),
        ]
    } else if is_october {
        vec![
            ("Semana 1 (01/10 a 07/10)", start_sep_ms, start_sep_ms + 7 * 86400 * 1000),
            ("Semana 2 (08/10 a 14/10)", start_sep_ms + 7 * 86400 * 1000, start_sep_ms + 14 * 86400 * 1000),
            ("Semana 3 (15/10 - Atual)", start_sep_ms + 14 * 86400 * 1000, end_sep_ms + 1000),
        ]
    } else if is_april {
        vec![
            ("Semana 1 (01/04 a 07/04)", start_sep_ms, start_sep_ms + 7 * 86400 * 1000),
            ("Semana 2 (08/04 a 14/04)", start_sep_ms + 7 * 86400 * 1000, start_sep_ms + 14 * 86400 * 1000),
            ("Semana 3 (15/04 a 21/04)", start_sep_ms + 14 * 86400 * 1000, start_sep_ms + 21 * 86400 * 1000),
            ("Semana 4 (22/04 a 28/04)", start_sep_ms + 21 * 86400 * 1000, start_sep_ms + 28 * 86400 * 1000),
            ("Semana 5 (29/04 a 30/04)", start_sep_ms + 28 * 86400 * 1000, end_sep_ms + 1000),
        ]
    } else {
        vec![
            ("Semana 1 (01/09 a 07/09)", start_sep_ms, start_sep_ms + 7 * 86400 * 1000),
            ("Semana 2 (08/09 a 14/09)", start_sep_ms + 7 * 86400 * 1000, start_sep_ms + 14 * 86400 * 1000),
            ("Semana 3 (15/09 a 21/09)", start_sep_ms + 14 * 86400 * 1000, start_sep_ms + 21 * 86400 * 1000),
            ("Semana 4 (22/09 a 28/09)", start_sep_ms + 21 * 86400 * 1000, start_sep_ms + 28 * 86400 * 1000),
            ("Semana 5 (29/09 a 30/09)", start_sep_ms + 28 * 86400 * 1000, end_sep_ms + 1000),
        ]
    };

    for (w_name, w_start, w_end) in weeks {
        let w_trades: Vec<&TradeLog> = res_5x_200.trades.iter().filter(|t| t.timestamp >= w_start && t.timestamp < w_end).collect();
        let w_pnl: f64 = w_trades.iter().map(|t| t.net_pnl_eur).sum();
        let w_wins = w_trades.iter().filter(|t| t.net_pnl_eur > 0.0).count();
        let w_losses = w_trades.len().saturating_sub(w_wins);
        let w_wr = if !w_trades.is_empty() { (w_wins as f64 / w_trades.len() as f64) * 100.0 } else { 0.0 };
        println!("  {:<26} | PnL: {:+6.2}€ | Trades: {:2} ({} W / {} L) | WR: {:4.1}%",
            w_name, w_pnl, w_trades.len(), w_wins, w_losses, w_wr
        );
    }

    let total_gross_wins: f64 = res_5x_200.trades.iter().filter(|t| t.gross_pnl_eur > 0.0).map(|t| t.gross_pnl_eur).sum();
    let total_gross_losses: f64 = res_5x_200.trades.iter().filter(|t| t.gross_pnl_eur < 0.0).map(|t| t.gross_pnl_eur.abs()).sum();
    let total_fees: f64 = res_5x_200.trades.iter().map(|t| t.fee_eur).sum();

    println!("==========================================================================================================");
    println!(" EXTRATO DETALHADO TRADE A TRADE — {} (Motor MERA Blindado / 5x / €200):", month_name);
    println!("==========================================================================================================");
    println!("  #  | DATA/HORA (UTC) | OPERAÇÃO / TIPO     | ENTRADA   -> SAÍDA    | PNL BRUTO (€) | TAXAS (€) | PNL LÍQ (€)  (%) | BANCA ATUAL | MOTIVO");
    println!("-----+-----------------+---------------------+-----------------------+---------------+-----------+------------------+-------------+---------------------------------------");
    for (i, t) in res_5x_200.trades.iter().enumerate() {
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
    println!("==========================================================================================================");
    println!(" RESUMO FINANCEIRO CONSOLIDADO ({}):", month_name);
    println!("  * Lucro Bruto (Vitórias):     +€{:.2}", total_gross_wins);
    println!("  * Prejuízo Bruto (Stops):     -€{:.2}", total_gross_losses);
    println!("  * Taxas / Slippage Totais:     €{:.2}", total_fees);
    println!("  * PnL Líquido Final:          {:+.2}€ ({:+.2}%)", res_5x_200.profit_eur, res_5x_200.total_return_pct);
    println!("  * Banca Final:                 €{:.2}", res_5x_200.final_eur);
    println!("==========================================================================================================\n");

    Ok(())
}
