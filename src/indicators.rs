use crate::config::CascadeConfig;
use crate::types::{Candle, CascadeTelemetry, IndicatorValues, MarketRegimeAnalysis, PositionSide};

#[derive(Debug, Clone)]
pub struct Ema {
    period: usize,
    k: f64,
    value: Option<f64>,
    sample_count: usize,
    accum: f64,
}

impl Ema {
    pub fn new(period: usize) -> Self {
        Self {
            period,
            k: 2.0 / (period as f64 + 1.0),
            value: None,
            sample_count: 0,
            accum: 0.0,
        }
    }

    pub fn update(&mut self, price: f64) -> Option<f64> {
        self.sample_count += 1;
        if let Some(prev) = self.value {
            let next = price * self.k + prev * (1.0 - self.k);
            self.value = Some(next);
            Some(next)
        } else {
            self.accum += price;
            if self.sample_count >= self.period {
                let initial = self.accum / self.period as f64;
                self.value = Some(initial);
                Some(initial)
            } else {
                None
            }
        }
    }

    pub fn value(&self) -> Option<f64> {
        self.value
    }
}

#[derive(Debug, Clone)]
pub struct Atr {
    period: usize,
    prev_close: Option<f64>,
    atr: Option<f64>,
    sample_count: usize,
    accum_tr: f64,
}

impl Atr {
    pub fn new(period: usize) -> Self {
        Self {
            period,
            prev_close: None,
            atr: None,
            sample_count: 0,
            accum_tr: 0.0,
        }
    }

    pub fn update(&mut self, high: f64, low: f64, close: f64) -> Option<f64> {
        let tr = if let Some(prev_c) = self.prev_close {
            let hl = high - low;
            let hc = (high - prev_c).abs();
            let lc = (low - prev_c).abs();
            hl.max(hc).max(lc)
        } else {
            high - low
        };

        self.prev_close = Some(close);
        self.sample_count += 1;

        if let Some(prev_atr) = self.atr {
            // Wilder's smoothing (RMA)
            let next_atr = (prev_atr * (self.period as f64 - 1.0) + tr) / self.period as f64;
            self.atr = Some(next_atr);
            Some(next_atr)
        } else {
            self.accum_tr += tr;
            if self.sample_count >= self.period {
                let initial = self.accum_tr / self.period as f64;
                self.atr = Some(initial);
                Some(initial)
            } else {
                None
            }
        }
    }

    pub fn value(&self) -> Option<f64> {
        self.atr
    }
}

#[derive(Debug, Clone)]
pub struct Rsi {
    period: usize,
    prev_close: Option<f64>,
    avg_gain: Option<f64>,
    avg_loss: Option<f64>,
    sample_count: usize,
    accum_gain: f64,
    accum_loss: f64,
    rsi: Option<f64>,
}

impl Rsi {
    pub fn new(period: usize) -> Self {
        Self {
            period,
            prev_close: None,
            avg_gain: None,
            avg_loss: None,
            sample_count: 0,
            accum_gain: 0.0,
            accum_loss: 0.0,
            rsi: None,
        }
    }

    pub fn update(&mut self, close: f64) -> Option<f64> {
        if self.prev_close.is_none() {
            self.prev_close = Some(close);
            return None;
        }

        let change = close - self.prev_close.unwrap();
        self.prev_close = Some(close);
        self.sample_count += 1;

        let gain = if change > 0.0 { change } else { 0.0 };
        let loss = if change < 0.0 { -change } else { 0.0 };

        if let (Some(prev_gain), Some(prev_loss)) = (self.avg_gain, self.avg_loss) {
            let next_gain = (prev_gain * (self.period as f64 - 1.0) + gain) / self.period as f64;
            let next_loss = (prev_loss * (self.period as f64 - 1.0) + loss) / self.period as f64;

            self.avg_gain = Some(next_gain);
            self.avg_loss = Some(next_loss);

            let rsi = if next_loss == 0.0 {
                100.0
            } else {
                let rs = next_gain / next_loss;
                100.0 - (100.0 / (1.0 + rs))
            };
            self.rsi = Some(rsi);
            Some(rsi)
        } else {
            self.accum_gain += gain;
            self.accum_loss += loss;

            if self.sample_count >= self.period {
                let init_gain = self.accum_gain / self.period as f64;
                let init_loss = self.accum_loss / self.period as f64;
                self.avg_gain = Some(init_gain);
                self.avg_loss = Some(init_loss);

                let rsi = if init_loss == 0.0 {
                    100.0
                } else {
                    let rs = init_gain / init_loss;
                    100.0 - (100.0 / (1.0 + rs))
                };
                self.rsi = Some(rsi);
                Some(rsi)
            } else {
                None
            }
        }
    }

    pub fn value(&self) -> Option<f64> {
        self.rsi
    }
}

#[derive(Debug, Clone)]
pub struct Supertrend {
    #[allow(dead_code)]
    period: usize,
    multiplier: f64,
    atr: Atr,
    prev_close: Option<f64>,
    final_upper_band: f64,
    final_lower_band: f64,
    direction: i8, // 1 = Bullish (Supertrend abaixo do preco), -1 = Bearish (Supertrend acima do preco)
    supertrend: Option<f64>,
}

impl Supertrend {
    pub fn new(period: usize, multiplier: f64) -> Self {
        Self {
            period,
            multiplier,
            atr: Atr::new(period),
            prev_close: None,
            final_upper_band: 0.0,
            final_lower_band: 0.0,
            direction: 1,
            supertrend: None,
        }
    }

