// Rust, strategy.rs, Motor MERA (Session VWAP + Nadaraya-Watson Envelope Scalping)
use crate::config::{CascadeConfig, MeraConfig, RiskConfig, StrategyConfig};
use crate::indicators::IndicatorEngine;
use crate::mera::MeraEngine;
use crate::types::{Candle, IndicatorValues, Position, PositionSide, Signal, SignalType};
use tracing::info;

pub struct PineSupertrendRsiStrategy {
    pub config: StrategyConfig,
    pub risk_config: RiskConfig,
    pub engine: IndicatorEngine,
    pub mera_engine: MeraEngine,
    pub trade_timestamps: Vec<i64>,
    pub last_indicator_values: IndicatorValues,
}

impl PineSupertrendRsiStrategy {
    pub fn new(strategy_config: StrategyConfig, risk_config: RiskConfig) -> Self {
        let mut engine = IndicatorEngine::new(
            strategy_config.supertrend_period,
            strategy_config.supertrend_multiplier,
            strategy_config.ema_fast_period,
            strategy_config.ema_slow_period,
            strategy_config.rsi_period,
            strategy_config.volume_sma_period,
        );
        engine.cascade_engine.set_config(strategy_config.cascade.clone());
        let mera_engine = MeraEngine::new(MeraConfig::default());

        Self {
            config: strategy_config,
            risk_config,
            engine,
            mera_engine,
            trade_timestamps: Vec::new(),
            last_indicator_values: IndicatorValues::default(),
        }
    }

    pub fn is_pullback_enabled(&self) -> bool {
        false
    }

    pub fn set_pullback_enabled(&mut self, _enabled: bool) {}

    pub fn is_cascade_enabled(&self) -> bool {
        true
    }

    pub fn set_cascade_enabled(&mut self, _enabled: bool) {}

    pub fn update_cascade_config(&mut self, cfg: CascadeConfig) {
        self.config.cascade = cfg.clone();
        self.engine.cascade_engine.set_config(cfg);
    }

    pub fn warmup_htf(&mut self, d1: Vec<Candle>, h4: Vec<Candle>) {
        self.engine.cascade_engine.set_htf_candles(d1, h4);
    }

    pub fn warmup(&mut self, candles: &[Candle]) {
        if candles.is_empty() {
            return;
        }
        self.mera_engine.warmup(candles);
        self.last_indicator_values.mera = self.mera_engine.last_telemetry.clone();

        self.engine.cascade_engine.set_config(self.config.cascade.clone());
        let mut last_ind = None;
        for candle in candles {
            last_ind = Some(self.engine.update(candle));
        }
        if let Some(mut ind) = last_ind {
            ind.mera = self.mera_engine.last_telemetry.clone();
            self.last_indicator_values = ind;
        }

        info!(
            "[WARMUP MERA] {} candles carregados | VWAP: {:.2} | NW Central: {:.2} | NW Bandas: [{:.2} - {:.2}]",
            candles.len(),
            self.last_indicator_values.mera.session_vwap,
            self.last_indicator_values.mera.nw_central,
            self.last_indicator_values.mera.nw_lower,
            self.last_indicator_values.mera.nw_upper,
        );
    }

