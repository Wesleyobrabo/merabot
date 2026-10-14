// Rust, strategy.rs, 24/7 Rolling 96-Period Breakout Strategy for Solana 15M
use crate::config::{CascadeConfig, RiskConfig, StrategyConfig};
use crate::indicators::IndicatorEngine;
use crate::types::{Candle, IndicatorValues, Position, PositionSide, Signal, SignalType};
use tracing::info;

pub struct PineSupertrendRsiStrategy {
    pub config: StrategyConfig,
    pub risk_config: RiskConfig,
    pub engine: IndicatorEngine,
    pub trade_timestamps: Vec<i64>, // Timestamps (ms) das operações executadas nas últimas 24h
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
        engine.set_rolling_lookback(strategy_config.rolling_lookback_candles);
        engine.cascade_engine.set_config(strategy_config.cascade.clone());

        Self {
            config: strategy_config,
            risk_config,
            engine,
            trade_timestamps: Vec::new(),
            last_indicator_values: IndicatorValues::default(),
        }
    }

    pub fn is_pullback_enabled(&self) -> bool {
        self.config.enable_pullback_entry
    }

    pub fn set_pullback_enabled(&mut self, enabled: bool) {
        self.config.enable_pullback_entry = enabled;
    }

    pub fn is_cascade_enabled(&self) -> bool {
        self.config.cascade.enabled
    }

    pub fn set_cascade_enabled(&mut self, enabled: bool) {
        self.config.cascade.enabled = enabled;
        self.engine.cascade_engine.cfg.enabled = enabled;
    }

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
        self.engine.cascade_engine.set_config(self.config.cascade.clone());
        let mut last_ind = None;
        for candle in candles {
            last_ind = Some(self.engine.update(candle));
        }
        if let (Some(mut ind), Some(last)) = (last_ind, candles.last()) {
            self.attach_rolling_telemetry(&mut ind, last.close_time);
            self.last_indicator_values = ind;
        }

        let r_high = self.last_indicator_values.rolling_high_96;
        let r_low = self.last_indicator_values.rolling_low_96;
        let valid = r_high > 0.0 && r_low > 0.0 && r_high > r_low;
        let hours = self.config.rolling_lookback_candles as f64 * 0.25;

        info!(
            "[WARMUP 24/7] Histórico aquecido com {} candles de 15m. Range {:.0}h ({} candles): [{:.2} - {:.2}] ({:.2}%) | Range Válido: {} | EMA200: {:.2} (Tendência: {}) | Vol SMA({}): {:.2} (Filtro Vol: {}) | Cascata MTF: {}",
            candles.len(),
            hours,
            self.config.rolling_lookback_candles,
            r_low,
            r_high,
            self.last_indicator_values.rolling_range_pct,
            if valid { "SIM" } else { "NÃO" },
            self.last_indicator_values.ema_200,
            if self.last_indicator_values.trend_filter_bullish { "ALTA" } else { "BAIXA" },
            self.config.volume_sma_period,
            self.last_indicator_values.volume_sma,
            if self.config.enable_volume_filter { "ATIVO" } else { "DESATIVADO" },
            if self.config.cascade.enabled { "ATIVADA" } else { "DESATIVADA" }
        );
    }

    pub fn on_candle(&mut self, candle: &Candle, current_position: &Position) -> Option<Signal> {
        // ROTA A: Motor Cascata Prop Desk (MTF Duplo + OLS + Confluência Bayesiana)
        if self.config.cascade.enabled {
            // 1. Checagem em candle aberto: Stop Loss intrabar
            if !candle.is_closed {
                let mut temp_engine = self.engine.clone();
                let mut ind = temp_engine.update_with_position(candle, current_position.side, current_position.entry_price);
                self.attach_rolling_telemetry(&mut ind, candle.close_time);
                self.last_indicator_values = ind;

                if current_position.side == PositionSide::Long {
                    let stop = current_position.stop_loss.or(self.engine.cascade_engine.active_long_stop);
                    if let Some(sl) = stop {
                        if candle.low <= sl {
                            return Some(Signal {
                                signal_type: SignalType::CloseLong,
                                price: sl,
                                timestamp: candle.close_time,
                                reason: format!("Cascade Stop Loss intrabar atingido em {:.2}", sl),
                                stop_loss: None,
                                take_profit: None,
                            });
                        }
                    }
                } else if current_position.side == PositionSide::Short {
                    let stop = current_position.stop_loss.or(self.engine.cascade_engine.active_short_stop);
                    if let Some(sl) = stop {
                        if candle.high >= sl {
                            return Some(Signal {
                                signal_type: SignalType::CloseShort,
                                price: sl,
                                timestamp: candle.close_time,
                                reason: format!("Cascade Stop Loss intrabar atingido em {:.2}", sl),
                                stop_loss: None,
                                take_profit: None,
                            });
                        }
                    }
                }
                return None;
            }

            // 2. Candle FECHADO: atualiza indicadores com a posição real
            let mut ind = self.engine.update_with_position(candle, current_position.side, current_position.entry_price);
            self.attach_rolling_telemetry(&mut ind, candle.close_time);
            self.last_indicator_values = ind.clone();

            // 3. Avaliação de saída de posição aberta (ratcheting stop Supertrend)
            if current_position.side != PositionSide::None {
                if current_position.side == PositionSide::Long {
                    let stop = current_position.stop_loss.or(ind.cascade.active_stop);
                    if let Some(sl) = stop {
                        if candle.low <= sl || candle.close <= sl {
                            return Some(Signal {
                                signal_type: SignalType::CloseLong,
                                price: sl,
                                timestamp: candle.close_time,
                                reason: format!("Cascade Trailing Stop atingido em {:.2}", sl),
                                stop_loss: None,
                                take_profit: None,
                            });
                        }
                    }
                } else if current_position.side == PositionSide::Short {
                    let stop = current_position.stop_loss.or(ind.cascade.active_stop);
                    if let Some(sl) = stop {
                        if candle.high >= sl || candle.close >= sl {
                            return Some(Signal {
                                signal_type: SignalType::CloseShort,
                                price: sl,
                                timestamp: candle.close_time,
                                reason: format!("Cascade Trailing Stop atingido em {:.2}", sl),
                                stop_loss: None,
                                take_profit: None,
                            });
                        }
                    }
                }
                return None;
            }

            // 4. Avaliação de Nova Entrada da Cascata MTF
            if ind.cascade.setup_long {
                let stop_price = ind.cascade.active_stop.unwrap_or(candle.close - (ind.atr * self.config.cascade.atr_mult));
                let reason = if ind.cascade.ignition_long { "Cascade Ignition Long" } else { "Cascade Confluence Long" };
                self.trade_timestamps.push(candle.close_time);
                info!(
                    "[CASCADE SINAL LONG] {} | Preço: {:.2} | Stop: {:.2} | Confluência: {:.2} | Macro D1: {} | Contexto 4h: {} | Size: {:.0}%",
                    reason, candle.close, stop_price, ind.cascade.confluence_score_long, ind.cascade.d_bias, ind.cascade.h_bias, ind.cascade.size_factor_long * 100.0
                );
                return Some(Signal {
                    signal_type: SignalType::OpenLong,
                    price: candle.close,
                    timestamp: candle.close_time,
                    reason: format!("{} (Confluência: {:.2})", reason, ind.cascade.confluence_score_long),
                    stop_loss: Some(stop_price),
                    take_profit: None,
                });
            } else if ind.cascade.setup_short {
                let stop_price = ind.cascade.active_stop.unwrap_or(candle.close + (ind.atr * self.config.cascade.atr_mult));
                let reason = if ind.cascade.ignition_short { "Cascade Ignition Short" } else { "Cascade Confluence Short" };
                self.trade_timestamps.push(candle.close_time);
                info!(
                    "[CASCADE SINAL SHORT] {} | Preço: {:.2} | Stop: {:.2} | Confluência: {:.2} | Macro D1: {} | Contexto 4h: {} | Size: {:.0}%",
                    reason, candle.close, stop_price, ind.cascade.confluence_score_short, ind.cascade.d_bias, ind.cascade.h_bias, ind.cascade.size_factor_short * 100.0
                );
                return Some(Signal {
                    signal_type: SignalType::OpenShort,
                    price: candle.close,
                    timestamp: candle.close_time,
                    reason: format!("{} (Confluência: {:.2})", reason, ind.cascade.confluence_score_short),
                    stop_loss: Some(stop_price),
                    take_profit: None,
                });
            }

            return None;
        }

        // ROTA B: Motor Padrão (24/7 Rolling Breakout)
        // 1. Checagem em candle aberto: Hard Stop Loss e Take Profit intrabar
        if !candle.is_closed {
            let mut temp_engine = self.engine.clone();
            let mut ind = temp_engine.update(candle);
            self.attach_rolling_telemetry(&mut ind, candle.close_time);
            self.last_indicator_values = ind;

            if current_position.side == PositionSide::Long {
                if let Some(sl) = current_position.stop_loss {
                    if candle.low <= sl {
                        return Some(Signal {
                            signal_type: SignalType::CloseLong,
                            price: sl,
                            timestamp: candle.close_time,
                            reason: format!("Hard Stop Loss intrabar atingido em {:.2}", sl),
                            stop_loss: None,
                            take_profit: None,
                        });
                    }
                }
                if let Some(tp) = current_position.take_profit {
                    if candle.high >= tp {
                        return Some(Signal {
                            signal_type: SignalType::CloseLong,
                            price: tp,
                            timestamp: candle.close_time,
                            reason: format!("Take Profit intrabar atingido em {:.2}", tp),
                            stop_loss: None,
                            take_profit: None,
                        });
                    }
                }
            } else if current_position.side == PositionSide::Short {
                if let Some(sl) = current_position.stop_loss {
                    if candle.high >= sl {
                        return Some(Signal {
                            signal_type: SignalType::CloseShort,
                            price: sl,
                            timestamp: candle.close_time,
                            reason: format!("Hard Stop Loss intrabar atingido em {:.2}", sl),
                            stop_loss: None,
                            take_profit: None,
                        });
                    }
                }
                if let Some(tp) = current_position.take_profit {
                    if candle.low <= tp {
                        return Some(Signal {
                            signal_type: SignalType::CloseShort,
                            price: tp,
                            timestamp: candle.close_time,
                            reason: format!("Take Profit intrabar atingido em {:.2}", tp),
                            stop_loss: None,
                            take_profit: None,
                        });
                    }
                }
            }
            return None;
        }

        // 2. CANDLE FECHADO: atualiza indicadores e range móvel
        let mut ind = self.engine.update(candle);
        self.attach_rolling_telemetry(&mut ind, candle.close_time);
        self.last_indicator_values = ind.clone();

        let trades_24h = ind.trades_last_24h;
        let limit_reached = trades_24h >= self.config.max_trades_24h;
        let lookback_hours = self.config.rolling_lookback_candles as f64 * 0.25;

        info!(
            "[CANDLE 15M FECHADO] Close: {:.2} | Range {:.0}h: [{:.2} - {:.2}] | EMA200: {:.2} ({}) | Vol: {:.2}/SMA {:.2} ({}) | Trades 24h: {}/{} | Pos: {:?}",
            candle.close,
            lookback_hours,
            ind.rolling_low_96, ind.rolling_high_96,
            ind.ema_200, if ind.trend_filter_bullish { "ALTA" } else { "BAIXA" },
            candle.volume, ind.volume_sma, if ind.is_volume_high { "OK" } else { "BAIXO" },
            trades_24h, self.config.max_trades_24h, current_position.side
        );

        // 3. Avaliação de saída de posição aberta no fechamento da vela
        if current_position.side != PositionSide::None {
            if current_position.side == PositionSide::Long {
                if let Some(sl) = current_position.stop_loss {
                    if candle.low <= sl {
                        return Some(Signal {
                            signal_type: SignalType::CloseLong,
                            price: sl,
                            timestamp: candle.close_time,
                            reason: format!("Stop Loss atingido em {:.2}", sl),
                            stop_loss: None,
                            take_profit: None,
                        });
                    }
                }
                if let Some(tp) = current_position.take_profit {
                    if candle.high >= tp {
                        return Some(Signal {
                            signal_type: SignalType::CloseLong,
                            price: tp,
                            timestamp: candle.close_time,
                            reason: format!("Take Profit atingido em {:.2}", tp),
                            stop_loss: None,
                            take_profit: None,
                        });
                    }
                }
            } else if current_position.side == PositionSide::Short {
                if let Some(sl) = current_position.stop_loss {
                    if candle.high >= sl {
                        return Some(Signal {
                            signal_type: SignalType::CloseShort,
                            price: sl,
                            timestamp: candle.close_time,
                            reason: format!("Stop Loss atingido em {:.2}", sl),
                            stop_loss: None,
                            take_profit: None,
                        });
                    }
                }
                if let Some(tp) = current_position.take_profit {
                    if candle.low <= tp {
                        return Some(Signal {
                            signal_type: SignalType::CloseShort,
                            price: tp,
                            timestamp: candle.close_time,
                            reason: format!("Take Profit atingido em {:.2}", tp),
                            stop_loss: None,
                            take_profit: None,
                        });
                    }
                }
            }

            return None;
        }

        // 4. Avaliação de Nova Entrada: Rompimento do Range Móvel
        let r_high = ind.rolling_high_96;
        let r_low = ind.rolling_low_96;
        let range_ready = r_high > 0.0 && r_low > 0.0 && r_high > r_low;

        if range_ready && !limit_reached {
            let is_touch = self.config.breakout_trigger == "touch";
            let long_triggered = if is_touch { candle.high >= r_high } else { candle.close > r_high };
            let short_triggered = if is_touch { candle.low <= r_low } else { candle.close < r_low };

            let volume_ok = !self.config.enable_volume_filter || ind.is_volume_high;
            let allow_long = (!self.config.enable_trend_filter || ind.trend_filter_bullish) && volume_ok;
            let allow_short = (!self.config.enable_trend_filter || !ind.trend_filter_bullish) && volume_ok;

            if long_triggered && !short_triggered && allow_long {
                let entry_price = if is_touch {
                    r_high * (1.0 + (self.risk_config.slippage_percent / 100.0))
                } else {
                    candle.close * (1.0 + (self.risk_config.slippage_percent / 100.0))
                };

                let sl = entry_price * (1.0 - (self.config.sl_percent / 100.0));
                let tp = if self.config.enable_take_profit {
                    Some(entry_price * (1.0 + (self.config.tp_percent / 100.0)))
                } else {
                    None
                };

                self.trade_timestamps.push(candle.close_time);
                info!(
                    "[SINAL LONG #{}/{}] Rompimento Range {:.0}h: Preço {:.2} > High {:.2} | Vol: {:.2} >= SMA {:.2} | EMA200: {:.2} | SL: {:.2} (-{:.1}%) | TP: {:?} (+{:.1}%)",
                    self.trade_timestamps.len(), self.config.max_trades_24h, lookback_hours, entry_price, r_high, candle.volume, ind.volume_sma, ind.ema_200, sl, self.config.sl_percent, tp, self.config.tp_percent
                );

                return Some(Signal {
                    signal_type: SignalType::OpenLong,
                    price: entry_price,
                    timestamp: candle.close_time,
                    reason: format!(
                        "Breakout LONG {:.0}h (#{}/{}): Preço({:.2}) > High({:.2}) | Vol({:.2}>={:.2}) | EMA200({:.2}) | TP: {:?} | SL: {:.2}",
                        lookback_hours, self.trade_timestamps.len(), self.config.max_trades_24h, entry_price, r_high, candle.volume, ind.volume_sma, ind.ema_200, tp, sl
                    ),
                    stop_loss: Some(sl),
                    take_profit: tp,
                });
            } else if short_triggered && !long_triggered && allow_short {
                let entry_price = if is_touch {
                    r_low * (1.0 - (self.risk_config.slippage_percent / 100.0))
                } else {
                    candle.close * (1.0 - (self.risk_config.slippage_percent / 100.0))
                };

                let sl = entry_price * (1.0 + (self.config.sl_percent / 100.0));
                let tp = if self.config.enable_take_profit {
                    Some(entry_price * (1.0 - (self.config.tp_percent / 100.0)))
                } else {
                    None
                };

                self.trade_timestamps.push(candle.close_time);
                info!(
                    "[SINAL SHORT #{}/{}] Rompimento Range {:.0}h: Preço {:.2} < Low {:.2} | Vol: {:.2} >= SMA {:.2} | EMA200: {:.2} | SL: {:.2} (+{:.1}%) | TP: {:?} (-{:.1}%)",
                    self.trade_timestamps.len(), self.config.max_trades_24h, lookback_hours, entry_price, r_low, candle.volume, ind.volume_sma, ind.ema_200, sl, self.config.sl_percent, tp, self.config.tp_percent
                );

                return Some(Signal {
                    signal_type: SignalType::OpenShort,
                    price: entry_price,
                    timestamp: candle.close_time,
                    reason: format!(
                        "Breakout SHORT {:.0}h (#{}/{}): Preço({:.2}) < Low({:.2}) | Vol({:.2}>={:.2}) | EMA200({:.2}) | TP: {:?} | SL: {:.2}",
                        lookback_hours, self.trade_timestamps.len(), self.config.max_trades_24h, entry_price, r_low, candle.volume, ind.volume_sma, ind.ema_200, tp, sl
                    ),
                    stop_loss: Some(sl),
                    take_profit: tp,
                });
            }
        }

        None
    }

    fn attach_rolling_telemetry(&mut self, ind: &mut IndicatorValues, current_time_ms: i64) {
        // Remove operações com mais de 24 horas (86.400.000 ms)
        self.trade_timestamps.retain(|&ts| current_time_ms - ts < 86_400_000);
        let trades_24h = self.trade_timestamps.len();
        let limit_reached = trades_24h >= self.config.max_trades_24h;

        let next_slot_mins = if limit_reached && !self.trade_timestamps.is_empty() {
            let oldest = self.trade_timestamps[0];
            let elapsed = current_time_ms - oldest;
            let remaining_ms = 86_400_000 - elapsed;
            Some((remaining_ms / 60_000).max(1))
        } else {
            None
        };

        ind.trades_last_24h = trades_24h;
        ind.max_trades_24h = self.config.max_trades_24h;
        ind.next_slot_cooldown_mins = next_slot_mins;

        ind.session_trades_today = trades_24h;
        ind.session_max_trades = self.config.max_trades_24h;
        ind.session_traded_today = limit_reached;
        ind.session_range_high = ind.rolling_high_96;
        ind.session_range_low = ind.rolling_low_96;
        ind.session_range_ready = ind.rolling_high_96 > 0.0 && ind.rolling_low_96 > 0.0 && ind.rolling_high_96 > ind.rolling_low_96;

        ind.session_phase = if limit_reached {
            format!("Limite 24h Atingido (3/3 trades • libera em ~{}m)", next_slot_mins.unwrap_or(0))
        } else {
            format!("Radar 24/7 Ativo (Trade {}/{} liberado)", trades_24h + 1, self.config.max_trades_24h)
        };
    }

    pub fn last_indicators(&self) -> IndicatorValues {
        self.last_indicator_values.clone()
    }
}
