#![allow(dead_code, unused_variables, unused_imports, unused_assignments)]
use anyhow::Result;
use binance_futures_bot::config::{AppConfig, CascadeConfig};
use binance_futures_bot::execution::ExecutionEngine;
use binance_futures_bot::risk::RiskManager;
use binance_futures_bot::storage::StorageManager;
use binance_futures_bot::strategy::PineSupertrendRsiStrategy;
use binance_futures_bot::types::{Candle, PositionSide, Signal, SignalType};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::env;

#[derive(Debug, Clone)]
struct SimResult {
    name: String,
    symbol: String,
    candles_count: usize,
    trades_count: usize,
    winning_trades: usize,
    losing_trades: usize,
    win_rate: f64,
    initial_balance: f64,
    final_balance: f64,
    net_profit: f64,
    net_profit_pct: f64,
    profit_factor: f64,
    max_drawdown: f64,
    long_trades: usize,
    long_wins: usize,
    short_trades: usize,
    short_wins: usize,
    buy_and_hold_pct: f64,
}

async fn fetch_klines(client: &reqwest::Client, symbol: &str, interval: &str, target_count: usize) -> Result<Vec<Candle>> {
    let mut all_candles: Vec<Candle> = Vec::new();
    let mut end_time: Option<i64> = None;

    let loops = (target_count / 1000) + 1;
    for _ in 0..loops {
        let mut url = format!(
            "https://fapi.binance.com/fapi/v1/klines?symbol={}&interval={}&limit=1000",
            symbol, interval
        );
        if let Some(et) = end_time {
            url.push_str(&format!("&endTime={}", et));
        }

        let resp: Vec<Vec<Value>> = client.get(&url).send().await?.json().await?;
        if resp.is_empty() {
            break;
        }

        let first_open_time = resp[0][0].as_i64().unwrap_or(0);
        end_time = Some(first_open_time - 1);

        let mut chunk = Vec::new();
        for item in resp {
            let open_time = item[0].as_i64().unwrap_or(0);
            let open: f64 = item[1].as_str().unwrap().parse().unwrap();
            let high: f64 = item[2].as_str().unwrap().parse().unwrap();
            let low: f64 = item[3].as_str().unwrap().parse().unwrap();
            let close: f64 = item[4].as_str().unwrap().parse().unwrap();
            let volume: f64 = item[5].as_str().unwrap().parse().unwrap();
            let close_time = item[6].as_i64().unwrap_or(0);

            chunk.push(Candle {
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
        chunk.extend(all_candles);
        all_candles = chunk;

        if all_candles.len() >= target_count {
            break;
        }
    }

    if all_candles.len() > target_count {
        all_candles = all_candles[all_candles.len() - target_count..].to_vec();
    }

    Ok(all_candles)
}

async fn run_simulation(
    name: &str,
    symbol: &str,
    warmup_15m: &[Candle],
    test_15m: &[Candle],
    d1_candles: &[Candle],
    h4_candles: &[Candle],
    cascade_cfg: CascadeConfig,
    base_config: AppConfig,
    initial_balance: f64,
) -> SimResult {
    let temp_storage = StorageManager::new(&format!("data/backtest_{}", name.replace(' ', "_")));
    let mut exec = ExecutionEngine::new_with_storage(base_config.clone(), temp_storage, initial_balance);

    let mut strat_cfg = base_config.strategy.clone();
    strat_cfg.cascade = cascade_cfg.clone();
    let mut strategy = PineSupertrendRsiStrategy::new(strat_cfg, base_config.risk.clone());
    let mut risk = RiskManager::new(base_config.risk.clone());

    // Aquecimento inicial
    strategy.warmup(warmup_15m);
    strategy.warmup_htf(d1_candles.to_vec(), h4_candles.to_vec());

    let mut long_trades = 0;
    let mut long_wins = 0;
    let mut short_trades = 0;
    let mut short_wins = 0;

    for candle in test_15m {
        // 1. Checagem de Stop Loss intrabar
        if exec.position.side != PositionSide::None && exec.position.amount > 0.0 {
            let mut stop_hit = false;
            let mut stop_price = 0.0;

            if let Some(sl) = exec.position.stop_loss {
                if exec.position.side == PositionSide::Long && candle.low <= sl {
                    stop_hit = true;
                    stop_price = sl;
                } else if exec.position.side == PositionSide::Short && candle.high >= sl {
                    stop_hit = true;
                    stop_price = sl;
                }
            }

            if stop_hit {
                let is_long = exec.position.side == PositionSide::Long;
                let sig = Signal {
                    signal_type: if is_long { SignalType::CloseLong } else { SignalType::CloseShort },
                    price: stop_price,
                    timestamp: candle.close_time,
                    reason: format!("Trailing Stop Intrabar em {:.4}", stop_price),
                    stop_loss: None,
                    take_profit: None,
                    partial_ratio: None,
                    tp_partial: None,
                    size_factor: None,
                };
                let _ = exec.process_signal(sig, &mut risk).await;
            }
        }

        // 2. Atualiza estado da vela
        exec.update_candle(candle, &mut risk);

        // 3. Avalia estratégia
        if let Some(sig) = strategy.on_candle(candle, &exec.position) {
            match sig.signal_type {
                SignalType::OpenLong => {
                    long_trades += 1;
                    let _ = exec.process_signal(sig, &mut risk).await;
                }
                SignalType::OpenShort => {
                    short_trades += 1;
                    let _ = exec.process_signal(sig, &mut risk).await;
                }
                SignalType::CloseLong | SignalType::CloseShort => {
                    let _ = exec.process_signal(sig, &mut risk).await;
                }
                _ => {}
            }
        }
    }

    // Fecha posição residual se aberta ao final
    if exec.position.side != PositionSide::None && exec.position.amount > 0.0 {
        if let Some(last_c) = test_15m.last() {
            let is_long = exec.position.side == PositionSide::Long;
            let sig = Signal {
                signal_type: if is_long { SignalType::CloseLong } else { SignalType::CloseShort },
                price: last_c.close,
                timestamp: last_c.close_time,
                reason: "Fechamento Fim do Backtest".to_string(),
                stop_loss: None,
                take_profit: None,
                partial_ratio: None,
                tp_partial: None,
                size_factor: None,
            };
            let _ = exec.process_signal(sig, &mut risk).await;
        }
    }

    // Consolida estatísticas dos trades
    let trades_count = exec.trade_history.len();
    let mut wins = 0;
    let mut losses = 0;
    let mut gross_profit = 0.0;
    let mut gross_loss = 0.0;

    for t in &exec.trade_history {
        let is_win = t.realized_pnl > 0.0;
        if is_win {
            wins += 1;
            gross_profit += t.realized_pnl;
            if t.side == binance_futures_bot::types::OrderSide::Buy {
                short_wins += 1; // Buy fecha Short
            } else {
                long_wins += 1; // Sell fecha Long
            }
        } else {
            losses += 1;
            gross_loss += t.realized_pnl.abs();
        }
    }

    let win_rate = if trades_count > 0 { (wins as f64 / trades_count as f64) * 100.0 } else { 0.0 };
    let profit_factor = if gross_loss > 0.0 { gross_profit / gross_loss } else if gross_profit > 0.0 { 99.9 } else { 0.0 };
    let final_balance = exec.account.balance;
    let net_profit = final_balance - initial_balance;
    let net_profit_pct = (net_profit / initial_balance) * 100.0;

    let first_price = test_15m.first().map(|c| c.open).unwrap_or(1.0);
    let last_price = test_15m.last().map(|c| c.close).unwrap_or(1.0);
    let buy_and_hold_pct = ((last_price - first_price) / first_price) * 100.0;

    SimResult {
        name: name.to_string(),
        symbol: symbol.to_string(),
        candles_count: test_15m.len(),
        trades_count,
        winning_trades: wins,
        losing_trades: losses,
        win_rate,
        initial_balance,
        final_balance,
        net_profit,
        net_profit_pct,
        profit_factor,
        max_drawdown: exec.account.max_drawdown,
        long_trades,
        long_wins,
        short_trades,
        short_wins,
        buy_and_hold_pct,
    }
}

fn print_result_card(res: &SimResult) {
    println!("┌────────────────────────────────────────────────────────────────────────┐");
    println!("│ CENÁRIO: {:<61} │", res.name);
    println!("├────────────────────────────────────────────────────────────────────────┤");
    println!("│ Ativo: {:<12} │ Velas: {:<10} │ Buy & Hold: {:>+19.2}% │", res.symbol, res.candles_count, res.buy_and_hold_pct);
    println!("│ Saldo Inicial: ${:>7.2} │ Saldo Final: ${:>7.2} │ Lucro Líquido: {:>+9.2} ({:>+6.2}%)│", res.initial_balance, res.final_balance, res.net_profit, res.net_profit_pct);
    println!("│ Trades Executados: {:<4}│ Vitórias / Derrotas: {:>2}W / {:<2}L │ Taxa de Acerto: {:>14.1}% │", res.trades_count, res.winning_trades, res.losing_trades, res.win_rate);
    println!("│ Profit Factor: {:>7.2} │ Max Drawdown: {:>7.2}% │ Trades Long / Short: {:>4} / {:<4} │", res.profit_factor, res.max_drawdown, res.long_trades, res.short_trades);
    println!("└────────────────────────────────────────────────────────────────────────┘");
}

#[tokio::main]
async fn main() -> Result<()> {
    let client = reqwest::Client::new();
    let symbol = env::args().nth(1).unwrap_or_else(|| "SOLUSDT".to_string());
    let initial_balance = 100.0;

    println!("==========================================================================");
    println!("  BACKTEST QUANTITATIVO: CASCATA MTF DUPLO & CONFLUÊNCIA BAYESIANA");
    println!("  Ativo: {} | Binance Futures USDⓈ-M Perpétuo", symbol);
    println!("==========================================================================");
    println!("Baixando dados históricos da Binance Futures (15m, 4h e 1D)...");

    // 1500 candles de 15m (~15.6 dias de dados)
    let candles_15m = fetch_klines(&client, &symbol, "15m", 1500).await?;
    let d1_candles = fetch_klines(&client, &symbol, "1d", 120).await?;
    let h4_candles = fetch_klines(&client, &symbol, "4h", 150).await?;

    println!("Dados baixados com sucesso:");
    println!("  • 15m: {} candles", candles_15m.len());
    println!("  • 4h:  {} candles", h4_candles.len());
    println!("  • 1D:  {} candles", d1_candles.len());

    let oct_start_ms = 1790812800000i64; // 01/10/2026 00:00:00 UTC

    let warmup_15m: Vec<Candle> = candles_15m.iter().filter(|c| c.open_time < oct_start_ms).cloned().collect();
    let test_15m: Vec<Candle> = candles_15m.iter().filter(|c| c.open_time >= oct_start_ms).cloned().collect();

    let t_start = DateTime::<Utc>::from_timestamp_millis(test_15m.first().unwrap().open_time).unwrap_or_default();
    let t_end = DateTime::<Utc>::from_timestamp_millis(test_15m.last().unwrap().close_time).unwrap_or_default();
    println!("Mês Atual: {} até {} UTC (Total: {} candles de 15m)", 
        t_start.format("%d/%m/%Y %H:%M"), 
        t_end.format("%d/%m/%Y %H:%M"),
        test_15m.len()
    );

    let base_cfg = AppConfig::load_or_create("config.toml")?;

    // CENÁRIO 1: Cascata MTF Rigorosa (Confluência 0.70 - SEM Ignição)
    let cascade_rigorosa = CascadeConfig {
        enabled: true,
        length: 100,
        sd_entry: 2.0,
        min_norm_slope: 0.10,
        use_ignition: false,
        ign_atr_mult: 1.5,
        ign_vol_mult: 1.8,
        use_mtf: true,
        min_confluence: 0.70,
        atr_len: 22,
        atr_mult: 3.0,
        risk_per_trade: 2.0,
    };
    let res1 = run_simulation(
        "Cascata MTF Prop Desk (Rigorosa 0.70 - SEM Ignição)",
        &symbol,
        &warmup_15m,
        &test_15m,
        &d1_candles,
        &h4_candles,
        cascade_rigorosa,
        base_cfg.clone(),
        initial_balance,
    ).await;

    // CENÁRIO 2: Cascata MTF Otimizada (Confluência 0.60 - SEM Ignição)
    let cascade_otimizada = CascadeConfig {
        enabled: true,
        length: 100,
        sd_entry: 1.8,
        min_norm_slope: 0.08,
        use_ignition: false,
        ign_atr_mult: 1.4,
        ign_vol_mult: 1.6,
        use_mtf: true,
        min_confluence: 0.60,
        atr_len: 22,
        atr_mult: 2.8,
        risk_per_trade: 2.0,
    };
    let res2 = run_simulation(
        "Cascata MTF Otimizada (Confluência 0.60 - SEM Ignição)",
        &symbol,
        &warmup_15m,
        &test_15m,
        &d1_candles,
        &h4_candles,
        cascade_otimizada,
        base_cfg.clone(),
        initial_balance,
    ).await;

    // CENÁRIO 3: Breakout 24/7 Clássico (Rolling 96 + EMA 200)
    let mut cascade_off = CascadeConfig::default();
    cascade_off.enabled = false;
    let res3 = run_simulation(
        "Breakout Clássico 24/7 (Canais Donchian 24h)",
        &symbol,
        &warmup_15m,
        &test_15m,
        &d1_candles,
        &h4_candles,
        cascade_off,
        base_cfg.clone(),
        initial_balance,
    ).await;

    println!("\n==========================================================================");
    println!("        RESULTADOS DO MÊS ATUAL (OUTUBRO) — SEM IGNIÇÃO DE MOMENTUM       ");
    println!("==========================================================================");
    print_result_card(&res1);
    print_result_card(&res2);
    print_result_card(&res3);

    // Também roda no ETHUSDT se o usuário tiver rodado SOLUSDT
    if symbol != "ETHUSDT" {
        println!("\n==========================================================================");
        println!("       AVALIAÇÃO NO MÊS ATUAL PARA ETHUSDT (SEM IGNIÇÃO DE MOMENTUM)      ");
        println!("==========================================================================");
        let eth_15m = fetch_klines(&client, "ETHUSDT", "15m", 1500).await?;
        let eth_d1 = fetch_klines(&client, "ETHUSDT", "1d", 120).await?;
        let eth_h4 = fetch_klines(&client, "ETHUSDT", "4h", 150).await?;

        let eth_warmup: Vec<Candle> = eth_15m.iter().filter(|c| c.open_time < oct_start_ms).cloned().collect();
        let eth_test: Vec<Candle> = eth_15m.iter().filter(|c| c.open_time >= oct_start_ms).cloned().collect();

        let mut eth_cfg = base_cfg.clone();
        eth_cfg.trading.symbol = "ETHUSDT".to_string();

        let eth_res = run_simulation(
            "ETHUSDT - Cascata MTF (Confluência 0.60 - SEM Ignição)",
            "ETHUSDT",
            &eth_warmup,
            &eth_test,
            &eth_d1,
            &eth_h4,
            CascadeConfig {
                enabled: true,
                length: 100,
                sd_entry: 1.8,
                min_norm_slope: 0.08,
                use_ignition: false,
                ign_atr_mult: 1.4,
                ign_vol_mult: 1.6,
                use_mtf: true,
                min_confluence: 0.60,
                atr_len: 22,
                atr_mult: 2.8,
                risk_per_trade: 2.0,
            },
            eth_cfg,
            initial_balance,
        ).await;

        print_result_card(&eth_res);
    }

    Ok(())
}
