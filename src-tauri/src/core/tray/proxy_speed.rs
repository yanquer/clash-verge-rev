//! 托盘代理连接速率采样模块
//!
//! 通过 Mihomo 连接快照计算经过代理节点的上传/下载速率，
//! 排除直连和拒绝连接，避免托盘网速混入非代理流量。

use std::collections::{HashMap, HashSet};

use tauri_plugin_mihomo::models::Connection;

const NON_PROXY_CHAINS: [&str; 3] = ["DIRECT", "REJECT", "REJECT-DROP"];

/// 托盘代理连接速率采样结果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProxyConnectionSpeed {
    pub up: u64,
    pub down: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ConnectionTrafficSnapshot {
    upload: u64,
    download: u64,
}

/// 代理连接速率采样器，保存上一帧连接流量并计算代理连接差值。
#[derive(Default)]
pub struct ProxyConnectionSpeedSampler {
    previous: HashMap<String, ConnectionTrafficSnapshot>,
}

impl ProxyConnectionSpeedSampler {
    /// 创建新的代理连接速率采样器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 根据当前连接快照计算经过代理节点的上传/下载速率。
    pub fn sample(&mut self, connections: &[Connection]) -> ProxyConnectionSpeed {
        let mut current_ids = HashSet::with_capacity(connections.len());
        let mut speed = ProxyConnectionSpeed::default();

        for connection in connections {
            current_ids.insert(connection.id.clone());
            if !Self::is_proxy_connection(connection) {
                self.previous.insert(connection.id.clone(), Self::snapshot(connection));
                continue;
            }

            if let Some(previous) = self.previous.get(&connection.id) {
                speed.up += connection.upload.saturating_sub(previous.upload);
                speed.down += connection.download.saturating_sub(previous.download);
            }

            self.previous.insert(connection.id.clone(), Self::snapshot(connection));
        }

        self.previous.retain(|id, _| current_ids.contains(id));
        speed
    }

    /// 判断连接是否经过实际代理节点。
    fn is_proxy_connection(connection: &Connection) -> bool {
        connection
            .chains
            .first()
            .map(|chain| {
                let upper = chain.to_ascii_uppercase();
                !NON_PROXY_CHAINS.contains(&upper.as_str())
            })
            .unwrap_or(false)
    }

    /// 从连接对象提取累计流量快照。
    const fn snapshot(connection: &Connection) -> ConnectionTrafficSnapshot {
        ConnectionTrafficSnapshot {
            upload: connection.upload,
            download: connection.download,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 构造指定链路和累计流量的 Mihomo 连接测试对象。
    fn connection(id: &str, chain: &str, upload: u64, download: u64) -> serde_json::Result<Connection> {
        serde_json::from_value(json!({
            "id": id,
            "metadata": {
                "network": "tcp",
                "type": "HTTP",
                "sourceIP": "127.0.0.1",
                "destinationIP": "1.1.1.1",
                "sourceGeoIP": null,
                "destinationGeoIP": null,
                "sourceIPASN": "",
                "destinationIPASN": "",
                "sourcePort": "50000",
                "destinationPort": "443",
                "inboundIP": "127.0.0.1",
                "inboundPort": "7897",
                "inboundName": "mixed",
                "inboundUser": "",
                "host": "example.com",
                "dnsMode": "normal",
                "uid": 0,
                "process": "",
                "processPath": "",
                "specialProxy": "",
                "specialRules": "",
                "remoteDestination": "",
                "dscp": 0,
                "sniffHost": ""
            },
            "upload": upload,
            "download": download,
            "start": "2026-05-09T00:00:00Z",
            "chains": [chain],
            "rule": "MATCH",
            "rulePayload": ""
        }))
    }

    #[test]
    fn sampler_ignores_first_frame_and_counts_proxy_delta() -> serde_json::Result<()> {
        let mut sampler = ProxyConnectionSpeedSampler::new();
        assert_eq!(
            sampler.sample(&[connection("1", "HK", 100, 200)?]),
            ProxyConnectionSpeed { up: 0, down: 0 }
        );
        assert_eq!(
            sampler.sample(&[connection("1", "HK", 180, 260)?]),
            ProxyConnectionSpeed { up: 80, down: 60 }
        );
        Ok(())
    }

    #[test]
    fn sampler_excludes_direct_and_reject_connections() -> serde_json::Result<()> {
        let mut sampler = ProxyConnectionSpeedSampler::new();
        sampler.sample(&[
            connection("1", "DIRECT", 100, 200)?,
            connection("2", "REJECT", 100, 200)?,
            connection("3", "REJECT-DROP", 100, 200)?,
        ]);
        assert_eq!(
            sampler.sample(&[
                connection("1", "DIRECT", 200, 400)?,
                connection("2", "REJECT", 200, 400)?,
                connection("3", "REJECT-DROP", 200, 400)?,
            ]),
            ProxyConnectionSpeed { up: 0, down: 0 }
        );
        Ok(())
    }

    #[test]
    fn sampler_handles_counter_reset() -> serde_json::Result<()> {
        let mut sampler = ProxyConnectionSpeedSampler::new();
        sampler.sample(&[connection("1", "HK", 200, 200)?]);
        assert_eq!(
            sampler.sample(&[connection("1", "HK", 100, 50)?]),
            ProxyConnectionSpeed { up: 0, down: 0 }
        );
        Ok(())
    }

    #[test]
    fn sampler_drops_disappeared_connections() -> serde_json::Result<()> {
        let mut sampler = ProxyConnectionSpeedSampler::new();
        sampler.sample(&[connection("1", "HK", 100, 200)?]);
        assert_eq!(sampler.sample(&[]), ProxyConnectionSpeed { up: 0, down: 0 });
        assert_eq!(
            sampler.sample(&[connection("1", "HK", 180, 260)?]),
            ProxyConnectionSpeed { up: 0, down: 0 }
        );
        assert_eq!(
            sampler.sample(&[connection("1", "HK", 220, 300)?]),
            ProxyConnectionSpeed { up: 40, down: 40 }
        );
        Ok(())
    }
}
