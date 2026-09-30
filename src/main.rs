//! Strymek: stream individual Linux applications to a browser, one URL per app.

mod apps;
mod auth;
mod config;
mod desktop;
mod keymap;
mod stream;
mod tls;
mod web;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use config::Config;
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "strymek", version, about = "Stream individual Linux apps to your browser, one URL per app")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server (default).
    Serve,
    /// Create the config file with sensible defaults.
    Init {
        /// Address to listen on, e.g. 10.66.0.1:10000 (your WireGuard address).
        #[arg(long)]
        bind: Option<String>,
    },
    /// Set the login password.
    Passwd {
        /// Read the password from standard input instead of prompting (for scripts).
        #[arg(long)]
        stdin: bool,
    },
    /// List the apps the dashboard will offer.
    Apps,
    /// Print where the CA certificate is, for trusting it on the Mac.
    Ca,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "strymek=info".into()),
        )
        .with_target(false)
        .init();
    let cli = Cli::parse();
    match cli.cmd.unwrap_or(Command::Serve) {
        Command::Serve => serve(),
        Command::Init { bind } => init(bind),
        Command::Passwd { stdin } => passwd(stdin),
        Command::Apps => {
            let cfg = Config::load()?;
            for a in apps::discover(&cfg) {
                println!("{:<40} {:<28} {}", a.id, a.name, a.argv.join(" "));
            }
            Ok(())
        }
        Command::Ca => {
            let cfg = Config::load()?;
            let files = tls::ensure(&cfg)?;
            match files.ca {
                Some(p) => println!("{}", p.display()),
                None => println!("Using your own certificate ({})", files.cert.display()),
            }
            Ok(())
        }
    }
}

fn init(bind: Option<String>) -> Result<()> {
    let mut cfg = Config::load()?;
    if let Some(b) = bind {
        cfg.bind = b.parse().context("--bind must look like 10.66.0.1:10000")?;
    }
    if cfg.tls_names.is_empty() {
        cfg.tls_names = Config::default_tls_names();
    }
    cfg.save()?;
    println!("Wrote {}", config::config_path().display());
    println!("  bind      = {}", cfg.bind);
    println!("  tls_names = {}", cfg.tls_names.join(", "));
    if cfg.password_hash.is_empty() {
        println!("Next: run `strymek passwd` to set the login password.");
    }
    Ok(())
}

fn passwd(from_stdin: bool) -> Result<()> {
    let mut cfg = Config::load()?;
    let p1 = if from_stdin {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        line.trim_end_matches(['\r', '\n']).to_string()
    } else {
        rpassword::prompt_password("New Strymek password: ")?
    };
    if p1.chars().count() < 12 {
        return Err(anyhow!("use at least 12 characters"));
    }
    if !from_stdin {
        let p2 = rpassword::prompt_password("Repeat: ")?;
        if p1 != p2 {
            return Err(anyhow!("passwords do not match"));
        }
    }
    cfg.password_hash = auth::hash_password(&p1)?;
    if cfg.tls_names.is_empty() {
        cfg.tls_names = Config::default_tls_names();
    }
    cfg.save()?;
    println!("Password saved to {}", config::config_path().display());
    Ok(())
}

fn serve() -> Result<()> {
    let cfg = Config::load()?;
    if cfg.password_hash.is_empty() {
        return Err(anyhow!("no password set: run `strymek passwd` first"));
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        tracing::info!("note: apps are started on private X displays, not your Wayland desktop");
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    let files = tls::ensure(&cfg)?;
    let ca_pem = files.ca.as_ref().and_then(|p| std::fs::read_to_string(p).ok());
    let bind = cfg.bind;
    let idle = cfg.session_idle_hours;
    let hash = cfg.password_hash.clone();
    let mgr = stream::Manager::new(cfg)?;

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(&files.cert, &files.key)
            .await
            .context("loading TLS certificate")?;
        let state = Arc::new(web::AppState { mgr: mgr.clone(), auth: auth::Auth::new(idle), password_hash: hash, ca_pem });
        let app = web::router(state);

        // Close streams whose app has exited.
        let m = mgr.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_secs(2));
            loop {
                t.tick().await;
                let m2 = m.clone();
                let done = tokio::task::spawn_blocking(move || {
                    m2.relock_check();
                    m2.finished()
                })
                .await
                .unwrap_or_default();
                for slug in done {
                    let m3 = m.clone();
                    tokio::task::spawn_blocking(move || m3.stop(&slug));
                }
            }
        });

        let handle = axum_server::Handle::new();
        let h2 = handle.clone();
        let m = mgr.clone();
        tokio::spawn(async move {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = term.recv() => {},
            }
            tracing::info!("shutting down: stopping all streams");
            let _ = tokio::task::spawn_blocking(move || m.stop_all()).await;
            h2.graceful_shutdown(Some(Duration::from_secs(2)));
        });

        tracing::info!("Strymek listening on https://{bind}/");
        axum_server::bind_rustls(bind, tls)
            .handle(handle)
            .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
            .await
            .context("server error")?;
        Ok::<_, anyhow::Error>(())
    })?;
    Ok(())
}
