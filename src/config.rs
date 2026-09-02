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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".parse().unwrap(),
            admin_listen: None,
            db_path: PathBuf::from("inginx.db"),
            log_level: "info".into(),
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
