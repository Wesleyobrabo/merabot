use crate::auth::{auth_middleware, login_get, login_post, AuthState};
use crate::config::{AppConfig, CascadeConfig};
use crate::execution::ExecutionEngine;
use crate::risk::RiskManager;
use crate::strategy::PineSupertrendRsiStrategy;
use crate::types::{AccountState, BotStatus, CascadeTelemetry, IndicatorValues, OrderFeasibility, OrderSide, Position, TradeRecord};
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

#[derive(Serialize)]
pub struct ChartCandlePoint {
    pub time: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

#[derive(Serialize)]
pub struct ChartLinePoint {
    pub time: i64,
    pub value: f64,
}

#[derive(Serialize)]
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
    pub markers: Vec<ChartMarkerPoint>,
    pub cascade: CascadeTelemetry,
    pub cascade_config: CascadeConfig,
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

    let candles = &strat.engine.recent_candles;
    let (baselines, uppers, lowers, stops) = strat.engine.cascade_engine.compute_series_for_candles(candles);

    let mut candle_pts = Vec::with_capacity(candles.len());
    let mut baseline_pts = Vec::with_capacity(candles.len());
    let mut upper_pts = Vec::with_capacity(candles.len());
    let mut lower_pts = Vec::with_capacity(candles.len());
    let mut stop_pts = Vec::with_capacity(candles.len());

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

