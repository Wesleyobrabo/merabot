use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub exchange: ExchangeConfig,
    pub trading: TradingConfig,
    pub risk: RiskConfig,
    pub strategy: StrategyConfig,
    pub server: ServerConfig,
    #[serde(default)]
    pub notifications: NotificationConfig,
    #[serde(default)]
    pub cascade: CascadeConfig,
    #[serde(default)]
    pub mera: MeraConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExchangeConfig {
    pub api_key: String,
    pub api_secret: String,
    #[serde(default)]
    pub testnet_api_key: String,
    #[serde(default)]
    pub testnet_api_secret: String,
    pub testnet: bool,
    pub base_url: Option<String>,
    pub ws_url: Option<String>,
}

impl ExchangeConfig {
    pub fn get_rest_url(&self) -> &str {
        if let Some(ref url) = self.base_url {
            return url;
        }
        if self.testnet {
            "https://testnet.binancefuture.com"
        } else {
            "https://fapi.binance.com"
        }
    }

    pub fn get_ws_url(&self) -> &str {
        if let Some(ref url) = self.ws_url {
            return url;
        }
        if self.testnet {
            "wss://stream.binancefuture.com/ws"
        } else {
            "wss://fstream.binance.com/ws"
        }
    }

    pub fn get_api_key(&self) -> &str {
        if self.testnet && !self.testnet_api_key.trim().is_empty() {
            &self.testnet_api_key
        } else {
            &self.api_key
        }
    }