    pub fn update(&mut self, high: f64, low: f64, close: f64) -> Option<(f64, i8)> {
        let current_atr = self.atr.update(high, low, close)?;
        let hl2 = (high + low) / 2.0;

        let basic_upper_band = hl2 + (self.multiplier * current_atr);
        let basic_lower_band = hl2 - (self.multiplier * current_atr);

        if let Some(prev_c) = self.prev_close {
            // Calculo da Upper Band Final
            if basic_upper_band < self.final_upper_band || prev_c > self.final_upper_band {
                self.final_upper_band = basic_upper_band;
            }

            // Calculo da Lower Band Final
            if basic_lower_band > self.final_lower_band || prev_c < self.final_lower_band {
                self.final_lower_band = basic_lower_band;
            }

            // Determinacao de direcao
            if self.direction == 1 {
                if close < self.final_lower_band {
                    self.direction = -1;
                    self.supertrend = Some(self.final_upper_band);
                } else {
                    self.supertrend = Some(self.final_lower_band);
                }
            } else {
                if close > self.final_upper_band {
                    self.direction = 1;
                    self.supertrend = Some(self.final_lower_band);
                } else {
                    self.supertrend = Some(self.final_upper_band);
                }
            }
        } else {
            self.final_upper_band = basic_upper_band;
            self.final_lower_band = basic_lower_band;
            self.supertrend = Some(self.final_lower_band);
            self.direction = 1;
        }

        self.prev_close = Some(close);
        self.supertrend.map(|val| (val, self.direction))
    }

    pub fn value(&self) -> Option<(f64, i8)> {
        self.supertrend.map(|val| (val, self.direction))
    }

    pub fn current_atr(&self) -> Option<f64> {
        self.atr.value()
    }
}

#[derive(Debug, Clone)]
pub struct VolumeSma {
    period: usize,
    buffer: Vec<f64>,
}

impl VolumeSma {
    pub fn new(period: usize) -> Self {
        Self {
            period: period.max(1),
            buffer: Vec::with_capacity(period + 1),
        }
    }

    pub fn update(&mut self, volume: f64) -> f64 {
        self.buffer.push(volume);
        if self.buffer.len() > self.period {
            self.buffer.remove(0);
        }
        let sum: f64 = self.buffer.iter().sum();
        sum / (self.buffer.len() as f64)
    }
}



#[derive(Debug, Clone)]
pub struct ChoppinessIndex {
    period: usize,
    candles: Vec<Candle>,
}

impl ChoppinessIndex {
    pub fn new(period: usize) -> Self {
        Self {
            period: period.max(2),
            candles: Vec::with_capacity(period + 2),
        }
    }

    pub fn update(&mut self, candle: &Candle) -> f64 {
        self.candles.push(candle.clone());
        if self.candles.len() > self.period + 1 {
            self.candles.remove(0);
        }

        if self.candles.len() < self.period {
            return 50.0;
        }

        let start_idx = self.candles.len() - self.period;
        let mut sum_tr = 0.0;
        let mut highest_high = f64::MIN;
        let mut lowest_low = f64::MAX;

        for i in start_idx..self.candles.len() {
            let c = &self.candles[i];
            if c.high > highest_high { highest_high = c.high; }
            if c.low < lowest_low { lowest_low = c.low; }

            let prev_c = if i > 0 { &self.candles[i - 1] } else { c };
            let tr1 = c.high - c.low;
            let tr2 = (c.high - prev_c.close).abs();
            let tr3 = (c.low - prev_c.close).abs();
            let tr = tr1.max(tr2).max(tr3);
            sum_tr += tr;
        }

        let range = highest_high - lowest_low;
        if range <= 1e-9 || sum_tr <= 1e-9 {
            return 50.0;
        }

        let n = self.period as f64;
        let ratio = sum_tr / range;
        if ratio <= 0.0 {
            return 50.0;
        }
        let chop = 100.0 * ratio.log10() / n.log10();
        chop.clamp(0.0, 100.0)
    }
}

#[derive(Debug, Clone)]
pub struct HtfAggregator {
    current_bucket: i64,
    current_high: f64,
    current_low: f64,
    current_close: f64,
    htf_ema: Ema,
    htf_supertrend: Supertrend,
    ema_trend_5m: Ema,
    pub last_bias: i8,
}

impl HtfAggregator {
    pub fn new() -> Self {
        Self {
            current_bucket: 0,
            current_high: 0.0,
            current_low: 0.0,
            current_close: 0.0,
            htf_ema: Ema::new(10),
            htf_supertrend: Supertrend::new(7, 3.0),
            ema_trend_5m: Ema::new(120),
            last_bias: 0,
        }
    }

    pub fn update(&mut self, candle: &Candle) -> i8 {
        let ema_5m = self.ema_trend_5m.update(candle.close);
        let bucket = candle.open_time / 3_600_000;

        if bucket != self.current_bucket && self.current_bucket != 0 {
            self.htf_ema.update(self.current_close);
            self.htf_supertrend.update(self.current_high, self.current_low, self.current_close);

            self.current_bucket = bucket;
            self.current_high = candle.high;
            self.current_low = candle.low;
            self.current_close = candle.close;
        } else {
            if self.current_bucket == 0 {
                self.current_bucket = bucket;
                self.current_high = candle.high;
                self.current_low = candle.low;
            } else {
                self.current_high = self.current_high.max(candle.high);
                self.current_low = self.current_low.min(candle.low);
            }
            self.current_close = candle.close;
        }

        let bias = if let Some(htf_ema_val) = self.htf_ema.value() {
            if candle.close > htf_ema_val {
                1
            } else if candle.close < htf_ema_val {
                -1
            } else {
                0
            }
        } else if let Some(e5m) = ema_5m {
            if candle.close > e5m {
                1
            } else if candle.close < e5m {
                -1
            } else {
                0
            }
        } else if let Some((_st_val, st_dir)) = self.htf_supertrend.value() {
            st_dir
        } else {
            0
        };

        self.last_bias = bias;
        bias
    }
}

