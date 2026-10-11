//! arouter CLI: `serve` (default) runs the proxy daemon; subcommands
//! inspect config/status. The daemon hot-reloads its registry on
//! SIGHUP, config-file changes, and the discovery interval (E2).
//! `init`, `doctor`, `monitor` land in E5/E6.

use arouter::catalog::Catalog;
use arouter::config::{AutoGrades, CallerMap, Config, KeyStore};
use arouter::discover;
use arouter::health::{Health, HealthPolicy};
use arouter::metrics::Metrics;
use arouter::proxy;
use arouter::quota;
use arouter::registry::Registry;
use arouter::usage::UsageLog;
use clap::{CommandFactory, Parser, Subcommand};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};

#[derive(Parser)]
#[command(
    name = "arouter",
    version,
    about = "quota-aware LLM agent router — pick a grade, not a provider"
)]
struct Cli {
    /// Launch the monitor TUI instead of a subcommand
    #[arg(long, global = true)]
    monitor: bool,
    #[command(subcommand)]
    cmd: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the proxy daemon (default when no subcommand is given)
    Serve {
        /// Config path (default ~/.config/arouter/config.toml or $AROUTER_CONFIG)
        #[arg(long)]
        config: Option<PathBuf>,
        /// Override the bind address from config
        #[arg(long)]
        bind: Option<String>,
    },
    /// Monitor TUI (htop-style view of the daemon)
    Monitor {
        /// Daemon base URL (default: the bind from --config, else 127.0.0.1:3929)
        #[arg(long)]
        url: Option<String>,
        /// Config path (used only to resolve the default URL)
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Seed config, merge pi models.json, install the systemd user unit
    Init {
        /// Config path to seed (default ~/.config/arouter/config.toml)
        #[arg(long)]
        config: Option<PathBuf>,
        /// Overwrite an existing config with the embedded example
        #[arg(long)]
        force: bool,
        /// Skip copying the binary to ~/.local/bin/arouter
        #[arg(long)]
        no_install_bin: bool,
        /// Skip the systemd user unit
        #[arg(long)]
        no_unit: bool,
        /// pi apiKey for the generated provider entries (default "local")
        #[arg(long)]
        pi_key: Option<String>,
    },
    /// Health checks: config, keys, daemon, upstreams, quota watcher, pi files
    Doctor {
        /// Config path (default ~/.config/arouter/config.toml)
        #[arg(long)]
        config: Option<PathBuf>,
        /// Daemon bind to probe (default: config [server].bind)
        #[arg(long)]
        bind: Option<String>,
    },
    /// Show daemon status (GET /status.json)
    Status {
        #[arg(long)]
        bind: Option<String>,
    },
    /// List models / grades; `--write` persists auto-grades
    Models {
        /// Show the attempt list for a request grade, e.g. "A+" or "A+..A"
        #[arg(long)]
        grade: Option<String>,
        /// Discover upstream models and write grades.auto.toml
        #[arg(long)]
        write: bool,
        /// Config path (where grades.auto.toml lands; default same as serve)
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// List configured profiles
    Profiles,
    /// Generate shell completions
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

/// host[:port] → http://host:port/status.json (http scheme optional)
fn normalize_url(u: &str) -> String {
    let u = u.trim_start_matches("http://");
    format!("http://{u}/status.json")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cmd = if cli.monitor {
        Command::Monitor {
            url: None,
            config: None,
        }
    } else {
        cli.cmd.unwrap_or(Command::Serve {
            config: None,
            bind: None,
        })
    };
    match cmd {
        Command::Serve { config, bind } => serve(config.as_deref(), bind).await,
        Command::Monitor { url, config } => {
            let url = match url {
                Some(u) => normalize_url(&u),
                None => {
                    // resolve from config bind if a config exists, else default
                    let cfg_path = config.or_else(arouter::config::default_config_path);
                    let bind = Config::load(cfg_path.as_deref())
                        .map(|c| c.server.bind)
                        .unwrap_or_else(|_| "127.0.0.1:3929".into());
                    normalize_url(&bind)
                }
            };
            arouter::monitor::run(url).await
        }
        Command::Init {
            config: init_config,
            force,
            no_install_bin,
            no_unit,
            pi_key,
        } => {
            let out = arouter::init::run(
                init_config.clone(),
                force,
                !no_install_bin,
                !no_unit,
                pi_key.as_deref().unwrap_or("local"),
                include_str!("../config.example.toml"),
            )?;
            println!("config      {}", out.config.display());
            if let Some(b) = &out.backup {
                println!("backup      {} (models.json before merge)", b.display());
            }
            if let Some(m) = &out.models_json {
                println!("pi models   {} (merged)", m.display());
            }
            if let Some(b) = &out.bin_installed {
                println!("binary      {} (copied)", b.display());
            }
            if let Some(u) = &out.unit {
                println!("systemd     {} (daemon-reload done; `systemctl --user enable --now arouter` to start)", u.display());
            }
            println!("\nrunning doctor…\n");
            let report = arouter::doctor::run(init_config.as_deref(), None).await;
            for (lvl, line) in &report.lines {
                println!("{} {}", lvl.tag(), line);
            }
            if report.overall == arouter::doctor::Level::Fail {
                anyhow::bail!("doctor found failures");
            }
            Ok(())
        }
        Command::Doctor { config, bind } => {
            let report = arouter::doctor::run(config.as_deref(), bind.as_deref()).await;
            for (lvl, line) in &report.lines {
                println!("{} {}", lvl.tag(), line);
            }
            match report.overall {
                arouter::doctor::Level::Fail => anyhow::bail!("doctor: FAIL"),
                arouter::doctor::Level::Warn => {
                    println!("doctor: ok (with warnings)");
                    Ok(())
                }
                arouter::doctor::Level::Ok => {
                    println!("doctor: all good");
                    Ok(())
                }
            }
        }
        Command::Status { bind } => status(bind).await,
        Command::Models {
            grade,
            write,
            config,
        } => models(grade.as_deref(), write, config.as_deref()).await,
        Command::Profiles => profiles(),
        Command::Completions { shell } => {
            let mut cmd = Cli::command();
            clap_complete::generate(shell, &mut cmd, "arouter", &mut std::io::stdout());
            Ok(())
        }
    }
}

// ─────────────────────────── serve ───────────────────────────

async fn serve(
    config: Option<&std::path::Path>,
    bind_override: Option<String>,
) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "arouter=info,axum=warn,tower=warn".into()),
        )
        .init();

