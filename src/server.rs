use crate::auth::{auth_middleware, login_get, login_post, AuthState};
use crate::config::{AppConfig, CascadeConfig, MeraConfig};
use crate::execution::ExecutionEngine;
use crate::risk::RiskManager;
use crate::strategy::PineSupertrendRsiStrategy;
use crate::types::{AccountState, BotStatus, CascadeTelemetry, IndicatorValues, MeraTelemetry, OrderFeasibility, OrderSide, Position, TradeRecord};
use axum::{
    extract::State,
    middleware,
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info};

#[derive(Clone)]
pub struct AppState {
    pub execution: Arc<RwLock<ExecutionEngine>>,
    pub risk_manager: Arc<RwLock<RiskManager>>,
    pub strategy: Arc<RwLock<PineSupertrendRsiStrategy>>,
    pub status: Arc<RwLock<BotStatus>>,
    pub symbol: String,
    pub is_paper: bool,
    pub auth: Option<Arc<AuthState>>,
}

#[derive(Serialize)]
pub struct StatusResponse {
    pub status: BotStatus,
    pub symbol: String,
    pub paper_trading: bool,
    pub account: AccountState,
    pub position: Position,
    pub indicators: IndicatorValues,
    pub recent_trades: Vec<TradeRecord>,
    pub circuit_breaker_active: bool,
    pub min_notional: f64,
    pub risk_per_trade: f64,
    pub pullback_enabled: bool,
    pub order_feasibility: OrderFeasibility,
}

#[derive(Deserialize)]
pub struct SetLeveragePayload {
    pub leverage: u32,
}

#[derive(Deserialize)]
pub struct SetRiskParamsPayload {
    pub min_notional: Option<f64>,
    pub risk_per_trade: Option<f64>,
}

#[derive(Deserialize)]
pub struct TogglePullbackPayload {
    pub enabled: bool,
}

