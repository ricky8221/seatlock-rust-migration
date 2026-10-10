//! Same environment contract as the Java service (application.yml): DB_URL is a *JDBC* URL,
//! DB_USER / DB_PASSWORD are required (no silent dev defaults), DB_POOL_SIZE defaults to 10, PORT
//! to 8080.

use anyhow::{bail, Context};
use sqlx::postgres::PgConnectOptions;
use std::str::FromStr;

pub struct Config {
    pub db_url: String,
    pub db_user: String,
    pub db_password: String,
    pub pool_size: u32,
    pub port: u16,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            db_url: required("DB_URL")?,
            db_user: required("DB_USER")?,
            db_password: required("DB_PASSWORD")?,
            pool_size: optional("DB_POOL_SIZE", 10)?,
            port: optional("PORT", 8080)?,
        })
    }

    /// `jdbc:postgresql://host:5432/db?x=y` -> sqlx options. Gotcha: the JDBC prefix is not a
    /// valid URL scheme for sqlx, and credentials come from separate variables.
    pub fn connect_options(&self) -> anyhow::Result<PgConnectOptions> {
        let url = self.db_url.strip_prefix("jdbc:").unwrap_or(&self.db_url);
        if !url.starts_with("postgresql://") && !url.starts_with("postgres://") {
            bail!("DB_URL must be a PostgreSQL URL, got {}", self.db_url);
        }
        Ok(PgConnectOptions::from_str(url)
            .context("parsing DB_URL")?
            .username(&self.db_user)
            .password(&self.db_password))
    }
}

fn required(name: &str) -> anyhow::Result<String> {
    std::env::var(name).with_context(|| format!("{name} must be set"))
}

fn optional<T: FromStr>(name: &str, default: T) -> anyhow::Result<T>
where
    T::Err: std::fmt::Display,
{
    match std::env::var(name) {
        Ok(v) => v.parse().map_err(|e| anyhow::anyhow!("{name}={v}: {e}")),
        Err(_) => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(url: &str) -> Config {
        Config {
            db_url: url.into(),
            db_user: "u".into(),
            db_password: "p".into(),
            pool_size: 10,
            port: 8080,
        }
    }

    #[test]
    fn accepts_jdbc_urls() {
        let opts = cfg("jdbc:postgresql://postgres:5432/seats").connect_options().unwrap();
        assert_eq!(opts.get_host(), "postgres");
        assert_eq!(opts.get_port(), 5432);
        assert_eq!(opts.get_database(), Some("seats"));
    }

    #[test]
    fn rejects_non_postgres_urls() {
        assert!(cfg("jdbc:mysql://x/y").connect_options().is_err());
    }
}
