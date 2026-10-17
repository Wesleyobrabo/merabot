use crate::types::{AccountState, Position, TradeRecord};
use anyhow::Result;
use std::fs;
use std::path::Path;
use tracing::info;

pub struct StorageManager {
    data_dir: String,
}

impl StorageManager {
    pub fn new(data_dir: &str) -> Self {
        if !Path::new(data_dir).exists() {
            let _ = fs::create_dir_all(data_dir);
        }
        Self {
            data_dir: data_dir.to_string(),
        }
    }

    fn save_atomic(path: &str, content: &str) -> Result<()> {
        let temp_path = format!("{}.tmp", path);
        fs::write(&temp_path, content)?;
        fs::rename(&temp_path, path)?;
        Ok(())
    }

    pub fn load_trades(&self) -> Vec<TradeRecord> {
        let path = format!("{}/trades.json", self.data_dir);
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(trades) = serde_json::from_str::<Vec<TradeRecord>>(&content) {
                info!("{} trades historicos carregados do disco ({}).", trades.len(), path);
                return trades;
            }
        }
        Vec::new()
    }

    pub fn save_trades(&self, trades: &[TradeRecord]) -> Result<()> {
        let path = format!("{}/trades.json", self.data_dir);
        let json = serde_json::to_string_pretty(trades)?;
        Self::save_atomic(&path, &json)?;
        Ok(())
    }

    pub fn load_account_state(&self) -> Option<AccountState> {
        let path = format!("{}/account.json", self.data_dir);
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(account) = serde_json::from_str::<AccountState>(&content) {
                info!(
                    "Estado da conta carregado do disco: Saldo: {:.2} | Peak: {:.2} | Trades: {}",
                    account.balance, account.peak_balance, account.total_trades
                );
                return Some(account);
            }
        }
        None
    }

    pub fn save_account_state(&self, account: &AccountState) -> Result<()> {
        let path = format!("{}/account.json", self.data_dir);
        let json = serde_json::to_string_pretty(account)?;
        Self::save_atomic(&path, &json)?;
        Ok(())
    }

    pub fn load_position(&self) -> Option<Position> {
        let path = format!("{}/position.json", self.data_dir);
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(position) = serde_json::from_str::<Position>(&content) {
                if position.amount > 0.0 {
                    info!(
                        "Posicao aberta recuperada do disco: {:?} {} {} @ {:.2} (SL: {:?})",
                        position.side, position.amount, position.symbol, position.entry_price, position.stop_loss
                    );
                    return Some(position);
                }
            }
        }
        None
    }

    pub fn save_position(&self, position: &Position) -> Result<()> {
        let path = format!("{}/position.json", self.data_dir);
        let json = serde_json::to_string_pretty(position)?;
        Self::save_atomic(&path, &json)?;
        Ok(())
    }
}