    pub fn get_api_secret(&self) -> &str {
        if self.testnet && !self.testnet_api_secret.trim().is_empty() {
            &self.testnet_api_secret
        } else {
            &self.api_secret
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradingConfig {
    pub symbol: String,
    pub interval: String,
    pub leverage: u32,
    pub margin_type: String,
    pub paper_trading: bool,
    pub initial_paper_balance: f64,
    #[serde(default = "default_auto_sync_balance")]
    pub auto_sync_balance: bool,
    #[serde(default = "default_sync_balance_interval")]
    pub sync_balance_interval_secs: u64,
    #[serde(default)]
    pub fetch_real_balance_on_paper: bool,
}

fn default_auto_sync_balance() -> bool {
    true
}

fn default_sync_balance_interval() -> u64 {
    60
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskConfig {
    pub risk_per_trade_percent: f64,
    pub max_drawdown_percent: f64,
    pub max_position_usdt: f64,
    pub min_order_qty: f64,
    pub qty_precision: u32,
    pub price_precision: u32,
    pub sl_atr_multiplier: f64,
    pub tp_atr_multiplier: f64,
    pub slippage_percent: f64,
    #[serde(default = "default_enable_breakeven")]
    pub enable_breakeven: bool,
    #[serde(default = "default_breakeven_atr_multiplier")]
    pub breakeven_atr_multiplier: f64,
    #[serde(default = "default_enable_daily_loss")]
    pub enable_daily_loss_limit: bool,
    #[serde(default = "default_max_daily_loss")]
    pub max_daily_loss_percent: f64,
    #[serde(default = "default_min_notional_usdt")]
    pub min_notional_usdt: f64,
    #[serde(default = "default_stop_loss_percent")]
    pub stop_loss_percent: f64,
}

fn default_stop_loss_percent() -> f64 {
    3.50
}

fn default_min_notional_usdt() -> f64 {
    5.50
}

fn default_enable_breakeven() -> bool {
    true
}

fn default_breakeven_atr_multiplier() -> f64 {
    1.0
}

fn default_enable_daily_loss() -> bool {
    true
}

fn default_max_daily_loss() -> f64 {
    4.0
}

fn default_enable_volume_filter() -> bool {
    true
}

fn default_volume_sma_period() -> usize {
    20
}

fn default_enable_pullback_entry() -> bool {
    true
}

fn default_pullback_rsi_min() -> f64 {
    38.0
}

fn default_pullback_rsi_max() -> f64 {
    62.0
}

fn default_pullback_cooldown_candles() -> usize {
    2
}

fn default_enable_chop_filter() -> bool {
    true
}

fn default_max_chop_threshold() -> f64 {
    60.0
}

fn default_enable_confluence_filter() -> bool {
    true
}

fn default_min_confluence_score() -> f64 {
    0.65
}

fn default_enable_htf_veto() -> bool {
    true
}

fn default_strategy_name() -> String {
    "session_breakout".to_string()
}

fn default_timezone_offset_hours() -> i32 {
    3 // GMT+3 por padrão (horário padrão de corretoras para Gold / Forex)
}

fn default_range_start_hour() -> u32 {
    0
}

fn default_range_end_hour() -> u32 {
    7
}

fn default_close_hour() -> u32 {
    20
}

fn default_sl_mode() -> String {
    "percent".to_string()
}

fn default_sl_percent() -> f64 {
    1.0
}

fn default_one_trade_per_day() -> bool {
    true
}

fn default_breakout_trigger() -> String {
    "close".to_string()
}

fn default_max_trades_per_day() -> usize {
    3
}

fn default_enable_take_profit() -> bool {
    true
}

fn default_tp_percent() -> f64 {
    2.0
}

fn default_enable_trend_filter() -> bool {
    true
}

fn default_ema_trend_period() -> usize {
    200
}

fn default_rolling_lookback_candles() -> usize {
    96
}

fn default_max_trades_24h() -> usize {
    3
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NotificationConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub telegram_bot_token: String,
    #[serde(default)]
    pub telegram_chat_id: String,
    #[serde(default)]
    pub discord_webhook_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyConfig {
    #[serde(default = "default_strategy_name", alias = "name")]
    pub strategy_name: String,
    #[serde(default = "default_timezone_offset_hours")]
    pub timezone_offset_hours: i32,
    #[serde(default = "default_range_start_hour")]
    pub range_start_hour: u32,
    #[serde(default = "default_range_end_hour")]
    pub range_end_hour: u32,
    #[serde(default = "default_close_hour")]
    pub close_hour: u32,
    #[serde(default = "default_sl_mode")]
    pub sl_mode: String,
    #[serde(default = "default_sl_percent")]
    pub sl_percent: f64,
    #[serde(default = "default_one_trade_per_day")]
    pub one_trade_per_day: bool,
    #[serde(default = "default_max_trades_per_day")]
    pub max_trades_per_day: usize,
    #[serde(default = "default_enable_take_profit")]
    pub enable_take_profit: bool,
    #[serde(default = "default_tp_percent")]
    pub tp_percent: f64,
    #[serde(default = "default_enable_trend_filter")]
    pub enable_trend_filter: bool,
    #[serde(default = "default_ema_trend_period")]
    pub ema_trend_period: usize,
    #[serde(default = "default_breakout_trigger")]
    pub breakout_trigger: String,
    #[serde(default = "default_rolling_lookback_candles")]
    pub rolling_lookback_candles: usize,
    #[serde(default = "default_max_trades_24h")]
    pub max_trades_24h: usize,

    pub supertrend_period: usize,
    pub supertrend_multiplier: f64,
    pub ema_fast_period: usize,
    pub ema_slow_period: usize,
    pub rsi_period: usize,
    pub rsi_overbought: f64,
    pub rsi_oversold: f64,
    #[serde(default = "default_enable_volume_filter")]
    pub enable_volume_filter: bool,
    #[serde(default = "default_volume_sma_period")]
    pub volume_sma_period: usize,
    #[serde(default = "default_enable_pullback_entry")]
    pub enable_pullback_entry: bool,
    #[serde(default = "default_pullback_rsi_min")]
    pub pullback_rsi_min: f64,
    #[serde(default = "default_pullback_rsi_max")]
    pub pullback_rsi_max: f64,
    #[serde(default = "default_pullback_cooldown_candles")]
    pub pullback_cooldown_candles: usize,
    #[serde(default = "default_enable_chop_filter")]
    pub enable_chop_filter: bool,
    #[serde(default = "default_max_chop_threshold")]
    pub max_chop_threshold: f64,
    #[serde(default = "default_enable_confluence_filter")]
    pub enable_confluence_filter: bool,
    #[serde(default = "default_min_confluence_score")]
    pub min_confluence_score: f64,
    #[serde(default = "default_enable_htf_veto")]
    pub enable_htf_veto: bool,
    #[serde(default)]
    pub cascade: CascadeConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CascadeConfig {
    #[serde(default = "default_cascade_enabled")]
    pub enabled: bool,
    #[serde(default = "default_cascade_length")]
    pub length: usize,
    #[serde(default = "default_cascade_sd_entry")]
    pub sd_entry: f64,
    #[serde(default = "default_cascade_min_norm_slope")]
    pub min_norm_slope: f64,
    #[serde(default = "default_cascade_use_ignition")]
    pub use_ignition: bool,
    #[serde(default = "default_cascade_ign_atr_mult")]
    pub ign_atr_mult: f64,
    #[serde(default = "default_cascade_ign_vol_mult")]
    pub ign_vol_mult: f64,
    #[serde(default = "default_cascade_use_mtf")]
    pub use_mtf: bool,
    #[serde(default = "default_cascade_min_confluence")]
    pub min_confluence: f64,
    #[serde(default = "default_cascade_atr_len")]
    pub atr_len: usize,
    #[serde(default = "default_cascade_atr_mult")]
    pub atr_mult: f64,
    #[serde(default = "default_cascade_risk_per_trade")]
    pub risk_per_trade: f64,
}

fn default_cascade_enabled() -> bool { true }
fn default_cascade_length() -> usize { 100 }
fn default_cascade_sd_entry() -> f64 { 2.0 }
fn default_cascade_min_norm_slope() -> f64 { 0.10 }
fn default_cascade_use_ignition() -> bool { true }
fn default_cascade_ign_atr_mult() -> f64 { 2.5 }
fn default_cascade_ign_vol_mult() -> f64 { 2.0 }
fn default_cascade_use_mtf() -> bool { true }
fn default_cascade_min_confluence() -> f64 { 0.70 }
fn default_cascade_atr_len() -> usize { 22 }
fn default_cascade_atr_mult() -> f64 { 3.0 }
fn default_cascade_risk_per_trade() -> f64 { 2.0 }

impl Default for CascadeConfig {
    fn default() -> Self {
        Self {
            enabled: default_cascade_enabled(),
            length: default_cascade_length(),
            sd_entry: default_cascade_sd_entry(),
            min_norm_slope: default_cascade_min_norm_slope(),
            use_ignition: default_cascade_use_ignition(),
            ign_atr_mult: default_cascade_ign_atr_mult(),
            ign_vol_mult: default_cascade_ign_vol_mult(),
            use_mtf: default_cascade_use_mtf(),
            min_confluence: default_cascade_min_confluence(),
            atr_len: default_cascade_atr_len(),
            atr_mult: default_cascade_atr_mult(),
            risk_per_trade: default_cascade_risk_per_trade(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub public_url: Option<String>,
    #[serde(default)]
    pub auto_tunnel: bool,
    /// Senha do dashboard (pode ser sobrescrita pela env DASHBOARD_PASSWORD)
    #[serde(default)]
    pub password: Option<String>,
}

impl AppConfig {
    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let toml_str = toml::to_string_pretty(self)
            .context("Falha ao serializar configuracao para TOML")?;
        fs::write(path, toml_str)
            .context("Falha ao gravar arquivo config.toml")?;
        Ok(())
    }

    pub fn load_or_create<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        if path.exists() {
            let content = fs::read_to_string(path)
                .with_context(|| format!("Falha ao ler arquivo de configuracao: {:?}", path))?;
            let mut config: AppConfig = toml::from_str(&content)
                .with_context(|| "Erro ao parsear arquivo config.toml")?;

            // Permite sobrescrever com variaveis de ambiente se existirem
            if let Ok(key) = std::env::var("BINANCE_API_KEY") {
                if !key.trim().is_empty() {
                    config.exchange.api_key = key;
                }
            }
            if let Ok(sec) = std::env::var("BINANCE_API_SECRET") {
                if !sec.trim().is_empty() {
                    config.exchange.api_secret = sec;
                }
            }
            if let Ok(key) = std::env::var("BINANCE_TESTNET_API_KEY") {
                if !key.trim().is_empty() {
                    config.exchange.testnet_api_key = key;
                }
            }
            if let Ok(sec) = std::env::var("BINANCE_TESTNET_API_SECRET") {
                if !sec.trim().is_empty() {
                    config.exchange.testnet_api_secret = sec;
                }
            }

            // Se modo testnet estiver ativo e as chaves de testnet estiverem preenchidas, utiliza-as
            if config.exchange.testnet {
                if !config.exchange.testnet_api_key.trim().is_empty() {
                    config.exchange.api_key = config.exchange.testnet_api_key.clone();
                }
                if !config.exchange.testnet_api_secret.trim().is_empty() {
                    config.exchange.api_secret = config.exchange.testnet_api_secret.clone();
                }
            }

            Ok(config)
        } else {
            let default_config = Self::default();
            let toml_str = toml::to_string_pretty(&default_config)?;
            fs::write(path, toml_str)?;
            Ok(default_config)
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            exchange: ExchangeConfig {
                api_key: "".to_string(),
                api_secret: "".to_string(),
                testnet_api_key: "".to_string(),
                testnet_api_secret: "".to_string(),
                testnet: true,
                base_url: None,
                ws_url: None,
            },
            trading: TradingConfig {
                symbol: "BTCUSDT".to_string(),
                interval: "1h".to_string(),
                leverage: 5,
                margin_type: "ISOLATED".to_string(),
                paper_trading: true,
                initial_paper_balance: 1000.0,
                auto_sync_balance: true,
                sync_balance_interval_secs: 60,
                fetch_real_balance_on_paper: false,
            },
            risk: RiskConfig {
                risk_per_trade_percent: 2.0,
                max_drawdown_percent: 15.0,
                max_position_usdt: 2000.0,
                min_order_qty: 0.001,
                qty_precision: 3,
                price_precision: 2,
                sl_atr_multiplier: 2.0,
                tp_atr_multiplier: 3.0,
                slippage_percent: 0.05, // 0.05% default slippage
                enable_breakeven: true,
                breakeven_atr_multiplier: 1.0,
                enable_daily_loss_limit: true,
                max_daily_loss_percent: 4.0,
                min_notional_usdt: 5.50,
                stop_loss_percent: 3.50,
            },
            strategy: StrategyConfig {
                strategy_name: default_strategy_name(),
                timezone_offset_hours: default_timezone_offset_hours(),
                range_start_hour: default_range_start_hour(),
                range_end_hour: default_range_end_hour(),
                close_hour: default_close_hour(),
                sl_mode: default_sl_mode(),
                sl_percent: default_sl_percent(),
                one_trade_per_day: default_one_trade_per_day(),
                max_trades_per_day: default_max_trades_per_day(),
                enable_take_profit: default_enable_take_profit(),
                tp_percent: default_tp_percent(),
                enable_trend_filter: default_enable_trend_filter(),
                ema_trend_period: default_ema_trend_period(),
                breakout_trigger: default_breakout_trigger(),
                rolling_lookback_candles: default_rolling_lookback_candles(),
                max_trades_24h: default_max_trades_24h(),
                supertrend_period: 10,
                supertrend_multiplier: 3.0,
                ema_fast_period: 9,
                ema_slow_period: 21,
                rsi_period: 14,
                rsi_overbought: 70.0,
                rsi_oversold: 30.0,
                enable_volume_filter: true,
                volume_sma_period: 20,
                enable_pullback_entry: true,
                pullback_rsi_min: 38.0,
                pullback_rsi_max: 62.0,
                pullback_cooldown_candles: 2,
                enable_chop_filter: true,
                max_chop_threshold: 60.0,
                enable_confluence_filter: true,
                min_confluence_score: 0.65,
                enable_htf_veto: true,
                cascade: CascadeConfig::default(),
            },
            server: ServerConfig {
                host: "127.0.0.1".to_string(),
                port: 8080,
                public_url: None,
                auto_tunnel: false,
                password: None,
            },
            notifications: NotificationConfig::default(),
            cascade: CascadeConfig::default(),
            mera: MeraConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeraConfig {
    #[serde(default = "default_mera_enabled")]
    pub enabled: bool,
    #[serde(default = "default_mera_bandwidth")]
    pub bandwidth: f64,
    #[serde(default = "default_mera_mult")]
    pub mult: f64,
    #[serde(default = "default_mera_lookback")]
    pub lookback: usize,
    #[serde(default = "default_mera_volume_mult")]
    pub volume_mult: f64,
    #[serde(default = "default_mera_min_wick_ratio")]
    pub min_wick_ratio: f64,
    #[serde(default = "default_mera_sl_percent")]
    pub sl_percent: f64,
    #[serde(default = "default_mera_tp_partial_percent")]
    pub tp_partial_percent: f64,
    #[serde(default = "default_mera_tp_final_percent")]
    pub tp_final_percent: f64,
    #[serde(default = "default_mera_be_offset_percent")]
    pub be_offset_percent: f64,
    #[serde(default = "default_mera_max_trades_24h")]
    pub max_trades_24h: usize,
    #[serde(default = "default_mera_cooldown_candles")]
    pub cooldown_candles: usize,
    #[serde(default = "default_mera_partial_ratio")]
    pub partial_ratio: f64,
    #[serde(default = "default_mera_min_nw_spread")]
    pub min_nw_spread: f64,
    #[serde(default = "default_mera_max_volume_mult")]
    pub max_volume_mult: f64,
    #[serde(default = "default_mera_use_htf_trend")]
    pub use_htf_trend: bool,
    #[serde(default = "default_mera_ema_trend_period")]
    pub ema_trend_period: usize,
    #[serde(default = "default_mera_use_bimodal")]
    pub use_bimodal: bool,
    #[serde(default = "default_mera_enable_ple")]
    pub enable_ple: bool,
    #[serde(default = "default_mera_ple_fade")]
    pub ple_fade_traps: bool,
    #[serde(default = "default_mera_ple_retest")]
    pub ple_retest_trend: bool,
    #[serde(default = "default_mera_use_hybrid")]
    pub use_hybrid: bool,
    #[serde(default = "default_mera_chop_sl_percent")]
    pub chop_sl_percent: f64,
    #[serde(default = "default_mera_chop_tp_partial_percent")]
    pub chop_tp_partial_percent: f64,
    #[serde(default = "default_mera_chop_tp_final_percent")]
    pub chop_tp_final_percent: f64,
    #[serde(default = "default_mera_chop_be_offset_percent")]
    pub chop_be_offset_percent: f64,
    #[serde(default = "default_mera_enable_drift_filter")]
    pub enable_drift_filter: bool,
    #[serde(default = "default_mera_drift_min_hours")]
    pub drift_min_hours: f64,
}

fn default_mera_enabled() -> bool { true }
fn default_mera_bandwidth() -> f64 { 8.0 }
fn default_mera_mult() -> f64 { 2.2 }
fn default_mera_lookback() -> usize { 150 }
fn default_mera_volume_mult() -> f64 { 0.85 }
fn default_mera_max_volume_mult() -> f64 { 2.5 }
fn default_mera_min_wick_ratio() -> f64 { 0.20 }
fn default_mera_sl_percent() -> f64 { 1.25 }
fn default_mera_tp_partial_percent() -> f64 { 2.80 }
fn default_mera_tp_final_percent() -> f64 { 6.50 }
fn default_mera_be_offset_percent() -> f64 { 0.25 }
fn default_mera_max_trades_24h() -> usize { 10 }
fn default_mera_cooldown_candles() -> usize { 6 }
fn default_mera_partial_ratio() -> f64 { 0.70 }
fn default_mera_min_nw_spread() -> f64 { 0.0030 }
fn default_mera_use_htf_trend() -> bool { true }
fn default_mera_ema_trend_period() -> usize { 200 }
fn default_mera_use_bimodal() -> bool { true }
fn default_mera_enable_ple() -> bool { true }
fn default_mera_ple_fade() -> bool { true }
fn default_mera_ple_retest() -> bool { false }
fn default_mera_use_hybrid() -> bool { true }
fn default_mera_chop_sl_percent() -> f64 { 1.15 }
fn default_mera_chop_tp_partial_percent() -> f64 { 1.05 }
fn default_mera_chop_tp_final_percent() -> f64 { 1.75 }
fn default_mera_chop_be_offset_percent() -> f64 { 0.20 }
fn default_mera_enable_drift_filter() -> bool { true }
fn default_mera_drift_min_hours() -> f64 { 8.0 }

impl Default for MeraConfig {
    fn default() -> Self {
        Self {
            enabled: default_mera_enabled(),
            bandwidth: default_mera_bandwidth(),
            mult: default_mera_mult(),
            lookback: default_mera_lookback(),
            volume_mult: default_mera_volume_mult(),
            max_volume_mult: default_mera_max_volume_mult(),
            min_wick_ratio: default_mera_min_wick_ratio(),
            sl_percent: default_mera_sl_percent(),
            tp_partial_percent: default_mera_tp_partial_percent(),
            tp_final_percent: default_mera_tp_final_percent(),
            be_offset_percent: default_mera_be_offset_percent(),
            max_trades_24h: default_mera_max_trades_24h(),
            cooldown_candles: default_mera_cooldown_candles(),
            partial_ratio: default_mera_partial_ratio(),
            min_nw_spread: default_mera_min_nw_spread(),
            use_htf_trend: default_mera_use_htf_trend(),
            ema_trend_period: default_mera_ema_trend_period(),
            use_bimodal: default_mera_use_bimodal(),
            enable_ple: default_mera_enable_ple(),
            ple_fade_traps: default_mera_ple_fade(),
            ple_retest_trend: default_mera_ple_retest(),
            use_hybrid: default_mera_use_hybrid(),
            chop_sl_percent: default_mera_chop_sl_percent(),
            chop_tp_partial_percent: default_mera_chop_tp_partial_percent(),
            chop_tp_final_percent: default_mera_chop_tp_final_percent(),
            chop_be_offset_percent: default_mera_chop_be_offset_percent(),
            enable_drift_filter: default_mera_enable_drift_filter(),
            drift_min_hours: default_mera_drift_min_hours(),
        }
    }
}