#[derive(Debug, Clone)]
pub struct IndicatorEngine {
    pub supertrend: Supertrend,
    pub ema_fast: Ema,
    pub ema_slow: Ema,
    pub ema_200: Ema,
    pub rsi: Rsi,
    pub volume_sma: VolumeSma,
    pub chop: ChoppinessIndex,
    pub htf: HtfAggregator,
    pub recent_candles: Vec<Candle>,
    pub rolling_lookback: usize,
    pub cascade_engine: CascadeEngine,
}

impl IndicatorEngine {
    pub fn new(
        st_period: usize,
        st_multiplier: f64,
        ema_fast_p: usize,
        ema_slow_p: usize,
        rsi_p: usize,
        volume_p: usize,
    ) -> Self {
        Self {
            supertrend: Supertrend::new(st_period, st_multiplier),
            ema_fast: Ema::new(ema_fast_p),
            ema_slow: Ema::new(ema_slow_p),
            ema_200: Ema::new(200),
            rsi: Rsi::new(rsi_p),
            volume_sma: VolumeSma::new(volume_p),
            chop: ChoppinessIndex::new(14),
            htf: HtfAggregator::new(),
            recent_candles: Vec::with_capacity(300),
            rolling_lookback: 48, // Padrão de 12 horas (48 velas de 15m)
            cascade_engine: CascadeEngine::new(CascadeConfig::default()),
        }
    }

    pub fn set_rolling_lookback(&mut self, lookback: usize) {
        self.rolling_lookback = lookback.max(1);
    }

    pub fn warmup(&mut self, candles: &[Candle]) {
        for candle in candles {
            self.update(candle);
        }
    }

    pub fn update(&mut self, candle: &Candle) -> IndicatorValues {
        let st_res = self.supertrend.update(candle.high, candle.low, candle.close);
        let ema_f = self.ema_fast.update(candle.close).unwrap_or(candle.close);
        let ema_s = self.ema_slow.update(candle.close).unwrap_or(candle.close);
        let ema_200_val = self.ema_200.update(candle.close).unwrap_or(candle.close);
        let rsi_val = self.rsi.update(candle.close).unwrap_or(50.0);
        let atr_val = self.supertrend.current_atr().unwrap_or(candle.high - candle.low);
        let vol_sma = self.volume_sma.update(candle.volume);
        let is_vol_high = candle.volume >= vol_sma;
        let chop_val = self.chop.update(candle);
        let htf_bias = self.htf.update(candle);

        let (st_val, st_dir) = st_res.unwrap_or((candle.close, 1));
        let trend_filter_bullish = candle.close >= ema_200_val;

        // Mantém janela deslizante de até 300 candles para cálculo estável do Range Móvel de 96 períodos (24h)
        self.recent_candles.push(candle.clone());
        if self.recent_candles.len() > 300 {
            self.recent_candles.remove(0);
        }

        let n_c = self.recent_candles.len();
        // Donchian 20 de Entrada (exclui a vela atual recém-inserida para evitar lookahead)
        let (donchian_high_20, donchian_low_20) = if n_c >= 21 {
            let start = n_c - 21;
            let end = n_c - 1;
            let mut h = f64::MIN;
            let mut l = f64::MAX;
            for c in &self.recent_candles[start..end] {
                if c.high > h { h = c.high; }
                if c.low < l { l = c.low; }
            }
            (h, l)
        } else {
            (candle.high, candle.low)
        };

        // Donchian 10 de Saída Estrutural (exclui a vela atual)
        let (donchian_exit_low_10, donchian_exit_high_10) = if n_c >= 11 {
            let start = n_c - 11;
            let end = n_c - 1;
            let mut h = f64::MIN;
            let mut l = f64::MAX;
            for c in &self.recent_candles[start..end] {
                if c.high > h { h = c.high; }
                if c.low < l { l = c.low; }
            }
            (l, h)
        } else {
            (candle.low, candle.high)
        };

        // Range Móvel Configurável (ex: 48 candles = 12 horas ou 96 candles = 24 horas anteriores à vela atual)
        let lookback = self.rolling_lookback.max(1);
        let (rolling_high_96, rolling_low_96) = if n_c >= lookback + 1 {
            let start = n_c - (lookback + 1);
            let end = n_c - 1;
            let mut h = f64::MIN;
            let mut l = f64::MAX;
            for c in &self.recent_candles[start..end] {
                if c.high > h { h = c.high; }
                if c.low < l { l = c.low; }
            }
            (h, l)
        } else if n_c > 1 {
            let mut h = f64::MIN;
            let mut l = f64::MAX;
            for c in &self.recent_candles[0..n_c - 1] {
                if c.high > h { h = c.high; }
                if c.low < l { l = c.low; }
            }
            (h, l)
        } else {
            (candle.high, candle.low)
        };

        let rolling_range_pct = if rolling_low_96 > 0.0 {
            ((rolling_high_96 - rolling_low_96) / rolling_low_96) * 100.0
        } else {
            0.0
        };

        let market_regime = Self::calculate_market_regime(&self.recent_candles);

        let current_side = if st_dir == 1 { PositionSide::Long } else { PositionSide::Short };
        let mut temp_vals = IndicatorValues {
            supertrend: st_val,
            supertrend_direction: st_dir,
            ema_fast: ema_f,
            ema_slow: ema_s,
            rsi: rsi_val,
            atr: atr_val,
            volume_sma: vol_sma,
            is_volume_high: is_vol_high,
            market_regime: market_regime.clone(),
            chop: chop_val,
            confluence_score: 0.0,
            htf_bias,
            donchian_high_20,
            donchian_low_20,
            donchian_exit_low_10,
            donchian_exit_high_10,
            ema_200: ema_200_val,
            trend_filter_bullish,
            rolling_high_96,
            rolling_low_96,
            rolling_range_pct,
            session_range_high: rolling_high_96,
            session_range_low: rolling_low_96,
            session_range_ready: n_c >= 20,
            ..Default::default()
        };
        let conf_score = Self::calculate_confluence(current_side, candle, &temp_vals);
        temp_vals.confluence_score = conf_score;
        let cascade_telem = self.cascade_engine.update(candle, PositionSide::None, 0.0);
        temp_vals.cascade = cascade_telem;

        temp_vals
    }