    let cfg = Arc::new(Config::load(config)?);
    let keys = KeyStore::load_default(cfg.server.env_file.as_deref());
    let health = Health::new(HealthPolicy::default());

    // initial registry: config + (optional) one discovery pass (startup is
    // allowed to be slower; serving must never be without candidates)
    let reloader = Reloader {
        shared: Arc::new(RwLock::new(Arc::new(Registry::from_config(&cfg, &keys)?))),
        config_path: config.map(|p| p.to_path_buf()),
        http: proxy::client(),
        discovery_interval_secs: cfg.routing.discovery_interval_secs,
    };
    reloader.reload("startup").await;
    let registry = reloader.shared.read().unwrap().clone();

    let callers = Arc::new(CallerMap::from_config(&cfg, &keys));
    let usage = Arc::new(UsageLog::open(&cfg.server.state_dir)?);
    let metrics = Arc::new(Metrics::new());
    let bind = bind_override.unwrap_or_else(|| cfg.server.bind.clone());

    // E3: quota watcher — spawns `aitop --json` on an interval and swaps
    // the snapshot behind an RwLock. Missing aitop degrades to
    // health-only routing (warned once, not fatal).
    let quota = quota::QuotaWatcher::new(quota::QuotaState::default());
    let provider_map = Arc::new(quota::ProviderMap {
        explicit: cfg.quota_watcher.provider_map.clone(),
    });
    if cfg.quota_watcher.interval_secs > 0 {
        let cmd = cfg.quota_watcher.command_or_default();
        let interval = std::time::Duration::from_secs(cfg.quota_watcher.interval_secs);
        let w = quota.clone();
        tokio::spawn(async move {
            quota::run_watcher(w, &cmd, interval).await;
        });
    }