        if let Some(&b) = baselines.get(i) {
            if b > 0.0 {
                baseline_pts.push(ChartLinePoint { time: t, value: b });
            }
        }
        if let Some(&u) = uppers.get(i) {
            if u > 0.0 {
                upper_pts.push(ChartLinePoint { time: t, value: u });
            }
        }
        if let Some(&l) = lowers.get(i) {
            if l > 0.0 {
                lower_pts.push(ChartLinePoint { time: t, value: l });
            }
        }
        if let Some(Some(s)) = stops.get(i) {
            if *s > 0.0 {
                stop_pts.push(ChartLinePoint { time: t, value: *s });
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
        baseline: baseline_pts,
        upper_entry: upper_pts,
        lower_entry: lower_pts,
        active_stop: stop_pts,
        markers,
        cascade: telem,
        cascade_config: cascade_cfg,
    };

    Json(resp)
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

const DASHBOARD_HTML_CONTENT: &str = r##"<!DOCTYPE html>
<html lang="pt-BR">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0, maximum-scale=1.0, user-scalable=no, viewport-fit=cover">
    <title>Binance Futures — Terminal Apple Pro</title>
    <link rel="preconnect" href="https://fonts.googleapis.com">
    <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
    <link href="https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600;700;800&family=JetBrains+Mono:wght@400;500;600;700&display=swap" rel="stylesheet">
    <script src="https://unpkg.com/lightweight-charts@4.2.1/dist/lightweight-charts.standalone.production.js"></script>
    <style>
        :root {
            /* Apple Dark Mode Palette */
            --bg-base: #000000;
            --bg-surface: rgba(28, 28, 30, 0.78);
            --bg-elevated: rgba(44, 44, 46, 0.75);
            --bg-card: rgba(30, 30, 32, 0.65);
            --bg-glass: rgba(255, 255, 255, 0.04);
            
            --border-subtle: rgba(255, 255, 255, 0.08);
            --border-focus: rgba(100, 210, 255, 0.4);
            
            --text-primary: #f5f5f7;
            --text-secondary: #a1a1a6;
            --text-tertiary: #6e6e73;
            
            /* Apple System Accents */
            --green: #30d158;
            --green-glow: rgba(48, 209, 88, 0.25);
            --red: #ff453a;
            --red-glow: rgba(255, 69, 58, 0.25);
            --blue: #0a84ff;
            --blue-glow: rgba(10, 132, 255, 0.25);
            --cyan: #64d2ff;
            --cyan-glow: rgba(100, 210, 255, 0.25);
            --amber: #ff9f0a;
            --amber-glow: rgba(255, 159, 10, 0.25);
            --purple: #bf5af2;

            /* Spatial Tokens */
            --radius-xs: 6px;
            --radius-sm: 10px;
            --radius-md: 14px;
            --radius-lg: 20px;
            --radius-pill: 9999px;

            --font-sans: -apple-system, BlinkMacSystemFont, "SF Pro Display", "SF Pro Text", "Inter", sans-serif;
            --font-mono: "SF Mono", "JetBrains Mono", ui-monospace, Menlo, Consolas, monospace;
        }

        * {
            box-sizing: border-box;
            margin: 0;
            padding: 0;
            -webkit-tap-highlight-color: transparent;
        }

        body {
            background-color: var(--bg-base);
            color: var(--text-primary);
            font-family: var(--font-sans);
            -webkit-font-smoothing: antialiased;
            -moz-osx-font-smoothing: grayscale;
            text-rendering: optimizeLegibility;
            min-height: 100vh;
            padding: 12px;
            padding-bottom: calc(28px + env(safe-area-inset-bottom));
            line-height: 1.45;
            background-image: 
                radial-gradient(ellipse 60% 40% at 20% 0%, rgba(10, 132, 255, 0.08) 0%, transparent 60%),
                radial-gradient(ellipse 50% 35% at 80% 100%, rgba(48, 209, 88, 0.06) 0%, transparent 60%);
            background-attachment: fixed;
        }

        @media (min-width: 768px) {
            body { padding: 24px; }
        }

        .container {
            max-width: 1280px;
            margin: 0 auto;
            display: flex;
            flex-direction: column;
            gap: 16px;
        }

        /* APPLE FROSTED GLASS BAR */
        .topbar {
            background: var(--bg-surface);
            border: 1px solid var(--border-subtle);
            border-radius: var(--radius-lg);
            padding: 14px 18px;
            display: flex;
            flex-direction: column;
            gap: 12px;
            box-shadow: 0 8px 32px rgba(0, 0, 0, 0.45);
            backdrop-filter: blur(28px) saturate(180%);
            -webkit-backdrop-filter: blur(28px) saturate(180%);
        }

        @media (min-width: 768px) {
            .topbar {
                flex-direction: row;
                align-items: center;
                justify-content: space-between;
                padding: 14px 22px;
            }
        }

        .brand-area {
            display: flex;
            align-items: center;
            justify-content: space-between;
            gap: 12px;
            flex-wrap: wrap;
            min-width: 0;
        }

        .brand-title {
            display: inline-flex;
            align-items: center;
            gap: 10px;
            font-size: 1.15rem;
            font-weight: 700;
            color: #ffffff;
            letter-spacing: -0.025em;
            white-space: nowrap;
        }

        .live-dot {
            width: 8px;
            height: 8px;
            border-radius: 50%;
            background: var(--green);
            box-shadow: 0 0 10px var(--green);
            animation: applePulse 2s cubic-bezier(0.4, 0, 0.6, 1) infinite;
        }

        @keyframes applePulse {
            0%, 100% { opacity: 1; transform: scale(1); }
            50% { opacity: 0.55; transform: scale(1.15); }
        }

        .badges-group {
            display: flex;
            align-items: center;
            gap: 6px;
            flex-wrap: wrap;
            min-width: 0;
        }

        .badge {
            font-family: var(--font-mono);
            font-size: 0.68rem;
            font-weight: 600;
            padding: 4px 10px;
            border-radius: var(--radius-pill);
            text-transform: uppercase;
            letter-spacing: 0.04em;
            white-space: nowrap;
            line-height: 1.25;
            display: inline-flex;
            align-items: center;
            gap: 4px;
            transition: all 0.2s ease;
        }

        .badge-symbol {
            background: rgba(100, 210, 255, 0.12);
            color: var(--cyan);
            border: 1px solid rgba(100, 210, 255, 0.28);
        }

        .badge-mode {
            background: rgba(191, 90, 242, 0.12);
            color: var(--purple);
            border: 1px solid rgba(191, 90, 242, 0.28);
        }

        .badge-live {
            background: rgba(48, 209, 88, 0.12);
            color: var(--green);
            border: 1px solid rgba(48, 209, 88, 0.28);
        }

        .status-running {
            background: rgba(48, 209, 88, 0.15);
            color: var(--green);
            border: 1px solid rgba(48, 209, 88, 0.35);
        }

        .status-paused {
            background: rgba(255, 159, 10, 0.15);
            color: var(--amber);
            border: 1px solid rgba(255, 159, 10, 0.35);
        }

        .status-stopped {
            background: rgba(255, 69, 58, 0.15);
            color: var(--red);
            border: 1px solid rgba(255, 69, 58, 0.35);
        }

        /* ACTIONS GROUP */
        .actions-group {
            display: grid;
            grid-template-columns: repeat(3, 1fr);
            gap: 8px;
            width: 100%;
        }

        @media (min-width: 768px) {
            .actions-group {
                display: flex;
                width: auto;
            }
        }

        .btn {
            font-family: var(--font-sans);
            font-size: 0.8rem;
            font-weight: 600;
            padding: 9px 16px;
            border-radius: var(--radius-sm);
            border: 1px solid transparent;
            cursor: pointer !important;
            touch-action: manipulation;
            -webkit-tap-highlight-color: transparent;
            display: inline-flex;
            align-items: center;
            justify-content: center;
            gap: 6px;
            transition: all 0.2s cubic-bezier(0.16, 1, 0.3, 1);
            user-select: none;
            white-space: nowrap;
            box-shadow: 0 2px 8px rgba(0, 0, 0, 0.2);
        }

        .btn:hover {
            transform: translateY(-1px);
            filter: brightness(1.12);
        }

        .btn:active {
            transform: scale(0.95) !important;
        }

        .btn-pause {
            background: rgba(255, 159, 10, 0.12);
            color: var(--amber);
            border-color: rgba(255, 159, 10, 0.3);
        }

        .btn-pause:hover {
            background: rgba(255, 159, 10, 0.24);
            border-color: var(--amber);
            box-shadow: 0 0 16px var(--amber-glow);
            color: #ffffff;
        }

        .btn-resume {
            background: rgba(48, 209, 88, 0.14);
            color: var(--green);
            border-color: rgba(48, 209, 88, 0.3);
        }

        .btn-resume:hover {
            background: var(--green);
            color: #000000;
            box-shadow: 0 0 16px var(--green-glow);
        }

        .btn-panic {
            background: rgba(255, 69, 58, 0.14);
            color: var(--red);
            border-color: rgba(255, 69, 58, 0.35);
        }

        .btn-panic:hover {
            background: var(--red);
            color: #ffffff;
            box-shadow: 0 0 16px var(--red-glow);
        }

        .badge-btn {
            cursor: pointer !important;
            touch-action: manipulation;
            -webkit-tap-highlight-color: transparent;
            background: rgba(100, 210, 255, 0.12);
            color: var(--cyan);
            border: 1px solid rgba(100, 210, 255, 0.3);
            border-radius: var(--radius-pill);
            padding: 4px 10px;
            font-family: var(--font-mono);
            font-size: 0.68rem;
            font-weight: 600;
            display: inline-flex;
            align-items: center;
            gap: 4px;
            transition: all 0.2s ease;
            user-select: none;
        }

        .badge-btn:hover {
            background: rgba(100, 210, 255, 0.22);
            border-color: var(--cyan);
            transform: translateY(-1px);
            filter: brightness(1.15);
        }

        .badge-btn:active {
            transform: scale(0.94);
        }

        .btn-edit-pill {
            background: rgba(255, 255, 255, 0.08);
            border: 1px solid rgba(255, 255, 255, 0.12);
            color: var(--text-secondary);
            font-family: var(--font-sans);
            font-size: 0.68rem;
            font-weight: 600;
            padding: 3px 8px;
            border-radius: var(--radius-pill);
            cursor: pointer !important;
            transition: all 0.15s ease;
            touch-action: manipulation;
            display: inline-flex;
            align-items: center;
            gap: 3px;
        }

        .btn-edit-pill:hover {
            background: rgba(100, 210, 255, 0.2);
            border-color: var(--cyan);
            color: #ffffff;
        }

        .btn-edit-pill:active {
            transform: scale(0.92);
        }

        .tf-btn-group {
            display: flex;
            align-items: center;
            gap: 3px;
            background: rgba(0, 0, 0, 0.4);
            padding: 3px;
            border-radius: var(--radius-pill);
            border: 1px solid var(--border-subtle);
        }

        .tf-btn {
            background: transparent;
            border: none;
            color: var(--text-secondary);
            font-family: var(--font-mono);
            font-size: 0.72rem;
            font-weight: 700;
            padding: 4px 8px;
            border-radius: var(--radius-pill);
            cursor: pointer !important;
            touch-action: manipulation;
            transition: all 0.15s ease;
        }

        .tf-btn:hover {
            color: #ffffff;
            background: rgba(255, 255, 255, 0.08);
        }

        .tf-btn.active {
            background: var(--cyan);
            color: #000000;
        }

        .apple-toast {
            position: fixed;
            top: 24px;
            left: 50%;
            transform: translateX(-50%) translateY(-100px);
            background: rgba(28, 28, 30, 0.94);
            border: 1px solid rgba(255, 255, 255, 0.18);
            backdrop-filter: blur(28px) saturate(180%);
            -webkit-backdrop-filter: blur(28px) saturate(180%);
            padding: 12px 22px;
            border-radius: var(--radius-pill);
            box-shadow: 0 12px 40px rgba(0, 0, 0, 0.6);
            z-index: 100000;
            font-size: 0.82rem;
            font-weight: 700;
            display: flex;
            align-items: center;
            gap: 10px;
            color: #ffffff;
            opacity: 0;
            pointer-events: none;
            transition: all 0.35s cubic-bezier(0.16, 1, 0.3, 1);
        }

        .apple-toast.show {
            transform: translateX(-50%) translateY(0);
            opacity: 1;
            pointer-events: auto;
        }

        /* STATS HERO GRID */
        .stats-grid {
            display: grid;
            grid-template-columns: repeat(2, minmax(0, 1fr));
            gap: 12px;
        }

        @media (min-width: 640px) {
            .stats-grid {
                grid-template-columns: repeat(3, minmax(0, 1fr));
            }
        }

        @media (min-width: 1024px) {
            .stats-grid {
                grid-template-columns: repeat(6, minmax(0, 1fr));
            }
        }

        .stat-card {
            background: var(--bg-card);
            border: 1px solid var(--border-subtle);
            border-radius: var(--radius-md);
            padding: 14px 16px;
            display: flex;
            flex-direction: column;
            justify-content: space-between;
            gap: 6px;
            backdrop-filter: blur(20px);
            -webkit-backdrop-filter: blur(20px);
            min-width: 0;
            overflow: hidden;
            box-shadow: 0 4px 20px rgba(0, 0, 0, 0.25);
            transition: transform 0.2s ease, border-color 0.2s ease;
        }

        .stat-card:hover {
            border-color: rgba(255, 255, 255, 0.18);
        }

        .stat-label {
            font-size: 0.68rem;
            font-weight: 600;
            color: var(--text-tertiary);
            text-transform: uppercase;
            letter-spacing: 0.06em;
            white-space: nowrap;
            overflow: hidden;
            text-overflow: ellipsis;
        }

        .stat-value {
            font-family: var(--font-mono);
            font-size: clamp(1.15rem, 2vw, 1.4rem);
            font-weight: 700;
            color: #ffffff;
            letter-spacing: -0.025em;
            font-variant-numeric: tabular-nums;
            white-space: nowrap;
            overflow: hidden;
            text-overflow: ellipsis;
        }

        .stat-sub {
            font-size: 0.72rem;
            font-family: var(--font-mono);
            color: var(--text-tertiary);
            font-variant-numeric: tabular-nums;
            white-space: nowrap;
            overflow: hidden;
            text-overflow: ellipsis;
        }

        /* POSITION HERO CARD */
        .position-card {
            background: var(--bg-surface);
            border: 1px solid var(--border-subtle);
            border-radius: var(--radius-lg);
            padding: 18px;
            position: relative;
            overflow: hidden;
            box-shadow: 0 8px 32px rgba(0, 0, 0, 0.35);
            backdrop-filter: blur(24px) saturate(180%);
            -webkit-backdrop-filter: blur(24px) saturate(180%);
        }

        .position-card::before {
            content: '';
            position: absolute;
            top: 0; left: 0; right: 0;
            height: 3px;
            background: var(--border-subtle);
        }

        .position-card.pos-long::before {
            background: linear-gradient(90deg, var(--green), transparent);
        }

        .position-card.pos-short::before {
            background: linear-gradient(90deg, var(--red), transparent);
        }

        .card-header {
            display: flex;
            align-items: center;
            justify-content: space-between;
            margin-bottom: 14px;
            flex-wrap: wrap;
            gap: 10px;
        }

        .card-title {
            font-size: 0.95rem;
            font-weight: 700;
            color: #ffffff;
            display: inline-flex;
            align-items: center;
            gap: 8px;
            letter-spacing: -0.015em;
        }

        .pos-hero-box {
            display: grid;
            grid-template-columns: 1fr;
            gap: 14px;
            background: var(--bg-elevated);
            border-radius: var(--radius-md);
            padding: 16px;
            border: 1px solid rgba(255, 255, 255, 0.05);
            margin-bottom: 14px;
        }

        @media (min-width: 640px) {
            .pos-hero-box {
                grid-template-columns: 1.5fr 1fr 1fr;
                align-items: center;
            }
        }

        .pos-pnl-main {
            display: flex;
            flex-direction: column;
            gap: 2px;
        }

        .pos-pnl-val {
            font-family: var(--font-mono);
            font-size: clamp(1.4rem, 2.8vw, 1.75rem);
            font-weight: 800;
            letter-spacing: -0.03em;
            font-variant-numeric: tabular-nums;
        }

        .pos-progress-container {
            margin: 14px 0 8px 0;
            background: rgba(0, 0, 0, 0.45);
            border-radius: var(--radius-sm);
            padding: 10px 14px;
            border: 1px solid rgba(255, 255, 255, 0.05);
        }

        .progress-labels {
            display: flex;
            justify-content: space-between;
            font-family: var(--font-mono);
            font-size: 0.72rem;
            color: var(--text-tertiary);
            margin-bottom: 6px;
            font-variant-numeric: tabular-nums;
        }

        .progress-bar-bg {
            height: 6px;
            background: rgba(255, 255, 255, 0.1);
            border-radius: var(--radius-pill);
            overflow: hidden;
            position: relative;
        }

        .progress-fill {
            height: 100%;
            width: 50%;
            background: var(--cyan);
            border-radius: var(--radius-pill);
            transition: width 0.3s ease;
        }

        .pos-details-grid {
            display: grid;
            grid-template-columns: repeat(2, minmax(0, 1fr));
            gap: 10px;
        }

        @media (min-width: 640px) {
            .pos-details-grid {
                grid-template-columns: repeat(4, minmax(0, 1fr));
            }
        }

        .detail-item {
            background: var(--bg-glass);
            border-radius: var(--radius-sm);
            padding: 10px 12px;
            border: 1px solid rgba(255, 255, 255, 0.05);
            min-width: 0;
        }

        .detail-item span {
            display: block;
            font-size: 0.65rem;
            color: var(--text-tertiary);
            text-transform: uppercase;
            font-weight: 600;
            letter-spacing: 0.04em;
            margin-bottom: 2px;
            white-space: nowrap;
        }

        .detail-item strong {
            font-family: var(--font-mono);
            font-size: 0.88rem;
            color: #ffffff;
            font-variant-numeric: tabular-nums;
            white-space: nowrap;
            overflow: hidden;
            text-overflow: ellipsis;
            display: block;
        }

        /* NEUTRAL PLACEHOLDER */
        .no-pos-box {
            padding: 28px 16px;
            text-align: center;
            display: flex;
            flex-direction: column;
            align-items: center;
            gap: 10px;
            background: rgba(255, 255, 255, 0.015);
            border-radius: var(--radius-md);
            border: 1px dashed rgba(255, 255, 255, 0.1);
        }

        .radar-scan {
            width: 40px;
            height: 40px;
            border-radius: 50%;
            border: 2px solid var(--cyan);
            position: relative;
            animation: appleRadar 2.4s linear infinite;
        }

        @keyframes appleRadar {
            0% { transform: rotate(0deg); box-shadow: 0 0 4px var(--cyan-glow); }
            100% { transform: rotate(360deg); box-shadow: 0 0 18px var(--cyan-glow); }
        }

        .radar-scan::after {
            content: '';
            position: absolute;
            top: 50%; left: 50%;
            width: 5px; height: 5px;
            background: #ffffff;
            border-radius: 50%;
            transform: translate(-50%, -50%);
        }

        /* 2-COLUMN SPLIT: STRATEGY COCKPIT & LIVE CHART */
        .content-split {
            display: grid;
            grid-template-columns: 1fr;
            gap: 16px;
        }

        @media (min-width: 960px) {
            .content-split {
                grid-template-columns: 1fr 1fr;
            }
        }

        .section-card {
            background: var(--bg-surface);
            border: 1px solid var(--border-subtle);
            border-radius: var(--radius-lg);
            padding: 18px;
            backdrop-filter: blur(24px) saturate(180%);
            -webkit-backdrop-filter: blur(24px) saturate(180%);
            box-shadow: 0 8px 32px rgba(0, 0, 0, 0.35);
        }

        /* COCKPIT LIST */
        .indicator-list {
            display: flex;
            flex-direction: column;
            gap: 8px;
        }

        .ind-item {
            display: flex;
            align-items: center;
            justify-content: space-between;
            padding: 10px 14px;
            background: var(--bg-elevated);
            border-radius: var(--radius-sm);
            border: 1px solid rgba(255, 255, 255, 0.04);
            font-size: 0.85rem;
            min-width: 0;
            gap: 10px;
        }

        .ind-name {
            color: var(--text-secondary);
            font-weight: 500;
            display: inline-flex;
            align-items: center;
            gap: 8px;
            white-space: nowrap;
            overflow: hidden;
            text-overflow: ellipsis;
            min-width: 0;
        }

        .ind-val {
            font-family: var(--font-mono);
            font-weight: 700;
            color: #ffffff;
            font-variant-numeric: tabular-nums;
            white-space: nowrap;
            text-align: right;
            flex-shrink: 0;
        }

        /* TRADINGVIEW WRAPPER */
        .chart-wrapper {
            background: var(--bg-surface);
            border: 1px solid var(--border-subtle);
            border-radius: var(--radius-lg);
            overflow: hidden;
            display: flex;
            flex-direction: column;
            backdrop-filter: blur(24px);
            -webkit-backdrop-filter: blur(24px);
            box-shadow: 0 8px 32px rgba(0, 0, 0, 0.35);
        }

        .chart-header {
            padding: 12px 18px;
            display: flex;
            align-items: center;
            justify-content: space-between;
            border-bottom: 1px solid var(--border-subtle);
            background: var(--bg-elevated);
        }

        .chart-toggle-btn {
            background: rgba(255, 255, 255, 0.08);
            border: 1px solid var(--border-subtle);
            color: var(--text-secondary);
            padding: 6px 12px;
            border-radius: var(--radius-pill);
            font-size: 0.75rem;
            font-weight: 600;
            cursor: pointer;
            transition: all 0.2s;
        }

        .chart-toggle-btn:hover {
            color: #ffffff;
            background: rgba(255, 255, 255, 0.14);
        }

        .chart-container {
            height: 390px;
            width: 100%;
            transition: height 0.3s cubic-bezier(0.16, 1, 0.3, 1);
        }

        @media (max-width: 640px) {
            .chart-container {
                height: 310px;
            }
            .chart-container.collapsed {
                height: 0px !important;
            }
        }

        /* EXTRATO DETALHADO BINANCE (APPLE STANDARD) */
        .trades-summary-strip {
            display: grid;
            grid-template-columns: repeat(auto-fit, minmax(130px, 1fr));
            gap: 10px;
            margin-bottom: 14px;
            padding: 12px 14px;
            background: var(--bg-elevated);
            border-radius: var(--radius-md);
            border: 1px solid var(--border-subtle);
        }

        .trade-sum-item {
            display: flex;
            flex-direction: column;
            gap: 3px;
            min-width: 0;
        }

        .trade-sum-label {
            font-size: 0.68rem;
            color: var(--text-tertiary);
            font-weight: 600;
            text-transform: uppercase;
            letter-spacing: 0.05em;
            white-space: nowrap;
            overflow: hidden;
            text-overflow: ellipsis;
        }

        .trade-sum-val {
            font-family: var(--font-mono);
            font-size: 0.92rem;
            font-weight: 700;
            font-variant-numeric: tabular-nums;
            white-space: nowrap;
        }

        .table-responsive {
            width: 100%;
            overflow-x: auto;
            -webkit-overflow-scrolling: touch;
            margin-top: 6px;
            border-radius: var(--radius-sm);
        }

        table {
            width: 100%;
            border-collapse: separate;
            border-spacing: 0;
            font-size: 0.8rem;
            white-space: nowrap;
        }

        th {
            font-family: var(--font-sans);
            font-size: 0.67rem;
            font-weight: 700;
            text-transform: uppercase;
            letter-spacing: 0.06em;
            color: var(--text-tertiary);
            padding: 10px 14px;
            text-align: left;
            border-bottom: 1px solid var(--border-subtle);
            background: rgba(255, 255, 255, 0.02);
            white-space: nowrap;
        }

        td {
            padding: 12px 14px;
            border-bottom: 1px solid rgba(255, 255, 255, 0.04);
            font-family: var(--font-mono);
            color: var(--text-secondary);
            font-variant-numeric: tabular-nums;
            white-space: nowrap;
        }

        tr:last-child td { border-bottom: none; }
        tr:hover td { background: rgba(255, 255, 255, 0.025); }

        .badge-trade-long {
            background: rgba(48, 209, 88, 0.16);
            color: var(--green);
            border: 1px solid rgba(48, 209, 88, 0.32);
            padding: 3px 8px;
            border-radius: var(--radius-pill);
            font-size: 0.70rem;
            font-weight: 700;
            letter-spacing: 0.04em;
            display: inline-block;
        }

        .badge-trade-short {
            background: rgba(255, 69, 58, 0.16);
            color: var(--red);
            border: 1px solid rgba(255, 69, 58, 0.32);
            padding: 3px 8px;
            border-radius: var(--radius-pill);
            font-size: 0.70rem;
            font-weight: 700;
            letter-spacing: 0.04em;
            display: inline-block;
        }

        .badge-trade-reason {
            background: rgba(255, 255, 255, 0.05);
            color: var(--text-secondary);
            border: 1px solid rgba(255, 255, 255, 0.08);
            padding: 3px 8px;
            border-radius: var(--radius-pill);
            font-size: 0.70rem;
            font-weight: 500;
            font-family: var(--font-sans);
            white-space: nowrap;
            display: inline-block;
        }

        /* FOOTER */
        .footer-bar {
            display: flex;
            align-items: center;
            justify-content: space-between;
            font-size: 0.75rem;
            color: var(--text-tertiary);
            padding: 6px 8px;
            flex-wrap: wrap;
            gap: 8px;
        }

        .heartbeat-text {
            font-family: var(--font-mono);
            display: inline-flex;
            align-items: center;
            gap: 8px;
        }

        /* ACCENT UTILITIES */
        .c-up { color: var(--green) !important; }
        .c-down { color: var(--red) !important; }
        .c-cyan { color: var(--cyan) !important; }
        .c-amber { color: var(--amber) !important; }
        .c-purple { color: var(--purple) !important; }

        /* APPLE ACTION SHEET / MODAL */
        .modal-overlay {
            display: none;
            position: fixed;
            top: 0; left: 0; right: 0; bottom: 0;
            background: rgba(0, 0, 0, 0.75);
            backdrop-filter: blur(16px);
            -webkit-backdrop-filter: blur(16px);
            z-index: 9999;
            align-items: center;
            justify-content: center;
            padding: 16px;
        }
        .modal-overlay.active { display: flex; }
        .modal-card {
            background: rgba(36, 36, 38, 0.95);
            border: 1px solid rgba(255, 255, 255, 0.12);
            border-radius: var(--radius-lg);
            width: 100%;
            max-width: 400px;
            padding: 22px;
            box-shadow: 0 20px 60px rgba(0, 0, 0, 0.7);
            animation: appleModalIn 0.28s cubic-bezier(0.16, 1, 0.3, 1);
        }
        @keyframes appleModalIn {
            from { transform: scale(0.95) translateY(12px); opacity: 0; }
            to { transform: scale(1) translateY(0); opacity: 1; }
        }
        .modal-header {
            display: flex;
            align-items: center;
            justify-content: space-between;
            margin-bottom: 14px;
            border-bottom: 1px solid var(--border-subtle);
            padding-bottom: 12px;
        }
        .modal-header h3 {
            font-size: 1.05rem;
            color: #ffffff;
            font-weight: 700;
            display: inline-flex;
            align-items: center;
            gap: 8px;
            letter-spacing: -0.015em;
        }
        .modal-close {
            background: rgba(255, 255, 255, 0.08);
            border: none;
            color: var(--text-secondary);
            font-size: 1.1rem;
            width: 28px;
            height: 28px;
            border-radius: 50%;
            cursor: pointer;
            display: inline-flex;
            align-items: center;
            justify-content: center;
            transition: background 0.15s;
        }
        .modal-close:hover {
            color: #ffffff;
            background: rgba(255, 255, 255, 0.15);
        }
        .quick-lev-grid {
            display: grid;
            grid-template-columns: repeat(3, 1fr);
            gap: 8px;
            margin-bottom: 14px;
        }
        .btn-lev-pill {
            background: rgba(255, 255, 255, 0.05);
            border: 1px solid rgba(255, 255, 255, 0.08);
            color: #ffffff;
            padding: 10px 0;
            border-radius: var(--radius-sm);
            font-family: var(--font-mono);
            font-size: 0.95rem;
            font-weight: 700;
            cursor: pointer;
            transition: all 0.15s ease;
        }
        .btn-lev-pill:hover, .btn-lev-pill.selected {
            background: rgba(100, 210, 255, 0.18);
            border-color: var(--cyan);
            color: var(--cyan);
        }
        .lev-input-row {
            display: flex;
            align-items: center;
            justify-content: space-between;
            background: rgba(0, 0, 0, 0.35);
            padding: 10px 14px;
            border-radius: var(--radius-sm);
            border: 1px solid var(--border-subtle);
            margin-bottom: 14px;
        }
        .lev-input {
            background: rgba(255, 255, 255, 0.06);
            border: 1px solid rgba(255, 255, 255, 0.12);
            color: #ffffff;
            font-family: var(--font-mono);
            font-size: 1.05rem;
            font-weight: 700;
            padding: 6px 12px;
            border-radius: var(--radius-xs);
            width: 80px;
            text-align: center;
        }
        .lev-input:focus {
            outline: none;
            border-color: var(--cyan);
        }
        .btn-apply-lev {
            width: 100%;
            background: linear-gradient(135deg, #0a84ff, #64d2ff);
            color: #000000;
            font-weight: 700;
            font-size: 0.9rem;
            padding: 13px;
            border-radius: var(--radius-pill);
            cursor: pointer;
            border: none;
            transition: filter 0.2s, transform 0.15s;
        }
        .btn-apply-lev:hover {
            filter: brightness(1.08);
        }
        .btn-apply-lev:active {
            transform: scale(0.98);
        }
        .btn-apply-lev:disabled {
            opacity: 0.6;
            cursor: not-allowed;
        }

        /* PROP DESK CASCADE HUD & CHART STYLES */
        .cascade-hud-wrapper {
            background: var(--bg-surface);
            border: 1px solid var(--border-subtle);
            border-radius: var(--radius-lg);
            padding: 16px 18px;
            box-shadow: 0 8px 32px rgba(0, 0, 0, 0.45);
            backdrop-filter: blur(28px) saturate(180%);
            -webkit-backdrop-filter: blur(28px) saturate(180%);
            margin-bottom: 16px;
        }
        .cascade-hud-header {
            display: flex;
            align-items: center;
            justify-content: space-between;
            margin-bottom: 12px;
            flex-wrap: wrap;
            gap: 8px;
        }
        .cascade-hud-grid {
            display: grid;
            grid-template-columns: repeat(auto-fit, minmax(130px, 1fr));
            gap: 10px;
        }
        .cascade-hud-card {
            background: rgba(255, 255, 255, 0.03);
            border: 1px solid rgba(255, 255, 255, 0.06);
            border-radius: var(--radius-sm);
            padding: 10px 12px;
            display: flex;
            flex-direction: column;
            gap: 4px;
        }
        .cascade-hud-lbl {
            font-size: 0.68rem;
            color: var(--text-tertiary);
            text-transform: uppercase;
            font-weight: 600;
            letter-spacing: 0.04em;
        }
        .cascade-hud-val {
            font-family: var(--font-mono);
            font-size: 0.95rem;
            font-weight: 700;
        }
        .chart-legend-strip {
            display: flex;
            align-items: center;
            gap: 14px;
            flex-wrap: wrap;
            padding: 8px 12px;
            background: rgba(0, 0, 0, 0.4);
            border-radius: var(--radius-sm);
            margin-bottom: 10px;
            font-size: 0.75rem;
            font-family: var(--font-mono);
        }
        .legend-item {
            display: flex;
            align-items: center;
            gap: 6px;
        }
        .legend-dot {
            width: 8px;
            height: 8px;
            border-radius: 50%;
            display: inline-block;
        }
    </style>
</head>
<body>
    <div class="container">
        <!-- TOP APP BAR -->
        <header class="topbar">
            <div class="brand-area">
                <div class="brand-title">
                    <span class="live-dot"></span>
                    <span>MERABOT <span style="color:#ffd60a; font-weight:800; font-size:0.90rem; margin-left:6px; letter-spacing:0.04em;">CASCATA TURBO MTF</span></span>
                </div>
                <div class="badges-group">
                    <button id="symbolTag" class="badge badge-symbol badge-btn" onclick="openLeverageModal()" title="Toque para alterar símbolo ou alavancagem">SOLUSDT 15m</button>
                    <span id="modeTag" class="badge badge-mode">PAPER</span>
                    <span id="statusTag" class="badge badge-status status-running">RUNNING</span>
                    <span class="badge" style="background: rgba(48, 209, 88, 0.14); color: #30d158; border: 1px solid rgba(48, 209, 88, 0.3); font-weight: 700;" title="Sincronização Ativa dos Filtros Diário D1 e 4 Horas">MTF D1+4H SYNC</span>
                    <span class="badge" style="background: rgba(100, 210, 255, 0.14); color: var(--cyan); border: 1px solid rgba(100, 210, 255, 0.3); font-weight: 700;" title="Parcial Automática 50% em +2.50% e Breakeven em +0.25%">PARCIAL 50% + BE</span>
                    <button id="btnOpenCascadeModal" class="badge badge-btn" style="background: rgba(255, 214, 10, 0.16); color: #ffd60a; border: 1px solid rgba(255, 214, 10, 0.35); cursor: pointer;" onclick="openCascadeModal()" title="Configurar Parâmetros da Cascata MTF">⚡ Cascata MTF ⚙️</button>
                    <button id="riskTag" class="badge badge-symbol badge-btn" onclick="openRiskModal()" title="Toque para calibrar risco da banca">🛡️ <span id="riskBadgeText">16.0% Risco</span> ✏️</button>
                </div>
            </div>
            <div class="actions-group">
                <button class="btn btn-pause" id="btnPause" onclick="triggerAction('pause')" title="Pausar novas entradas do bot">
                    ⏸ Pausar
                </button>
                <button class="btn btn-resume" id="btnResume" onclick="triggerAction('resume')" title="Retomar operações do bot">
                    ▶ Retomar
                </button>
                <button class="btn btn-panic" id="btnPanic" onclick="openPanicModal()" title="Encerrar todas as posições imediatamente a mercado">
                    🚨 Panic Close
                </button>
            </div>
        </header>

        <!-- STATS HERO METRICS -->
        <section class="stats-grid">
            <div class="stat-card" style="cursor: pointer;" onclick="manualSync()" title="Toque para consultar saldo em tempo real na Binance">
                <div class="stat-label" id="lblBalance">Saldo Carteira</div>
                <div class="stat-value" id="valBalance">$0.00</div>
                <div class="stat-sub" id="subBalanceType">Binance Futures USDT</div>
            </div>
            <div class="stat-card">
                <div class="stat-label">Equity Líquido</div>
                <div class="stat-value" id="valEquity">$0.00</div>
                <div class="stat-sub" id="subAvailable">Livre: $0.00</div>
            </div>
            <div class="stat-card">
                <div class="stat-label">PnL Não Realizado</div>
                <div class="stat-value" id="valUnrealized">$0.00</div>
                <div class="stat-sub" id="subROE">ROE: 0.00%</div>
            </div>
            <div class="stat-card">
                <div class="stat-label">Taxa de Acerto</div>
                <div class="stat-value" id="valWinRate">0.0%</div>
                <div class="stat-sub" id="valTradeCount">0 trades</div>
            </div>
            <div class="stat-card">
                <div class="stat-label">Drawdown Diário</div>
                <div class="stat-value" id="valDailyDD">0.00%</div>
                <div class="stat-sub">Limite: 25.0%</div>
            </div>
            <div class="stat-card">
                <div class="stat-label">Drawdown Global</div>
                <div class="stat-value" id="valMaxDD">0.00%</div>
                <div class="stat-sub" id="valPeakBalance">Topo: $0.00</div>
            </div>
        </section>

        <!-- ACTIVE POSITION RADAR / DETAILS -->
        <section class="position-card" id="posCard">
            <div class="card-header">
                <div class="card-title">
                    <span>📡 Posição no Mercado</span>
                    <span id="posSideBadge" class="badge badge-symbol">NEUTRO</span>
                </div>
                <button id="posLevTag" class="badge badge-mode badge-btn" onclick="openLeverageModal()" title="Toque para alterar alavancagem">
                    ⚡ <span id="posLevText">5x ISOLATED</span> ✏️
                </button>
            </div>

            <!-- Posicao Aberta -->
            <div id="posActiveBox" style="display: none;">
                <div class="pos-hero-box">
                    <div class="pos-pnl-main">
                        <span style="font-size: 0.68rem; color: var(--text-tertiary); text-transform: uppercase; font-weight: 600; letter-spacing: 0.05em;">PnL Não Realizado</span>
                        <div class="pos-pnl-val" id="posHeroPnl">+$0.00</div>
                    </div>
                    <div>
                        <span style="font-size: 0.68rem; color: var(--text-tertiary); text-transform: uppercase; font-weight: 600; letter-spacing: 0.05em;">Preço Mark Atual</span>
                        <div style="font-family: var(--font-mono); font-size: 1.15rem; font-weight: 700; color: #ffffff;" id="posCurrentPrice">$0.00</div>
                    </div>
                    <div>
                        <span style="font-size: 0.68rem; color: var(--text-tertiary); text-transform: uppercase; font-weight: 600; letter-spacing: 0.05em;">Preço de Entrada</span>
                        <div style="font-family: var(--font-mono); font-size: 1.15rem; font-weight: 700; color: var(--text-secondary);" id="posEntryPrice">$0.00</div>
                    </div>
                </div>

                <!-- Barra Dinamica SL -> Entrada -> TP -->
                <div class="pos-progress-container">
                    <div class="progress-labels">
                        <span id="labelSL" class="c-down">SL Supertrend: $0.00</span>
                        <span id="labelEntry" style="color:#ffffff;">Entrada & BE (+0.25%)</span>
                        <span id="labelTP" class="c-cyan">Parcial 50% (+2.50%)</span>
                    </div>
                    <div class="progress-bar-bg">
                        <div class="progress-fill" id="posBarFill"></div>
                    </div>
                </div>

                <div class="pos-details-grid">
                    <div class="detail-item">
                        <span>Quantidade</span>
                        <strong id="posAmount">0.00</strong>
                    </div>
                    <div class="detail-item">
                        <span>Valor Nocional</span>
                        <strong id="posNotional">$0.00</strong>
                    </div>
                    <div class="detail-item">
                        <span>Stop Loss (Supertrend)</span>
                        <strong id="posSL" class="c-down">--</strong>
                    </div>
                    <div class="detail-item">
                        <span>Distância Segura SL</span>
                        <strong id="posSLDist" class="c-amber">--</strong>
                    </div>
                    <div class="detail-item">
                        <span>Risco Máx da Operação</span>
                        <strong id="posMaxRisk" class="c-down">--</strong>
                    </div>
                    <div class="detail-item">
                        <span>Preço Liquidação Est.</span>
                        <strong id="posLiqPrice" style="color: var(--text-tertiary);">--</strong>
                    </div>
                    <div class="detail-item">
                        <span>Margem até Liquidação</span>
                        <strong id="posLiqDist" class="c-up">--</strong>
                    </div>
                    <div class="detail-item">
                        <span>Parcial 50% (+2.50%)</span>
                        <strong id="posTP" class="c-up">--</strong>
                    </div>
                </div>

                <!-- Ações Rápidas da Posição -->
                <div style="display: flex; gap: 8px; margin-top: 14px; flex-wrap: wrap;">
                    <button class="btn" style="background: rgba(100, 210, 255, 0.12); color: var(--cyan); border-color: rgba(100, 210, 255, 0.3); font-size: 0.75rem; padding: 7px 14px;" onclick="openLeverageModal()">
                        ⚡ Alterar Alavancagem
                    </button>
                    <button class="btn" style="background: rgba(255, 159, 10, 0.12); color: var(--amber); border-color: rgba(255, 159, 10, 0.3); font-size: 0.75rem; padding: 7px 14px;" onclick="openRiskModal()">
                        🛡️ Calibrar Risco
                    </button>
                    <button class="btn btn-panic" style="font-size: 0.75rem; padding: 7px 14px;" onclick="openPanicModal()">
                        🚨 Fechar Posição a Mercado
                    </button>
                </div>
            </div>

            <!-- Placeholder Posição Neutra -->
            <div id="posEmptyBox" class="no-pos-box">
                <div class="radar-scan"></div>
                <div style="font-size: 0.95rem; font-weight: 700; color: #ffd60a; letter-spacing: 0.03em;">Radar Cascata Turbo MTF Ativo</div>
                <div style="font-size: 0.80rem; color: var(--text-secondary); max-width: 520px; line-height: 1.5; text-align: center;" id="radarMarketStatus">
                    Sincronizando OLS 100, Bandas Adaptativas e Filtros MTF (D1 + 4h)...
                </div>
            </div>
        </section>

        <!-- CAPACIDADE DE ORDEM & VIABILIDADE OPERACIONAL (SEM ALAVANCAGEM / COM ALAVANCAGEM) -->
        <section class="section-card" id="feasibilityCard" style="margin-top: 16px; border-color: rgba(100, 210, 255, 0.25); background: rgba(16, 24, 38, 0.75);">
            <div class="card-header" style="flex-wrap: wrap; gap: 8px;">
                <div style="display: flex; align-items: center; gap: 10px; flex-wrap: wrap;">
                    <div class="card-title">⚡ Viabilidade Operacional & Margem (Futuros USDⓈ-M 1x vs Alavancagem)</div>
                    <span id="feasibilityBadge" class="badge badge-live">CALCULANDO...</span>
                </div>
                <button class="btn" style="background: rgba(100, 210, 255, 0.12); color: var(--cyan); border-color: rgba(100, 210, 255, 0.3); font-size: 0.72rem; padding: 5px 12px;" onclick="openLeverageModal()">
                    ⚡ Ajustar Alavancagem
                </button>
            </div>
            
            <div id="feasibilityBanner" style="margin: 10px 0 14px 0; padding: 12px 16px; border-radius: var(--radius-md); background: rgba(255, 255, 255, 0.04); border-left: 4px solid var(--cyan); font-size: 0.85rem; line-height: 1.45;">
                <span id="feasibilityDiagnosis">Consultando regras do par na Binance e margem livre...</span>
            </div>

            <div style="display: grid; grid-template-columns: repeat(auto-fit, minmax(180px, 1fr)); gap: 10px;">
                <div class="ind-item">
                    <div class="ind-name"><span>Operar sem Alavancagem (1x)</span></div>
                    <div class="ind-val" id="valFeas1x">--</div>
                </div>
                <div class="ind-item">
                    <div class="ind-name"><span>Custo Mínimo Ordem (1x)</span></div>
                    <div class="ind-val c-cyan" id="valMinCost1x">$0.00 USDT</div>
                </div>
                <div class="ind-item">
                    <div class="ind-name"><span>Alavancagem Mínima Necessária</span></div>
                    <div class="ind-val c-amber" id="valMinLevNeeded">1x</div>
                </div>
                <div class="ind-item">
                    <div class="ind-name"><span>Alavancagem Máxima Binance</span></div>
                    <div class="ind-val" style="color: var(--purple);" id="valMaxLevAllowed">50x</div>
                </div>
                <div class="ind-item">
                    <div class="ind-name"><span>Margem Requerida Atual</span></div>
                    <div class="ind-val" id="valMarginRequiredCurrent">$0.00 USDT</div>
                </div>
                <div class="ind-item">
                    <div class="ind-name"><span>Poder de Compra Máximo</span></div>
                    <div class="ind-val c-up" id="valMaxPurchasingPower">$0.00 USDT</div>
                </div>
            </div>
        </section>

        <!-- BLINDAGEM DE RISCO INSTITUCIONAL -->
        <section class="section-card" style="margin-top: 16px;">
            <div class="card-header" style="flex-wrap: wrap; gap: 8px;">
                <div style="display: flex; align-items: center; gap: 10px; flex-wrap: wrap;">
                    <div class="card-title">🛡️ Blindagem de Risco & Stop Loss Contínuo</div>
                    <span id="slActiveBadge" class="badge badge-live">STOP LOSS: 100% MONITORADO</span>
                </div>
                <div style="font-size: 0.74rem; color: var(--text-tertiary);">
                    Execução nativa STOP_MARKET na Binance + Failsafe em memória
                </div>
            </div>
            <div style="display: grid; grid-template-columns: repeat(auto-fit, minmax(180px, 1fr)); gap: 10px; margin-top: 10px;">
                <div class="ind-item">
                    <div class="ind-name">
                        <span>🛑 Stop Loss de Entrada</span>
                    </div>
                    <div class="ind-val c-down">1.2% Otimizado</div>
                </div>
                <div class="ind-item">
                    <div class="ind-name">
                        <span>📡 Ordem Exchange</span>
                    </div>
                    <div class="ind-val c-cyan">STOP_MARKET Nativa</div>
                </div>
                <div class="ind-item" style="cursor: pointer;" onclick="openRiskModal()" title="Toque para alterar Risco">
                    <div class="ind-name">
                        <span>⚖️ Risco por Operação</span>
                    </div>
                    <div class="ind-val c-amber"><span id="dashRiskPerTrade">16.0%</span> Banca ✏️</div>
                </div>
                <div class="ind-item" style="cursor: pointer;" onclick="openRiskModal()" title="Toque para alterar Nocional">
                    <div class="ind-name">
                        <span>📦 Piso Nocional Mínimo</span>
                    </div>
                    <div class="ind-val c-cyan"><span id="dashMinNotional">$5.50</span> USDT ✏️</div>
                </div>
                <div class="ind-item" style="cursor: pointer;" onclick="openLeverageModal()" title="Toque para alterar Alavancagem">
                    <div class="ind-name">
                        <span>⚡ Alavancagem Isolada</span>
                    </div>
                    <div class="ind-val"><span id="dashLevText">5x ISOLATED</span> ✏️</div>
                </div>
                <div class="ind-item">
                    <div class="ind-name">
                        <span>🛡️ Margem de Liquidação</span>
                    </div>
                    <div class="ind-val c-up">>18% Folga Segura</div>
                </div>
                <div class="ind-item">
                    <div class="ind-name">
                        <span>🔒 Limite Diário (DD)</span>
                    </div>
                    <div class="ind-val" id="dashDailyCBLimit">25.0% Max</div>
                </div>
                <div class="ind-item">
                    <div class="ind-name">
                        <span>🚨 Limite Global (DD)</span>
                    </div>
                    <div class="ind-val" id="dashGlobalCBLimit">35.0% Max</div>
                </div>
            </div>
        </section>

        <!-- SPLIT: COCKPIT & LIVE CHART -->
        <section class="content-split">
            <!-- COCKPIT CASCATA TURBO MTF (NOVO MOTOR OFICIAL) -->
            <div class="section-card" style="border: 1px solid rgba(255, 214, 10, 0.25);">
                <div class="card-header">
                    <div class="card-title">⚡ Cockpit Cascata Turbo MTF (OLS 100 • Bandas Adaptativas)</div>
                    <span id="cbTag" class="badge badge-live" style="background: rgba(255, 214, 10, 0.16); color: #ffd60a; border: 1px solid rgba(255, 214, 10, 0.35);">CASCATA MTF ATIVA</span>
                </div>
                <div class="indicator-list">
                    <div class="ind-item">
                        <div class="ind-name">
                            <span>🧭 Alinhamento MTF (Diário D1 & 4H)</span>
                        </div>
                        <div class="ind-val" style="display: flex; align-items: center; gap: 8px;">
                            <span id="cockpitD1Badge" class="badge badge-live" style="font-weight: 700;">D1: NEUTRO</span>
                            <span id="cockpitH4Badge" class="badge badge-mode" style="font-weight: 700;">4H: NEUTRO</span>
                        </div>
                    </div>
                    <div class="ind-item">
                        <div class="ind-name">
                            <span>📈 OLS 100 & Bandas Adaptativas (15m)</span>
                        </div>
                        <div class="ind-val">
                            <span id="cockpitUpper" class="c-up" title="Banda Superior de Rompimento Long">Long: $0.00</span>
                            <span style="color: var(--text-tertiary); margin: 0 4px;">|</span>
                            <span id="cockpitBaseline" style="color: #ffd60a;" title="Linha de Regressão Linear OLS 100">OLS: $0.00</span>
                            <span style="color: var(--text-tertiary); margin: 0 4px;">|</span>
                            <span id="cockpitLower" class="c-down" title="Banda Inferior de Rompimento Short">Short: $0.00</span>
                        </div>
                    </div>
                    <div class="ind-item">
                        <div class="ind-name">
                            <span>🎯 Metas da Posição (Cascata Turbo)</span>
                        </div>
                        <div class="ind-val">
                            <span class="c-cyan" style="font-weight: 700;">Parcial 50% (+2.50%)</span>
                            <span style="color: var(--text-tertiary); margin: 0 4px;">|</span>
                            <span class="c-up" style="font-weight: 700;">Breakeven (+0.25%)</span>
                            <span style="color: var(--text-tertiary); margin: 0 4px;">|</span>
                            <span class="c-amber" style="font-size: 0.78rem;">Trailing Supertrend</span>
                        </div>
                    </div>
                    <div class="ind-item">
                        <div class="ind-name">
                            <span>🌊 Choppiness Index (Regime 15m)</span>
                        </div>
                        <div class="ind-val">
                            <span id="cockpitChopVal" style="font-family: var(--font-mono); font-weight: 700;">0.0</span>
                            <span id="cockpitChopBadge" class="badge badge-live" style="margin-left: 6px;">CALCULANDO</span>
                        </div>
                    </div>
                    <div class="ind-item">
                        <div class="ind-name">
                            <span>🚀 Smart Ignition & Confluência</span>
                        </div>
                        <div class="ind-val">
                            <span id="cockpitIgnitionStatus" class="c-cyan" style="font-weight: 600;">Monitorando Expansão de Volatilidade</span>
                        </div>
                    </div>
                    <div class="ind-item" style="cursor: pointer;" onclick="openRiskModal()" title="Toque para calibrar risco por trade">
                        <div class="ind-name">
                            <span>🛡️ Risco por Trade / Alavancagem</span>
                        </div>
                        <div class="ind-val">
                            <span id="indRiskVal" class="c-amber">16.0%</span>
                            <span style="color: var(--text-tertiary); margin: 0 4px;">/</span>
                            <span id="cockpitLevVal" class="c-cyan">5x ISOLATED</span>
                            <span style="font-size: 0.72rem; margin-left: 4px;">✏️</span>
                        </div>
                    </div>
                </div>
            </div>

            <!-- PROP DESK CASCADE HUD (ESPELHO DA TABELA PINE SCRIPT) -->
            <div class="cascade-hud-wrapper" id="cascadeHudWrapper">
                <div class="cascade-hud-header">
                    <div style="display: flex; align-items: center; gap: 10px; flex-wrap: wrap;">
                        <span style="font-size: 0.90rem; font-weight: 800; color: #ffd60a; letter-spacing: 0.04em;">
                            ⚡ PROP DESK CASCADE — MTF DUPLO & CONFLUÊNCIA BAYESIANA
                        </span>
                        <span id="cascadeActiveBadge" class="badge badge-live">MOTOR ATIVO</span>
                    </div>
                    <button class="btn" style="background: rgba(255, 214, 10, 0.15); color: #ffd60a; border-color: rgba(255, 214, 10, 0.35); font-size: 0.74rem; padding: 5px 12px; cursor: pointer;" onclick="openCascadeModal()">
                        ⚙️ Calibrar Parâmetros
                    </button>
                </div>

                <div class="cascade-hud-grid">
                    <div class="cascade-hud-card">
                        <span class="cascade-hud-lbl">Macro (1D Veto)</span>
                        <span class="cascade-hud-val" id="hudMacroBias">NEUTRO</span>
                    </div>
                    <div class="cascade-hud-card">
                        <span class="cascade-hud-lbl">Contexto (4h)</span>
                        <span class="cascade-hud-val" id="hudCtxBias">NEUTRO</span>
                    </div>
                    <div class="cascade-hud-card">
                        <span class="cascade-hud-lbl">Convicção Long</span>
                        <span class="cascade-hud-val" id="hudConvLong">--</span>
                    </div>
                    <div class="cascade-hud-card">
                        <span class="cascade-hud-lbl">Convicção Short</span>
                        <span class="cascade-hud-val" id="hudConvShort">--</span>
                    </div>
                    <div class="cascade-hud-card">
                        <span class="cascade-hud-lbl">Slope Exec (OLS/ATR)</span>
                        <span class="cascade-hud-val" id="hudSlopeExec">0.000</span>
                    </div>
                    <div class="cascade-hud-card">
                        <span class="cascade-hud-lbl">Estrutura (Pivots)</span>
                        <span class="cascade-hud-val" id="hudScoreStruct">0.00</span>
                    </div>
                    <div class="cascade-hud-card">
                        <span class="cascade-hud-lbl">Momentum Inst.</span>
                        <span class="cascade-hud-val" id="hudScoreMom">0.00</span>
                    </div>
                    <div class="cascade-hud-card">
                        <span class="cascade-hud-lbl">Exaustão (RSI)</span>
                        <span class="cascade-hud-val" id="hudScoreExh">0.00</span>
                    </div>
                    <div class="cascade-hud-card">
                        <span class="cascade-hud-lbl">Choppiness Index</span>
                        <span class="cascade-hud-val" id="hudScoreChop">0.00</span>
                    </div>
                    <div class="cascade-hud-card" style="background: rgba(48, 209, 88, 0.08); border-color: rgba(48, 209, 88, 0.25);">
                        <span class="cascade-hud-lbl" style="color: var(--green);">SCORE LONG</span>
                        <span class="cascade-hud-val c-up" id="hudScoreLong">0.00</span>
                    </div>
                    <div class="cascade-hud-card" style="background: rgba(255, 69, 58, 0.08); border-color: rgba(255, 69, 58, 0.25);">
                        <span class="cascade-hud-lbl" style="color: var(--red);">SCORE SHORT</span>
                        <span class="cascade-hud-val c-down" id="hudScoreShort">0.00</span>
                    </div>
                    <div class="cascade-hud-card">
                        <span class="cascade-hud-lbl">Limiar Mínimo</span>
                        <span class="cascade-hud-val" style="color: var(--text-secondary);" id="hudMinThreshold">0.70</span>
                    </div>
                </div>
            </div>

            <!-- NATIVE LIGHTWEIGHT CHART (BINANCE REALTIME + OVERLAYS) -->
            <div class="chart-wrapper">
                <div class="chart-header">
                    <div style="display: flex; align-items: center; gap: 10px; flex-wrap: wrap;">
                        <span style="font-size: 0.88rem; font-weight: 700; color: #ffffff;">Gráfico Nativo Independente (Lightweight Charts)</span>
                        <span class="badge" style="background: rgba(255, 214, 10, 0.12); color: #ffd60a; border: 1px solid rgba(255, 214, 10, 0.25); font-size: 0.68rem; font-weight: 700;" id="chartStatusBadge">AO VIVO BINANCE</span>
                    </div>
                    <div style="display: flex; align-items: center; gap: 8px;">
                        <button class="chart-toggle-btn" onclick="openCascadeModal()">⚙️ Configurar Indicador</button>
                        <button class="chart-toggle-btn" onclick="toggleChart()">Minimizar / Expandir</button>
                    </div>
                </div>

                <!-- LEGENDA DINAMICA DAS BANDAS E INDICADORES -->
                <div class="chart-legend-strip">
                    <div class="legend-item">
                        <span class="legend-dot" style="background: #ffd60a;"></span>
                        <span style="color: #ffd60a;">OLS Baseline:</span>
                        <strong id="legBaseline" style="color: #ffffff;">--</strong>
                    </div>
                    <div class="legend-item">
                        <span class="legend-dot" style="background: #30d158;"></span>
                        <span style="color: #30d158;">Upper Band (Entrada Long):</span>
                        <strong id="legUpper" style="color: #ffffff;">--</strong>
                    </div>
                    <div class="legend-item">
                        <span class="legend-dot" style="background: #ff453a;"></span>
                        <span style="color: #ff453a;">Lower Band (Entrada Short):</span>
                        <strong id="legLower" style="color: #ffffff;">--</strong>
                    </div>
                    <div class="legend-item">
                        <span class="legend-dot" style="background: #ff9f0a;"></span>
                        <span style="color: #ff9f0a;">Trailing Stop Ativo:</span>
                        <strong id="legStop" style="color: #ffffff;">--</strong>
                    </div>
                </div>

                <div class="chart-container" id="chartContainer" style="height: 500px; position: relative;">
                    <div id="nativeChart" style="width: 100%; height: 100%;"></div>
                </div>
            </div>
        </section>

        <!-- EXTRATO DETALHADO BINANCE -->
        <section class="section-card">
            <div class="card-header" style="flex-wrap: wrap; gap: 8px;">
                <div style="display: flex; align-items: center; gap: 10px; flex-wrap: wrap;">
                    <div class="card-title">📋 Extrato Detalhado de Operações (Binance Realized)</div>
                    <span class="badge" style="background: rgba(240, 185, 11, 0.14); color: #f0b90b; border: 1px solid rgba(240, 185, 11, 0.28); font-size: 0.68rem; font-weight: 700;">
                        BINANCE USER TRADES SYNC
                    </span>
                </div>
                <div style="font-size: 0.74rem; color: var(--text-tertiary);" id="tradesCountSubtitle">
                    0 operações registradas
                </div>
            </div>

            <!-- SUMMARY STRIP -->
            <div class="trades-summary-strip" id="tradesSummaryStrip">
                <div class="trade-sum-item">
                    <span class="trade-sum-label">PnL Líquido Total</span>
                    <span class="trade-sum-val" id="sumNetPnl">$0.00</span>
                </div>
                <div class="trade-sum-item">
                    <span class="trade-sum-label">Taxas Binance Pagas</span>
                    <span class="trade-sum-val c-amber" id="sumCommission">$0.0000</span>
                </div>
                <div class="trade-sum-item">
                    <span class="trade-sum-label">Taxa de Acerto</span>
                    <span class="trade-sum-val" id="sumWinRate">0.0%</span>
                </div>
                <div class="trade-sum-item">
                    <span class="trade-sum-label">Volume Nocional</span>
                    <span class="trade-sum-val c-cyan" id="sumVolume">$0.00</span>
                </div>
            </div>

            <div class="table-responsive">
                <table>
                    <thead>
                        <tr>
                            <th>Data / Hora</th>
                            <th>Operação</th>
                            <th>Entrada → Saída</th>
                            <th>Qtd & Nocional</th>
                            <th>PnL Bruto</th>
                            <th>Taxas Binance</th>
                            <th>PnL Líquido Oficial</th>
                            <th>Saldo Carteira</th>
                            <th>Motivo Fechamento</th>
                        </tr>
                    </thead>
                    <tbody id="tradeHistoryBody">
                        <tr>
                            <td colspan="9" style="text-align:center; padding: 32px 16px; color: var(--text-tertiary);">
                                <div style="font-size: 1.1rem; margin-bottom: 6px;">⏳</div>
                                Nenhuma operação concluída até o momento.<br>
                                <span style="font-size: 0.72rem; color: var(--text-tertiary);">
                                    Ao fechar uma posição, o extrato oficial com taxas e PnL realizados direto da Binance será exibido aqui.
                                </span>
                            </td>
                        </tr>
                    </tbody>
                </table>
            </div>
        </section>

        <!-- FOOTER HEARTBEAT -->
        <footer class="footer-bar">
            <div class="heartbeat-text">
                <span class="live-dot" style="width:6px; height:6px;"></span>
                <span>Conexão WebSocket ativa</span>
            </div>
            <div style="display: flex; align-items: center; gap: 10px; flex-wrap: wrap;">
                <button class="btn btn-secondary" id="btnManualSync" onclick="manualSync()" style="padding: 5px 12px; font-size: 0.76rem; border-radius: 8px;">
                    🔄 Sincronizar Agora
                </button>
                <div style="font-family: var(--font-mono);" id="lastSyncTime">
                    Sincronizado: --:--:--
                </div>
            </div>
        </footer>
    </div>

    <!-- MODAL DE ALAVANCAGEM INTERATIVO MOBILE -->
    <div id="leverageModal" class="modal-overlay" onclick="closeLeverageModal(event)">
        <div class="modal-card" onclick="event.stopPropagation()">
            <div class="modal-header">
                <h3>⚡ Alavancagem Binance Futures</h3>
                <button class="modal-close" onclick="closeLeverageModal()">✕</button>
            </div>
            <div class="modal-body">
                <p style="font-size: 0.78rem; color: var(--text-secondary); margin-bottom: 10px; line-height: 1.4;">
                    Altere a alavancagem em <strong id="modalSymbol" class="c-cyan">SOLUSDT Perpétuo (USDⓈ-M)</strong>. Use <strong class="c-green">1x</strong> para operar sem alavancagem (Margem integral 100% USDT no Mercado Futuro).
                </p>
                <div id="modalLevReadinessHint" style="font-size: 0.76rem; background: rgba(255, 255, 255, 0.04); padding: 8px 12px; border-radius: var(--radius-sm); margin-bottom: 12px; border-left: 3px solid var(--cyan);">
                    Calculando viabilidade de margem...
                </div>
                <div class="quick-lev-grid">
                    <button class="btn-lev-pill" onclick="selectQuickLev(1)">1x (Sem Alavancagem)</button>
                    <button class="btn-lev-pill" onclick="selectQuickLev(2)">2x</button>
                    <button class="btn-lev-pill" onclick="selectQuickLev(3)">3x</button>
                    <button class="btn-lev-pill" onclick="selectQuickLev(5)">5x</button>
                    <button class="btn-lev-pill" onclick="selectQuickLev(10)">10x</button>
                    <button class="btn-lev-pill" onclick="selectQuickLev(20)">20x</button>
                </div>
                <div class="lev-input-row">
                    <span style="font-size: 0.82rem; color: var(--text-secondary); font-weight: 600;">Personalizado:</span>
                    <div style="display: flex; align-items: center; gap: 4px;">
                        <input type="number" id="customLevInput" min="1" max="125" value="1" class="lev-input">
                        <span style="font-size: 0.9rem; font-weight: 700; color: #ffffff;">x</span>
                    </div>
                </div>
                <div id="levStatusMsg" style="font-size: 0.75rem; margin-bottom: 12px; min-height: 18px; text-align: center;"></div>
                <button class="btn btn-apply-lev" id="btnConfirmLev" onclick="applyLeverage()">
                    Aplicar na Binance
                </button>
            </div>
        </div>
    </div>

    <!-- MODAL DE FILTROS DE RISCO E NOCIONAL MOBILE -->
    <div id="riskModal" class="modal-overlay" onclick="closeRiskModal(event)">
        <div class="modal-card" onclick="event.stopPropagation()">
            <div class="modal-header">
                <h3>🛡️ Calibração de Risco por Operação</h3>
                <button class="modal-close" onclick="closeRiskModal()">✕</button>
            </div>
            <div class="modal-body">
                <p style="font-size: 0.78rem; color: var(--text-secondary); margin-bottom: 12px; line-height: 1.4;">
                    Configure a porcentagem de risco financeiro tolerado por cada trade e o limite nocional máximo permitido para a posição.
                </p>
                <div style="font-size: 0.8rem; font-weight: 700; color: #ffffff; margin-bottom: 6px;">
                    Risco por Trade (% da Banca):
                </div>
                <div class="quick-lev-grid" style="margin-bottom: 10px;">
                    <button class="btn-lev-pill btn-risk-pill" onclick="selectQuickRisk(8.0)">8.0%</button>
                    <button class="btn-lev-pill btn-risk-pill" onclick="selectQuickRisk(12.0)">12.0%</button>
                    <button class="btn-lev-pill btn-risk-pill" onclick="selectQuickRisk(16.0)">16.0%</button>
                </div>
                <div class="lev-input-row" style="margin-bottom: 14px;">
                    <span style="font-size: 0.82rem; color: var(--text-secondary); font-weight: 600;">Personalizado:</span>
                    <div style="display: flex; align-items: center; gap: 4px;">
                        <input type="number" id="customRiskInput" step="0.5" min="0.5" max="25.0" value="16.0" class="lev-input" style="width: 80px;">
                        <span style="font-size: 0.9rem; font-weight: 700; color: var(--amber);">%</span>
                    </div>
                </div>

                <div style="font-size: 0.8rem; font-weight: 700; color: #ffffff; margin-bottom: 6px;">
                    Buffer Nocional Mínimo (USDT):
                </div>
                <div class="quick-lev-grid" style="margin-bottom: 10px;">
                    <button class="btn-lev-pill btn-notional-pill" onclick="selectQuickNotional(5.10)">$5.10</button>
                    <button class="btn-lev-pill btn-notional-pill" onclick="selectQuickNotional(5.50)">$5.50</button>
                    <button class="btn-lev-pill btn-notional-pill" onclick="selectQuickNotional(6.00)">$6.00</button>
                </div>
                <div class="lev-input-row">
                    <span style="font-size: 0.82rem; color: var(--text-secondary); font-weight: 600;">Personalizado:</span>
                    <div style="display: flex; align-items: center; gap: 4px;">
                        <span style="font-size: 0.9rem; font-weight: 700; color: var(--cyan);">$</span>
                        <input type="number" id="customNotionalInput" step="0.10" min="5.0" max="100.0" value="5.50" class="lev-input" style="width: 80px;">
                    </div>
                </div>

                <div id="riskStatusMsg" style="font-size: 0.75rem; margin-bottom: 12px; min-height: 18px; text-align: center;"></div>
                <button class="btn btn-apply-lev" id="btnConfirmRisk" onclick="applyRiskParams()">
                    Salvar Parâmetros no Bot
                </button>
            </div>
        </div>
    </div>

    <!-- MODAL DE CONFIRMAÇÃO DE FECHAMENTO PANIC -->
    <div id="panicModal" class="modal-overlay" onclick="closePanicModal(event)">
        <div class="modal-card" style="border-color: rgba(255, 69, 58, 0.4);" onclick="event.stopPropagation()">
            <div class="modal-header">
                <h3 style="color: var(--red);">🚨 Fechar Posições a Mercado</h3>
                <button class="modal-close" onclick="closePanicModal()">✕</button>
            </div>
            <div class="modal-body">
                <p style="font-size: 0.85rem; color: #ffffff; margin-bottom: 12px; line-height: 1.5; font-weight: 500;">
                    Tem certeza de que deseja executar a <strong>Ordem de Pânico</strong>?
                </p>
                <div style="background: rgba(255, 69, 58, 0.1); border: 1px solid rgba(255, 69, 58, 0.25); border-radius: 8px; padding: 10px; margin-bottom: 16px; font-size: 0.78rem; color: #ffb3b0; line-height: 1.4;">
                    ⚠️ Isto cancelará ordens ativas e fechará a posição imediatamente a preço de mercado na Binance Futures.
                </div>
                <div style="display: flex; gap: 8px; justify-content: flex-end;">
                    <button class="btn btn-secondary" onclick="closePanicModal()" style="flex: 1;">Cancelar</button>
                    <button class="btn btn-danger" id="btnConfirmPanic" onclick="executePanicClose()" style="flex: 1.3;">🚨 Confirmar e Fechar</button>
                </div>
            </div>
        </div>
    </div>

    <!-- MODAL DE CONFIGURAÇÃO DA CASCATA MTF -->
    <div id="cascadeModal" class="modal-overlay" onclick="closeCascadeModal(event)">
        <div class="modal-card" style="max-width: 540px; border-color: rgba(255, 214, 10, 0.4);" onclick="event.stopPropagation()">
            <div class="modal-header">
                <div>
                    <h3 style="color: #ffd60a;">⚡ Parâmetros da Estratégia Cascata MTF</h3>
                    <div style="font-size: 0.74rem; color: var(--text-tertiary);">Configuração ao vivo do motor quantitativo Prop Desk</div>
                </div>
                <button class="modal-close" onclick="closeCascadeModal()">✕</button>
            </div>
            <div class="modal-body">
                <div style="display: flex; justify-content: space-between; align-items: center; background: rgba(255, 255, 255, 0.04); padding: 10px 14px; border-radius: 8px; margin-bottom: 12px;">
                    <span style="font-size: 0.82rem; font-weight: 600;">Ativar Motor Cascata Prop Desk</span>
                    <input type="checkbox" id="cascadeEnabledInput" style="transform: scale(1.3); cursor: pointer;" checked>
                </div>
                
                <div style="display: grid; grid-template-columns: 1fr 1fr; gap: 10px; margin-bottom: 12px;">
                    <div>
                        <label style="font-size: 0.75rem; color: var(--text-secondary); display: block; margin-bottom: 4px;">Período Regressão (OLS)</label>
                        <input type="number" id="cascadeLengthInput" min="10" max="500" value="100" class="lev-input" style="width: 100%; text-align: left;">
                    </div>
                    <div>
                        <label style="font-size: 0.75rem; color: var(--text-secondary); display: block; margin-bottom: 4px;">Multiplicador Banda ATR</label>
                        <input type="number" id="cascadeSdEntryInput" step="0.1" min="0.5" max="5.0" value="2.0" class="lev-input" style="width: 100%; text-align: left;">
                    </div>
                </div>

                <div style="display: grid; grid-template-columns: 1fr 1fr; gap: 10px; margin-bottom: 12px;">
                    <div>
                        <label style="font-size: 0.75rem; color: var(--text-secondary); display: block; margin-bottom: 4px;">Slope Normalizado Mín.</label>
                        <input type="number" id="cascadeMinSlopeInput" step="0.01" min="0.01" max="1.0" value="0.10" class="lev-input" style="width: 100%; text-align: left;">
                    </div>
                    <div>
                        <label style="font-size: 0.75rem; color: var(--text-secondary); display: block; margin-bottom: 4px;">Score Mínimo Confluência</label>
                        <input type="number" id="cascadeMinConfInput" step="0.05" min="0.4" max="1.0" value="0.70" class="lev-input" style="width: 100%; text-align: left;">
                    </div>
                </div>

                <div style="display: grid; grid-template-columns: 1fr 1fr; gap: 10px; margin-bottom: 12px;">
                    <div>
                        <label style="font-size: 0.75rem; color: var(--text-secondary); display: block; margin-bottom: 4px;">Comprimento ATR (Stop)</label>
                        <input type="number" id="cascadeAtrLenInput" min="5" max="100" value="22" class="lev-input" style="width: 100%; text-align: left;">
                    </div>
                    <div>
                        <label style="font-size: 0.75rem; color: var(--text-secondary); display: block; margin-bottom: 4px;">Multiplicador Stop ATR</label>
                        <input type="number" id="cascadeAtrMultInput" step="0.1" min="1.0" max="6.0" value="3.0" class="lev-input" style="width: 100%; text-align: left;">
                    </div>
                </div>

                <div style="display: grid; grid-template-columns: 1fr 1fr; gap: 10px; margin-bottom: 14px;">
                    <div>
                        <label style="font-size: 0.75rem; color: var(--text-secondary); display: block; margin-bottom: 4px;">Risco por Operação (%)</label>
                        <input type="number" id="cascadeRiskInput" step="0.5" min="0.5" max="10.0" value="2.0" class="lev-input" style="width: 100%; text-align: left;">
                    </div>
                    <div style="display: flex; flex-direction: column; justify-content: center; gap: 6px;">
                        <label style="display: flex; align-items: center; gap: 8px; font-size: 0.78rem; cursor: pointer;">
                            <input type="checkbox" id="cascadeUseMtfInput" checked> Ativar Cascata MTF (1D + 4h)
                        </label>
                        <label style="display: flex; align-items: center; gap: 8px; font-size: 0.78rem; cursor: pointer;">
                            <input type="checkbox" id="cascadeUseIgnitionInput" checked> Ativar Ignição de Momentum
                        </label>
                    </div>
                </div>

                <div id="cascadeStatusMsg" style="font-size: 0.75rem; margin-bottom: 12px; min-height: 18px; text-align: center;"></div>
                <div style="display: flex; gap: 8px;">
                    <button class="btn btn-secondary" onclick="closeCascadeModal()" style="flex: 1;">Cancelar</button>
                    <button class="btn btn-apply-lev" id="btnConfirmCascade" onclick="applyCascadeConfig()" style="flex: 1.5; background: linear-gradient(135deg, #ffd60a, #ff9f0a); color: #000000;">
                        Salvar e Aplicar
                    </button>
                </div>
            </div>
        </div>
    </div>

    <!-- APPLE TOAST FEEDBACK NOTIFICATION -->
    <div id="appleToast" class="apple-toast">
        <span id="toastIcon">ℹ️</span>
        <span id="toastMsg">Notificação</span>
    </div>

    <script>
        let currentLeverage = 5;
        let currentSymbol = 'SOLUSDT';
        let currentMinNotional = 5.50;
        let currentRiskPerTrade = 16.0;
        let currentTimeframe = '15';
        let chartLoadedSymbol = 'SOLUSDT';

        function showToast(msg, type = 'info') {
            const toast = document.getElementById('appleToast');
            const icon = document.getElementById('toastIcon');
            const text = document.getElementById('toastMsg');
            if (!toast) return;
            if (type === 'success') icon.textContent = '✅';
            else if (type === 'error') icon.textContent = '❌';
            else if (type === 'warning') icon.textContent = '⚠️';
            else icon.textContent = 'ℹ️';
            text.textContent = msg;
            toast.classList.add('show');
            clearTimeout(window.__toastTimeout);
            window.__toastTimeout = setTimeout(() => {
                toast.classList.remove('show');
            }, 3200);
        }

        function toggleChart() {
            const container = document.getElementById('chartContainer');
            if (container) container.classList.toggle('collapsed');
        }

        function switchTimeframe(tf) {
            currentTimeframe = tf;
            document.querySelectorAll('.tf-btn').forEach(btn => {
                const label = btn.textContent.trim().toLowerCase();
                const expected = (tf === 'D' ? '1d' : (parseInt(tf) >= 60 ? (parseInt(tf)/60)+'h' : tf+'m')).toLowerCase();
                btn.classList.toggle('active', label === expected);
            });
            showToast('Timeframe de execução: ' + (tf === 'D' ? '1D' : (parseInt(tf) >= 60 ? (parseInt(tf)/60)+'h' : tf+'m')), 'info');
        }

        function updateChartSymbol(symbol) {
            if (!symbol || chartLoadedSymbol === symbol) return;
            chartLoadedSymbol = symbol;
            const badge = document.getElementById('chartStatusBadge');
            if (badge) badge.textContent = symbol + ' • AO VIVO';
        }

        async function manualSync() {
            const btn = document.getElementById('btnManualSync');
            if (btn) {
                btn.disabled = true;
                btn.textContent = '🔄 Consultando Binance...';
            }
            try {
                const res = await fetch('/api/action/sync_balance', { method: 'POST' });
                const data = await res.json();
                await fetchStatus();
                if (data.success) {
                    if (data.is_paper) {
                        showToast('Saldo Paper sincronizado: $' + (data.balance || 0).toFixed(2), 'info');
                    } else {
                        showToast('Saldo Real Binance: $' + (data.balance || 0).toFixed(2) + ' USDT | Livre: $' + (data.available_balance || 0).toFixed(2), 'success');
                    }
                } else {
                    showToast('Falha na Binance: ' + (data.error || 'Erro'), 'error');
                }
            } catch (e) {
                showToast('Erro ao sincronizar: ' + e, 'error');
            } finally {
                if (btn) {
                    btn.disabled = false;
                    btn.textContent = '🔄 Sincronizar Agora';
                }
            }
        }

        function openPanicModal() {
            document.getElementById('panicModal').classList.add('active');
        }

        function closePanicModal(e) {
            if (e && e.target !== e.currentTarget && e.target.tagName !== 'BUTTON') return;
            document.getElementById('panicModal').classList.remove('active');
        }

        async function executePanicClose() {
            const btn = document.getElementById('btnConfirmPanic');
            if (btn) {
                btn.disabled = true;
                btn.textContent = 'Fechando a mercado...';
            }
            try {
                const res = await fetch('/api/action/emergency_close', { method: 'POST' });
                const data = await res.json();
                closePanicModal();
                if (data.status === 'success' || data.success) {
                    showToast('Posições fechadas a mercado na Binance!', 'success');
                } else {
                    showToast(data.message || 'Ordem de emergência processada', 'warning');
                }
                fetchStatus();
            } catch (err) {
                showToast('Erro ao fechar posições: ' + err, 'error');
            } finally {
                if (btn) {
                    btn.disabled = false;
                    btn.textContent = '🚨 Confirmar e Fechar';
                }
            }
        }

        async function triggerAction(action) {
            if (action === 'emergency_close') {
                openPanicModal();
                return;
            }
            try {
                const res = await fetch('/api/action/' + action, { method: 'POST' });
                const data = await res.json();
                if (action === 'pause') {
                    showToast('Bot pausado com sucesso', 'warning');
                } else if (action === 'resume') {
                    showToast('Bot retomado com sucesso', 'success');
                } else {
                    showToast('Ação executada: ' + action, 'info');
                }
                fetchStatus();
            } catch (err) {
                showToast('Erro na solicitação: ' + err, 'error');
            }
        }

        async function fetchStatus() {
            try {
                const res = await fetch('/api/status');
                if (!res.ok) {
                    const syncElem = document.getElementById('lastSyncTime');
                    if (syncElem) syncElem.textContent = 'Aguardando bot...';
                    return;
                }
                const data = await res.json();

                // 1. Relógio de sincronização imediato
                const now = new Date();
                const syncElem = document.getElementById('lastSyncTime');
                if (syncElem) syncElem.textContent = 'Sincronizado: ' + now.toLocaleTimeString('pt-BR');

                // 2. Gráfico TradingView
                try {
                    updateChartSymbol(data.symbol || 'SOLUSDT');
                } catch (e) {
                    console.warn("Chart update error:", e);
                }

                // 3. Top Header Tags
                const symTag = document.getElementById('symbolTag');
                if (symTag) symTag.textContent = (data.symbol || 'SOLUSDT') + ' ' + (currentTimeframe === 'D' ? '1D' : (parseInt(currentTimeframe) >= 60 ? (parseInt(currentTimeframe)/60)+'h' : currentTimeframe+'m'));

                const modeTag = document.getElementById('modeTag');
                if (modeTag) {
                    modeTag.textContent = data.paper_trading ? 'PAPER (SIMULADO)' : 'BINANCE REAL';
                    modeTag.className = 'badge ' + (data.paper_trading ? 'badge-mode' : 'badge-live');
                }

                const statusTag = document.getElementById('statusTag');
                if (statusTag && data.status) {
                    statusTag.textContent = data.status.toUpperCase();
                    statusTag.className = 'badge badge-status status-' + data.status.toLowerCase();
                }

                // 4. Balances
                if (data.account) {
                    const balElem = document.getElementById('valBalance');
                    if (balElem) balElem.textContent = '$' + (data.account.balance || 0).toFixed(2);

                    const eqElem = document.getElementById('valEquity');
                    if (eqElem) eqElem.textContent = '$' + (data.account.equity || 0).toFixed(2);

                    const subAvail = document.getElementById('subAvailable');
                    if (subAvail) subAvail.textContent = 'Margem Livre: $' + (data.account.available_balance || data.account.balance || 0).toFixed(2);

                    const subBalType = document.getElementById('subBalanceType');
                    if (subBalType) {
                        subBalType.innerHTML = data.paper_trading 
                            ? '<span class="c-amber">Simulação Virtual ✏️</span>' 
                            : '<span class="c-up" style="font-weight:700;">● Binance Live Real 🔄</span>';
                    }

                    const lblBalance = document.getElementById('lblBalance');
                    if (lblBalance) {
                        lblBalance.textContent = data.paper_trading ? 'Saldo Virtual (Paper)' : 'Saldo Real Binance';
                    }
                }

                const unPnl = data.position.unrealized_pnl;
                const unPnlElem = document.getElementById('valUnrealized');
                const pnlSign = unPnl >= 0 ? '+$' : '-$';
                unPnlElem.textContent = pnlSign + Math.abs(unPnl).toFixed(2);
                unPnlElem.className = 'stat-value ' + (unPnl >= 0 ? 'c-up' : 'c-down');

                // ROE
                const notional = data.position.amount * data.position.current_price;
                const margin = data.position.leverage > 0 ? (notional / data.position.leverage) : 0;
                const roe = margin > 0 ? (unPnl / margin) * 100 : 0;
                document.getElementById('subROE').textContent = `ROE: ${roe >= 0 ? '+' : ''}${roe.toFixed(2)}%`;

                // Win rate
                const total = data.account.total_trades;
                const wins = data.account.winning_trades;
                const winRate = total > 0 ? ((wins / total) * 100).toFixed(1) : '0.0';
                document.getElementById('valWinRate').textContent = `${winRate}%`;
                document.getElementById('valTradeCount').textContent = `${wins}W / ${total - wins}L (${total} tot)`;

                document.getElementById('valDailyDD').textContent = data.account.daily_drawdown.toFixed(2) + '%';
                document.getElementById('valMaxDD').textContent = data.account.max_drawdown.toFixed(2) + '%';
                document.getElementById('valPeakBalance').textContent = 'Topo: $' + data.account.peak_balance.toFixed(2);

                // Position Card
                const posCard = document.getElementById('posCard');
                const posSideBadge = document.getElementById('posSideBadge');
                const posActiveBox = document.getElementById('posActiveBox');
                const posEmptyBox = document.getElementById('posEmptyBox');

                if (data.position.side !== 'None' && data.position.amount > 0) {
                    posActiveBox.style.display = 'block';
                    posEmptyBox.style.display = 'none';
                    
                    const isLong = data.position.side === 'Long';
                    posCard.className = 'position-card ' + (isLong ? 'pos-long' : 'pos-short');
                    posSideBadge.textContent = isLong ? 'LONG (COMPRADO)' : 'SHORT (VENDIDO)';
                    posSideBadge.className = 'badge ' + (isLong ? 'badge-live' : 'badge-status status-stopped');

                    document.getElementById('posHeroPnl').textContent = pnlSign + Math.abs(unPnl).toFixed(2);
                    document.getElementById('posHeroPnl').className = 'pos-pnl-val ' + (unPnl >= 0 ? 'c-up' : 'c-down');

                    document.getElementById('posCurrentPrice').textContent = '$' + data.position.current_price.toFixed(2);
                    document.getElementById('posEntryPrice').textContent = '$' + data.position.entry_price.toFixed(2);
                    document.getElementById('posAmount').textContent = data.position.amount.toFixed(4) + ' ' + data.symbol.replace('USDT', '');
                    document.getElementById('posNotional').textContent = '$' + notional.toFixed(2);
                    document.getElementById('posLevText').textContent = data.position.leverage + 'x ISOLATED';
                    currentLeverage = data.position.leverage;

                    const sl = data.position.stop_loss;
                    const tpPrice = isLong 
                        ? (data.position.entry_price * 1.025)
                        : (data.position.entry_price * 0.975);
                    document.getElementById('posSL').textContent = sl ? '$' + sl.toFixed(2) : '--';
                    document.getElementById('posTP').textContent = data.position.entry_price > 0 ? '$' + tpPrice.toFixed(2) + ' (+2.5%)' : '--';

                    // Distance to Stop Loss
                    const slDistElem = document.getElementById('posSLDist');
                    if (sl && data.position.current_price > 0) {
                        const distPct = isLong 
                            ? ((data.position.current_price - sl) / data.position.current_price) * 100
                            : ((sl - data.position.current_price) / data.position.current_price) * 100;
                        const distUsdt = Math.abs(data.position.current_price - sl);
                        slDistElem.textContent = `${distPct >= 0 ? '+' : ''}${distPct.toFixed(2)}% ($${distUsdt.toFixed(2)})`;
                        slDistElem.className = distPct > 1.5 ? 'c-up' : (distPct > 0.6 ? 'c-amber' : 'c-down');
                    } else {
                        slDistElem.textContent = '--';
                    }

                    // Max Risk USDT
                    const maxRiskElem = document.getElementById('posMaxRisk');
                    if (sl && data.position.amount > 0) {
                        const riskUsdt = Math.abs(data.position.entry_price - sl) * data.position.amount;
                        const riskPctMargin = margin > 0 ? (riskUsdt / margin) * 100 : 0;
                        maxRiskElem.textContent = `-$${riskUsdt.toFixed(2)} (${riskPctMargin.toFixed(1)}% margem)`;
                    } else {
                        maxRiskElem.textContent = '--';
                    }

                    // Liquidation price & safety margin
                    const liqPrice = data.position.liquidation_price || (isLong 
                        ? (data.position.entry_price * (1.0 - (1.0 / (data.position.leverage || 5)) + 0.005))
                        : (data.position.entry_price * (1.0 + (1.0 / (data.position.leverage || 5)) - 0.005)));
                    const liqPriceElem = document.getElementById('posLiqPrice');
                    const liqDistElem = document.getElementById('posLiqDist');
                    if (liqPrice > 0 && data.position.current_price > 0) {
                        liqPriceElem.textContent = '$' + liqPrice.toFixed(2);
                        const liqDistPct = isLong 
                            ? ((data.position.current_price - liqPrice) / data.position.current_price) * 100
                            : ((liqPrice - data.position.current_price) / data.position.current_price) * 100;
                        liqDistElem.textContent = `+${liqDistPct.toFixed(1)}% folga`;
                    } else {
                        liqPriceElem.textContent = '--';
                        liqDistElem.textContent = '--';
                    }

                    document.getElementById('labelSL').textContent = sl ? 'SL (1.2%): $' + sl.toFixed(2) : 'SL: --';
                    document.getElementById('labelTP').textContent = data.position.entry_price > 0 ? 'TP (+2.5%): $' + tpPrice.toFixed(2) : 'TP: --';

                    if (sl && tpPrice && tpPrice > 0) {
                        const totalRange = Math.abs(tpPrice - sl);
                        const currentProgress = isLong 
                            ? ((data.position.current_price - sl) / totalRange) * 100
                            : ((sl - data.position.current_price) / totalRange) * 100;
                        const clamped = Math.max(4, Math.min(96, currentProgress));
                        document.getElementById('posBarFill').style.width = clamped + '%';
                        document.getElementById('posBarFill').style.background = unPnl >= 0 ? 'var(--green)' : 'var(--red)';
                    }
                } else {
                    posActiveBox.style.display = 'none';
                    posEmptyBox.style.display = 'flex';
                    posCard.className = 'position-card';
                    posSideBadge.textContent = 'NEUTRO';
                    posSideBadge.className = 'badge badge-symbol';
                    document.getElementById('posLevText').textContent = data.position.leverage + 'x ISOLATED';
                    currentLeverage = data.position.leverage;
                }

                // Institutional Risk Management Section
                const riskPerTrade = data.risk_per_trade || 16.0;
                const minNotional = data.min_notional || 5.50;
                document.getElementById('dashRiskPerTrade').textContent = riskPerTrade.toFixed(1) + '%';
                document.getElementById('dashMinNotional').textContent = '$' + minNotional.toFixed(2);
                document.getElementById('dashLevText').textContent = (data.position.leverage || 5) + 'x ISOLATED';

                const slTag = document.getElementById('slActiveBadge');
                if (slTag) {
                    if (data.position.amount > 0 && data.position.stop_loss) {
                        slTag.textContent = 'STOP LOSS ATIVO EM $' + data.position.stop_loss.toFixed(2);
                        slTag.className = 'badge badge-live';
                    } else if (data.position.amount > 0) {
                        slTag.textContent = 'STOP LOSS ATIVO (AUTO-PROTEÇÃO)';
                        slTag.className = 'badge badge-live';
                    } else {
                        slTag.textContent = 'STOP SUPERTREND: PRONTO PARA PRÓXIMA ENTRADA';
                        slTag.className = 'badge badge-mode';
                    }
                }

                currentSymbol = data.symbol;

                // Cascata Turbo MTF Status & Indicators
                const c = (data.indicators && data.indicators.cascade) ? data.indicators.cascade : null;
                const radarRegText = document.getElementById('radarMarketStatus');

                if (c) {
                    // Cockpit D1 & 4H
                    const d1El = document.getElementById('cockpitD1Badge');
                    if (d1El) {
                        const isBull = c.d_bias === 1 || c.d_bias === 'ALTA';
                        const isBear = c.d_bias === -1 || c.d_bias === 'BAIXA';
                        d1El.textContent = 'D1: ' + (isBull ? 'ALTA (BULL)' : (isBear ? 'BAIXA (BEAR)' : 'NEUTRO'));
                        d1El.className = 'badge ' + (isBull ? 'badge-live' : (isBear ? 'badge-status status-stopped' : 'badge-mode'));
                        d1El.style.background = isBull ? 'rgba(48, 209, 88, 0.18)' : (isBear ? 'rgba(255, 69, 58, 0.18)' : 'rgba(255, 255, 255, 0.08)');
                        d1El.style.color = isBull ? 'var(--green)' : (isBear ? 'var(--red)' : 'var(--text-tertiary)');
                    }

                    const h4El = document.getElementById('cockpitH4Badge');
                    if (h4El) {
                        const isBull = c.h_bias === 1 || c.h_bias === 'ALTA';
                        const isBear = c.h_bias === -1 || c.h_bias === 'BAIXA';
                        h4El.textContent = '4H: ' + (isBull ? 'ALTA' : (isBear ? 'BAIXA' : 'NEUTRO'));
                        h4El.className = 'badge ' + (isBull ? 'badge-live' : (isBear ? 'badge-status status-stopped' : 'badge-mode'));
                        h4El.style.background = isBull ? 'rgba(48, 209, 88, 0.18)' : (isBear ? 'rgba(255, 69, 58, 0.18)' : 'rgba(255, 255, 255, 0.08)');
                        h4El.style.color = isBull ? 'var(--green)' : (isBear ? 'var(--red)' : 'var(--text-tertiary)');
                    }

                    // Cockpit Adaptive Bands
                    const cuEl = document.getElementById('cockpitUpper');
                    if (cuEl && c.upper_entry > 0) cuEl.textContent = 'Long: $' + c.upper_entry.toFixed(2);
                    const cbEl = document.getElementById('cockpitBaseline');
                    if (cbEl && c.baseline > 0) cbEl.textContent = 'OLS: $' + c.baseline.toFixed(2);
                    const clEl = document.getElementById('cockpitLower');
                    if (clEl && c.lower_entry > 0) clEl.textContent = 'Short: $' + c.lower_entry.toFixed(2);

                    // Choppiness Index
                    const chopV = document.getElementById('cockpitChopVal');
                    const chopB = document.getElementById('cockpitChopBadge');
                    if (chopV) chopV.textContent = (c.chop_val || 0).toFixed(1);
                    if (chopB) {
                        if (c.chop_val < 46.0) {
                            chopB.textContent = 'TENDÊNCIA FORTE';
                            chopB.className = 'badge badge-live';
                            chopB.style.background = 'rgba(48, 209, 88, 0.18)';
                            chopB.style.color = 'var(--green)';
                        } else if (c.chop_val > 54.0) {
                            chopB.textContent = 'CONSOLIDAÇÃO / LATERAL';
                            chopB.className = 'badge badge-mode';
                            chopB.style.background = 'rgba(255, 159, 10, 0.18)';
                            chopB.style.color = 'var(--amber)';
                        } else {
                            chopB.textContent = 'TRANSIÇÃO / MISTO';
                            chopB.className = 'badge badge-mode';
                            chopB.style.background = 'rgba(100, 210, 255, 0.18)';
                            chopB.style.color = 'var(--cyan)';
                        }
                    }

                    // Smart Ignition
                    const ignEl = document.getElementById('cockpitIgnitionStatus');
                    if (ignEl) {
                        if (c.ignition_long) {
                            ignEl.innerHTML = '<span class="c-up" style="font-weight:800;">⚡ IGNITION LONG DETECTADO!</span>';
                        } else if (c.ignition_short) {
                            ignEl.innerHTML = '<span class="c-down" style="font-weight:800;">⚡ IGNITION SHORT DETECTADO!</span>';
                        } else if (c.chop_val > 58.0) {
                            ignEl.innerHTML = '<span class="c-amber">Smart Ignition: Travado por Alta Congestão (Chop > 58)</span>';
                        } else {
                            ignEl.innerHTML = '<span class="c-cyan">Smart Ignition: Monitorando Rompimento OLS 100</span>';
                        }
                    }

                    // Radar Market Status Text
                    if (radarRegText) {
                        const d1Str = (c.d_bias === 1 || c.d_bias === 'ALTA') ? '<span class="c-up" style="font-weight:700;">ALTA (BULL)</span>' : (c.d_bias === -1 || c.d_bias === 'BAIXA') ? '<span class="c-down" style="font-weight:700;">BAIXA (BEAR)</span>' : '<span style="color: var(--text-tertiary);">NEUTRO</span>';
                        const h4Str = (c.h_bias === 1 || c.h_bias === 'ALTA') ? '<span class="c-up" style="font-weight:700;">ALTA</span>' : (c.h_bias === -1 || c.h_bias === 'BAIXA') ? '<span class="c-down" style="font-weight:700;">BAIXA</span>' : '<span style="color: var(--text-tertiary);">NEUTRO</span>';
                        const chopDesc = c.chop_val < 46.0 ? '<span class="c-up">Tendência Forte (' + c.chop_val.toFixed(1) + ')</span>' : (c.chop_val > 54.0 ? '<span class="c-amber">Consolidação (' + c.chop_val.toFixed(1) + ')</span>' : '<span class="c-cyan">Transição (' + c.chop_val.toFixed(1) + ')</span>');
                        radarRegText.innerHTML = `Macro D1: ${d1Str} | Contexto 4h: ${h4Str} | Choppiness: ${chopDesc}<br>Bandas Adaptativas: Long <strong class="c-up">$${(c.upper_entry || 0).toFixed(2)}</strong> | OLS <strong style="color:#ffd60a;">$${(c.baseline || 0).toFixed(2)}</strong> | Short <strong class="c-down">$${(c.lower_entry || 0).toFixed(2)}</strong>`;
                    }
                }

                const levElem = document.getElementById('cockpitLevVal');
                if (levElem) {
                    levElem.textContent = (data.position.leverage || 5) + 'x ISOLATED';
                }

                const cbTag = document.getElementById('cbTag');
                if (cbTag) {
                    if (data.circuit_breaker_active) {
                        cbTag.textContent = 'CIRCUIT BREAKER: BLOQUEADO';
                        cbTag.className = 'badge badge-status status-stopped';
                    } else {
                        cbTag.textContent = 'CASCATA MTF ATIVA';
                        cbTag.className = 'badge badge-live';
                    }
                }

                // Risk & Notional display
                document.getElementById('riskBadgeText').textContent = riskPerTrade.toFixed(1) + '% Risco';
                document.getElementById('indRiskVal').textContent = riskPerTrade.toFixed(1) + '%';
                document.getElementById('indNotionalVal').textContent = '$50.00 máx';
                currentRiskPerTrade = riskPerTrade;
                currentMinNotional = minNotional;

                // Capacidade de Ordem & Viabilidade de Alavancagem
                if (data.order_feasibility) {
                    const of = data.order_feasibility;
                    const badge = document.getElementById('feasibilityBadge');
                    const diag = document.getElementById('feasibilityDiagnosis');
                    const feas1x = document.getElementById('valFeas1x');
                    const minCost1x = document.getElementById('valMinCost1x');
                    const minLevNeeded = document.getElementById('valMinLevNeeded');
                    const maxLevAllowed = document.getElementById('valMaxLevAllowed');
                    const marginReq = document.getElementById('valMarginRequiredCurrent');
                    const maxPower = document.getElementById('valMaxPurchasingPower');

                    if (badge) {
                        if (of.can_open_unleveraged) {
                            badge.textContent = 'ORDEM VIÁVEL EM 1X (SEM ALAVANCAGEM)';
                            badge.className = 'badge badge-live';
                            badge.style.background = 'rgba(48, 209, 88, 0.2)';
                            badge.style.color = 'var(--green)';
                        } else if (of.can_open_current_leverage) {
                            badge.textContent = `ORDEM VIÁVEL COM ${of.current_leverage}x`;
                            badge.className = 'badge badge-live';
                            badge.style.background = 'rgba(100, 210, 255, 0.2)';
                            badge.style.color = 'var(--cyan)';
                        } else {
                            badge.textContent = `MARGEM INSUFICIENTE EM ${of.current_leverage}x`;
                            badge.className = 'badge badge-status status-stopped';
                            badge.style.background = 'rgba(255, 69, 58, 0.2)';
                            badge.style.color = 'var(--red)';
                        }
                    }

                    if (diag) {
                        diag.innerHTML = of.diagnosis_message;
                    }

                    if (feas1x) {
                        feas1x.textContent = of.can_open_unleveraged ? '✔ SIM (Saldo Suficiente)' : '✖ NÃO (Requer Alavancagem)';
                        feas1x.className = 'ind-val ' + (of.can_open_unleveraged ? 'c-up' : 'c-amber');
                    }

                    if (minCost1x) minCost1x.textContent = '$' + of.min_order_cost_unleveraged.toFixed(2) + ' USDT';
                    if (minLevNeeded) {
                        minLevNeeded.textContent = of.min_leverage_required + 'x Mínimo';
                        minLevNeeded.className = 'ind-val ' + (of.min_leverage_required === 1 ? 'c-up' : (of.min_leverage_required <= of.max_leverage_allowed ? 'c-cyan' : 'c-down'));
                    }
                    if (maxLevAllowed) maxLevAllowed.textContent = of.max_leverage_allowed + 'x Teto';
                    if (marginReq) marginReq.textContent = '$' + of.required_margin_current.toFixed(2) + ' USDT (' + of.current_leverage + 'x)';
                    if (maxPower) maxPower.textContent = '$' + of.max_purchasing_power.toFixed(2) + ' USDT';

                    const modalHint = document.getElementById('modalLevReadinessHint');
                    if (modalHint) {
                        modalHint.innerHTML = `Saldo Livre: <strong class="c-cyan">$${of.available_balance.toFixed(2)}</strong> | Mínimo p/ Ordem: <strong class="c-amber">${of.min_leverage_required}x</strong> | Teto Binance: <strong class="c-purple">${of.max_leverage_allowed}x</strong>`;
                    }
                }

                // Recent Trades Table (Binance Detailed Statement)
                const tbody = document.getElementById('tradeHistoryBody');
                if (data.recent_trades && data.recent_trades.length > 0) {
                    let totalNet = 0;
                    let totalComm = 0;
                    let wins = 0;
                    let totalVol = 0;

                    const rows = data.recent_trades.map(t => {
                        const netPnl = t.realized_pnl || 0;
                        const grossPnl = t.gross_pnl || (netPnl + (t.commission || 0));
                        const comm = t.commission || 0;
                        const notional = t.notional > 0 ? t.notional : (t.quantity * t.price);
                        const isWin = netPnl >= 0;

                        totalNet += netPnl;
                        totalComm += comm;
                        totalVol += notional;
                        if (isWin) wins++;

                        // Format timestamp
                        let timeStr = '--:--:--';
                        if (t.timestamp) {
                            const ts = t.timestamp > 10000000000 ? t.timestamp : t.timestamp * 1000;
                            const d = new Date(ts);
                            timeStr = d.toLocaleDateString('pt-BR', { day: '2-digit', month: '2-digit' }) + ' ' + 
                                      d.toLocaleTimeString('pt-BR', { hour: '2-digit', minute: '2-digit', second: '2-digit' });
                        }

                        // Side Badge
                        const isLong = t.side === 'Buy' || t.side === 'Long';
                        const sideBadge = isLong 
                            ? `<span class="badge-trade-long">LONG</span>`
                            : `<span class="badge-trade-short">SHORT</span>`;

                        // Entry and Exit Prices
                        const entryPrice = t.entry_price > 0 ? t.entry_price : t.price;
                        const pricesStr = `$${entryPrice.toFixed(2)} → $${t.price.toFixed(2)}`;

                        // Quantity and Notional
                        const baseSymbol = (t.symbol || data.symbol || 'SOLUSDT').replace('USDT', '');
                        const qtyStr = `${t.quantity.toFixed(4)} ${baseSymbol}`;
                        const notionalStr = `<span style="color: var(--text-tertiary); font-size: 0.72rem;">($${notional.toFixed(2)})</span>`;

                        // Gross PnL
                        const grossSign = grossPnl >= 0 ? '+$' : '-$';
                        const grossClass = grossPnl >= 0 ? 'c-up' : 'c-down';
                        const grossStr = `<span class="${grossClass}">${grossSign}${Math.abs(grossPnl).toFixed(4)}</span>`;

                        // Commission
                        const commStr = `<span class="c-amber">-$${Math.abs(comm).toFixed(4)}</span>`;

                        // Net PnL (Official Realized)
                        const netSign = netPnl >= 0 ? '+$' : '-$';
                        const netClass = netPnl >= 0 ? 'c-up' : 'c-down';
                        const pctStr = (t.pnl_percent !== undefined && t.pnl_percent !== 0) 
                            ? ` <span style="font-size: 0.72rem; opacity: 0.85;">(${t.pnl_percent >= 0 ? '+' : ''}${t.pnl_percent.toFixed(2)}%)</span>` 
                            : '';
                        const netStr = `<strong class="${netClass}">${netSign}${Math.abs(netPnl).toFixed(4)}</strong>${pctStr}`;

                        // Balance After
                        const balStr = t.balance_after > 0 ? `$${t.balance_after.toFixed(2)}` : '--';

                        // Reason Badge
                        const reasonStr = `<span class="badge-trade-reason">${t.reason || 'Concluído'}</span>`;

                        return `<tr>
                            <td style="color: var(--text-tertiary); font-size: 0.74rem;">${timeStr}</td>
                            <td>${sideBadge}</td>
                            <td>${pricesStr}</td>
                            <td>${qtyStr} ${notionalStr}</td>
                            <td>${grossStr}</td>
                            <td>${commStr}</td>
                            <td>${netStr}</td>
                            <td>${balStr}</td>
                            <td>${reasonStr}</td>
                        </tr>`;
                    }).join('');

                    tbody.innerHTML = rows;

                    // Update summary strip
                    const sumNetElem = document.getElementById('sumNetPnl');
                    sumNetElem.textContent = (totalNet >= 0 ? '+$' : '-$') + Math.abs(totalNet).toFixed(4);
                    sumNetElem.className = 'trade-sum-val ' + (totalNet >= 0 ? 'c-up' : 'c-down');

                    document.getElementById('sumCommission').textContent = `-$${totalComm.toFixed(4)} USDT`;

                    const winrate = data.recent_trades.length > 0 ? ((wins / data.recent_trades.length) * 100).toFixed(1) : '0.0';
                    document.getElementById('sumWinRate').textContent = `${winrate}% (${wins}/${data.recent_trades.length})`;
                    document.getElementById('sumVolume').textContent = `$${totalVol.toFixed(2)}`;

                    document.getElementById('tradesCountSubtitle').textContent = `${data.recent_trades.length} operações registradas`;
                }

            } catch (err) {
                console.error("Erro ao sincronizar dashboard:", err);
            }
        }

        function openLeverageModal() {
            document.getElementById('customLevInput').value = currentLeverage || 5;
            document.getElementById('modalSymbol').textContent = currentSymbol || 'SOLUSDT';
            document.getElementById('levStatusMsg').textContent = '';
            document.querySelectorAll('.btn-lev-pill').forEach(b => {
                b.classList.toggle('selected', parseInt(b.textContent) === currentLeverage);
            });
            document.getElementById('leverageModal').classList.add('active');
        }

        function closeLeverageModal(e) {
            if (e && e.target !== e.currentTarget && e.target.tagName !== 'BUTTON') return;
            document.getElementById('leverageModal').classList.remove('active');
        }

        function selectQuickLev(val) {
            document.getElementById('customLevInput').value = val;
            document.querySelectorAll('.btn-lev-pill').forEach(b => {
                b.classList.toggle('selected', parseInt(b.textContent) === val);
            });
        }

        async function applyLeverage() {
            const val = parseInt(document.getElementById('customLevInput').value);
            if (!val || val < 1 || val > 125) {
                showToast('Alavancagem deve ser entre 1x e 125x', 'error');
                return;
            }
            const btn = document.getElementById('btnConfirmLev');
            const msg = document.getElementById('levStatusMsg');
            btn.disabled = true;
            btn.textContent = 'Enviando para Binance...';
            msg.textContent = '';

            try {
                const res = await fetch('/api/action/set_leverage', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({ leverage: val })
                });
                const data = await res.json();
                if (data.success) {
                    msg.innerHTML = '<span class="c-up" style="font-weight:700;">✔ Alavancagem atualizada para ' + data.leverage + 'x na Binance!</span>';
                    showToast('Alavancagem alterada para ' + data.leverage + 'x!', 'success');
                    setTimeout(() => {
                        document.getElementById('leverageModal').classList.remove('active');
                        fetchStatus();
                    }, 600);
                } else {
                    msg.innerHTML = '<span class="c-down" style="font-weight:700;">✖ ' + (data.error || 'Falha ao alterar') + '</span>';
                    showToast(data.error || 'Falha ao alterar alavancagem', 'error');
                }
            } catch (err) {
                msg.innerHTML = '<span class="c-down">✖ Erro: ' + err + '</span>';
                showToast('Erro de rede: ' + err, 'error');
            } finally {
                btn.disabled = false;
                btn.textContent = 'Aplicar na Binance';
            }
        }

        /* MODAL DE RISCO E NOCIONAL */
        function openRiskModal() {
            document.getElementById('customNotionalInput').value = currentMinNotional.toFixed(2);
            document.getElementById('customRiskInput').value = currentRiskPerTrade.toFixed(1);
            document.getElementById('riskStatusMsg').textContent = '';
            document.querySelectorAll('.btn-notional-pill').forEach(b => {
                const val = parseFloat(b.textContent.replace('$', ''));
                b.classList.toggle('selected', Math.abs(val - currentMinNotional) < 0.05);
            });
            document.querySelectorAll('.btn-risk-pill').forEach(b => {
                const val = parseFloat(b.textContent.replace('%', ''));
                b.classList.toggle('selected', Math.abs(val - currentRiskPerTrade) < 0.1);
            });
            document.getElementById('riskModal').classList.add('active');
        }

        function closeRiskModal(e) {
            if (e && e.target !== e.currentTarget && e.target.tagName !== 'BUTTON') return;
            document.getElementById('riskModal').classList.remove('active');
        }

        function selectQuickNotional(val) {
            document.getElementById('customNotionalInput').value = val.toFixed(2);
            document.querySelectorAll('.btn-notional-pill').forEach(b => {
                const bVal = parseFloat(b.textContent.replace('$', ''));
                b.classList.toggle('selected', Math.abs(bVal - val) < 0.05);
            });
        }

        function selectQuickRisk(val) {
            document.getElementById('customRiskInput').value = val.toFixed(1);
            document.querySelectorAll('.btn-risk-pill').forEach(b => {
                const bVal = parseFloat(b.textContent.replace('%', ''));
                b.classList.toggle('selected', Math.abs(bVal - val) < 0.1);
            });
        }

        async function applyRiskParams() {
            const notional = parseFloat(document.getElementById('customNotionalInput').value);
            const risk = parseFloat(document.getElementById('customRiskInput').value);
            if (!notional || notional < 5.0) {
                showToast('Piso Nocional deve ser pelo menos $5.00 USDT', 'error');
                return;
            }
            if (!risk || risk < 0.5 || risk > 25.0) {
                showToast('Risco por trade deve ser entre 0.5% e 25.0%', 'error');
                return;
            }
            const btn = document.getElementById('btnConfirmRisk');
            const msg = document.getElementById('riskStatusMsg');
            btn.disabled = true;
            btn.textContent = 'Salvando parâmetros...';
            msg.textContent = '';

            try {
                const res = await fetch('/api/action/set_risk_params', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({ min_notional: notional, risk_per_trade: risk })
                });
                const data = await res.json();
                if (data.success) {
                    msg.innerHTML = '<span class="c-up" style="font-weight:700;">✔ Parâmetros atualizados: $' + data.min_notional.toFixed(2) + ' min | ' + data.risk_per_trade.toFixed(1) + '% risco</span>';
                    showToast('Risco atualizado: ' + data.risk_per_trade.toFixed(1) + '% | Min: $' + data.min_notional.toFixed(2), 'success');
                    setTimeout(() => {
                        document.getElementById('riskModal').classList.remove('active');
                        fetchStatus();
                    }, 600);
                } else {
                    msg.innerHTML = '<span class="c-down" style="font-weight:700;">✖ ' + (data.error || 'Falha ao salvar') + '</span>';
                    showToast(data.error || 'Falha ao atualizar parâmetros', 'error');
                }
            } catch (err) {
                msg.innerHTML = '<span class="c-down">✖ Erro: ' + err + '</span>';
                showToast('Erro de rede: ' + err, 'error');
            } finally {
                btn.disabled = false;
                btn.textContent = 'Salvar Parâmetros no Bot';
            }
        }