    pub fn update_with_position(&mut self, candle: &Candle, pos_side: PositionSide, entry_price: f64) -> IndicatorValues {
        let mut vals = self.update(candle);
        let cascade_telem = self.cascade_engine.update(candle, pos_side, entry_price);
        vals.cascade = cascade_telem;
        vals
    }

    pub fn calculate_confluence(
        target_side: PositionSide,
        candle: &Candle,
        ind: &IndicatorValues,
    ) -> f64 {
        let mut score: f64 = 0.0;
        match target_side {
            PositionSide::Long => {
                // 1. Estrutura de Mercado (peso 0.30)
                if ind.supertrend_direction == 1 { score += 0.15; }
                if ind.ema_fast >= ind.ema_slow { score += 0.10; }
                if candle.close > ind.ema_slow { score += 0.05; }

                // 2. Momento Institucional & KER (peso 0.25)
                if ind.market_regime.net_pct > 0.0 && ind.market_regime.slope_pct > 0.0 {
                    score += 0.10;
                }
                if ind.market_regime.efficiency_ratio >= 0.35 {
                    score += 0.15;
                } else if ind.market_regime.efficiency_ratio >= 0.20 {
                    score += 0.08;
                }

                // 3. Expansão de Volatilidade e Volume (peso 0.20)
                if candle.volume >= ind.volume_sma {
                    score += 0.10;
                } else if candle.volume >= ind.volume_sma * 0.75 {
                    score += 0.05;
                }
                let tr = (candle.high - candle.low).max((candle.high - candle.close).abs());
                if tr >= ind.atr {
                    score += 0.10;
                } else if tr >= ind.atr * 0.75 {
                    score += 0.05;
                }

                // 4. RSI Dinâmico Adaptativo (peso 0.15)
                if ind.rsi >= 48.0 && ind.rsi <= 65.0 {
                    score += 0.15;
                } else if ind.rsi >= 40.0 && ind.rsi <= 72.0 {
                    score += 0.08;
                }

                // 5. Índice de Choppiness (peso 0.10)
                if ind.chop < 45.0 {
                    score += 0.10;
                } else if ind.chop < 55.0 {
                    score += 0.07;
                } else if ind.chop < 60.0 {
                    score += 0.03;
                }
            }
            PositionSide::Short => {
                // 1. Estrutura de Mercado (peso 0.30)
                if ind.supertrend_direction == -1 { score += 0.15; }
                if ind.ema_fast <= ind.ema_slow { score += 0.10; }
                if candle.close < ind.ema_slow { score += 0.05; }

                // 2. Momento Institucional & KER (peso 0.25)
                if ind.market_regime.net_pct < 0.0 && ind.market_regime.slope_pct < 0.0 {
                    score += 0.10;
                }
                if ind.market_regime.efficiency_ratio >= 0.35 {
                    score += 0.15;
                } else if ind.market_regime.efficiency_ratio >= 0.20 {
                    score += 0.08;
                }

                // 3. Expansão de Volatilidade e Volume (peso 0.20)
                if candle.volume >= ind.volume_sma {
                    score += 0.10;
                } else if candle.volume >= ind.volume_sma * 0.75 {
                    score += 0.05;
                }
                let tr = (candle.high - candle.low).max((candle.high - candle.close).abs());
                if tr >= ind.atr {
                    score += 0.10;
                } else if tr >= ind.atr * 0.75 {
                    score += 0.05;
                }

                // 4. RSI Dinâmico Adaptativo (peso 0.15)
                if ind.rsi >= 35.0 && ind.rsi <= 52.0 {
                    score += 0.15;
                } else if ind.rsi >= 28.0 && ind.rsi <= 60.0 {
                    score += 0.08;
                }

                // 5. Índice de Choppiness (peso 0.10)
                if ind.chop < 45.0 {
                    score += 0.10;
                } else if ind.chop < 55.0 {
                    score += 0.07;
                } else if ind.chop < 60.0 {
                    score += 0.03;
                }
            }
            PositionSide::None => {}
        }

        score.clamp(0.0, 1.0)
    }

