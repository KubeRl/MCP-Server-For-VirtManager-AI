mod agent;
mod config;
mod console;
mod display;
mod fsops;
mod hypervisor;
mod server;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::extract::{Request, State as AxumState};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use clap::{Parser, ValueEnum};
use rmcp::ServiceExt;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};

use crate::config::Config;
use crate::console::Consoles;
use crate::hypervisor::Hypervisor;
use crate::server::{State, VirtServer};

#[derive(Clone, Copy, ValueEnum)]
enum Transport {
    Stdio,
    Http,
}

/// MCP server giving an AI access to running libvirt (virt-manager) VMs through
/// the QEMU guest agent, serial console and graphical display.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Config file (default: $XDG_CONFIG_HOME/virtmanager-mcp/config.toml).
    #[arg(short, long)]
    config: Option<PathBuf>,
    #[arg(short, long, value_enum, default_value = "stdio")]
    transport: Transport,
    /// libvirt URI, overrides the config file.
    #[arg(long)]
    uri: Option<String>,
    /// HTTP listen address, overrides the config file.
    #[arg(long)]
    bind: Option<String>,
    /// Force read-only mode regardless of the config file.
    #[arg(long)]
    read_only: bool,
    /// Additional VM to allow (repeatable), added to allowed_vms from the config file.
    #[arg(long = "allow", value_name = "VM")]
    allow: Vec<String>,
    /// Additional VM the AI may start and stop (repeatable), added to power_vms.
    #[arg(long = "allow-power", value_name = "VM")]
    allow_power: Vec<String>,
    /// Additional host directory copy_to_vm may read from (repeatable), added to host_share_dirs.
    #[arg(long = "share", value_name = "DIR")]
    share: Vec<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // stdout carries the MCP protocol in stdio mode, so log to stderr.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let mut config = match &cli.config {
        Some(p) => Config::load(p, true)?,
        None => match Config::default_path() {
            Some(p) => Config::load(&p, false)?,
            None => Config::default(),
        },
    };
    if let Some(uri) = cli.uri {
        config.uri = uri;
    }
    if let Some(bind) = cli.bind {
        config.http.bind = bind;
    }
    if let Ok(token) = std::env::var("VIRTMANAGER_MCP_TOKEN") {
        config.http.token = Some(token);
    }
    config.read_only |= cli.read_only;
    config.allowed_vms.extend(cli.allow);
    config.power_vms.extend(cli.allow_power);
    config.host_share_dirs.extend(cli.share);
    config.validate()?;

    if config.allowed_vms.is_empty() {
        tracing::warn!("allowed_vms is empty: no VM will be accessible");
    }

    hypervisor::start_event_loop()?;
    let hv = Hypervisor::connect(config.clone())?;
    tracing::info!(
        uri = %config.uri,
        read_only = config.read_only,
        allowed = ?config.allowed_vms,
        power = ?config.power_vms,
        shares = ?config.host_share_dirs,
        "connected to libvirt"
    );
    let state = Arc::new(State {
        hv,
        consoles: Consoles::default(),
    });

    match cli.transport {
        Transport::Stdio => {
            let service = VirtServer::new(state).serve(rmcp::transport::stdio()).await?;
            service.waiting().await?;
        }
        Transport::Http => serve_http(state, &config).await?,
    }
    Ok(())
}

async fn serve_http(state: Arc<State>, config: &Config) -> Result<()> {
    let token = match config.http.token.as_deref() {
        Some(t) if !t.is_empty() => Arc::new(format!("Bearer {t}")),
        _ => bail!("HTTP mode requires http.token in the config file or VIRTMANAGER_MCP_TOKEN"),
    };
    let service: StreamableHttpService<VirtServer, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(VirtServer::new(state.clone())),
        Default::default(),
        StreamableHttpServerConfig::default(),
    );
    let app = axum::Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn_with_state(token, require_bearer));

    let listener = tokio::net::TcpListener::bind(&config.http.bind)
        .await
        .with_context(|| format!("cannot bind {}", config.http.bind))?;
    tracing::info!("listening on http://{}/mcp", listener.local_addr()?);
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

async fn require_bearer(AxumState(expected): AxumState<Arc<String>>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| constant_time_eq(v.as_bytes(), expected.as_bytes()));
    if ok {
        next.run(req).await
    } else {
        Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(header::WWW_AUTHENTICATE, "Bearer")
            .body("unauthorized".into())
            .unwrap()
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