        /* ==========================================================
           LIGHTWEIGHT CHARTS NATIVE INTEGRATION & CASCADE MTF
           ========================================================== */
        let nativeChart = null;
        let candleSeries = null;
        let baselineSeries = null;
        let upperSeries = null;
        let lowerSeries = null;
        let stopSeries = null;
        let lastCascadeConfig = null;
        let chartDataLoaded = false;

        function initNativeChart() {
            const chartDiv = document.getElementById('nativeChart');
            if (!chartDiv || !window.LightweightCharts) return;

            nativeChart = LightweightCharts.createChart(chartDiv, {
                width: chartDiv.clientWidth,
                height: 500,
                layout: {
                    background: { color: '#0c0d10' },
                    textColor: '#a1a1a6',
                    fontFamily: '-apple-system, BlinkMacSystemFont, "SF Pro Text", "Inter", sans-serif',
                },
                grid: {
                    vertLines: { color: 'rgba(255, 255, 255, 0.04)' },
                    horzLines: { color: 'rgba(255, 255, 255, 0.04)' },
                },
                crosshair: {
                    mode: LightweightCharts.CrosshairMode.Normal,
                    vertLine: {
                        color: 'rgba(255, 255, 255, 0.25)',
                        width: 1,
                        style: 3,
                    },
                    horzLine: {
                        color: 'rgba(255, 255, 255, 0.25)',
                        width: 1,
                        style: 3,
                    },
                },
                rightPriceScale: {
                    borderColor: 'rgba(255, 255, 255, 0.08)',
                    scaleMargins: {
                        top: 0.1,
                        bottom: 0.15,
                    },
                },
                timeScale: {
                    borderColor: 'rgba(255, 255, 255, 0.08)',
                    timeVisible: true,
                    secondsVisible: false,
                },
            });

            candleSeries = nativeChart.addCandlestickSeries({
                upColor: '#30d158',
                downColor: '#ff453a',
                borderVisible: false,
                wickUpColor: '#30d158',
                wickDownColor: '#ff453a',
            });

            baselineSeries = nativeChart.addLineSeries({
                color: '#ffd60a',
                lineWidth: 2,
                title: 'OLS Baseline',
                priceLineVisible: false,
            });

            upperSeries = nativeChart.addLineSeries({
                color: '#30d158',
                lineWidth: 1.5,
                lineStyle: LightweightCharts.LineStyle.Dashed,
                title: 'Upper Band',
                priceLineVisible: false,
            });

            lowerSeries = nativeChart.addLineSeries({
                color: '#ff453a',
                lineWidth: 1.5,
                lineStyle: LightweightCharts.LineStyle.Dashed,
                title: 'Lower Band',
                priceLineVisible: false,
            });

            stopSeries = nativeChart.addLineSeries({
                color: '#ff9f0a',
                lineWidth: 2,
                lineStyle: LightweightCharts.LineStyle.Solid,
                title: 'Supertrend Stop',
                priceLineVisible: false,
            });

            // Crosshair subscriber to update legend strip on hover
            nativeChart.subscribeCrosshairMove(param => {
                if (!param || !param.time || !param.seriesData) return;
                const bVal = param.seriesData.get(baselineSeries);
                const uVal = param.seriesData.get(upperSeries);
                const lVal = param.seriesData.get(lowerSeries);
                const sVal = param.seriesData.get(stopSeries);

                if (bVal && bVal.value !== undefined) {
                    document.getElementById('legBaseline').textContent = '$' + bVal.value.toFixed(2);
                }
                if (uVal && uVal.value !== undefined) {
                    document.getElementById('legUpper').textContent = '$' + uVal.value.toFixed(2);
                }
                if (lVal && lVal.value !== undefined) {
                    document.getElementById('legLower').textContent = '$' + lVal.value.toFixed(2);
                }
                if (sVal && sVal.value !== undefined) {
                    document.getElementById('legStop').textContent = '$' + sVal.value.toFixed(2);
                }
            });

            window.addEventListener('resize', () => {
                if (nativeChart && chartDiv) {
                    nativeChart.applyOptions({
                        width: chartDiv.clientWidth,
                    });
                }
            });
        }

