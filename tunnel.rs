use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tracing::{info, warn};

pub struct CloudflareTunnel {
    pub url: String,
    pub child: Child,
}

impl CloudflareTunnel {
    pub async fn start(port: u16) -> Option<Self> {
        let binary_candidates = [
            "cloudflared",
            r"C:\Program Files (x86)\cloudflared\cloudflared.exe",
            r"C:\Program Files\cloudflared\cloudflared.exe",
        ];

        let mut chosen_bin = None;
        for bin in &binary_candidates {
            if bin == &"cloudflared" {
                if let Ok(output) = std::process::Command::new(bin).arg("--version").output() {
                    if output.status.success() {
                        chosen_bin = Some(bin.to_string());
                        break;
                    }
                }
            } else if Path::new(bin).exists() {
                chosen_bin = Some(bin.to_string());
                break;
            }
        }

        let bin_path = match chosen_bin {
            Some(b) => b,
            None => {
                warn!("cloudflared.exe nao foi encontrado no sistema. Auto-tunnel desativado.");
                return None;
            }
        };

        info!("Iniciando Cloudflare Tunnel automatico na porta {}...", port);

        let mut child = match Command::new(&bin_path)
            .args(["tunnel", "--url", &format!("http://localhost:{}", port)])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                warn!("Falha ao spawnar processo do cloudflared: {}", e);
                return None;
            }
        };

        let stderr = child.stderr.take();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(1);

        if let Some(err_stream) = stderr {
            tokio::spawn(async move {
                let mut reader = BufReader::new(err_stream).lines();
                let mut url_sent = false;
                // Continua drenando o stderr apos achar a URL para o pipe nunca fechar
                while let Ok(Some(line)) = reader.next_line().await {
                    if url_sent {
                        continue;
                    }
                    if let Some(start) = line.find("https://") {
                        let sub = &line[start..];
                        if let Some(end) = sub.find(".trycloudflare.com") {
                            let url = sub[..end + ".trycloudflare.com".len()].to_string();
                            let _ = tx.send(url).await;
                            url_sent = true;
                        }
                    }
                }
            });
        }

        match tokio::time::timeout(tokio::time::Duration::from_secs(12), rx.recv()).await {
            Ok(Some(url)) => {
                info!("Cloudflare Tunnel ativo com sucesso: {}", url);
                Some(Self { url, child })
            }
            _ => {
                warn!("Tempo limite ao aguardar URL do Cloudflare Tunnel. Usando conexao local.");
                let _ = child.kill().await;
                None
            }
        }
    }
}
