use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct AppConfig {
    pub binance_apikey: String,
    pub binance_secret: String,
    pub database_url: String,
    pub jwt_secret: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// 监听地址。默认 127.0.0.1：该 API 持有交易所下单权限，绝不应默认对公网开放。
    /// 需要远程访问时应经由反向代理，或显式设置 BIND_ADDR=0.0.0.0 并自行承担风险。
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,
}

fn default_port() -> u16 {
    3000
}

fn default_bind_addr() -> String {
    "127.0.0.1".to_string()
}

pub fn load() -> anyhow::Result<AppConfig> {
    envy::from_env::<AppConfig>().map_err(|e| anyhow::anyhow!("配置加载失败: {e}"))
}
