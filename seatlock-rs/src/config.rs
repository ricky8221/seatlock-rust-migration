//! Same environment contract as the Java service (application.yml + Spring relaxed binding):
//! DB_URL is a *JDBC* URL, DB_USER / DB_PASSWORD are required (no silent dev defaults),
//! DB_POOL_SIZE defaults to 10, PORT to 8080, and the `seats.*` properties map to SEATS_* vars.

use anyhow::{bail, Context};
use sqlx::postgres::PgConnectOptions;
use std::str::FromStr;
use std::time::Duration;

pub struct Config {
    pub db_url: String,
    pub db_user: String,
    pub db_password: String,
    pub pool_size: u32,
    pub port: u16,
    pub seats: SeatsConfig,
    /// `seats.scheduling.enabled`: background jobs on/off (integration tests turn them off).
    pub scheduling_enabled: bool,
    /// Test-only HTTP hooks (clock control, run a job now). Never set in production.
    pub test_hooks: bool,
    /// Tokio worker threads; 0 = one per CPU (Tokio's default).
    pub worker_threads: usize,
}

/// `ReservationProperties`.
#[derive(Clone, Debug)]
pub struct SeatsConfig {
    pub hold_ttl: Duration,
    pub max_seats_per_hold: usize,
    pub expiry_batch_size: i64,
    pub expiry_sweep_interval: Duration,
    pub idempotency_retention: Duration,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let seats = SeatsConfig {
            hold_ttl: duration("SEATS_HOLD_TTL", "PT5M")?,
            max_seats_per_hold: optional("SEATS_MAX_SEATS_PER_HOLD", 10)?,
            expiry_batch_size: optional("SEATS_EXPIRY_BATCH_SIZE", 100)?,
            expiry_sweep_interval: duration("SEATS_EXPIRY_SWEEP_INTERVAL", "PT5S")?,
            idempotency_retention: duration("SEATS_IDEMPOTENCY_RETENTION", "PT24H")?,
        };
        if !(1..=100).contains(&seats.max_seats_per_hold) {
            bail!("seats.max-seats-per-hold must be between 1 and 100");
        }
        if seats.expiry_batch_size < 1 {
            bail!("seats.expiry-batch-size must be at least 1");
        }
        Ok(Self {
            db_url: required("DB_URL")?,
            db_user: required("DB_USER")?,
            db_password: required("DB_PASSWORD")?,
            pool_size: optional("DB_POOL_SIZE", 10)?,
            port: optional("PORT", 8080)?,
            seats,
            scheduling_enabled: optional("SEATS_SCHEDULING_ENABLED", true)?,
            test_hooks: optional("SEATLOCK_TEST_HOOKS", false)?,
            worker_threads: optional("TOKIO_WORKER_THREADS", 0)?,
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
            .password(&self.db_password)
            .application_name("seatlock-rs"))
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
        Ok(v) => v.trim().parse().map_err(|e| anyhow::anyhow!("{name}={v}: {e}")),
        Err(_) => Ok(default),
    }
}

fn duration(name: &str, default: &str) -> anyhow::Result<Duration> {
    let raw = std::env::var(name).unwrap_or_else(|_| default.to_string());
    let d = parse_duration(&raw).with_context(|| format!("{name}={raw} is not a duration"))?;
    if d.is_zero() {
        bail!("{name} must be positive but was {raw}");
    }
    Ok(d)
}

/// Spring Boot's `DurationStyle`: ISO-8601 (`PT5M`, `PT0.5S`, `P1D`) or simple (`5m`, `10s`,
/// `100ms`, `1h`, `1d`; a bare number is milliseconds).
pub fn parse_duration(raw: &str) -> anyhow::Result<Duration> {
    let s = raw.trim();
    let upper = s.to_ascii_uppercase();
    if let Some(rest) = upper.strip_prefix('P') {
        let (days, time) = match rest.split_once('T') {
            Some((d, t)) => (d, t),
            None => (rest, ""),
        };
        let mut secs = 0f64;
        if !days.is_empty() {
            secs += days.strip_suffix('D').context("ISO duration: expected D")?.parse::<f64>()? * 86_400.0;
        }
        let mut num = String::new();
        for c in time.chars() {
            match c {
                'H' => secs += num.parse::<f64>()? * 3600.0,
                'M' => secs += num.parse::<f64>()? * 60.0,
                'S' => secs += num.parse::<f64>()?,
                _ => {
                    num.push(c);
                    continue;
                }
            }
            num.clear();
        }
        if !num.is_empty() || secs < 0.0 {
            bail!("invalid ISO-8601 duration");
        }
        return Ok(Duration::from_secs_f64(secs));
    }
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (n, unit) = s.split_at(split);
    let n: u64 = n.parse()?;
    Ok(match unit.to_ascii_lowercase().as_str() {
        "" | "ms" => Duration::from_millis(n),
        "ns" => Duration::from_nanos(n),
        "us" => Duration::from_micros(n),
        "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n * 60),
        "h" => Duration::from_secs(n * 3600),
        "d" => Duration::from_secs(n * 86_400),
        other => bail!("unknown duration unit {other}"),
    })
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
            seats: SeatsConfig {
                hold_ttl: Duration::from_secs(300),
                max_seats_per_hold: 10,
                expiry_batch_size: 100,
                expiry_sweep_interval: Duration::from_secs(5),
                idempotency_retention: Duration::from_secs(86_400),
            },
            scheduling_enabled: true,
            test_hooks: false,
            worker_threads: 0,
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

    #[test]
    fn spring_durations() {
        assert_eq!(parse_duration("PT5M").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("PT24H").unwrap(), Duration::from_secs(86_400));
        assert_eq!(parse_duration("PT0.5S").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("P1DT1H").unwrap(), Duration::from_secs(90_000));
        assert_eq!(parse_duration("10s").unwrap(), Duration::from_secs(10));
        assert_eq!(parse_duration("250").unwrap(), Duration::from_millis(250));
    }
}
