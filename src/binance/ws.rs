use crate::types::Candle;
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

pub struct BinanceFuturesWs {
    ws_base_url: String,
    symbol: String,
    interval: String,
}

impl BinanceFuturesWs {
    pub fn new(ws_base_url: String, symbol: String, interval: String) -> Self {
        Self {
            ws_base_url,
            symbol,
            interval,
        }
    }

    pub async fn run(&self, candle_tx: mpsc::Sender<Candle>) {
        let stream_name = format!("{}@kline_{}", self.symbol.to_lowercase(), self.interval);
        let endpoint = format!("{}/{}", self.ws_base_url, stream_name);

        let mut backoff = Duration::from_secs(1);

        loop {
            info!("Conectando ao WebSocket da Binance Futures: {}", endpoint);

            match connect_async(&endpoint).await {
                Ok((ws_stream, _response)) => {
                    info!("WebSocket conectado com sucesso em {}", endpoint);
                    backoff = Duration::from_secs(1); // Reset do backoff apos conexao bem sucedida

                    let (mut write, mut read) = ws_stream.split();

                    loop {
                        match tokio::time::timeout(Duration::from_secs(18), read.next()).await {
                            Ok(Some(msg_result)) => match msg_result {
                                Ok(Message::Text(text)) => {
                                    if let Ok(candle) = Self::parse_kline_payload(&text) {
                                        if let Err(e) = candle_tx.send(candle).await {
                                            error!("Erro ao enviar candle para o canal de processamento: {}", e);
                                            return;
                                        }
                                    }
                                }
                                Ok(Message::Ping(payload)) => {
                                    if let Err(e) = write.send(Message::Pong(payload)).await {
                                        warn!("Falha ao responder Ping: {}", e);
                                        break;
                                    }
                                }
                                Ok(Message::Close(frame)) => {
                                    warn!("WebSocket fechado pelo servidor: {:?}", frame);
                                    break;
                                }
                                Err(e) => {
                                    error!("Erro no stream WebSocket: {}", e);
                                    break;
                                }
                                _ => {}
                            },
                            Ok(None) => {
                                warn!("WebSocket finalizado pelo servidor (stream encerrada).");
                                break;
                            }
                            Err(_) => {
                                warn!("Watchdog Ativo: 18s sem resposta da Binance. Conexao congelada. Forcando reinicio...");
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("Falha ao conectar no WebSocket: {}. Reconectando em {:?}", e, backoff);
                }
            }

            sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    }

    fn parse_kline_payload(text: &str) -> Result<Candle> {
        let v: Value = serde_json::from_str(text).context("Falha ao parsear JSON")?;
        let k = v.get("k").context("Payload sem campo 'k'")?;

        let open_time = k["t"].as_i64().unwrap_or(0);
        let close_time = k["T"].as_i64().unwrap_or(0);
        let open: f64 = k["o"].as_str().unwrap_or("0").parse().unwrap_or(0.0);
        let high: f64 = k["h"].as_str().unwrap_or("0").parse().unwrap_or(0.0);
        let low: f64 = k["l"].as_str().unwrap_or("0").parse().unwrap_or(0.0);
        let close: f64 = k["c"].as_str().unwrap_or("0").parse().unwrap_or(0.0);
        let volume: f64 = k["v"].as_str().unwrap_or("0").parse().unwrap_or(0.0);
        let is_closed = k["x"].as_bool().unwrap_or(false);

        Ok(Candle {
            open_time,
            close_time,
            open,
            high,
            low,
            close,
            volume,
            is_closed,
        })
    }
}
