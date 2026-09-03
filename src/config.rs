use anyhow::Result;
use serde::Deserialize;
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub listen: SocketAddr,
    pub admin_listen: Option<SocketAddr>,
    pub db_path: PathBuf,
    pub log_level: String,
    /// Largest request body the proxy will buffer, in bytes. The body is read
    /// into memory before it can be forwarded, so this is the per-request
    /// memory an authenticated client can pin — and, without the pre-auth
    /// gate, it used to be what an anonymous one could. 32 MiB covers a
    /// base64 image or a short audio clip; raise it for large transcription
    /// uploads, but know what each concurrent request then costs.
    pub max_body_bytes: usize,
    /// Failed admin logins from one address before it is locked out. The
    /// counter is per source IP and resets on a success.
    pub login_max_failures: u32,
    /// How long a locked-out address stays locked, in seconds. Doubles on each
    /// further lockout up to an hour, so a persistent guesser slows to a crawl
    /// while a fat-fingered admin waits only the base interval.
    pub login_lockout_secs: u64,
    /// Set this only when inginx sits behind a reverse proxy that is the sole
    /// way in. It then believes the proxy about the client: `X-Real-IP` /
    /// `X-Forwarded-For` for the address the login lockout is keyed on, and
    /// `X-Forwarded-Proto` for whether to mark the session cookie `Secure`.
    /// Left off (the default), those headers are ignored, because a client
    /// that can reach inginx directly could forge them — spoofing an IP to
    /// walk around the brute-force wall. It also silences the startup warning
    /// about the admin GUI sharing a public listener: with a proxy in front,
    /// that is a deliberate arrangement rather than an exposure.
    pub trust_proxy_headers: bool,
    /// How long to wait for an upstream backend to produce response headers,
    /// in seconds. A backend that accepts the connection and then never
    /// answers — llama-swap with a model that failed to start is the case
    /// this was written for — used to pin the request until the *client* gave
    /// up, and because the log record is written after the response, the
    /// request left no trace at all. 0 disables the limit.
    pub upstream_timeout_secs: u64,
    /// How long a streaming response may go without a byte before it is
    /// abandoned, in seconds. The headers arrived, so `upstream_timeout_secs`
    /// is already satisfied; this covers a stream that stalls mid-flight.
    /// 0 disables the limit.
    pub upstream_stream_idle_secs: u64,
    /// Retry a request once when the backend answers 502/503 without having
    /// produced any output. Model swappers report a swap-in-progress that way,
    /// and the second attempt lands after the model is up. Only non-streaming
    /// responses are retried, and only before any byte reaches the client.
    pub upstream_retry_on_5xx: bool,
    /// Failed API-key authentications from one address before it is locked
    /// out of `/v1`, and how long that lockout lasts. Same doubling wall as
    /// the admin login, keyed the same way — see `ratelimit`.
    pub api_auth_max_failures: u32,
    pub api_auth_lockout_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".parse().unwrap(),
            admin_listen: None,
            db_path: PathBuf::from("inginx.db"),
            log_level: "info".into(),
            max_body_bytes: 32 * 1024 * 1024,
            login_max_failures: 5,
            login_lockout_secs: 300,
            trust_proxy_headers: false,
            upstream_timeout_secs: 300,
            upstream_stream_idle_secs: 120,
            upstream_retry_on_5xx: true,
            api_auth_max_failures: 20,
            api_auth_lockout_secs: 300,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        if !path.exists() {
            return Ok(Config::default());
        }
        Ok(toml::from_str(&std::fs::read_to_string(path)?)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_defaults_when_missing() {
        let c = Config::load(Path::new("/nonexistent/inginx.toml")).unwrap();
        assert_eq!(c.listen.to_string(), "0.0.0.0:8080");
        assert!(c.admin_listen.is_none());
        assert_eq!(c.db_path, PathBuf::from("inginx.db"));
    }

    #[test]
    fn load_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(
            &p,
            "listen = \"127.0.0.1:9000\"\nadmin_listen = \"127.0.0.1:9001\"\ndb_path = \"/data/x.db\"\n",
        )
        .unwrap();
        let c = Config::load(&p).unwrap();
        assert_eq!(c.listen.to_string(), "127.0.0.1:9000");
        assert_eq!(c.admin_listen.unwrap().to_string(), "127.0.0.1:9001");
        assert_eq!(c.db_path, PathBuf::from("/data/x.db"));
    }
}