#[derive(Serialize, Clone, Debug)]
pub struct ChartCandlePoint {
    pub time: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

#[derive(Serialize, Clone, Debug)]
pub struct ChartLinePoint {
    pub time: i64,
    pub value: f64,
}

#[derive(Serialize, Clone, Debug)]
pub struct ChartMarkerPoint {
    pub time: i64,
    pub position: String,
    pub color: String,
    pub shape: String,
    pub text: String,
}

#[derive(Serialize)]
pub struct ChartDataResponse {
    pub symbol: String,
    pub interval: String,
    pub candles: Vec<ChartCandlePoint>,
    pub baseline: Vec<ChartLinePoint>,
    pub upper_entry: Vec<ChartLinePoint>,
    pub lower_entry: Vec<ChartLinePoint>,
    pub active_stop: Vec<ChartLinePoint>,
    pub vwap: Vec<ChartLinePoint>,
    pub nw_central: Vec<ChartLinePoint>,
    pub nw_upper: Vec<ChartLinePoint>,
    pub nw_lower: Vec<ChartLinePoint>,
    pub active_tp: Vec<ChartLinePoint>,
    pub markers: Vec<ChartMarkerPoint>,
    pub cascade: CascadeTelemetry,
    pub cascade_config: CascadeConfig,
    pub mera: MeraTelemetry,
    pub mera_config: MeraConfig,
}

pub async fn start_server(state: AppState, host: &str, port: u16) {
    let app = Router::new()
        .route("/", get(dashboard_html))
        .route("/api/status", get(get_status))
        .route("/api/chart_data", get(get_chart_data))
        .route("/api/action/pause", post(action_pause))
        .route("/api/action/resume", post(action_resume))
        .route("/api/action/emergency_close", post(action_emergency_close))
        .route("/api/action/set_leverage", post(action_set_leverage))
        .route("/api/action/set_risk_params", post(action_set_risk_params))
        .route("/api/action/toggle_pullback", post(action_toggle_pullback))
        .route("/api/action/update_cascade_config", post(action_update_cascade_config))
        .route("/api/action/update_mera_config", post(action_update_mera_config))
        .route("/api/action/sync_balance", post(action_sync_balance))
        .route("/login", get(login_get).post(login_post))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", host, port).parse().unwrap();
    info!("Iniciando Dashboard Web de Gestao em http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn get_status(State(state): State<AppState>) -> impl IntoResponse {
    let exec = state.execution.read().await;
    let risk = state.risk_manager.read().await;
    let strat = state.strategy.read().await;
    let status = *state.status.read().await;

    let recent = exec.trade_history.iter().rev().take(50).cloned().collect();
    let min_notional = risk.min_notional();
    let risk_per_trade = risk.risk_per_trade();
    let pullback_enabled = strat.is_pullback_enabled();

    let last_price = if exec.position.current_price > 0.0 {
        exec.position.current_price
    } else if let Some(ref candle) = exec.last_candle {
        candle.close
    } else if strat.last_indicators().cascade.baseline > 0.0 {
        strat.last_indicators().cascade.baseline
    } else {
        100.0
    };

    let order_feasibility = risk.evaluate_order_feasibility(
        &exec.account,
        last_price,
        exec.config.trading.leverage,
    );

    let resp = StatusResponse {
        status,
        symbol: state.symbol.clone(),
        paper_trading: state.is_paper,
        account: exec.account.clone(),
        position: exec.position.clone(),
        indicators: strat.last_indicators(),
        recent_trades: recent,
        circuit_breaker_active: risk.is_halted(),
        min_notional,
        risk_per_trade,
        pullback_enabled,
        order_feasibility,
    };

    Json(resp)
}

async fn action_pause(State(state): State<AppState>) -> impl IntoResponse {
    let mut st = state.status.write().await;
    *st = BotStatus::Paused;
    info!("Comando Web: Bot colocado em PAUSA.");
    Json(serde_json::json!({ "success": true, "status": "Paused" }))
}

async fn action_resume(State(state): State<AppState>) -> impl IntoResponse {
    let mut st = state.status.write().await;
    *st = BotStatus::Running;
    let mut risk = state.risk_manager.write().await;
    risk.reset_circuit_breaker();
    info!("Comando Web: Bot RETOMADO.");
    Json(serde_json::json!({ "success": true, "status": "Running" }))
}

async fn action_emergency_close(State(state): State<AppState>) -> impl IntoResponse {
    let mut st = state.status.write().await;
    *st = BotStatus::EmergencyStopped;
    let mut exec = state.execution.write().await;
    let _ = exec.emergency_close_all().await;
    info!("Comando Web: EMERGENCIA - Todas as posicoes fechadas!");
    Json(serde_json::json!({ "success": true, "status": "EmergencyStopped" }))
}

async fn action_set_leverage(
    State(state): State<AppState>,
    Json(payload): Json<SetLeveragePayload>,
) -> impl IntoResponse {
    if payload.leverage < 1 || payload.leverage > 125 {
        return Json(serde_json::json!({
            "success": false,
            "error": "A alavancagem deve ser entre 1x e 125x"
        }));
    }

    let mut exec = state.execution.write().await;
    match exec.set_leverage(payload.leverage).await {
        Ok(lev) => {
            info!("Alavancagem alterada com sucesso para {}x na Binance", lev);
            Json(serde_json::json!({ "success": true, "leverage": lev }))
        }
        Err(e) => {
            error!("Falha ao alterar alavancagem na Binance: {}", e);
            Json(serde_json::json!({ "success": false, "error": e.to_string() }))
        }
    }
}

async fn action_set_risk_params(
    State(state): State<AppState>,
    Json(payload): Json<SetRiskParamsPayload>,
) -> impl IntoResponse {
    let mut risk = state.risk_manager.write().await;
    let mut exec = state.execution.write().await;

    if let Some(notional) = payload.min_notional {
        if notional < 5.0 || notional > 500.0 {
            return Json(serde_json::json!({
                "success": false,
                "error": "Min Notional deve ser entre 5.00 e 500.00 USDT"
            }));
        }
        risk.set_min_notional(notional);
        exec.config.risk.min_notional_usdt = notional;
    }

    if let Some(rpt) = payload.risk_per_trade {
        if rpt < 0.5 || rpt > 25.0 {
            return Json(serde_json::json!({
                "success": false,
                "error": "Risco por trade deve ser entre 0.5% e 25.0%"
            }));
        }
        risk.set_risk_per_trade(rpt);
        exec.config.risk.risk_per_trade_percent = rpt;
    }

    info!(
        "Parametros de risco atualizados via Web: Min Notional = {:.2} USDT, Risco = {:.2}%",
        risk.min_notional(), risk.risk_per_trade()
    );

    Json(serde_json::json!({
        "success": true,
        "min_notional": risk.min_notional(),
        "risk_per_trade": risk.risk_per_trade()
    }))
}

async fn action_toggle_pullback(
    State(state): State<AppState>,
    Json(payload): Json<TogglePullbackPayload>,
) -> impl IntoResponse {
    let mut strat = state.strategy.write().await;
    strat.set_pullback_enabled(payload.enabled);
    info!("Comando Web: Pullback Re-entry configurado para {}", payload.enabled);
    Json(serde_json::json!({ "success": true, "pullback_enabled": payload.enabled }))
}

async fn action_sync_balance(State(state): State<AppState>) -> impl IntoResponse {
    let mut exec = state.execution.write().await;
    match exec.sync_live_balance().await {
        Ok(Some(info)) => {
            info!(
                "Sincronizacao de carteira sob demanda (Web): Saldo={:.2} USDT | Livre={:.2} USDT | Equity={:.2} USDT",
                info.total_balance, info.available_balance, exec.account.equity
            );
            Json(serde_json::json!({
                "success": true,
                "balance": info.total_balance,
                "available_balance": info.available_balance,
                "equity": exec.account.equity,
                "is_paper": state.is_paper,
            }))
        }
        Ok(None) => {
            Json(serde_json::json!({
                "success": true,
                "balance": exec.account.balance,
                "available_balance": exec.account.available_balance,
                "equity": exec.account.equity,
                "is_paper": state.is_paper,
            }))
        }
        Err(e) => {
            error!("Erro ao sincronizar saldo com a Binance: {}", e);
            Json(serde_json::json!({ "success": false, "error": e.to_string() }))
        }
    }
}

async fn get_chart_data(State(state): State<AppState>) -> impl IntoResponse {
    let strat = state.strategy.read().await;
    let exec = state.execution.read().await;
    let cascade_cfg = strat.config.cascade.clone();
    let telem = strat.last_indicators().cascade;
    let mera_telem = strat.last_indicators().mera;

    let candles = &strat.engine.recent_candles;
    let (vwaps, centrals, uppers, lowers) = strat.mera_engine.compute_series_for_candles(candles);

    let mut candle_pts = Vec::with_capacity(candles.len());
    let mut vwap_pts = Vec::with_capacity(candles.len());
    let mut nw_central_pts = Vec::with_capacity(candles.len());
    let mut nw_upper_pts = Vec::with_capacity(candles.len());
    let mut nw_lower_pts = Vec::with_capacity(candles.len());
    let mut stop_pts = Vec::with_capacity(candles.len());
    let mut tp_pts = Vec::with_capacity(candles.len());

    let active_sl = exec.position.stop_loss.or(mera_telem.active_stop);
    let active_tp = exec.position.take_profit.or(mera_telem.tp_partial);

    for (i, c) in candles.iter().enumerate() {
        let t = c.open_time / 1000;
        candle_pts.push(ChartCandlePoint {
            time: t,
            open: c.open,
            high: c.high,
            low: c.low,
            close: c.close,
            volume: c.volume,
        });

        if let Some(&vw) = vwaps.get(i) {
            if vw > 0.0 {
                vwap_pts.push(ChartLinePoint { time: t, value: vw });
            }
        }
        if let Some(&nc) = centrals.get(i) {
            if nc > 0.0 {
                nw_central_pts.push(ChartLinePoint { time: t, value: nc });
            }
        }
        if let Some(&nu) = uppers.get(i) {
            if nu > 0.0 {
                nw_upper_pts.push(ChartLinePoint { time: t, value: nu });
            }
        }
        if let Some(&nl) = lowers.get(i) {
            if nl > 0.0 {
                nw_lower_pts.push(ChartLinePoint { time: t, value: nl });
            }
        }

        // Se houver posição aberta, traçar linhas de Stop Loss e Take Profit
        if exec.position.amount > 0.0 {
            if let Some(sl) = active_sl {
                if sl > 0.0 {
                    stop_pts.push(ChartLinePoint { time: t, value: sl });
                }
            }
            if let Some(tp) = active_tp {
                if tp > 0.0 {
                    tp_pts.push(ChartLinePoint { time: t, value: tp });
                }
            }
        }
    }

    let mut markers = Vec::new();
    for tr in &exec.trade_history {
        let t = tr.timestamp / 1000;
        let is_buy = tr.side == OrderSide::Buy;
        markers.push(ChartMarkerPoint {
            time: t,
            position: if is_buy { "belowBar".to_string() } else { "aboveBar".to_string() },
            color: if is_buy { "#30d158".to_string() } else { "#ff453a".to_string() },
            shape: if is_buy { "arrowUp".to_string() } else { "arrowDown".to_string() },
            text: format!("{} @ {:.2}", if is_buy { "COMPRA" } else { "VENDA" }, tr.price),
        });
    }

    let interval = exec.config.trading.interval.clone();

    let resp = ChartDataResponse {
        symbol: state.symbol.clone(),
        interval,
        candles: candle_pts,
        baseline: nw_central_pts.clone(),
        upper_entry: nw_upper_pts.clone(),
        lower_entry: nw_lower_pts.clone(),
        active_stop: stop_pts,
        vwap: vwap_pts,
        nw_central: nw_central_pts,
        nw_upper: nw_upper_pts,
        nw_lower: nw_lower_pts,
        active_tp: tp_pts,
        markers,
        cascade: telem,
        cascade_config: cascade_cfg,
        mera: mera_telem,
        mera_config: strat.mera_engine.config.clone(),
    };

    Json(resp)
}

async fn action_update_mera_config(
    State(state): State<AppState>,
    Json(payload): Json<MeraConfig>,
) -> impl IntoResponse {
    {
        let mut strat = state.strategy.write().await;
        strat.mera_engine.set_config(payload.clone());
    }

    if let Ok(mut config) = AppConfig::load_or_create("config.toml") {
        config.mera = payload.clone();
        let _ = config.save_to_file("config.toml");
    }

    info!("[MERA CONFIG] Parâmetros do motor MERA atualizados com sucesso via Dashboard.");
    Json(serde_json::json!({ "success": true, "config": payload }))
}

async fn action_update_cascade_config(
    State(state): State<AppState>,
    Json(payload): Json<CascadeConfig>,
) -> impl IntoResponse {
    {
        let mut strat = state.strategy.write().await;
        strat.update_cascade_config(payload.clone());
    }

    if let Ok(mut config) = AppConfig::load_or_create("config.toml") {
        config.strategy.cascade = payload.clone();
        config.cascade = payload.clone();
        let _ = config.save_to_file("config.toml");
    }

    info!("[CASCADE CONFIG] Parâmetros da Cascata MTF atualizados com sucesso via Dashboard.");
    Json(serde_json::json!({ "success": true, "config": payload }))
}

async fn dashboard_html() -> impl IntoResponse {
    Html(DASHBOARD_HTML_CONTENT)
}

const DASHBOARD_HTML_CONTENT: &str = include_str!("dashboard.html");