    // E8: local-engine watcher (ollama /api/ps, llama.cpp /slots) and
    // warm-cache stickiness for the local pool.
    let local = std::sync::Arc::new(arouter::localeng::LocalEngines::default());
    let sticky = std::sync::Arc::new(arouter::localeng::StickyMap::new(
        cfg.routing.sticky_local_secs,
    ));
    if cfg.local_engines.poll_secs > 0 && !cfg.local_engines.engines.is_empty() {
        let urls: Vec<(String, Option<String>)> = cfg
            .local_engines
            .engines
            .iter()
            .map(|e| (e.url.clone(), e.kind.clone()))
            .collect();
        let interval = std::time::Duration::from_secs(cfg.local_engines.poll_secs);
        let (w, http) = (local.clone(), proxy::client());
        tokio::spawn(async move {
            arouter::localeng::run_watcher(w, http, urls, interval).await;
        });
        tracing::info!(
            "local engines: polling {} engine(s) every {}s",
            cfg.local_engines.engines.len(),
            cfg.local_engines.poll_secs
        );
    }

    let state = proxy::AppState {
        cfg: cfg.clone(),
        registry: reloader.shared.clone(),
        usage: usage.clone(),
        callers: callers.clone(),
        metrics: metrics.clone(),
        health: health.clone(),
        quota,
        provider_map,
        local,
        sticky,
        http: proxy::client(),
        started: Instant::now(),
    };

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    print_banner(&bind, registry.models.len());
    tracing::info!(
        "arouter v{} listening on http://{bind} ({} models, {} profiles, {} callers, auth required: {})",
        env!("CARGO_PKG_VERSION"),
        registry.models.len(),
        registry.profiles.len(),
        callers.len(),
        cfg.auth.require
    );
    for pool in arouter::config::POOLS {
        let per_pool: Vec<&arouter::registry::Model> = registry
            .models
            .iter()
            .filter(|m| m.pools.iter().any(|p| p == pool))
            .collect();
        if !per_pool.is_empty() {
            let mut grades: Vec<String> = per_pool
                .iter()
                .map(|m| registry.ladder.token(m.grade_rank).to_string())
                .collect();
            grades.sort();
            grades.dedup();
            tracing::info!(
                "  pool {pool}: {} models (grades: {})",
                per_pool.len(),
                grades.join(", ")
            );
        }
    }
    drop(registry);

    let reload_task = tokio::spawn(reload_loop(reloader));
    let app = proxy::router(state);
    let server = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());
    tokio::select! {
        r = server => r?,
        _ = reload_task => {},
    }
    tracing::info!("shut down cleanly");
    Ok(())
}

/// Startup banner: ANSI-shadow wordmark + boot summary, printed once the
/// listener is bound (plain text — stays clean in journald/pipes).
fn print_banner(bind: &str, models: usize) {
    println!(
        "\n █████╗ ██████╗ ██████╗ ███████╗████████╗██████╗  ██████╗ \n\
██╔══██╗██╔══██╗██╔══██╗██╔════╝╚══██╔══╝██╔══██╗██╔═══██╗\n\
███████║██████╔╝██████╔╝█████╗     ██║   ██████╔╝██║   ██║\n\
██╔══██║██╔══██╗██╔══██╗██╔══╝     ██║   ██╔══██╗██║   ██║\n\
██║  ██║██║  ██║██║  ██║███████╗   ██║   ██║  ██║╚██████╔╝\n\
╚═╝  ╚═╝╚═╝  ╚═╝╚═╝  ╚═╝╚══════╝   ╚═╝   ╚═╝  ╚═╝ ╚═════╝ \n\
  pick a grade, not a provider · v{v}\n\
  listening http://{bind} · {models} models\n",
        v = env!("CARGO_PKG_VERSION"),
        bind = bind,
        models = models,
    );
}

/// Rebuilds the registry from disk + discovery and swaps it in place.
/// On any failure the previous registry keeps serving.
struct Reloader {
    shared: Arc<RwLock<Arc<Registry>>>,
    config_path: Option<PathBuf>,
    http: reqwest::Client,
    discovery_interval_secs: u64,
}