        async function fetchChartData() {
            try {
                const res = await fetch('/api/chart_data');
                if (!res.ok) return;
                const data = await res.json();

                if (!nativeChart) {
                    initNativeChart();
                }

                if (candleSeries && data.candles && data.candles.length > 0) {
                    candleSeries.setData(data.candles);
                    if (data.markers && data.markers.length > 0) {
                        candleSeries.setMarkers(data.markers);
                    }
                    if (!chartDataLoaded) {
                        nativeChart.timeScale().fitContent();
                        chartDataLoaded = true;
                    }
                }

                if (baselineSeries && data.baseline) baselineSeries.setData(data.baseline);
                if (upperSeries && data.upper_entry) upperSeries.setData(data.upper_entry);
                if (lowerSeries && data.lower_entry) lowerSeries.setData(data.lower_entry);
                if (stopSeries && data.active_stop) stopSeries.setData(data.active_stop);

                // Update default legend strip from last values
                if (data.baseline && data.baseline.length > 0) {
                    const lastB = data.baseline[data.baseline.length - 1].value;
                    document.getElementById('legBaseline').textContent = '$' + lastB.toFixed(2);
                }
                if (data.upper_entry && data.upper_entry.length > 0) {
                    const lastU = data.upper_entry[data.upper_entry.length - 1].value;
                    document.getElementById('legUpper').textContent = '$' + lastU.toFixed(2);
                }
                if (data.lower_entry && data.lower_entry.length > 0) {
                    const lastL = data.lower_entry[data.lower_entry.length - 1].value;
                    document.getElementById('legLower').textContent = '$' + lastL.toFixed(2);
                }
                if (data.active_stop && data.active_stop.length > 0) {
                    const lastS = data.active_stop[data.active_stop.length - 1].value;
                    document.getElementById('legStop').textContent = '$' + lastS.toFixed(2);
                } else {
                    document.getElementById('legStop').textContent = 'Nenhum';
                }

                // Update Prop Desk Cascade HUD
                if (data.cascade) {
                    const c = data.cascade;
                    const macroEl = document.getElementById('hudMacroBias');
                    if (macroEl) {
                        const isBull = c.d_bias === 1 || c.d_bias === 'ALTA';
                        const isBear = c.d_bias === -1 || c.d_bias === 'BAIXA';
                        macroEl.textContent = isBull ? 'ALTA (BULL)' : (isBear ? 'BAIXA (BEAR)' : 'NEUTRO');
                        macroEl.className = 'cascade-hud-val ' + (isBull ? 'c-up' : (isBear ? 'c-down' : ''));
                    }

                    const ctxEl = document.getElementById('hudCtxBias');
                    if (ctxEl) {
                        const isBull = c.h_bias === 1 || c.h_bias === 'ALTA';
                        const isBear = c.h_bias === -1 || c.h_bias === 'BAIXA';
                        ctxEl.textContent = isBull ? 'ALTA' : (isBear ? 'BAIXA' : 'NEUTRO');
                        ctxEl.className = 'cascade-hud-val ' + (isBull ? 'c-up' : (isBear ? 'c-down' : ''));
                    }

                    const convLEl = document.getElementById('hudConvLong');
                    if (convLEl) convLEl.textContent = (c.size_factor_long * 100).toFixed(0) + '%';

                    const convSEl = document.getElementById('hudConvShort');
                    if (convSEl) convSEl.textContent = (c.size_factor_short * 100).toFixed(0) + '%';

                    const slopeEl = document.getElementById('hudSlopeExec');
                    if (slopeEl) {
                        slopeEl.textContent = c.norm_slope.toFixed(3);
                        slopeEl.className = 'cascade-hud-val ' + (c.norm_slope > 0 ? 'c-up' : (c.norm_slope < 0 ? 'c-down' : ''));
                    }

                    const stEl = document.getElementById('hudScoreStruct');
                    if (stEl) stEl.textContent = c.structure_score_long.toFixed(2);

                    const momEl = document.getElementById('hudScoreMom');
                    if (momEl) momEl.textContent = c.momentum_score_long.toFixed(2);

                    const exhEl = document.getElementById('hudScoreExh');
                    if (exhEl) exhEl.textContent = c.exhaustion_score_long.toFixed(2);

                    const chopEl = document.getElementById('hudScoreChop');
                    if (chopEl) chopEl.textContent = c.chop_score.toFixed(2);

                    const scLongEl = document.getElementById('hudScoreLong');
                    if (scLongEl) scLongEl.textContent = c.confluence_score_long.toFixed(2);

                    const scShortEl = document.getElementById('hudScoreShort');
                    if (scShortEl) scShortEl.textContent = c.confluence_score_short.toFixed(2);
                }

                if (data.cascade_config) {
                    lastCascadeConfig = data.cascade_config;
                    const threshEl = document.getElementById('hudMinThreshold');
                    if (threshEl) threshEl.textContent = data.cascade_config.min_confluence.toFixed(2);

                    const badge = document.getElementById('cascadeActiveBadge');
                    if (badge) {
                        badge.textContent = data.cascade_config.enabled ? 'MOTOR ATIVO' : 'DESATIVADO';
                        badge.className = 'badge ' + (data.cascade_config.enabled ? 'badge-live' : 'badge-status status-stopped');
                    }
                }

                const chartStatus = document.getElementById('chartStatusBadge');
                if (chartStatus) {
                    chartStatus.textContent = `${data.symbol || 'BINANCE'} ${data.interval || '15m'} • AO VIVO`;
                }

            } catch (err) {
                console.error("Erro ao sincronizar gráfico nativo:", err);
            }
        }