    pub fn on_candle(&mut self, candle: &Candle, current_position: &Position) -> Option<Signal> {
        // 1. AVALIAÇÃO PRINCIPAL PELO MOTOR MERA (VWAP + NADARAYA-WATSON 5M)
        let (mera_telem, mera_signal) = self.mera_engine.update(candle, current_position);
        self.last_indicator_values.mera = mera_telem;

        if let Some(sig) = mera_signal {
            return Some(sig);
        }

        let pos_entry = current_position.entry_price;
        let pos_side = current_position.side;
        let is_in_pos = pos_side != PositionSide::None && current_position.amount > 0.0;

        // 2. CHECAGEM INTRABAR (TICK A TICK OU VELA ABERTA)
        if !candle.is_closed {
            if is_in_pos {
                let mut temp_engine = self.engine.clone();
                let ind = temp_engine.update_with_position(candle, pos_side, pos_entry);
                self.last_indicator_values = ind.clone();

                if pos_side == PositionSide::Long {
                    // A. Take Profit Parcial de 50% em +2.50%
                    if !current_position.partial_taken && candle.high >= pos_entry * 1.025 {
                        let partial_exit = pos_entry * 1.025;
                        info!("[CASCATA TURBO] Take Profit Parcial 50% atingido para LONG @ {:.2}", partial_exit);
                        return Some(Signal {
                            signal_type: SignalType::PartialCloseLong,
                            price: partial_exit,
                            timestamp: candle.close_time,
                            reason: "Take Profit Parcial (+2.50%)".to_string(),
                            stop_loss: Some(pos_entry * 1.0025), // Já reposiciona restante no breakeven
                            take_profit: None,
                            tp_partial: None,
                            partial_ratio: Some(0.5),
                            size_factor: None,
                        });
                    }

                    // B. Stop Loss ou Trailing Stop
                    let mut stop = current_position.stop_loss.or(ind.cascade.active_stop);
                    if current_position.be_locked || current_position.partial_taken {
                        let be_floor = pos_entry * 1.0025;
                        stop = Some(match stop {
                            Some(s) => s.max(be_floor),
                            None => be_floor,
                        });
                    }

                    if let Some(sl) = stop {
                        if candle.low <= sl {
                            let reason = if current_position.be_locked || current_position.partial_taken {
                                "Breakeven Seguro / Trailing Stop acionado"
                            } else {
                                "Stop Loss Supertrend intrabar atingido"
                            };
                            return Some(Signal {
                                signal_type: SignalType::CloseLong,
                                price: sl,
                                timestamp: candle.close_time,
                                reason: format!("{} em {:.2}", reason, sl),
                                stop_loss: None,
                                take_profit: None,
                                tp_partial: None,
                                partial_ratio: None,
                                size_factor: None,
                            });
                        }
                    }
                } else if pos_side == PositionSide::Short {
                    // A. Take Profit Parcial de 50% em +2.50% (preço cai 2.5%)
                    if !current_position.partial_taken && candle.low <= pos_entry * 0.975 {
                        let partial_exit = pos_entry * 0.975;
                        info!("[CASCATA TURBO] Take Profit Parcial 50% atingido para SHORT @ {:.2}", partial_exit);
                        return Some(Signal {
                            signal_type: SignalType::PartialCloseShort,
                            price: partial_exit,
                            timestamp: candle.close_time,
                            reason: "Take Profit Parcial (+2.50%)".to_string(),
                            stop_loss: Some(pos_entry * 0.9975),
                            take_profit: None,
                            tp_partial: None,
                            partial_ratio: Some(0.5),
                            size_factor: None,
                        });
                    }

                    // B. Stop Loss ou Trailing Stop
                    let mut stop = current_position.stop_loss.or(ind.cascade.active_stop);
                    if current_position.be_locked || current_position.partial_taken {
                        let be_ceiling = pos_entry * 0.9975;
                        stop = Some(match stop {
                            Some(s) => s.min(be_ceiling),
                            None => be_ceiling,
                        });
                    }

                    if let Some(sl) = stop {
                        if candle.high >= sl {
                            let reason = if current_position.be_locked || current_position.partial_taken {
                                "Breakeven Seguro / Trailing Stop acionado"
                            } else {
                                "Stop Loss Supertrend intrabar atingido"
                            };
                            return Some(Signal {
                                signal_type: SignalType::CloseShort,
                                price: sl,
                                timestamp: candle.close_time,
                                reason: format!("{} em {:.2}", reason, sl),
                                stop_loss: None,
                                take_profit: None,
                                tp_partial: None,
                                partial_ratio: None,
                                size_factor: None,
                            });
                        }
                    }
                }
            }
            return None;
        }

        // 2. CANDLE 15M FECHADO: atualiza indicadores completos com posição ativa
        let ind = self.engine.update_with_position(candle, pos_side, pos_entry);
        self.last_indicator_values = ind.clone();

        let c = &ind.cascade;
        info!(
            "[CANDLE 15M FECHADO] Close: {:.2} | Base OLS: {:.2} | Bandas [{:.2} - {:.2}] | D1: {} | 4h: {} | Chop: {:.1} | Pos: {:?}",
            candle.close, c.baseline, c.lower_entry, c.upper_entry, c.d_bias, c.h_bias, c.chop_val, current_position.side
        );

        // 3. SE POSIÇÃO ESTIVER ABERTA NO FECHAMENTO
        if is_in_pos {
            if pos_side == PositionSide::Long {
                let mut stop = current_position.stop_loss.or(c.active_stop);
                if current_position.be_locked || current_position.partial_taken {
                    let be_floor = pos_entry * 1.0025;
                    stop = Some(match stop {
                        Some(s) => s.max(be_floor),
                        None => be_floor,
                    });
                }
                if let Some(sl) = stop {
                    if candle.low <= sl || candle.close <= sl {
                        return Some(Signal {
                            signal_type: SignalType::CloseLong,
                            price: sl,
                            timestamp: candle.close_time,
                            reason: format!("Cascade Trailing Stop fechado em {:.2}", sl),
                            stop_loss: None,
                            take_profit: None,
                            tp_partial: None,
                            partial_ratio: None,
                            size_factor: None,
                        });
                    }
                }
            } else if pos_side == PositionSide::Short {
                let mut stop = current_position.stop_loss.or(c.active_stop);
                if current_position.be_locked || current_position.partial_taken {
                    let be_ceiling = pos_entry * 0.9975;
                    stop = Some(match stop {
                        Some(s) => s.min(be_ceiling),
                        None => be_ceiling,
                    });
                }
                if let Some(sl) = stop {
                    if candle.high >= sl || candle.close >= sl {
                        return Some(Signal {
                            signal_type: SignalType::CloseShort,
                            price: sl,
                            timestamp: candle.close_time,
                            reason: format!("Cascade Trailing Stop fechado em {:.2}", sl),
                            stop_loss: None,
                            take_profit: None,
                            tp_partial: None,
                            partial_ratio: None,
                            size_factor: None,
                        });
                    }
                }
            }
            return None;
        }

        // 4. AVALIAÇÃO DE NOVA ENTRADA (CASCATA TURBO MTF)
        if c.setup_long {
            let stop_price = c.active_stop.unwrap_or(candle.close - (ind.atr * self.config.cascade.atr_mult)).min(candle.close * 0.985);
            let reason = if c.ignition_long { "Cascade Smart Ignition Long" } else { "Cascade Adaptive Long" };
            self.trade_timestamps.push(candle.close_time);
            info!(
                "[CASCADE SINAL LONG] {} | Preço: {:.2} | Stop Supertrend: {:.2} | Confluência: {:.2} | Macro D1: {} | Contexto 4h: {} | Size: {:.0}%",
                reason, candle.close, stop_price, c.confluence_score_long, c.d_bias, c.h_bias, c.size_factor_long * 100.0
            );
            return Some(Signal {
                signal_type: SignalType::OpenLong,
                price: candle.close,
                timestamp: candle.close_time,
                reason: format!("{} (Confluência: {:.2})", reason, c.confluence_score_long),
                stop_loss: Some(stop_price),
                take_profit: None,
                tp_partial: None,
                partial_ratio: None,
                size_factor: Some(c.size_factor_long),
            });
        } else if c.setup_short {
            let stop_price = c.active_stop.unwrap_or(candle.close + (ind.atr * self.config.cascade.atr_mult)).max(candle.close * 1.015);
            let reason = if c.ignition_short { "Cascade Smart Ignition Short" } else { "Cascade Adaptive Short" };
            self.trade_timestamps.push(candle.close_time);
            info!(
                "[CASCADE SINAL SHORT] {} | Preço: {:.2} | Stop Supertrend: {:.2} | Confluência: {:.2} | Macro D1: {} | Contexto 4h: {} | Size: {:.0}%",
                reason, candle.close, stop_price, c.confluence_score_short, c.d_bias, c.h_bias, c.size_factor_short * 100.0
            );
            return Some(Signal {
                signal_type: SignalType::OpenShort,
                price: candle.close,
                timestamp: candle.close_time,
                reason: format!("{} (Confluência: {:.2})", reason, c.confluence_score_short),
                stop_loss: Some(stop_price),
                take_profit: None,
                tp_partial: None,
                partial_ratio: None,
                size_factor: Some(c.size_factor_short),
            });
        }

        None
    }

    pub fn last_indicators(&self) -> IndicatorValues {
        self.last_indicator_values.clone()
    }
}
