// language: Rust, file: src/types.rs, target: Windows 11, MSVC
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candle {
    pub open_time: i64,
    pub close_time: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub is_closed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum OrderSide {
    Buy,
    Sell,
}

#[allow(dead_code)]
impl OrderSide {
    pub fn opposite(&self) -> Self {
        match self {
            OrderSide::Buy => OrderSide::Sell,
            OrderSide::Sell => OrderSide::Buy,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum OrderType {
    Market,
    Limit,
    StopMarket,
    TakeProfitMarket,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum OrderStatus {
    New,
    PartiallyFilled,
    Filled,
    Canceled,
    Rejected,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum PositionSide {
    Long,
    Short,
    None,
}

/// CORREÇÃO ADICIONADA: Identifica qual motor abriu a posição
/// para que a estratégia principal possa aplicar a gestão correta
/// (MERA: parciais dinâmicas + BE cirúrgico | Cascade: trailing stop + BE fixo)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum PositionOrigin {
    Mera,
    Cascade,
    Unknown,
}

impl Default for PositionOrigin {
    fn default() -> Self {
        Self::Unknown
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub symbol: String,
    pub side: PositionSide,
    pub amount: f64,
    pub entry_price: f64,
    pub current_price: f64,
    pub unrealized_pnl: f64,
    pub leverage: u32,
    pub stop_loss: Option<f64>,
    pub take_profit: Option<f64>,
    #[serde(default)]
    pub tp_partial: Option<f64>,
    #[serde(default)]
    pub liquidation_price: Option<f64>,
    #[serde(default)]
    pub max_loss_usdt: Option<f64>,
    #[serde(default)]
    pub distance_to_sl_pct: Option<f64>,
    #[serde(default)]
    pub be_locked: bool,
    #[serde(default)]
    pub partial_taken: bool,
    #[serde(default)]
    pub initial_amount: f64,
    /// CORREÇÃO ADICIONADA: Origem da posição (MERA, Cascade ou Unknown)
    #[serde(default)]
    pub origin: PositionOrigin,
}

impl Position {
    pub fn new(symbol: String, leverage: u32) -> Self {
        Self {
            symbol,
            side: PositionSide::None,
            amount: 0.0,
            entry_price: 0.0,
            current_price: 0.0,
            unrealized_pnl: 0.0,
            leverage,
            stop_loss: None,
            take_profit: None,
            tp_partial: None,
            liquidation_price: None,
            max_loss_usdt: None,
            distance_to_sl_pct: None,
            be_locked: false,
            partial_taken: false,
            initial_amount: 0.0,
            origin: PositionOrigin::Unknown,
        }
    }

    pub fn update_pnl(&mut self, mark_price: f64) {
        self.current_price = mark_price;
        if self.amount == 0.0 || self.entry_price == 0.0 {
            self.unrealized_pnl = 0.0;
            self.liquidation_price = None;
            self.max_loss_usdt = None;
            self.distance_to_sl_pct = None;
            return;
        }
        let lev = self.leverage.max(1) as f64;
        match self.side {
            PositionSide::Long => {
                self.unrealized_pnl = (mark_price - self.entry_price) * self.amount;
                let liq = (self.entry_price * (1.0 - (1.0 / lev) + 0.005)).max(0.0);
                self.liquidation_price = Some(liq);
                if let Some(sl) = self.stop_loss {
                    let max_loss = (self.entry_price - sl).max(0.0) * self.amount;
                    self.max_loss_usdt = Some(max_loss);
                    if mark_price > 0.0 {
                        let dist = ((mark_price - sl) / mark_price) * 100.0;
                        self.distance_to_sl_pct = Some(dist);
                    }
                }
            }
            PositionSide::Short => {
                self.unrealized_pnl = (self.entry_price - mark_price) * self.amount;
                let liq = self.entry_price * (1.0 + (1.0 / lev) - 0.005);
                self.liquidation_price = Some(liq);
                if let Some(sl) = self.stop_loss {
                    let max_loss = (sl - self.entry_price).max(0.0) * self.amount;
                    self.max_loss_usdt = Some(max_loss);
                    if mark_price > 0.0 {
                        let dist = ((sl - mark_price) / mark_price) * 100.0;
                        self.distance_to_sl_pct = Some(dist);
                    }
                }
            }
            PositionSide::None => {
                self.unrealized_pnl = 0.0;
                self.liquidation_price = None;
                self.max_loss_usdt = None;
                self.distance_to_sl_pct = None;
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeRecord {
    pub id: String,
    pub symbol: String,
    pub side: OrderSide,
    #[serde(default)]
    pub entry_price: f64,
    pub price: f64,
    pub quantity: f64,
    #[serde(default)]
    pub notional: f64,
    #[serde(default)]
    pub gross_pnl: f64,
    #[serde(default)]
    pub commission: f64,
    pub realized_pnl: f64,
    #[serde(default)]
    pub pnl_percent: f64,
    #[serde(default)]
    pub balance_after: f64,
    pub timestamp: i64,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignalType {
    OpenLong,
    OpenShort,
    CloseLong,
    CloseShort,
    PartialCloseLong,
    PartialCloseShort,
    Hold,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    pub signal_type: SignalType,
    pub price: f64,
    pub timestamp: i64,
    pub reason: String,
    pub stop_loss: Option<f64>,
    pub take_profit: Option<f64>,
    #[serde(default)]
    pub tp_partial: Option<f64>,
    #[serde(default)]
    pub partial_ratio: Option<f64>,
    #[serde(default)]
    pub size_factor: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BotStatus {
    Running,
    Paused,
    EmergencyStopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketRegimeAnalysis {
    pub regime: String,          // "EM ALTA", "EM BAIXA", "LATERALIZADO"
    pub net_pct: f64,             // Variação percentual líquida dos últimos 10 candles
    pub efficiency_ratio: f64,    // Kaufman Efficiency Ratio (0.0 a 1.0)
    pub slope_pct: f64,           // Inclinação da regressão linear normalizada (% por candle)
    pub range_position_pct: f64,  // Posição no range dos 10 candles (0% fundo, 100% topo)
    pub description: String,      // Resumo explicativo curto
}

impl Default for MarketRegimeAnalysis {
    fn default() -> Self {
        Self {
            regime: "LATERALIZADO".to_string(),
            net_pct: 0.0,
            efficiency_ratio: 0.0,
            slope_pct: 0.0,
            range_position_pct: 50.0,
            description: "Aguardando histórico de candles".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IndicatorValues {
    pub supertrend: f64,
    pub supertrend_direction: i8, // 1 = Bullish, -1 = Bearish
    pub ema_fast: f64,
    pub ema_slow: f64,
    pub rsi: f64,
    pub atr: f64,
    pub volume_sma: f64,
    pub is_volume_high: bool,
    #[serde(default)]
    pub market_regime: MarketRegimeAnalysis,
    #[serde(default)]
    pub chop: f64,
    #[serde(default)]
    pub confluence_score: f64,
    #[serde(default)]
    pub htf_bias: i8, // 1 = Bullish, -1 = Bearish, 0 = Neutral
    #[serde(default)]
    pub donchian_high_20: f64,
    #[serde(default)]
    pub donchian_low_20: f64,
    #[serde(default)]
    pub donchian_exit_low_10: f64,
    #[serde(default)]
    pub donchian_exit_high_10: f64,
    #[serde(default)]
    pub session_range_high: f64,
    #[serde(default)]
    pub session_range_low: f64,
    #[serde(default)]
    pub session_range_ready: bool,
    #[serde(default)]
    pub session_phase: String,
    #[serde(default)]
    pub session_traded_today: bool,
    #[serde(default)]
    pub session_trades_today: usize,
    #[serde(default)]
    pub session_max_trades: usize,
    #[serde(default)]
    pub ema_200: f64,
    #[serde(default)]
    pub trend_filter_bullish: bool,
    #[serde(default)]
    pub rolling_high_96: f64,
    #[serde(default)]
    pub rolling_low_96: f64,
    #[serde(default)]
    pub rolling_range_pct: f64,
    #[serde(default)]
    pub trades_last_24h: usize,
    #[serde(default)]
    pub max_trades_24h: usize,
    #[serde(default)]
    pub next_slot_cooldown_mins: Option<i64>,
    #[serde(default)]
    pub cascade: CascadeTelemetry,
    #[serde(default)]
    pub mera: MeraTelemetry,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MeraTelemetry {
    pub enabled: bool,
    pub session_vwap: f64,
    pub vwap_upper: f64,
    pub vwap_lower: f64,
    pub nw_central: f64,
    pub nw_upper: f64,
    pub nw_lower: f64,
    pub nw_slope_pct: f64,
    pub bias_bullish: bool,
    pub bias_bearish: bool,
    pub volume_ratio: f64,
    pub is_volume_ok: bool,
    pub setup_long: bool,
    pub setup_short: bool,
    pub signal_reason: String,
    pub active_stop: Option<f64>,
    pub tp_partial: Option<f64>,
    pub tp_final: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CascadeTelemetry {
    pub enabled: bool,
    pub baseline: f64,
    pub upper_entry: f64,
    pub lower_entry: f64,
    pub norm_slope: f64,
    pub is_uptrend: bool,
    pub is_downtrend: bool,
    pub d_bias: i8,          // Macro 1D (1=Bull, -1=Bear, 0=Neutro)
    pub h_bias: i8,          // Context 4h (1=Bull, -1=Bear, 0=Neutro)
    pub align_long: usize,
    pub align_short: usize,
    pub veto_long: bool,
    pub veto_short: bool,
    pub size_factor_long: f64,
    pub size_factor_short: f64,
    pub structure_score_long: f64,
    pub structure_score_short: f64,
    pub momentum_score_long: f64,
    pub momentum_score_short: f64,
    pub volatility_score: f64,
    pub chop_val: f64,
    pub chop_score: f64,
    pub rsi_val: f64,
    pub exhaustion_score_long: f64,
    pub exhaustion_score_short: f64,
    pub confluence_score_long: f64,
    pub confluence_score_short: f64,
    pub min_confluence: f64,
    pub active_stop: Option<f64>,
    pub ignition_long: bool,
    pub ignition_short: bool,
    pub setup_long: bool,
    pub setup_short: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WalletInfo {
    pub total_balance: f64,
    pub available_balance: f64,
    pub unrealized_pnl: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolRules {
    pub symbol: String,
    pub min_qty: f64,
    pub max_qty: f64,
    pub step_size: f64,
    pub qty_precision: u32,
    pub min_price: f64,
    pub max_price: f64,
    pub tick_size: f64,
    pub price_precision: u32,
    pub min_notional: f64,
    pub max_notional_bracket: Option<f64>,
    #[serde(default)]
    pub max_leverage: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OrderFeasibility {
    pub can_open_unleveraged: bool,
    pub can_open_current_leverage: bool,
    pub current_leverage: u32,
    pub min_notional: f64,
    pub min_order_qty: f64,
    pub current_price: f64,
    pub min_order_cost_unleveraged: f64,
    pub required_margin_current: f64,
    pub available_balance: f64,
    pub min_leverage_required: u32,
    pub max_leverage_allowed: u32,
    pub max_purchasing_power: f64,
    pub diagnosis_message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountState {
    pub balance: f64,
    #[serde(default)]
    pub available_balance: f64,
    pub initial_balance: f64,
    pub equity: f64,
    pub total_trades: usize,
    pub winning_trades: usize,
    pub losing_trades: usize,
    pub max_drawdown: f64,
    pub peak_balance: f64,
    #[serde(default)]
    pub daily_starting_balance: f64,
    #[serde(default)]
    pub daily_realized_pnl: f64,
    #[serde(default)]
    pub daily_drawdown: f64,
    #[serde(default)]
    pub last_daily_reset_day: u32,
}

impl AccountState {
    pub fn new(initial_balance: f64) -> Self {
        Self {
            balance: initial_balance,
            available_balance: initial_balance,
            initial_balance,
            equity: initial_balance,
            total_trades: 0,
            winning_trades: 0,
            losing_trades: 0,
            max_drawdown: 0.0,
            peak_balance: initial_balance,
            daily_starting_balance: initial_balance,
            daily_realized_pnl: 0.0,
            daily_drawdown: 0.0,
            last_daily_reset_day: 0,
        }
    }
}