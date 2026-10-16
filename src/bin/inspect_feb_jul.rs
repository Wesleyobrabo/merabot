// language: Rust, file: src/bin/inspect_feb_jul.rs, target: Windows 11, MSVC
use anyhow::Result;
use binance_futures_bot::config::AppConfig;
use binance_futures_bot::mera::MeraEngine;
use binance_futures_bot::types::{Candle, Position, PositionSide, SignalType};

#[tokio::main]
async fn main() -> Result<()> {
    let mut config = AppConfig::load_or_create("config.toml")?.mera;
    config.use_hybrid = true;
    config.enable_drift_filter = true;

    let warmup_candles_5m = 576;
    let warmup_offset_ms = (warmup_candles_5m as i64) * 5 * 60 * 1000;

    let target_months = vec![
        ("09. Setembro 2026",  1788220800000i64, 1790812799999i64),
    ];

    for (name, start, end) in target_months {
        let fetch_start = start - warmup_offset_ms;
        let cache_file = format!("data/cache_ETHUSDT_{}_{}.json", fetch_start, end);
        let data = std::fs::read_to_string(&cache_file)?;
        let candles: Vec<Candle> = serde_json::from_str(&data)?;

        let mut engine = MeraEngine::new(config.clone());
        engine.warmup(&candles[..warmup_candles_5m]);

        let mut pos = Position::new("ETHUSDT".to_string(), 5);
        let eur_to_usdt = 1.08;
        let mut balance_eur = 200.0;

        println!("\n=======================================================");
        println!(" INSPEÇÃO DETALHADA: {}", name);
        println!("=======================================================");

        for c in &candles[warmup_candles_5m..] {
            let (telem, sig_opt) = engine.update(c, &pos);

            if let Some(sig) = sig_opt {
                match sig.signal_type {
                    SignalType::OpenLong => {
                        if pos.side == PositionSide::None {
                            pos.side = PositionSide::Long;
                            pos.entry_price = sig.price * 1.0005;
                            pos.amount = 0.20;
                            println!("  [ENTRY LONG]  Price: {:.2} | LCP Score: {:.1} | Regime: {:?} | VetoL: {} | Reason: {}",
                                sig.price, telem.nw_central, telem.active_stop, telem.bias_bullish, sig.reason);
                        }
                    }
                    SignalType::OpenShort => {
                        if pos.side == PositionSide::None {
                            pos.side = PositionSide::Short;
                            pos.entry_price = sig.price * 0.9995;
                            pos.amount = 0.20;
                            println!("  [ENTRY SHORT] Price: {:.2} | LCP Score: {:.1} | Regime: {:?} | VetoS: {} | Reason: {}",
                                sig.price, telem.nw_central, telem.active_stop, telem.bias_bearish, sig.reason);
                        }
                    }
                    SignalType::CloseLong => {
                        if pos.side == PositionSide::Long {
                            let exit_p = sig.price * 0.9995;
                            let pnl = (exit_p - pos.entry_price) * pos.amount / eur_to_usdt;
                            balance_eur += pnl;
                            println!("  [EXIT LONG]   Price: {:.2} | PnL: {:+6.2}€ | Reason: {}", sig.price, pnl, sig.reason);
                            pos = Position::new("ETHUSDT".to_string(), 5);
                        }
                    }
                    SignalType::CloseShort => {
                        if pos.side == PositionSide::Short {
                            let exit_p = sig.price * 1.0005;
                            let pnl = (pos.entry_price - exit_p) * pos.amount / eur_to_usdt;
                            balance_eur += pnl;
                            println!("  [EXIT SHORT]  Price: {:.2} | PnL: {:+6.2}€ | Reason: {}", sig.price, pnl, sig.reason);
                            pos = Position::new("ETHUSDT".to_string(), 5);
                        }
                    }
                    _ => {}
                }
            }
        }
        println!("Saldo Final: {:.2}€", balance_eur);
    }

    Ok(())
}