        /* MODAL DE CONFIGURAÇÃO DA CASCATA MTF */
        function openCascadeModal() {
            if (lastCascadeConfig) {
                document.getElementById('cascadeEnabledInput').checked = !!lastCascadeConfig.enabled;
                document.getElementById('cascadeLengthInput').value = lastCascadeConfig.length || 100;
                document.getElementById('cascadeSdEntryInput').value = (lastCascadeConfig.sd_entry || 2.0).toFixed(1);
                document.getElementById('cascadeMinSlopeInput').value = (lastCascadeConfig.min_norm_slope || 0.10).toFixed(2);
                document.getElementById('cascadeMinConfInput').value = (lastCascadeConfig.min_confluence || 0.70).toFixed(2);
                document.getElementById('cascadeAtrLenInput').value = lastCascadeConfig.atr_len || 22;
                document.getElementById('cascadeAtrMultInput').value = (lastCascadeConfig.atr_mult || 3.0).toFixed(1);
                document.getElementById('cascadeRiskInput').value = (lastCascadeConfig.risk_per_trade || 2.0).toFixed(1);
                document.getElementById('cascadeUseMtfInput').checked = !!lastCascadeConfig.use_mtf;
                document.getElementById('cascadeUseIgnitionInput').checked = !!lastCascadeConfig.use_ignition;
            }
            document.getElementById('cascadeStatusMsg').textContent = '';
            document.getElementById('cascadeModal').classList.add('active');
        }

