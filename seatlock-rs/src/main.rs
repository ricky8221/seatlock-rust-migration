//! seatlock-rs: a Rust port of seatlock (event seat holds with expiring holds, bookings and
//! idempotent retries). See docs/WRITEUP.md for how the Spring pieces map onto these modules.

mod clock;
mod config;
mod domain;
mod idempotency;
mod input;
mod instant;
mod java;
mod metrics;
mod ops;
mod problem;
mod repo;
mod service;
mod web;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use axum::{middleware, Router};
use sqlx::{postgres::PgPoolOptions, PgPool};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

pub struct AppState {
    pub pool: PgPool,
    pub clock: clock::Clock,
    pub seats: config::SeatsConfig,
    pub metrics: Arc<metrics::Metrics>,
    /// springdoc's document as captured from Java, kept as text so key order is preserved.
    pub openapi: &'static str,
}

/// The service: the router, wrapped (as a whole, not per route) in the middleware that has to
/// see every response, including axum's own 405s and the fallback 404.
pub type Service = tower::util::BoxCloneService<axum::extract::Request, axum::response::Response, std::convert::Infallible>;

pub fn app(state: Arc<AppState>, test_hooks: bool) -> Service {
    let router = Router::new()
        .merge(web::api_routes())
        .merge(ops::routes(test_hooks))
        .fallback(problem::no_route)
        .with_state(state.clone());
    // Innermost first: problem bodies are rendered before the idempotency layer stores the
    // response, and the timer sees the final status.
    let svc = tower::ServiceBuilder::new()
        .layer(middleware::from_fn_with_state(state.metrics.clone(), metrics::track_http))
        .layer(middleware::from_fn_with_state(state.clone(), idempotency::idempotency))
        .layer(middleware::from_fn(problem::render_problems))
        .service(router.into_service());
    Service::new(svc)
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = config::Config::from_env()?;
    let mut rt = tokio::runtime::Builder::new_multi_thread();
    rt.enable_all();
    if cfg.worker_threads > 0 {
        rt.worker_threads(cfg.worker_threads);
    }
    rt.build()?.block_on(run(cfg))
}

async fn run(cfg: config::Config) -> anyhow::Result<()> {
    let pool = PgPoolOptions::new()
        .max_connections(cfg.pool_size)
        .acquire_timeout(Duration::from_secs(30)) // Hikari connectionTimeout default
        .connect_with(cfg.connect_options()?)
        .await?;

    let clock = if cfg.test_hooks {
        tracing::warn!("SEATLOCK_TEST_HOOKS is on: clock is manual and /__test endpoints are exposed");
        clock::Clock::Manual(std::sync::Mutex::new(time::OffsetDateTime::now_utc()))
    } else {
        clock::Clock::System
    };
    let state = Arc::new(AppState {
        pool,
        clock,
        seats: cfg.seats.clone(),
        metrics: Arc::new(metrics::Metrics::default()),
        openapi: include_str!("openapi.json"),
    });

    if cfg.scheduling_enabled {
        spawn_jobs(state.clone());
    }

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", cfg.port)).await?;
    info!(port = cfg.port, pool_size = cfg.pool_size, "seatlock-rs listening");
    axum::serve(listener, axum::ServiceExt::into_make_service(app(state, cfg.test_hooks)))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// The two `@Scheduled` jobs, with Spring's fixed-*delay* semantics: the next run starts a full
/// interval after the previous one finished, so a slow sweep never overlaps itself.
fn spawn_jobs(state: Arc<AppState>) {
    let s = state.clone();
    tokio::spawn(async move {
        let every = s.seats.expiry_sweep_interval;
        loop {
            tokio::time::sleep(every).await;
            match service::sweep_expired_holds(&s).await {
                Ok(0) => {}
                Ok(n) => info!("Expired {n} hold(s)"),
                Err(e) => tracing::warn!(error = ?e, "hold expiry sweep failed"),
            }
        }
    });
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        loop {
            match service::purge_idempotency_records(&state).await {
                Ok(0) => {}
                Ok(n) => info!("Deleted {n} expired idempotency record(s)"),
                Err(e) => tracing::warn!(error = ?e, "idempotency cleanup failed"),
            }
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
}

/// `server.shutdown: graceful` equivalent: stop accepting, drain in-flight requests.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    info!("shutdown signal received, draining");
}
