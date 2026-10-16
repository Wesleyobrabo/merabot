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
    short_trades: usize,
    buy_and_hold_pct: f64,
}

async fn fetch_range_klines(
    client: &reqwest::Client,
    symbol: &str,
    interval: &str,
    start_ms: i64,
    end_ms: i64,
) -> Result<Vec<Candle>> {
    let mut all_candles: Vec<Candle> = Vec::new();
    let mut current_start = start_ms;

    while current_start < end_ms {
        let url = format!(
            "https://fapi.binance.com/fapi/v1/klines?symbol={}&interval={}&startTime={}&endTime={}&limit=1000",
            symbol, interval, current_start, end_ms
        );

        let resp: Vec<Vec<Value>> = client.get(&url).send().await?.json().await?;
        if resp.is_empty() {
            break;
        }

        let mut last_open = 0;
        for row in resp {
            let open_time = row[0].as_i64().unwrap_or(0);
            let open: f64 = row[1].as_str().unwrap().parse().unwrap();
            let high: f64 = row[2].as_str().unwrap().parse().unwrap();
            let low: f64 = row[3].as_str().unwrap().parse().unwrap();
            let close: f64 = row[4].as_str().unwrap().parse().unwrap();
            let volume: f64 = row[5].as_str().unwrap().parse().unwrap();
            let close_time = row[6].as_i64().unwrap_or(0);

            last_open = open_time;

            all_candles.push(Candle {
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

        if last_open >= end_ms || last_open <= current_start {
            break;
        }
        current_start = last_open + 1;
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
) -> (SimResult, Vec<String>) {
    let temp_storage = StorageManager::new(&format!("data/backtest_april_{}", name.replace(' ', "_")));
    let mut exec = ExecutionEngine::new_with_storage(base_config.clone(), temp_storage, initial_balance);

    let mut strat_cfg = base_config.strategy.clone();
    strat_cfg.cascade = cascade_cfg.clone();
    let mut strategy = PineSupertrendRsiStrategy::new(strat_cfg, base_config.risk.clone());
    let mut risk = RiskManager::new(base_config.risk.clone());

    strategy.warmup(warmup_15m);
    strategy.warmup_htf(d1_candles.to_vec(), h4_candles.to_vec());

    let mut long_trades = 0;
    let mut short_trades = 0;
    let mut trade_logs = Vec::new();

    for candle in test_15m {
        let c_time = DateTime::<Utc>::from_timestamp_millis(candle.close_time).unwrap_or_default();

        // 1. Checa Stop Loss intrabar
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
                    tp_partial: None,
                    partial_ratio: None,
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
                    trade_logs.push(format!(
                        "[{}] ENTRADA LONG  #{} @ {:.2} | Stop Supertrend: {:.2} | {}",
                        c_time.format("%d/%m %H:%M"),
                        long_trades + short_trades,
                        sig.price,
                        sig.stop_loss.unwrap_or(0.0),
                        sig.reason
                    ));
                    let _ = exec.process_signal(sig, &mut risk).await;
                }
                SignalType::OpenShort => {
                    short_trades += 1;
                    trade_logs.push(format!(
                        "[{}] ENTRADA SHORT #{} @ {:.2} | Stop Supertrend: {:.2} | {}",
                        c_time.format("%d/%m %H:%M"),
                        long_trades + short_trades,
                        sig.price,
                        sig.stop_loss.unwrap_or(0.0),
                        sig.reason
                    ));
                    let _ = exec.process_signal(sig, &mut risk).await;
                }
                SignalType::CloseLong | SignalType::CloseShort => {
                    trade_logs.push(format!(
                        "[{}] SAÍDA A MERCADO ({:?}) @ {:.2} | {}",
                        c_time.format("%d/%m %H:%M"),
                        sig.signal_type,
                        sig.price,
                        sig.reason
                    ));
                    let _ = exec.process_signal(sig, &mut risk).await;
                }
                _ => {}
            }
        }
    }

    // Fecha posição residual ao final do mês
    if exec.position.side != PositionSide::None && exec.position.amount > 0.0 {
        if let Some(last_c) = test_15m.last() {
            let is_long = exec.position.side == PositionSide::Long;
            let sig = Signal {
                signal_type: if is_long { SignalType::CloseLong } else { SignalType::CloseShort },
                price: last_c.close,
                timestamp: last_c.close_time,
                reason: "Fechamento Fim do Mês (30/04 23:59)".to_string(),
                stop_loss: None,
                take_profit: None,
                tp_partial: None,
                partial_ratio: None,
                size_factor: None,
            };
            let _ = exec.process_signal(sig, &mut risk).await;
        }
    }

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

    (
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
            short_trades,
            buy_and_hold_pct,
        },
        trade_logs,
    )
}