impl Reloader {
    async fn reload(&self, why: &str) -> bool {
        let cfg = match Config::load(self.config_path.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("reload ({why}) — config invalid, keeping old registry: {e}");
                return false;
            }
        };
        let keys = KeyStore::load_default(cfg.server.env_file.as_deref());
        let auto = AutoGrades::load(self.config_path.as_deref());
        let ladder = cfg.grades.ladder();
        let catalog = match Catalog::embedded(&ladder) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("reload ({why}) — embedded catalog invalid: {e}");
                return false;
            }
        };
        // discovery_interval_secs = 0 disables discovery entirely —
        // including the startup/SIGHUP pass (docs/grading.md: "0 = off").
        let discovered = if self.discovery_interval_secs > 0 {
            discover::discover(&self.http, &keys, &cfg.models).await
        } else {
            Vec::new()
        };
        let merged = discover::merge_discovered(&cfg, &discovered, &auto, &catalog);
        match Registry::from_config(&merged, &keys) {
            Ok(reg) => {
                let n = reg.models.len();
                let manual = merged
                    .models
                    .iter()
                    .filter(|m| m.provenance.is_empty())
                    .count();
                *self.shared.write().unwrap() = Arc::new(reg);
                if discovered.is_empty() {
                    tracing::info!(
                        "registry reloaded ({why}): {n} models (no new upstream models)"
                    );
                } else {
                    tracing::info!(
                        "registry reloaded ({why}): {n} models (+{new} discovered, {manual} manual)",
                        new = discovered.len(),
                    );
                }
                true
            }
            Err(e) => {
                tracing::error!("reload ({why}) — registry invalid, keeping old registry: {e}");
                false
            }
        }
    }
}

/// Triggers: SIGHUP (always), config file mtime change, discovery
/// interval. All run the same full reload — cheap and idempotent.
async fn reload_loop(r: Reloader) {
    let r = Arc::new(r);
    #[cfg(unix)]
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .expect("install SIGHUP handler");
    let mut last_mtime = file_mtime(&r.config_path);
    // single 5s heartbeat: config-mtime + discovery schedule. The bind
    // address is fixed at startup; everything else hot-reloads.
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let discovery = if r.discovery_interval_secs > 0 {
        Some(tokio::time::interval(Duration::from_secs(
            r.discovery_interval_secs.max(5),
        )))
    } else {
        None
    };
    let mut discovery = discovery;
    loop {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = sighup.recv() => {
                    tracing::info!("SIGHUP: reloading registry");
                    r.reload("SIGHUP").await;
                    last_mtime = file_mtime(&r.config_path);
                }
                _ = tick.tick() => {
                    let m = file_mtime(&r.config_path);
                    if m != last_mtime {
                        last_mtime = m;
                        r.reload("config changed").await;
                    }
                }
                _ = async { discovery.as_mut().expect("checked Some").tick().await }, if discovery.is_some() => {
                    r.reload("discovery poll").await;
                }
            }
        }
        #[cfg(not(unix))]
        {
            tick.tick().await;
            let m = file_mtime(&r.config_path);
            if m != last_mtime {
                last_mtime = m;
                r.reload("config changed").await;
            }
        }
    }
}

