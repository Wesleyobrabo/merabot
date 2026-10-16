#![allow(dead_code)]

use crate::types::{Candle, OrderSide, Position, PositionSide, SymbolRules, WalletInfo};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use hmac::{Hmac, Mac};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::Value;
use sha2::Sha256;
use tracing::{debug, info, warn};

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn precision_from_step(step: f64) -> u32 {
    if step <= 0.0 {
        return 2;
    }
    let s = format!("{:.8}", step);
    let trimmed = s.trim_end_matches('0');
    if let Some(pos) = trimmed.find('.') {
        (trimmed.len() - pos - 1) as u32
    } else {
        0
    }
}

type HmacSha256 = Hmac<Sha256>;

#[allow(dead_code)]
#[derive(Clone)]
pub struct BinanceFuturesClient {
    client: reqwest::Client,
    api_key: String,
    api_secret: String,
    base_url: String,
    time_offset: Arc<AtomicI64>,
}

impl BinanceFuturesClient {
    pub fn new(api_key: String, api_secret: String, base_url: String) -> Self {
        let mut headers = HeaderMap::new();
        if !api_key.is_empty() {
            if let Ok(val) = HeaderValue::from_str(&api_key) {
                headers.insert("X-MBX-APIKEY", val);
            }
        }

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(4))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            client,
            api_key,
            api_secret,
            base_url,
            time_offset: Arc::new(AtomicI64::new(0)),
        }
    }

    pub async fn sync_server_time(&self) -> Result<i64> {
        let start = Utc::now().timestamp_millis();
        let server_time = self.get_server_time().await?;
        let end = Utc::now().timestamp_millis();
        let local_mid = (start + end) / 2;
        let offset = server_time - local_mid;
        self.time_offset.store(offset, Ordering::Relaxed);
        info!(
            "[SINCRONIA RELÓGIO BINANCE] Server: {} | Local: {} | Offset: {}ms",
            server_time, local_mid, offset
        );
        Ok(offset)
    }

    pub fn get_timestamp(&self) -> i64 {
        Utc::now().timestamp_millis() + self.time_offset.load(Ordering::Relaxed)
    }

    fn sign(&self, query: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(self.api_secret.as_bytes())
            .expect("HMAC can take key of any size");
        mac.update(query.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    fn build_signed_query(&self, params: &str) -> String {
        let ts = self.get_timestamp();
        let delimiter = if params.is_empty() { "" } else { "&" };
        let full_query = format!("{}{}timestamp={}&recvWindow=10000", params, delimiter, ts);
        let signature = self.sign(&full_query);
        format!("{}&signature={}", full_query, signature)
    }

    pub async fn get_server_time(&self) -> Result<i64> {
        let url = format!("{}/fapi/v1/time", self.base_url);
        let resp = self.client.get(&url).send().await?.json::<Value>().await?;
        if let Some(time) = resp.get("serverTime").and_then(|v| v.as_i64()) {
            Ok(time)
        } else {
            bail!("Falha ao obter serverTime: {:?}", resp);
        }
    }

    pub async fn get_klines(&self, symbol: &str, interval: &str, limit: usize) -> Result<Vec<Candle>> {
        let url = format!(
            "{}/fapi/v1/klines?symbol={}&interval={}&limit={}",
            self.base_url, symbol, interval, limit
        );

        let resp = self.client.get(&url).send().await?.json::<Value>().await?;
        let arr = resp.as_array().context("Resposta de klines nao e um array")?;

        let mut candles = Vec::with_capacity(arr.len());
        for item in arr {
            let row = item.as_array().context("Linha de kline invalida")?;
            if row.len() >= 7 {
                let open_time = row[0].as_i64().unwrap_or(0);
                let open: f64 = row[1].as_str().unwrap_or("0").parse().unwrap_or(0.0);
                let high: f64 = row[2].as_str().unwrap_or("0").parse().unwrap_or(0.0);
                let low: f64 = row[3].as_str().unwrap_or("0").parse().unwrap_or(0.0);
                let close: f64 = row[4].as_str().unwrap_or("0").parse().unwrap_or(0.0);
                let volume: f64 = row[5].as_str().unwrap_or("0").parse().unwrap_or(0.0);
                let close_time = row[6].as_i64().unwrap_or(0);

                candles.push(Candle {
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
        }

        Ok(candles)
    }

    pub async fn get_klines_with_fallback(&self, symbol: &str, interval: &str, limit: usize) -> Result<Vec<Candle>> {
        match self.get_klines(symbol, interval, limit).await {
            Ok(candles) if candles.len() >= 96 => {
                return Ok(candles);
            }
            Ok(few) => {
                warn!("Endpoint {} retornou apenas {} candles (esperado >= 96). Recorrendo ao endpoint público de produção...", self.base_url, few.len());
            }
            Err(e) => {
                warn!("Falha ao consultar klines em {}: {}. Recorrendo ao endpoint público de produção...", self.base_url, e);
            }
        }

        let fallback_url = format!(
            "https://fapi.binance.com/fapi/v1/klines?symbol={}&interval={}&limit={}",
            symbol, interval, limit
        );
        let resp = self.client.get(&fallback_url).send().await?.json::<Value>().await?;
        let arr = resp.as_array().context("Resposta de klines de fallback nao e array")?;

        let mut candles = Vec::with_capacity(arr.len());
        for item in arr {
            let row = item.as_array().context("Linha de kline invalida")?;
            if row.len() >= 7 {
                let open_time = row[0].as_i64().unwrap_or(0);
                let open: f64 = row[1].as_str().unwrap_or("0").parse().unwrap_or(0.0);
                let high: f64 = row[2].as_str().unwrap_or("0").parse().unwrap_or(0.0);
                let low: f64 = row[3].as_str().unwrap_or("0").parse().unwrap_or(0.0);
                let close: f64 = row[4].as_str().unwrap_or("0").parse().unwrap_or(0.0);
                let volume: f64 = row[5].as_str().unwrap_or("0").parse().unwrap_or(0.0);
                let close_time = row[6].as_i64().unwrap_or(0);

                candles.push(Candle {
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
        }

        if candles.is_empty() {
            bail!("Nenhum candle historico obtido para inicializacao");
        }

        Ok(candles)
    }

    pub async fn get_symbol_rules(&self, symbol: &str) -> Result<SymbolRules> {
        let mut urls = vec![format!("{}/fapi/v1/exchangeInfo", self.base_url)];
        if !self.base_url.contains("fapi.binance.com") {
            urls.push("https://fapi.binance.com/fapi/v1/exchangeInfo".to_string());
        }

        let mut target_symbol_data = None;
        let mut last_err = String::from("Nenhum endpoint respondeu");

        for url in urls {
            match self.client.get(&url).send().await {
                Ok(resp) => {
                    if let Ok(json) = resp.json::<Value>().await {
                        if let Some(symbols) = json.get("symbols").and_then(|s| s.as_array()) {
                            if let Some(sym) = symbols.iter().find(|s| {
                                s.get("symbol")
                                    .and_then(|v| v.as_str())
                                    .map(|name| name.eq_ignore_ascii_case(symbol))
                                    .unwrap_or(false)
                            }) {
                                target_symbol_data = Some(sym.clone());
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    last_err = format!("{}: {}", url, e);
                }
            }
        }

        let sym = target_symbol_data
            .ok_or_else(|| anyhow::anyhow!("Par {} nao encontrado no exchangeInfo da Binance ({})", symbol, last_err))?;

        let qty_precision_raw = sym.get("quantityPrecision").and_then(|v| v.as_u64()).unwrap_or(2) as u32;
        let price_precision_raw = sym.get("pricePrecision").and_then(|v| v.as_u64()).unwrap_or(2) as u32;

        let mut min_qty: f64 = 0.01;
        let mut max_qty: f64 = 100000.0;
        let mut step_size: f64 = 0.01;
        let mut min_price: f64 = 0.01;
        let mut max_price: f64 = 100000.0;
        let mut tick_size: f64 = 0.01;
        let mut min_notional: f64 = 5.0;

        if let Some(filters) = sym.get("filters").and_then(|f| f.as_array()) {
            for f in filters {
                let f_type = f.get("filterType").and_then(|v| v.as_str()).unwrap_or("");
                match f_type {
                    "LOT_SIZE" => {
                        if let Some(v) = f.get("minQty").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()) {
                            min_qty = v;
                        }
                        if let Some(v) = f.get("maxQty").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()) {
                            max_qty = v;
                        }
                        if let Some(v) = f.get("stepSize").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()) {
                            step_size = v;
                        }
                    }
                    "MARKET_LOT_SIZE" => {
                        if let Some(v) = f.get("maxQty").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()) {
                            if v > 0.0 {
                                max_qty = max_qty.min(v);
                            }
                        }
                    }
                    "PRICE_FILTER" => {
                        if let Some(v) = f.get("minPrice").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()) {
                            min_price = v;
                        }
                        if let Some(v) = f.get("maxPrice").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()) {
                            max_price = v;
                        }
                        if let Some(v) = f.get("tickSize").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()) {
                            tick_size = v;
                        }
                    }
                    "MIN_NOTIONAL" => {
                        if let Some(v) = f.get("notional").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()) {
                            min_notional = v;
                        }
                    }
                    _ => {}
                }
            }
        }

        let qty_precision = qty_precision_raw.max(precision_from_step(step_size));
        let price_precision = price_precision_raw.max(precision_from_step(tick_size));

        // Tenta consultar leverageBracket para teto maximo nocional e alavancagem suportada na exchange
        let bracket_url = format!("{}/fapi/v1/leverageBracket?symbol={}", self.base_url, symbol);
        let mut max_notional_bracket = None;
        let mut max_leverage = None;

        if let Ok(resp) = self.client.get(&bracket_url).send().await {
            if let Ok(val) = resp.json::<Value>().await {
                if let Some(b0) = val.as_array()
                    .and_then(|arr| arr.first())
                    .and_then(|item| item.get("brackets"))
                    .and_then(|b| b.as_array())
                    .and_then(|b_arr| b_arr.first())
                {
                    max_notional_bracket = b0.get("notionalCap").and_then(|nc| nc.as_f64());
                    max_leverage = b0.get("initialLeverage").and_then(|l| l.as_u64()).map(|l| l as u32);
                }
            }
        }

        // Se a chamada de bracket exigir assinatura na testnet, utiliza teto seguro de 50x para SOL / 75x geral
        let max_leverage = max_leverage.or_else(|| {
            if symbol.starts_with("BTC") || symbol.starts_with("ETH") {
                Some(100)
            } else if symbol.starts_with("SOL") {
                Some(50)
            } else {
                Some(50)
            }
        });

        Ok(SymbolRules {
            symbol: symbol.to_string(),
            min_qty,
            max_qty,
            step_size,
            qty_precision,
            min_price,
            max_price,
            tick_size,
            price_precision,
            min_notional,
            max_notional_bracket,
            max_leverage,
        })
    }

    pub async fn set_leverage(&self, symbol: &str, leverage: u32) -> Result<()> {
        let signed_query = self.build_signed_query(&format!("symbol={}&leverage={}", symbol, leverage));
        let url = format!("{}/fapi/v1/leverage?{}", self.base_url, signed_query);

        let resp = self.client.post(&url).send().await?.json::<Value>().await?;
        if resp.get("code").is_some() && resp["code"] != 200 {
            bail!("Erro ao configurar alavancagem: {:?}", resp);
        }
        info!("Alavancagem configurada para {}x em {}", leverage, symbol);
        Ok(())
    }

    pub async fn set_margin_type(&self, symbol: &str, margin_type: &str) -> Result<()> {
        let signed_query = self.build_signed_query(&format!("symbol={}&marginType={}", symbol, margin_type));
        let url = format!("{}/fapi/v1/marginType?{}", self.base_url, signed_query);

        let resp = self.client.post(&url).send().await?.json::<Value>().await?;
        if let Some(code) = resp.get("code").and_then(|c| c.as_i64()) {
            if code == -4046 {
                debug!("Tipo de margem ja esta configurado como {}", margin_type);
                return Ok(());
            }
            if code != 200 {
                warn!("Aviso marginType: {:?}", resp);
            }
        }
        Ok(())
    }

    pub async fn get_account_wallet_info(&self) -> Result<WalletInfo> {
        let signed_query = self.build_signed_query("");
        let url = format!("{}/fapi/v2/balance?{}", self.base_url, signed_query);

        let resp = self.client.get(&url).send().await?.json::<Value>().await?;
        let arr = resp.as_array().context("Resposta de saldo nao e array")?;

        for item in arr {
            if item.get("asset").and_then(|a| a.as_str()) == Some("USDT") {
                let total: f64 = item
                    .get("balance")
                    .and_then(|b| b.as_str())
                    .unwrap_or("0")
                    .parse()
                    .unwrap_or(0.0);
                let available: f64 = item
                    .get("availableBalance")
                    .and_then(|b| b.as_str())
                    .unwrap_or("0")
                    .parse()
                    .unwrap_or(0.0);
                let unpnl: f64 = item
                    .get("crossUnPnl")
                    .and_then(|b| b.as_str())
                    .unwrap_or("0")
                    .parse()
                    .unwrap_or(0.0);

                return Ok(WalletInfo {
                    total_balance: total,
                    available_balance: available,
                    unrealized_pnl: unpnl,
                });
            }
        }

        bail!("Ativo USDT nao encontrado no saldo");
    }

    pub async fn get_account_usdt_balance(&self) -> Result<f64> {
        let info = self.get_account_wallet_info().await?;
        Ok(info.total_balance)
    }

    pub async fn get_position_risk(&self, symbol: &str) -> Result<Option<Position>> {
        let signed_query = self.build_signed_query(&format!("symbol={}", symbol));
        let url = format!("{}/fapi/v2/positionRisk?{}", self.base_url, signed_query);

        let resp = self.client.get(&url).send().await?.json::<Value>().await?;
        let arr = resp.as_array().context("Resposta de positionRisk nao e array")?;

        for item in arr {
            if item.get("symbol").and_then(|s| s.as_str()) == Some(symbol) {
                let amt_str = item.get("positionAmt").and_then(|v| v.as_str()).unwrap_or("0");
                let amt: f64 = amt_str.parse().unwrap_or(0.0);

                let entry_str = item.get("entryPrice").and_then(|v| v.as_str()).unwrap_or("0");
                let entry: f64 = entry_str.parse().unwrap_or(0.0);

                let mark_str = item.get("markPrice").and_then(|v| v.as_str()).unwrap_or("0");
                let mark: f64 = mark_str.parse().unwrap_or(0.0);

                let pnl_str = item.get("unRealizedProfit").and_then(|v| v.as_str()).unwrap_or("0");
                let pnl: f64 = pnl_str.parse().unwrap_or(0.0);

                let lev_str = item.get("leverage").and_then(|v| v.as_str()).unwrap_or("1");
                let lev: u32 = lev_str.parse().unwrap_or(1);

                let side = if amt > 0.0 {
                    PositionSide::Long
                } else if amt < 0.0 {
                    PositionSide::Short
                } else {
                    PositionSide::None
                };

                return Ok(Some(Position {
                    symbol: symbol.to_string(),
                    side,
                    amount: amt.abs(),
                    entry_price: entry,
                    current_price: mark,
                    unrealized_pnl: pnl,
                    leverage: lev,
                    stop_loss: None,
                    take_profit: None,
                    tp_partial: None,
                    liquidation_price: None,
                    max_loss_usdt: None,
                    distance_to_sl_pct: None,
                    be_locked: false,
                    partial_taken: false,
                    initial_amount: amt.abs(),
                }));
            }
        }

        Ok(None)
    }

    pub async fn place_market_order(
        &self,
        symbol: &str,
        side: OrderSide,
        quantity: f64,
        reduce_only: bool,
        client_order_id: Option<String>,
        qty_precision: u32,
    ) -> Result<Value> {
        let side_str = match side {
            OrderSide::Buy => "BUY",
            OrderSide::Sell => "SELL",
        };
        let qty_fmt = format!("{:.*}", qty_precision as usize, quantity);

        let mut params = format!(
            "symbol={}&side={}&type=MARKET&quantity={}",
            symbol, side_str, qty_fmt
        );

        if reduce_only {
            params.push_str("&reduceOnly=true");
        }

        if let Some(cid) = client_order_id {
            params.push_str(&format!("&newClientOrderId={}", cid));
        }

        let signed_query = self.build_signed_query(&params);
        let url = format!("{}/fapi/v1/order?{}", self.base_url, signed_query);

        let resp = self.client.post(&url).send().await?.json::<Value>().await?;
        if let Some(code) = resp.get("code") {
            if code != 200 {
                bail!("Erro ao enviar ordem a mercado na Binance: {:?}", resp);
            }
        }

        info!("Ordem a mercado enviada com sucesso: {:?}", resp);
        Ok(resp)
    }

    pub async fn place_stop_market_order(
        &self,
        symbol: &str,
        side: OrderSide,
        stop_price: f64,
        client_order_id: Option<String>,
        price_precision: u32,
    ) -> Result<Value> {
        let side_str = match side {
            OrderSide::Buy => "BUY",
            OrderSide::Sell => "SELL",
        };
        let price_fmt = format!("{:.*}", price_precision as usize, stop_price);

        let mut params = format!(
            "symbol={}&side={}&type=STOP_MARKET&stopPrice={}&closePosition=true",
            symbol, side_str, price_fmt
        );

        if let Some(cid) = client_order_id {
            params.push_str(&format!("&newClientOrderId={}", cid));
        }

        let signed_query = self.build_signed_query(&params);
        let url = format!("{}/fapi/v1/order?{}", self.base_url, signed_query);

        let resp = self.client.post(&url).send().await?.json::<Value>().await?;
        if let Some(code) = resp.get("code") {
            if code != 200 {
                bail!("Erro ao posicionar Stop Loss na Binance: {:?}", resp);
            }
        }

        info!("Stop Loss nativo (STOP_MARKET) posicionado na Binance em {} a {}: {:?}", symbol, price_fmt, resp);
        Ok(resp)
    }

    pub async fn place_take_profit_market_order(
        &self,
        symbol: &str,
        side: OrderSide,
        stop_price: f64,
        client_order_id: Option<String>,
        price_precision: u32,
    ) -> Result<Value> {
        let side_str = match side {
            OrderSide::Buy => "BUY",
            OrderSide::Sell => "SELL",
        };
        let price_fmt = format!("{:.*}", price_precision as usize, stop_price);

        let mut params = format!(
            "symbol={}&side={}&type=TAKE_PROFIT_MARKET&stopPrice={}&closePosition=true",
            symbol, side_str, price_fmt
        );

        if let Some(cid) = client_order_id {
            params.push_str(&format!("&newClientOrderId={}", cid));
        }

        let signed_query = self.build_signed_query(&params);
        let url = format!("{}/fapi/v1/order?{}", self.base_url, signed_query);

        let resp = self.client.post(&url).send().await?.json::<Value>().await?;
        if let Some(code) = resp.get("code") {
            if code != 200 {
                bail!("Erro ao posicionar Take Profit na Binance: {:?}", resp);
            }
        }

        info!("Take Profit nativo (TAKE_PROFIT_MARKET) posicionado na Binance em {} a {}: {:?}", symbol, price_fmt, resp);
        Ok(resp)
    }

    pub async fn cancel_all_open_orders(&self, symbol: &str) -> Result<()> {
        let signed_query = self.build_signed_query(&format!("symbol={}", symbol));
        let url = format!("{}/fapi/v1/allOpenOrders?{}", self.base_url, signed_query);

        let resp = self.client.delete(&url).send().await?.json::<Value>().await?;
        info!("Todas as ordens abertas para {} foram canceladas: {:?}", symbol, resp);
        Ok(())
    }

    pub async fn get_user_trades(&self, symbol: &str, limit: usize) -> Result<Vec<Value>> {
        let signed_query = self.build_signed_query(&format!("symbol={}&limit={}", symbol, limit));
        let url = format!("{}/fapi/v1/userTrades?{}", self.base_url, signed_query);

        let resp = self.client.get(&url).send().await?.json::<Value>().await?;
        if let Some(arr) = resp.as_array() {
            Ok(arr.clone())
        } else {
            bail!("Falha ao consultar userTrades na Binance: {:?}", resp);
        }
    }
}