fn print_result_card(res: &SimResult) {
    println!("┌────────────────────────────────────────────────────────────────────────┐");
    println!("│ CENÁRIO: {:<61} │", res.name);
    println!("├────────────────────────────────────────────────────────────────────────┤");
    println!("│ Ativo: {:<12} │ Mês: Abril/2026 (1350h) │ Buy & Hold: {:>+17.2}% │", res.symbol, res.buy_and_hold_pct);
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

    // Abril de 2026: 01/04/2026 00:00:00 até 30/04/2026 23:59:59 UTC
    let april_start_ms = 1775001600000i64;
    let april_end_ms = 1777593599999i64;

    // Warmup: 200 candles de 15m antes de 01/04
    let warmup_start_ms = april_start_ms - (200 * 15 * 60 * 1000);

    // Warmup diário e 4h: 120 dias antes de 01/04
    let htf_start_ms = april_start_ms - (120 * 24 * 60 * 60 * 1000);

    println!("==========================================================================");
    println!("  BACKTEST QUANTITATIVO MENSAL — MÊS DE ABRIL COMPLETO");
    println!("  Ativo: {} | Período: 01/04/2026 00:00 até 30/04/2026 23:59 UTC", symbol);
    println!("==========================================================================");
    println!("Baixando velas históricas de Abril na Binance Futures...");

    let all_15m = fetch_range_klines(&client, &symbol, "15m", warmup_start_ms, april_end_ms).await?;
    let all_4h = fetch_range_klines(&client, &symbol, "4h", htf_start_ms, april_end_ms).await?;
    let all_1d = fetch_range_klines(&client, &symbol, "1d", htf_start_ms, april_end_ms).await?;

    let warmup_15m: Vec<Candle> = all_15m.iter().filter(|c| c.open_time < april_start_ms).cloned().collect();
    let test_15m: Vec<Candle> = all_15m.iter().filter(|c| c.open_time >= april_start_ms && c.open_time <= april_end_ms).cloned().collect();

    let d1_warmup: Vec<Candle> = all_1d.iter().filter(|c| c.open_time < april_start_ms).cloned().collect();
    let h4_warmup: Vec<Candle> = all_4h.iter().filter(|c| c.open_time < april_start_ms).cloned().collect();

    println!("Velas carregadas:");
    println!("  • Warmup 15m:  {} candles", warmup_15m.len());
    println!("  • Mês de Abril: {} candles de 15m (Total: {} dias)", test_15m.len(), test_15m.len() / 96);
    println!("  • Histórico 4h:  {} candles", h4_warmup.len());
    println!("  • Histórico 1D:  {} candles", d1_warmup.len());

    let p_open = test_15m.first().unwrap().open;
    let p_close = test_15m.last().unwrap().close;
    let p_high = test_15m.iter().map(|c| c.high).fold(f64::MIN, f64::max);
    let p_low = test_15m.iter().map(|c| c.low).fold(f64::MAX, f64::min);
    let buy_and_hold = ((p_close - p_open) / p_open) * 100.0;
    let amp_mes = ((p_high - p_low) / p_low) * 100.0;

    println!("\nComportamento do Mercado em Abril para {}:", symbol);
    println!("  • Abertura (01/04): ${:.2}", p_open);
    println!("  • Fechamento (30/04): ${:.2} (Variação Buy & Hold: {:+.2}%)", p_close, buy_and_hold);
    println!("  • Mínima do Mês:    ${:.2} | Máxima do Mês: ${:.2} (Amplitude: {:.2}%)", p_low, p_high, amp_mes);

    let base_cfg = AppConfig::load_or_create("config.toml")?;

    // CENÁRIO 1: Cascata MTF Rigorosa (Confluência 0.70 - Sem Ignição)
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
    let (res1, logs1) = run_simulation(
        "Cascata MTF Prop Desk (Rigorosa 0.70 - SEM Ignição)",
        &symbol,
        &warmup_15m,
        &test_15m,
        &d1_warmup,
        &h4_warmup,
        cascade_rigorosa,
        base_cfg.clone(),
        initial_balance,
    ).await;

    // CENÁRIO 2: Cascata MTF Otimizada (Confluência 0.60 - Sem Ignição)
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
    let (res2, logs2) = run_simulation(
        "Cascata MTF Otimizada (Confluência 0.60 - SEM Ignição)",
        &symbol,
        &warmup_15m,
        &test_15m,
        &d1_warmup,
        &h4_warmup,
        cascade_otimizada,
        base_cfg.clone(),
        initial_balance,
    ).await;

    // CENÁRIO 3: Breakout Clássico 24/7 (Canais Donchian 24h)
    let mut cascade_off = CascadeConfig::default();
    cascade_off.enabled = false;
    let (res3, _) = run_simulation(
        "Breakout Clássico 24/7 (Canais Donchian 24h)",
        &symbol,
        &warmup_15m,
        &test_15m,
        &d1_warmup,
        &h4_warmup,
        cascade_off,
        base_cfg.clone(),
        initial_balance,
    ).await;

    println!("\n==========================================================================");
    println!("                   RELATÓRIO DE PERFORMANCE — ABRIL                      ");
    println!("==========================================================================");
    print_result_card(&res1);
    print_result_card(&res2);
    print_result_card(&res3);

    println!("\n==========================================================================");
    println!("     DIÁRIO DE OPERAÇÕES DA CASCATA MTF EM ABRIL (CENÁRIO OTIMIZADO)      ");
    println!("==========================================================================");
    if logs2.is_empty() {
        println!("NENHUMA OPERAÇÃO EXECUTADA. Capital 100% preservado em cash!");
        println!("(Sem o gatilho de ignição, a Cascata bloqueou todos os falsos rompimentos da consolidação lateral).");
    } else {
        for l in &logs2 {
            println!("  {}", l);
        }
    }

    // Avaliação também para ETHUSDT no mês de Abril
    if symbol != "ETHUSDT" {
        println!("\n==========================================================================");
        println!("       AVALIAÇÃO EM ABRIL PARA ETHUSDT (PAR ORIGINAL PINE SCRIPT)         ");
        println!("==========================================================================");
        let eth_all_15m = fetch_range_klines(&client, "ETHUSDT", "15m", warmup_start_ms, april_end_ms).await?;
        let eth_all_4h = fetch_range_klines(&client, "ETHUSDT", "4h", htf_start_ms, april_end_ms).await?;
        let eth_all_1d = fetch_range_klines(&client, "ETHUSDT", "1d", htf_start_ms, april_end_ms).await?;

        let eth_warmup_15m: Vec<Candle> = eth_all_15m.iter().filter(|c| c.open_time < april_start_ms).cloned().collect();
        let eth_test_15m: Vec<Candle> = eth_all_15m.iter().filter(|c| c.open_time >= april_start_ms && c.open_time <= april_end_ms).cloned().collect();
        let eth_d1: Vec<Candle> = eth_all_1d.iter().filter(|c| c.open_time < april_start_ms).cloned().collect();
        let eth_h4: Vec<Candle> = eth_all_4h.iter().filter(|c| c.open_time < april_start_ms).cloned().collect();

        let mut eth_cfg = base_cfg.clone();
        eth_cfg.trading.symbol = "ETHUSDT".to_string();

        let (eth_res, eth_logs) = run_simulation(
            "ETHUSDT - Cascata MTF (Confluência 0.60 - SEM Ignição)",
            "ETHUSDT",
            &eth_warmup_15m,
            &eth_test_15m,
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
        println!("\nDiário de Operações ETHUSDT em Abril:");
        for l in &eth_logs {
            println!("  {}", l);
        }
    }

    Ok(())
}