fn file_mtime(path: &Option<PathBuf>) -> Option<SystemTime> {
    path.as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install ctrl-c handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

// ─────────────────────────── read-only subcommands ───────────────────────────

async fn status(bind: Option<String>) -> anyhow::Result<()> {
    let bind = match bind {
        Some(b) => b,
        None => Config::load(None)?.server.bind,
    };
    let resp = reqwest::get(format!("http://{bind}/status.json")).await?;
    let json: serde_json::Value = resp.json().await?;
    println!("{}", serde_json::to_string_pretty(&json)?);
    Ok(())
}

/// Config + discovery + auto-grading, without touching the daemon —
/// what `arouter models` shows and what `serve` starts from.
async fn local_registry(config: Option<&std::path::Path>) -> anyhow::Result<(Config, Registry)> {
    let cfg = Config::load(config)?;
    let keys = KeyStore::load_default(cfg.server.env_file.as_deref());
    let auto = AutoGrades::load(config);
    let ladder = cfg.grades.ladder();
    let catalog = Catalog::embedded(&ladder)?;
    let discovered = discover::discover(&proxy::client(), &keys, &cfg.models).await;
    let merged = discover::merge_discovered(&cfg, &discovered, &auto, &catalog);
    let reg = Registry::from_config(&merged, &keys)?;
    Ok((cfg, reg))
}

async fn models(
    grade: Option<&str>,
    write: bool,
    config: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let cfg = Config::load(config)?;
    let keys = KeyStore::load_default(cfg.server.env_file.as_deref());
    let auto = AutoGrades::load(config);
    let ladder = cfg.grades.ladder();
    let catalog = Catalog::embedded(&ladder)?;
    let discovered = discover::discover(&proxy::client(), &keys, &cfg.models).await;
    let merged = discover::merge_discovered(&cfg, &discovered, &auto, &catalog);
    let reg = Registry::from_config(&merged, &keys)?;

    if write {
        let ag = discover::auto_grades_from(&merged.models, catalog.version);
        let path = ag.write(config)?;
        println!(
            "wrote {}: {} entries (regenerate any time; delete to fall back to catalog/heuristic)",
            path.display(),
            ag.models.len()
        );
    }

    if !discovered.is_empty() {
        println!("discovered on upstreams ({}):", discovered.len());
        for m in &discovered {
            let mc = merged.models.iter().find(|m2| m2.id == m.id);
            let (g, prov, why) = match mc {
                Some(mc) => (
                    mc.grade.clone(),
                    mc.provenance.clone(),
                    mc.rationale.clone(),
                ),
                None => ("?".into(), "?".into(), String::new()),
            };
            println!("  {:<38} {:>3}  {:<9} {why}", m.id, g, prov);
        }
    }

    match grade {
        Some(g) => {
            let req = reg.ladder.parse_request(g).ok_or_else(|| {
                anyhow::anyhow!("unknown grade {g:?}; valid: {}", reg.ladder.list())
            })?;
            for pool in arouter::config::POOLS {
                let list = reg.attempt_list("default", pool, &req);
                if list.is_empty() {
                    continue;
                }
                println!("attempt order for {g:?} in pool {pool}:");
                for (i, m) in list.iter().enumerate() {
                    println!(
                        "  {}. {:<34} grade {}  weight {}  {}",
                        i + 1,
                        m.id,
                        reg.ladder.token(m.grade_rank),
                        m.weight,
                        if m.blended_cost > 0.0 {
                            format!("${:.2}/Mtok blended", m.blended_cost)
                        } else {
                            "-".into()
                        }
                    );
                }
            }
        }
        None => {
            println!("models ({}):", reg.models.len());
            for pool in arouter::config::POOLS {
                for rank in 0..reg.ladder.grades.len() {
                    let ids: Vec<String> = reg
                        .models
                        .iter()
                        .filter(|m| m.grade_rank == rank && m.pools.iter().any(|p| p == pool))
                        .map(|m| {
                            if m.provenance == "manual" {
                                m.id.clone()
                            } else {
                                format!("{} [{}]", m.id, m.provenance)
                            }
                        })
                        .collect();
                    if !ids.is_empty() {
                        println!(
                            "  [{pool}] {:>2}: {}",
                            reg.ladder.token(rank),
                            ids.join(", ")
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

fn profiles() -> anyhow::Result<()> {
    let (cfg, reg) = futures::executor::block_on(local_registry(None))?;
    if cfg.profiles.is_empty() {
        println!("no profiles configured — add [profiles.<name>] sections (see docs/profiles.md)");
        return Ok(());
    }
    for (name, p) in &cfg.profiles {
        let eff = &reg.profiles[name];
        let vis = if !eff.include.is_empty() {
            format!("include {}", eff.include.join(","))
        } else if !eff.exclude.is_empty() {
            format!("exclude {}", eff.exclude.join(","))
        } else {
            "all models".into()
        };
        println!(
            "{} — {} [{vis}]",
            name,
            if p.description.is_empty() {
                "-"
            } else {
                &p.description
            }
        );
    }
    Ok(())
}