    /// Cálculo matemático de regime de mercado sobre os últimos 10 candles:
    /// 1. Variação percentual líquida (Net Return %)
    /// 2. Kaufman Efficiency Ratio (KER): Deslocamento direcional / Ruído total
    /// 3. Regressão Linear: Inclinação matemática dos fechamentos
    /// 4. Posição no Range Donchian de 10 períodos (0% fundo a 100% topo)
    pub fn calculate_market_regime(candles: &[Candle]) -> MarketRegimeAnalysis {
        let n = candles.len();
        if n < 2 {
            return MarketRegimeAnalysis::default();
        }

        let c_first = candles[0].close;
        let c_last = candles[n - 1].close;

        // 1. Variação percentual líquida
        let net_pct = if c_first > 0.0 {
            ((c_last - c_first) / c_first) * 100.0
        } else {
            0.0
        };

        // 2. Kaufman Efficiency Ratio (KER): Deslocamento líquido / Caminho total
        let net_displacement = (c_last - c_first).abs();
        let mut total_path = 0.0;
        for i in 1..n {
            total_path += (candles[i].close - candles[i - 1].close).abs();
        }
        let efficiency_ratio = if total_path > 1e-9 {
            (net_displacement / total_path).clamp(0.0, 1.0)
        } else {
            0.0
        };

        // 3. Regressão Linear Simples sobre os preços de fechamento
        let mut sum_x = 0.0;
        let mut sum_x2 = 0.0;
        let mut sum_y = 0.0;
        let mut sum_xy = 0.0;
        let nf = n as f64;

        for (i, c) in candles.iter().enumerate() {
            let x = i as f64;
            let y = c.close;
            sum_x += x;
            sum_x2 += x * x;
            sum_y += y;
            sum_xy += x * y;
        }

        let denom = nf * sum_x2 - sum_x * sum_x;
        let slope = if denom.abs() > 1e-9 {
            (nf * sum_xy - sum_x * sum_y) / denom
        } else {
            0.0
        };
        let slope_pct = if c_last > 0.0 {
            (slope / c_last) * 100.0
        } else {
            0.0
        };

        // 4. Posição no Range (Donchian dos N candles)
        let mut highest_h = candles[0].high;
        let mut lowest_l = candles[0].low;
        for c in candles {
            if c.high > highest_h { highest_h = c.high; }
            if c.low < lowest_l { lowest_l = c.low; }
        }
        let range_span = highest_h - lowest_l;
        let range_position_pct = if range_span > 1e-9 {
            (((c_last - lowest_l) / range_span) * 100.0).clamp(0.0, 100.0)
        } else {
            50.0
        };

        // 5. Confluência matemática para classificação do Regime
        let (regime, description) = if efficiency_ratio < 0.28 || (net_pct.abs() < 0.20 && slope_pct.abs() < 0.025) {
            (
                "LATERALIZADO".to_string(),
                format!(
                    "Mercado sem direção definida nos últimos {} candles: oscilação de {:+.2}% com baixa eficiência ({:.0}%)",
                    n, net_pct, efficiency_ratio * 100.0
                )
            )
        } else if net_pct > 0.15 && slope > 0.0 && (efficiency_ratio >= 0.28 || range_position_pct >= 60.0) {
            (
                "EM ALTA".to_string(),
                format!(
                    "Tendência de Alta nos últimos {} candles: {:+.2}% | Eficiência: {:.0}% | Preço no topo do range ({:.0}%)",
                    n, net_pct, efficiency_ratio * 100.0, range_position_pct
                )
            )
        } else if net_pct < -0.15 && slope < 0.0 && (efficiency_ratio >= 0.28 || range_position_pct <= 40.0) {
            (
                "EM BAIXA".to_string(),
                format!(
                    "Tendência de Baixa nos últimos {} candles: {:+.2}% | Eficiência: {:.0}% | Preço no fundo do range ({:.0}%)",
                    n, net_pct, efficiency_ratio * 100.0, range_position_pct
                )
            )
        } else if net_pct > 0.30 {
            (
                "EM ALTA".to_string(),
                format!("Pressão Compradora nos últimos {} candles: {:+.2}%", n, net_pct)
            )
        } else if net_pct < -0.30 {
            (
                "EM BAIXA".to_string(),
                format!("Pressão Vendedora nos últimos {} candles: {:+.2}%", n, net_pct)
            )
        } else {
            (
                "LATERALIZADO".to_string(),
                format!("Consolidação neutra nos últimos {} candles: variação de {:+.2}%", n, net_pct)
            )
        };

        MarketRegimeAnalysis {
            regime,
            net_pct,
            efficiency_ratio,
            slope_pct,
            range_position_pct,
            description,
        }
    }
}

// ==========================================
// C A S C A D A   P R O P   D E S K   ( M T F   D U P L O )
// ==========================================

pub fn calculate_linreg(prices: &[f64], length: usize) -> f64 {
    let n = prices.len().min(length);
    if n < 2 {
        return *prices.last().unwrap_or(&0.0);
    }
    let slice = &prices[prices.len() - n..];
    let n_f = n as f64;
    let sum_x = (n_f - 1.0) * n_f / 2.0;
    let sum_x2 = (n_f - 1.0) * n_f * (2.0 * n_f - 1.0) / 6.0;
    let mut sum_y = 0.0;
    let mut sum_xy = 0.0;
    for (i, &y) in slice.iter().enumerate() {
        let x = i as f64;
        sum_y += y;
        sum_xy += x * y;
    }
    let denom = n_f * sum_x2 - sum_x * sum_x;
    if denom.abs() < 1e-12 {
        return slice[n - 1];
    }
    let m = (n_f * sum_xy - sum_x * sum_y) / denom;
    let b = (sum_y - m * sum_x) / n_f;
    m * (n_f - 1.0) + b
}

pub fn calculate_atr_slice(candles: &[Candle], period: usize) -> f64 {
    if candles.is_empty() {
        return 0.0;
    }
    let mut atr = Atr::new(period);
    let mut last_val = candles[0].high - candles[0].low;
    for c in candles {
        if let Some(v) = atr.update(c.high, c.low, c.close) {
            last_val = v;
        } else {
            last_val = c.high - c.low;
        }
    }
    last_val
}

#[derive(Debug, Clone)]
pub struct CascadeEngine {
    pub cfg: CascadeConfig,
    pub recent_candles: Vec<Candle>,
    pub candles_1d: Vec<Candle>,
    pub candles_4h: Vec<Candle>,
    current_4h_bucket: i64,
    current_1d_bucket: i64,
    current_4h_candle: Option<Candle>,
    current_1d_candle: Option<Candle>,

    pub d_bias: i8,
    pub d_ns: f64,
    pub h_bias: i8,
    pub h_ns: f64,

    pub slope_ema: Ema,
    pub supertrend: Supertrend,
    pub atr: Atr,
    pub atr_history: Vec<f64>,
    pub rsi: Rsi,
    pub chop: ChoppinessIndex,
    pub volume_sma: VolumeSma,

    pub last_hh: Option<f64>,
    pub last_hl: Option<f64>,
    pub last_lh: Option<f64>,
    pub last_ll: Option<f64>,

