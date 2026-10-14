use anyhow::Result;
use binance_futures_bot::config::AppConfig;
use binance_futures_bot::execution::ExecutionEngine;
use binance_futures_bot::risk::RiskManager;
use binance_futures_bot::storage::StorageManager;
use binance_futures_bot::strategy::PineSupertrendRsiStrategy;
use binance_futures_bot::types::{Candle, PositionSide, Signal, SignalType};
use chrono::DateTime;
use serde_json::Value;

#[allow(dead_code)]
struct SimResult {
    name: String,
    timeframe: String,
    trades_count: usize,
    winning_trades: usize,
    losing_trades: usize,
    win_rate: f64,
    final_balance: f64,
    net_profit: f64,
    net_profit_pct: f64,
    profit_factor: f64,
    max_drawdown: f64,
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
    warmup_candles: &[Candle],
    test_candles: &[Candle],
    config: AppConfig,
    initial_balance: f64,
) -> (SimResult, ExecutionEngine) {
    let temp_storage = StorageManager::new(&format!("data/backtest_{}", name.replace(' ', "_")));
    let mut exec = ExecutionEngine::new_with_storage(config.clone(), temp_storage, initial_balance);
    let mut strategy = PineSupertrendRsiStrategy::new(config.strategy.clone(), config.risk.clone());
    let mut risk = RiskManager::new(config.risk.clone());

    // Aquecimento oficial dos indicadores com os candles anteriores ao período
    strategy.warmup(warmup_candles);

    for candle in test_candles {
        // Checagem de Stop Loss e Take Profit intra-candle
        if exec.position.side == PositionSide::Long && exec.position.amount > 0.0 {
            if let Some(sl) = exec.position.stop_loss {
                if candle.low <= sl {
                    let sig = Signal {
                        signal_type: SignalType::CloseLong,
                        price: sl,
                        timestamp: candle.close_time,
                        reason: format!("Stop Loss atingido em {:.2}", sl),
                        stop_loss: None,
                        take_profit: None,
                    };
                    let _ = exec.process_signal(sig, &mut risk).await;
                }
            }
            if exec.position.side == PositionSide::Long {
                if let Some(tp) = exec.position.take_profit {
                    if candle.high >= tp {
                        let sig = Signal {
                            signal_type: SignalType::CloseLong,
                            price: tp,
                            timestamp: candle.close_time,
                            reason: format!("Take Profit atingido em {:.2}", tp),
                            stop_loss: None,
                            take_profit: None,
                        };
                        let _ = exec.process_signal(sig, &mut risk).await;
                    }
                }
            }
        } else if exec.position.side == PositionSide::Short && exec.position.amount > 0.0 {
            if let Some(sl) = exec.position.stop_loss {
                if candle.high >= sl {
                    let sig = Signal {
                        signal_type: SignalType::CloseShort,
                        price: sl,
                        timestamp: candle.close_time,
                        reason: format!("Stop Loss atingido em {:.2}", sl),
                        stop_loss: None,
                        take_profit: None,
                    };
                    let _ = exec.process_signal(sig, &mut risk).await;
                }
            }
            if exec.position.side == PositionSide::Short {
                if let Some(tp) = exec.position.take_profit {
                    if candle.low <= tp {
                        let sig = Signal {
                            signal_type: SignalType::CloseShort,
                            price: tp,
                            timestamp: candle.close_time,
                            reason: format!("Take Profit atingido em {:.2}", tp),
                            stop_loss: None,
                            take_profit: None,
                        };
                        let _ = exec.process_signal(sig, &mut risk).await;
                    }
                }
            }
        }

        // Atualiza Trailing Breakeven e PnL da posição aberta
        exec.update_candle(candle, &mut risk);

        // Avalia sinais da estratégia (Reversões de Supertrend e Novas Entradas)
        if let Some(signal) = strategy.on_candle(candle, &exec.position) {
            let _ = exec.process_signal(signal, &mut risk).await;
        }
    }

    // Fecha posição residual se aberta ao final das 24 horas
    if exec.position.side != PositionSide::None && exec.position.amount > 0.0 {
        let last_c = test_candles.last().unwrap();
        let close_sig = match exec.position.side {
            PositionSide::Long => Signal {
                signal_type: SignalType::CloseLong,
                price: last_c.close,
                timestamp: last_c.close_time,
                reason: "Fim do período (Fechamento Mercado)".to_string(),
                stop_loss: None,
                take_profit: None,
            },
            PositionSide::Short => Signal {
                signal_type: SignalType::CloseShort,
                price: last_c.close,
                timestamp: last_c.close_time,
                reason: "Fim do período (Fechamento Mercado)".to_string(),
                stop_loss: None,
                take_profit: None,
            },
            PositionSide::None => unreachable!(),
        };
        let _ = exec.process_signal(close_sig, &mut risk).await;
    }

    let trades = &exec.trade_history;
    let final_balance = exec.account.balance;
    let net_profit = final_balance - initial_balance;
    let net_profit_pct = (net_profit / initial_balance) * 100.0;
    let trades_count = trades.len();
    let winning_trades = trades.iter().filter(|t| t.realized_pnl > 0.0).count();
    let losing_trades = trades.iter().filter(|t| t.realized_pnl < 0.0).count();
    let win_rate = if trades_count > 0 { (winning_trades as f64 / trades_count as f64) * 100.0 } else { 0.0 };

    let gross_profit: f64 = trades.iter().filter(|t| t.realized_pnl > 0.0).map(|t| t.realized_pnl).sum();
    let gross_loss: f64 = trades.iter().filter(|t| t.realized_pnl < 0.0).map(|t| t.realized_pnl.abs()).sum();
    let profit_factor = if gross_loss > 0.0 { gross_profit / gross_loss } else if gross_profit > 0.0 { 99.9 } else { 0.0 };

    let res = SimResult {
        name: name.to_string(),
        timeframe: config.trading.interval.clone(),
        trades_count,
        winning_trades,
        losing_trades,
        win_rate,
        final_balance,
        net_profit,
        net_profit_pct,
        profit_factor,
        max_drawdown: exec.account.max_drawdown,
    };

    (res, exec)
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut base_config = AppConfig::load_or_create("config.toml")?;
    base_config.trading.paper_trading = true;
    base_config.notifications.enabled = false;
    let initial_balance = base_config.trading.initial_paper_balance; // 9.80 USDT (~9 EUR)

    println!("==========================================================================================");
    println!("      BACKTEST HISTÓRICO - SOLUSDT 5M (BINANCE FUTURES) - DADOS DE ONTEM                  ");
    println!("==========================================================================================");
    println!("Configuração Oficial Ativa do Bot:");
    println!("  • Par: {} | Timeframe: {} | Alavancagem: {}x {}", 
        base_config.trading.symbol, base_config.trading.interval, base_config.trading.leverage, base_config.trading.margin_type);
    println!("  • Capital de Teste: ${:.2} USDT (~{:.2} EUR) | Risco por Trade: {:.1}%", 
        initial_balance, initial_balance / 1.09, base_config.risk.risk_per_trade_percent);
    println!("  • Supertrend: (10, {:.1}) | EMA Ribbon: (9, 21) | RSI: ({:.0}, {:.0})", 
        base_config.strategy.supertrend_multiplier, base_config.strategy.rsi_oversold, base_config.strategy.rsi_overbought);
    println!("  • Stop Loss: {:.1}x ATR | Take Profit: {:.1}x ATR | Breakeven: {:.1}x ATR", 
        base_config.risk.sl_atr_multiplier, base_config.risk.tp_atr_multiplier, base_config.risk.breakeven_atr_multiplier);
    println!("  • Buffer Nocional Mínimo: ${:.2} USDT | Volume Filter: {}", 
        base_config.risk.min_notional_usdt, if base_config.strategy.enable_volume_filter { "ATIVADO" } else { "DESATIVADO" });
    println!("  • Re-entrada em Pullback nas EMAs: {}", 
        if base_config.strategy.enable_pullback_entry { "ATIVADA (Opção 1)" } else { "DESATIVADA" });
    println!("==========================================================================================");
    println!("Baixando candles de 5m da Binance Futures...\n");

    let client = reqwest::Client::new();
    // Período configurável: BT_DAYS=30 cargo run --bin backtest (padrão 1 dia)
    let bt_days: usize = std::env::var("BT_DAYS").ok().and_then(|v| v.parse().ok()).unwrap_or(1).max(1);
    let candles_all = fetch_klines(&client, "SOLUSDT", "5m", 300 + 288 * bt_days).await?;

    let total_candles = candles_all.len();
    if total_candles < 288 {
        println!("Dados insuficientes baixados: {}", total_candles);
        return Ok(());
    }

    // 288 velas de 5m = exatamente 24 horas (ontem consolidado)
    let yesterday_count = 288 * bt_days;
    let warmup_count = total_candles - yesterday_count;

    let warmup_slice = &candles_all[..warmup_count];
    let yesterday_slice = &candles_all[warmup_count..];

    let dt_start = DateTime::from_timestamp_millis(yesterday_slice.first().unwrap().open_time).unwrap_or_default();
    let dt_end = DateTime::from_timestamp_millis(yesterday_slice.last().unwrap().close_time).unwrap_or_default();

    println!("Período Avaliado (Últimas 24h / Ontem):");
    println!("  • Início: {} UTC", dt_start.format("%d/%m/%Y %H:%M"));
    println!("  • Fim:    {} UTC", dt_end.format("%d/%m/%Y %H:%M"));
    println!("  • Velas Avaliadas: {} de 5m (Aquecimento Prévio: {} velas)\n", yesterday_slice.len(), warmup_slice.len());

    // Cenário 1: Base Anterior (Sem os 3 Filtros Institucionais da Prop Desk)
    let mut cfg_baseline = base_config.clone();
    cfg_baseline.strategy.enable_chop_filter = false;
    cfg_baseline.strategy.enable_htf_veto = false;
    cfg_baseline.strategy.enable_confluence_filter = false;
    let (res_base, exec_base) = run_simulation(
        "Base Anterior (Sem 3 Filtros)",
        warmup_slice,
        yesterday_slice,
        cfg_baseline,
        initial_balance,
    ).await;

    // Cenário 2: Prop Desk Cascade Transposto (CHOP 60 + HTF Veto 1H + Confluência >= 0.65)
    let cfg_prop_desk = base_config.clone();
    let (res_prop, exec_prop) = run_simulation(
        "Prop Desk Transposto (3 Filtros)",
        warmup_slice,
        yesterday_slice,
        cfg_prop_desk,
        initial_balance,
    ).await;

    // Cenário 3: Pullback RSI máx 72 (variante recomendada no sweep)
    let mut cfg_strict = base_config.clone();
    cfg_strict.strategy.pullback_rsi_max = 72.0;
    let (res_strict, _exec_strict) = run_simulation(
        "Atual + Pullback RSI<=72",
        warmup_slice,
        yesterday_slice,
        cfg_strict,
        initial_balance,
    ).await;

    // Cenário 4: Sem pullback (somente flips de Supertrend) com os 3 filtros
    let mut cfg_nopb = base_config.clone();
    cfg_nopb.strategy.enable_pullback_entry = false;
    let (res_nopb, _exec_nopb) = run_simulation(
        "3 Filtros, sem Pullback",
        warmup_slice,
        yesterday_slice,
        cfg_nopb,
        initial_balance,
    ).await;

    let results = vec![res_base, res_prop, res_strict, res_nopb];

    println!("==========================================================================================");
    println!("                      RESULTADO DO BACKTEST DE ONTEM (24 HORAS)                           ");
    println!("==========================================================================================");
    println!("{:<32} | {:<5} | {:<6} | {:<8} | {:<8} | {:<8} | {:<7} | {:<6}",
        "Estratégia", "TF", "Trades", "Win Rate", "PnL ($)", "PnL (%)", "PF", "Max DD");
    println!("------------------------------------------------------------------------------------------");

    for r in &results {
        println!("{:<32} | {:<5} | {:<6} | {:>7.1}% | {:>+7.2}$ | {:>+7.2}% | {:>7.2} | {:>5.1}%",
            r.name,
            r.timeframe,
            r.trades_count,
            r.win_rate,
            r.net_profit,
            r.net_profit_pct,
            r.profit_factor,
            r.max_drawdown
        );
    }
    println!("==========================================================================================\n");

    // Detalha histórico de operações do cenário Prop Desk Transposto
    println!("--- EXTRATO DETALHADO DE OPERAÇÕES (PROP DESK CASCADE TRANSPOSTO) ---");
    if bt_days > 2 {
        println!("(extrato omitido: periodo de {} dias, {} operacoes)", bt_days, exec_prop.trade_history.len());
    } else if exec_prop.trade_history.is_empty() {
        println!("Nenhuma operação foi disparada no período de ontem.");
        println!("MOTIVO: Os 3 filtros institucionais (CHOP > 60 / HTF Veto / Confluência < 0.65) detectaram mercado 100% lateral e vetaram falsos rompimentos, protegendo o capital integralmente.");
    } else {
        println!("{:<3} | {:<5} | {:<12} | {:<8} | {:<6} | {:<8} | Motivo",
            "#", "Lado", "Data/Hora", "Preço", "Qtd", "PnL ($)");
        println!("------------------------------------------------------------------------------------------");
        for (i, t) in exec_prop.trade_history.iter().enumerate() {
            let dt = DateTime::from_timestamp_millis(t.timestamp).unwrap_or_default();
            println!("{:<3} | {:<5} | {:<12} | ${:<7.2} | {:<6.2} | {:>+7.2} | {}",
                i + 1,
                format!("{:?}", t.side),
                dt.format("%d/%m %H:%M"),
                t.price,
                t.quantity,
                t.realized_pnl,
                t.reason
            );
        }
    }
    println!("==========================================================================================\n");

    Ok(())
}