        function closeCascadeModal(e) {
            if (e && e.target !== e.currentTarget && e.target.tagName !== 'BUTTON') return;
            document.getElementById('cascadeModal').classList.remove('active');
        }

        async function applyCascadeConfig() {
            const btn = document.getElementById('btnConfirmCascade');
            const msg = document.getElementById('cascadeStatusMsg');
            btn.disabled = true;
            btn.textContent = 'Gravando parâmetros...';
            msg.textContent = '';

            const payload = {
                enabled: document.getElementById('cascadeEnabledInput').checked,
                length: parseInt(document.getElementById('cascadeLengthInput').value) || 100,
                sd_entry: parseFloat(document.getElementById('cascadeSdEntryInput').value) || 2.0,
                min_norm_slope: parseFloat(document.getElementById('cascadeMinSlopeInput').value) || 0.10,
                use_ignition: document.getElementById('cascadeUseIgnitionInput').checked,
                ign_atr_mult: 1.5,
                ign_vol_mult: 1.8,
                use_mtf: document.getElementById('cascadeUseMtfInput').checked,
                min_confluence: parseFloat(document.getElementById('cascadeMinConfInput').value) || 0.70,
                atr_len: parseInt(document.getElementById('cascadeAtrLenInput').value) || 22,
                atr_mult: parseFloat(document.getElementById('cascadeAtrMultInput').value) || 3.0,
                risk_per_trade: parseFloat(document.getElementById('cascadeRiskInput').value) || 2.0
            };

            try {
                const res = await fetch('/api/action/update_cascade_config', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify(payload)
                });
                const data = await res.json();
                if (data.success) {
                    lastCascadeConfig = data.config;
                    msg.innerHTML = '<span class="c-up" style="font-weight:700;">✔ Parâmetros da Cascata atualizados!</span>';
                    showToast('Parâmetros da Cascata salvos com sucesso!', 'success');
                    setTimeout(() => {
                        document.getElementById('cascadeModal').classList.remove('active');
                        fetchChartData();
                    }, 500);
                } else {
                    msg.innerHTML = '<span class="c-down" style="font-weight:700;">✖ Falha ao salvar</span>';
                    showToast('Falha ao atualizar parâmetros', 'error');
                }
            } catch (err) {
                msg.innerHTML = '<span class="c-down">✖ Erro: ' + err + '</span>';
                showToast('Erro de rede: ' + err, 'error');
            } finally {
                btn.disabled = false;
                btn.textContent = 'Salvar e Aplicar';
            }
        }

        setInterval(fetchStatus, 1000);
        setInterval(fetchChartData, 2000);
        fetchStatus();
        fetchChartData();
    </script>
</body>
</html>
"##;