    pub active_long_stop: Option<f64>,
    pub active_short_stop: Option<f64>,
    pub last_telemetry: CascadeTelemetry,
}

impl CascadeEngine {
    pub fn new(cfg: CascadeConfig) -> Self {
        let atr_len = cfg.atr_len.max(1);
        let atr_mult = cfg.atr_mult;
        Self {
            cfg,
            recent_candles: Vec::with_capacity(500),
            candles_1d: Vec::with_capacity(200),
            candles_4h: Vec::with_capacity(300),
            current_4h_bucket: 0,
            current_1d_bucket: 0,
            current_4h_candle: None,
            current_1d_candle: None,
            d_bias: 0,
            d_ns: 0.0,
            h_bias: 0,
            h_ns: 0.0,
            slope_ema: Ema::new(3),
            supertrend: Supertrend::new(atr_len, atr_mult),
            atr: Atr::new(atr_len),
            atr_history: Vec::with_capacity(60),
            rsi: Rsi::new(14),
            chop: ChoppinessIndex::new(14),
            volume_sma: VolumeSma::new(20),
            last_hh: None,
            last_hl: None,
            last_lh: None,
            last_ll: None,
            active_long_stop: None,
            active_short_stop: None,
            last_telemetry: CascadeTelemetry::default(),
        }
    }

    pub fn set_config(&mut self, cfg: CascadeConfig) {
        if self.cfg.atr_len != cfg.atr_len || (self.cfg.atr_mult - cfg.atr_mult).abs() > 1e-6 {
            self.supertrend = Supertrend::new(cfg.atr_len.max(1), cfg.atr_mult);
            self.atr = Atr::new(cfg.atr_len.max(1));
        }
        self.cfg = cfg;
    }

    pub fn set_htf_candles(&mut self, d1: Vec<Candle>, h4: Vec<Candle>) {
        if !d1.is_empty() {
            let (bias, ns) = Self::run_htf_engine(&d1, self.cfg.length, self.cfg.atr_len, self.cfg.min_norm_slope);
            self.d_bias = bias;
            self.d_ns = ns;
            self.candles_1d = d1;
        }
        if !h4.is_empty() {
            let (bias, ns) = Self::run_htf_engine(&h4, self.cfg.length, self.cfg.atr_len, self.cfg.min_norm_slope);
            self.h_bias = bias;
            self.h_ns = ns;
            self.candles_4h = h4;
        }
    }

    pub fn run_htf_engine(candles: &[Candle], len: usize, alen: usize, min_norm_slope: f64) -> (i8, f64) {
        if candles.len() < 5 {
            return (0, 0.0);
        }
        let closes: Vec<f64> = candles.iter().map(|c| c.close).collect();
        let base = calculate_linreg(&closes, len);
        let base_3 = if closes.len() >= 4 {
            calculate_linreg(&closes[..closes.len() - 3], len)
        } else {
            base
        };
        let raw_slope = (base - base_3) / 3.0;
        let a = calculate_atr_slice(candles, alen);
        let ns = if a > 0.0 { raw_slope / a } else { 0.0 };
        let bias = if ns > min_norm_slope {
            1
        } else if ns < -min_norm_slope {
            -1
        } else {
            0
        };
        (bias, ns)
    }

    pub fn update_htf_buckets(&mut self, candle: &Candle) {
        // 4h bucket (14,400,000 ms)
        let bucket_4h = candle.open_time / 14_400_000;
        if bucket_4h != self.current_4h_bucket && self.current_4h_bucket != 0 {
            if let Some(c4) = self.current_4h_candle.take() {
                self.candles_4h.push(c4);
                if self.candles_4h.len() > 300 {
                    self.candles_4h.remove(0);
                }
                let (b, ns) = Self::run_htf_engine(&self.candles_4h, self.cfg.length, self.cfg.atr_len, self.cfg.min_norm_slope);
                self.h_bias = b;
                self.h_ns = ns;
            }
            self.current_4h_bucket = bucket_4h;
            self.current_4h_candle = Some(candle.clone());
        } else {
            if self.current_4h_bucket == 0 {
                self.current_4h_bucket = bucket_4h;
                self.current_4h_candle = Some(candle.clone());
            } else if let Some(ref mut c4) = self.current_4h_candle {
                c4.high = c4.high.max(candle.high);
                c4.low = c4.low.min(candle.low);
                c4.close = candle.close;
                c4.volume += candle.volume;
                c4.close_time = candle.close_time;
            }
        }

        // 1D bucket (86,400,000 ms)
        let bucket_1d = candle.open_time / 86_400_000;
        if bucket_1d != self.current_1d_bucket && self.current_1d_bucket != 0 {
            if let Some(c1) = self.current_1d_candle.take() {
                self.candles_1d.push(c1);
                if self.candles_1d.len() > 200 {
                    self.candles_1d.remove(0);
                }
                let (b, ns) = Self::run_htf_engine(&self.candles_1d, self.cfg.length, self.cfg.atr_len, self.cfg.min_norm_slope);
                self.d_bias = b;
                self.d_ns = ns;
            }
            self.current_1d_bucket = bucket_1d;
            self.current_1d_candle = Some(candle.clone());
        } else {
            if self.current_1d_bucket == 0 {
                self.current_1d_bucket = bucket_1d;
                self.current_1d_candle = Some(candle.clone());
            } else if let Some(ref mut c1) = self.current_1d_candle {
                c1.high = c1.high.max(candle.high);
                c1.low = c1.low.min(candle.low);
                c1.close = candle.close;
                c1.volume += candle.volume;
                c1.close_time = candle.close_time;
            }
        }
    }

