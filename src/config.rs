use std::{
    collections::HashMap,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use crate::artifact::ARTIFACT_DIRECTORY;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub addr: SocketAddr,
    pub db_path: String,
    pub token: Option<String>,
    pub rust_log: String,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        Self::from_getter(|name| std::env::var(name).ok())
    }

    pub fn from_map(values: &HashMap<String, String>) -> Result<Self, String> {
        Self::from_getter(|name| values.get(name).cloned())
    }

    pub fn from_getter(mut get: impl FnMut(&str) -> Option<String>) -> Result<Self, String> {
        let addr_text = get("ORCIS_ADDR").unwrap_or_else(|| "127.0.0.1:8080".to_owned());
        let addr = addr_text
            .parse()
            .map_err(|error| format!("invalid ORCIS_ADDR {addr_text:?}: {error}"))?;

        Ok(Self {
            addr,
            db_path: get("ORCIS_DB_PATH").unwrap_or_else(|| "orcis.db".to_owned()),
            token: get("ORCIS_TOKEN"),
            rust_log: get("RUST_LOG").unwrap_or_else(|| "info".to_owned()),
        })
    }

    pub fn artifact_dir(&self) -> PathBuf {
        Path::new(&self.db_path)
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(ARTIFACT_DIRECTORY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_overrides() {
        let config = Config::from_map(&HashMap::new()).expect("defaults are valid");
        assert_eq!(config.addr, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(config.db_path, "orcis.db");
        assert_eq!(config.artifact_dir(), PathBuf::from("artifacts"));
        assert_eq!(config.rust_log, "info");

        let values = HashMap::from([
            ("ORCIS_ADDR".to_owned(), "0.0.0.0:9000".to_owned()),
            ("ORCIS_DB_PATH".to_owned(), "/tmp/orcis.db".to_owned()),
            ("ORCIS_TOKEN".to_owned(), "secret".to_owned()),
            ("RUST_LOG".to_owned(), "debug".to_owned()),
        ]);
        let config = Config::from_map(&values).expect("overrides are valid");
        assert_eq!(config.addr, "0.0.0.0:9000".parse().unwrap());
        assert_eq!(config.db_path, "/tmp/orcis.db");
        assert_eq!(config.artifact_dir(), PathBuf::from("/tmp/artifacts"));
        assert_eq!(config.token.as_deref(), Some("secret"));
        assert_eq!(config.rust_log, "debug");
    }

    #[test]
    fn invalid_address_is_clear() {
        let values = HashMap::from([("ORCIS_ADDR".to_owned(), "not an address".to_owned())]);
        let error = Config::from_map(&values).unwrap_err();
        assert!(error.contains("invalid ORCIS_ADDR"));
    }
}