    pub fn update_pivots(&mut self) {
        let n = self.recent_candles.len();
        let swing_len = 10;
        if n < swing_len * 2 + 1 {
            return;
        }
        let target_idx = n - 1 - swing_len;
        let target_high = self.recent_candles[target_idx].high;
        let target_low = self.recent_candles[target_idx].low;

        let is_pivot_high = (target_idx.saturating_sub(swing_len)..=target_idx + swing_len)
            .all(|i| i == target_idx || self.recent_candles[i].high <= target_high);

        let is_pivot_low = (target_idx.saturating_sub(swing_len)..=target_idx + swing_len)
            .all(|i| i == target_idx || self.recent_candles[i].low >= target_low);

        if is_pivot_high {
            match self.last_hh {
                None => self.last_hh = Some(target_high),
                Some(prev_hh) => {
                    if target_high > prev_hh {
                        self.last_hh = Some(target_high);
                    } else {
                        self.last_lh = Some(target_high);
                    }
                }
            }
        }

        if is_pivot_low {
            match self.last_hl {
                None => self.last_hl = Some(target_low),
                Some(prev_hl) => {
                    if target_low < prev_hl {
                        self.last_hl = Some(target_low);
                    } else {
                        self.last_ll = Some(target_low);
                    }
                }
            }
        }
    }

    pub fn update(&mut self, candle: &Candle, pos_side: PositionSide, entry_price: f64) -> CascadeTelemetry {
        self.update_htf_buckets(candle);
        self.recent_candles.push(candle.clone());
        if self.recent_candles.len() > 500 {
            self.recent_candles.remove(0);
        }
        self.update_pivots();

        let closes: Vec<f64> = self.recent_candles.iter().map(|c| c.close).collect();
        let baseline = calculate_linreg(&closes, self.cfg.length);
        let baseline_3 = if closes.len() >= 4 {
            calculate_linreg(&closes[..closes.len() - 3], self.cfg.length)
        } else {
            baseline
        };
        let raw_slope = (baseline - baseline_3) / 3.0;
        let slope_smooth = self.slope_ema.update(raw_slope).unwrap_or(raw_slope);

        let atr_val = self.atr.update(candle.high, candle.low, candle.close).unwrap_or(candle.high - candle.low);
        self.atr_history.push(atr_val);
        if self.atr_history.len() > 50 {
            self.atr_history.remove(0);
        }
        let atr_sma = if !self.atr_history.is_empty() {
            self.atr_history.iter().sum::<f64>() / self.atr_history.len() as f64
        } else {
            atr_val
        };
        let atr_expansion = if atr_sma > 0.0 { atr_val / atr_sma } else { 1.0 };

        let norm_slope = if atr_val > 0.0 { slope_smooth / atr_val } else { 0.0 };
        let is_uptrend = norm_slope > self.cfg.min_norm_slope;
        let is_downtrend = norm_slope < -self.cfg.min_norm_slope;

        let upper_entry = baseline + (atr_val * self.cfg.sd_entry);
        let lower_entry = baseline - (atr_val * self.cfg.sd_entry);

        let st_res = self.supertrend.update(candle.high, candle.low, candle.close);
        let (st_line, st_dir) = st_res.unwrap_or((candle.close, 1));

        // Trailing stop ratcheting
        let mut active_stop: Option<f64> = None;
        match pos_side {
            PositionSide::Long => {
                let stop = if st_dir == 1 {
                    match self.active_long_stop {
                        Some(prev) => prev.max(st_line),
                        None => st_line,
                    }
                } else {
                    let ep = if entry_price > 0.0 { entry_price } else { candle.close };
                    match self.active_long_stop {
                        Some(prev) => prev.max(ep),
                        None => ep,
                    }
                };
                self.active_long_stop = Some(stop);
                self.active_short_stop = None;
                active_stop = Some(stop);
            }
            PositionSide::Short => {
                let stop = if st_dir == -1 {
                    match self.active_short_stop {
                        Some(prev) => prev.min(st_line),
                        None => st_line,
                    }
                } else {
                    let ep = if entry_price > 0.0 { entry_price } else { candle.close };
                    match self.active_short_stop {
                        Some(prev) => prev.min(ep),
                        None => ep,
                    }
                };
                self.active_short_stop = Some(stop);
                self.active_long_stop = None;
                active_stop = Some(stop);
            }
            PositionSide::None => {
                self.active_long_stop = None;
                self.active_short_stop = None;
            }
        }

        // Market structure score
        let mut structure_score_long = 0.0;
        if let Some(hh) = self.last_hh {
            if candle.close > hh {
                if let Some(hl) = self.last_hl {
                    if candle.low > hl { structure_score_long = 1.0; } else { structure_score_long = 0.7; }
                } else {
                    structure_score_long = 0.7;
                }
            } else if candle.close > hh * 0.995 {
                structure_score_long = 0.5;
            }
        }

        let mut structure_score_short = 0.0;
        if let Some(ll) = self.last_ll {
            if candle.close < ll {
                if let Some(lh) = self.last_lh {
                    if candle.high < lh { structure_score_short = 1.0; } else { structure_score_short = 0.7; }
                } else {
                    structure_score_short = 0.7;
                }
            } else if candle.close < ll * 1.005 {
                structure_score_short = 0.5;
            }
        }

        // Momentum institutional
        let vol_sma = self.volume_sma.update(candle.volume);
        let price_change = if self.recent_candles.len() >= 4 {
            candle.close - self.recent_candles[self.recent_candles.len() - 4].close
        } else {
            0.0
        };

        let momentum_score_long = if price_change > 0.0 && candle.volume > vol_sma * 1.5 {
            1.0
        } else if price_change > 0.0 && candle.volume > vol_sma {
            0.7
        } else if price_change > 0.0 {
            0.4
        } else {
            0.0
        };

        let momentum_score_short = if price_change < 0.0 && candle.volume > vol_sma * 1.5 {
            1.0
        } else if price_change < 0.0 && candle.volume > vol_sma {
            0.7
        } else if price_change < 0.0 {
            0.4
        } else {
            0.0
        };

        // Volatility regime
        let volatility_score = if atr_expansion > 1.3 {
            1.0
        } else if atr_expansion > 1.1 {
            0.8
        } else if atr_expansion > 0.9 {
            0.5
        } else {
            0.0
        };

        // Choppiness
        let chop_val = self.chop.update(candle);
        let strong_trend_regime = chop_val < 50.0;
        let chop_score = if chop_val < 38.2 {
            1.0
        } else if chop_val < 50.0 {
            0.7
        } else if chop_val < 61.8 {
            0.4
        } else {
            0.0
        };

        // RSI Adaptativo
        let rsi_val = self.rsi.update(candle.close).unwrap_or(50.0);
        let exhaustion_score_long = if strong_trend_regime {
            if rsi_val > 50.0 { 1.0 } else if rsi_val > 45.0 { 0.7 } else { 0.4 }
        } else if rsi_val > 40.0 && rsi_val < 70.0 {
            1.0
        } else if rsi_val > 35.0 && rsi_val < 75.0 {
            0.7
        } else if rsi_val > 30.0 && rsi_val < 80.0 {
            0.4
        } else {
            0.0
        };

        let exhaustion_score_short = if strong_trend_regime {
            if rsi_val < 50.0 { 1.0 } else if rsi_val < 55.0 { 0.7 } else { 0.4 }
        } else if rsi_val < 60.0 && rsi_val > 30.0 {
            1.0
        } else if rsi_val < 65.0 && rsi_val > 25.0 {
            0.7
        } else if rsi_val < 70.0 && rsi_val > 20.0 {
            0.4
        } else {
            0.0
        };

        // Confluence total
        let confluence_score_long = structure_score_long * 0.30
            + momentum_score_long * 0.25
            + volatility_score * 0.20
            + exhaustion_score_long * 0.15
            + chop_score * 0.10;

        let confluence_score_short = structure_score_short * 0.30
            + momentum_score_short * 0.25
            + volatility_score * 0.20
            + exhaustion_score_short * 0.15
            + chop_score * 0.10;

        // Momentum Ignition
        let candle_range = candle.high - candle.low;
        let ignition_long = self.cfg.use_ignition
            && candle_range > atr_val * self.cfg.ign_atr_mult
            && candle.volume > vol_sma * self.cfg.ign_vol_mult
            && candle.close > candle.open
            && (candle.close - candle.low) > candle_range * 0.6
            && candle.close > baseline
            && confluence_score_long >= 0.5;

        let ignition_short = self.cfg.use_ignition
            && candle_range > atr_val * self.cfg.ign_atr_mult
            && candle.volume > vol_sma * self.cfg.ign_vol_mult
            && candle.close < candle.open
            && (candle.high - candle.close) > candle_range * 0.6
            && candle.close < baseline
            && confluence_score_short >= 0.5;

        // MTF cascade logic
        let align_long = (if self.d_bias == 1 { 1 } else { 0 }) + (if self.h_bias == 1 { 1 } else { 0 });
        let align_short = (if self.d_bias == -1 { 1 } else { 0 }) + (if self.h_bias == -1 { 1 } else { 0 });
        let veto_long = self.d_bias == -1;
        let veto_short = self.d_bias == 1;

        let mtf_ok_long = !self.cfg.use_mtf || (!veto_long && align_long >= 1);
        let mtf_ok_short = !self.cfg.use_mtf || (!veto_short && align_short >= 1);

        let size_factor_long = if !self.cfg.use_mtf { 1.0 } else if align_long >= 2 { 1.0 } else { 0.5 };
        let size_factor_short = if !self.cfg.use_mtf { 1.0 } else if align_short >= 2 { 1.0 } else { 0.5 };

        let setup_long = mtf_ok_long && ((candle.close > upper_entry && is_uptrend && confluence_score_long >= self.cfg.min_confluence) || ignition_long);
        let setup_short = mtf_ok_short && ((candle.close < lower_entry && is_downtrend && confluence_score_short >= self.cfg.min_confluence) || ignition_short);

        let telem = CascadeTelemetry {
            enabled: self.cfg.enabled,
            baseline,
            upper_entry,
            lower_entry,
            norm_slope,
            is_uptrend,
            is_downtrend,
            d_bias: self.d_bias,
            h_bias: self.h_bias,
            align_long,
            align_short,
            veto_long,
            veto_short,
            size_factor_long,
            size_factor_short,
            structure_score_long,
            structure_score_short,
            momentum_score_long,
            momentum_score_short,
            volatility_score,
            chop_val,
            chop_score,
            rsi_val,
            exhaustion_score_long,
            exhaustion_score_short,
            confluence_score_long,
            confluence_score_short,
            min_confluence: self.cfg.min_confluence,
            active_stop,
            ignition_long,
            ignition_short,
            setup_long,
            setup_short,
        };

        self.last_telemetry = telem.clone();
        telem
    }

    pub fn compute_series_for_candles(
        &self,
        candles: &[Candle],
    ) -> (Vec<f64>, Vec<f64>, Vec<f64>, Vec<Option<f64>>) {
        let mut sim = CascadeEngine::new(self.cfg.clone());
        let mut baselines = Vec::with_capacity(candles.len());
        let mut uppers = Vec::with_capacity(candles.len());
        let mut lowers = Vec::with_capacity(candles.len());
        let mut stops = Vec::with_capacity(candles.len());

        for c in candles {
            let telem = sim.update(c, PositionSide::None, 0.0);
            baselines.push(telem.baseline);
            uppers.push(telem.upper_entry);
            lowers.push(telem.lower_entry);
            stops.push(telem.active_stop);
        }

        (baselines, uppers, lowers, stops)
    }
}
