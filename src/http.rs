//! Bringing the server up: the router, TLS, and the small shared helpers.
//!
//! This is the only place that knows about ports, certificates and CORS. The handlers
//! themselves live in `addon`, `play` and `ui`.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::http::{Method, header};
use axum::routing::{get, post};
use tokio::sync::{Mutex, RwLock};

use crate::app::*;
use crate::ui::*;
use crate::ui_downloads::*;
use crate::config::Config;
use crate::library::Library;
use crate::ncore::NcoreClient;

use crate::addon::*;
use crate::play::*;

pub async fn serve() -> Result<()> {
    let path = Config::path_from_env();
    let mut cfg = Config::load(&path)?;
    cfg.apply_env_overrides();
    cfg.prepare_layout();

    // A key is required: the stream URLs are unauthenticated apart from this, and an
    // empty one would leave the server open to anyone who can reach the port.
    if cfg.auth.api_key.trim().is_empty() {
        cfg.auth.api_key = random_key();
        cfg.save(&path)?;
        tracing::info!(api_key = %cfg.auth.api_key, "generated an api key and saved it");
    }

    // The secret that signs login tokens. Generated once and kept, because it is what
    // makes a token survive a restart: without it every restart would log the admin out.
    if cfg.auth.session_secret.trim().is_empty() {
        cfg.auth.session_secret = crate::webui::random_secret();
        cfg.save(&path)?;
        tracing::info!("generated a session secret");
    }

    // First run: work out this machine's address so HTTPS, and therefore the television,
    // work without anything being configured by hand. Saved so it is visible and can be
    // corrected on a machine with several networks.
    if cfg.network.enable_https && cfg.network.host_ip.trim().is_empty() {
        match crate::tls::detect_lan_ipv4() {
            Some(ip) => {
                cfg.network.host_ip = ip.clone();
                cfg.save(&path)?;
                tracing::info!(host_ip = %ip, "detected this machine's network address");
            }
            None => tracing::warn!(
                "could not work out this machine's network address; set network.host_ip \
                 to enable https"
            ),
        }
    }

    // The folders the server works in, made now rather than at the moment each is first
    // needed. Everything below would create its own on demand, but then a fresh install
    // shows half a structure until something happens to fill it in, and "where does it put
    // things" is a question the folder itself should answer.
    for dir in [
        cfg.torrent.save_path.as_str(),
        cfg.storage.torrent_files_dir.as_str(),
        cfg.network.cert_cache_dir.as_str(),
    ] {
        if dir.trim().is_empty() {
            continue;
        }
        if let Err(e) = std::fs::create_dir_all(dir) {
            // Not fatal on its own: the download folder may be on a disk that is not there
            // yet, and the interface is the place to correct that.
            tracing::warn!(error = %e, dir, "could not create the folder");
        }
    }

    let tmdb = match crate::tmdb::TmdbClient::new(&cfg.tmdb.api_key, &cfg.tmdb.language) {
        Ok(c) => Some(c),
        Err(e) => {
            // Not fatal: IMDb-id requests still work. Only TMDB-sourced titles,
            // which includes most Hungarian series, become unresolvable.
            tracing::warn!(error = %e, "TMDB is not configured; tmdb: ids cannot be resolved");
            None
        }
    };

    let ncore = NcoreClient::new(&cfg.ncore.username, &cfg.ncore.password)?;
    if cfg.ncore.username.is_empty() {
        tracing::warn!("ncore.username is empty; searches will fail until it is set");
    } else {
        // A failure here is not fatal: the client re-logins on demand, and the
        // server should still start so the settings can be fixed.
        if let Err(e) = ncore.login().await {
            tracing::warn!(error = %e, "nCore login failed at startup");
        }
    }

    // A store that will not parse is fatal on purpose: it is the only record of what
    // is on the disk, and starting with an empty one would orphan every download and
    // silently drop every seeding obligation.
    let store = crate::state::Store::load(std::path::Path::new(&cfg.storage.state_path))
        .with_context(|| {
            format!(
                "{} could not be read; move it aside to start fresh",
                cfg.storage.state_path
            )
        })?;

    let shared_cfg: crate::config::Shared = Arc::new(RwLock::new(cfg.clone()));
    let cfg_generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let lib = Library::new(shared_cfg.clone(), cfg_generation.clone(), store.clone()).await?;
    let state = Arc::new(AppState {
        lib,
        ncore: RwLock::new(ncore),
        tmdb: RwLock::new(tmdb),
        cfg: shared_cfg,
        cfg_path: path.clone(),
        cfg_generation,
        sources: Mutex::new(HashMap::new()),
        ui: crate::webui::Ui::default(),
        store: store.clone(),
        owed: RwLock::new(OwedSnapshot::default()),
        last_notice: RwLock::new(HashMap::new()),
        https_host: RwLock::new(None),
        disks: RwLock::new(None),
    });

    state.ui.sessions.set_secret(&cfg.auth.session_secret).await;
    state.check_disk_space().await;

    // Before anything else can be served: a torrent that is not open is not seeding,
    // and seeding time is what a private tracker is owed.
    let restored = state.lib.restore(&store.items().await).await;
    // Write back whichever file each one settled on, so a record written before the
    // index was tracked stops needing the name fallback.
    for (hash, index) in restored {
        store.set_file_index(&hash, index).await;
    }
    let _ = store.flush().await;

    crate::state::spawn_flusher(
        store.clone(),
        std::time::Duration::from_secs(cfg.storage.flush_interval_secs.max(1)),
    );
    // Problems reported from anywhere in the program, and the ones that report themselves only
    // by how the machine feels.
    crate::app::spawn_problem_reporter(state.clone(), crate::alerts::channel());
    crate::app::spawn_watchdog(state.clone());

    crate::maintenance::spawn(
        Arc::new(ServerWorld {
            state: state.clone(),
        }),
        store.clone(),
    );

    // Stremio runs in a browser, at web.stremio.com or inside the desktop app's own
    // web view, so every addon response has to carry CORS headers. Without them the
    // browser refuses to hand the body to the page and reports only "Failed to fetch",
    // which says nothing about the cause. The preflight matters too: a request with a
    // Range header is not simple, so the browser asks OPTIONS first, and an addon that
    // answers 405 there cannot be installed.
    //
    // Scoped to the addon routes on purpose. The settings pages authenticate with a
    // cookie, and leaving them out means no other origin can read them at all.
    let cors = tower_http::cors::CorsLayer::new()
        .allow_origin(tower_http::cors::Any)
        .allow_methods([Method::GET, Method::HEAD, Method::OPTIONS])
        .allow_headers(tower_http::cors::Any)
        // Chrome's private network access rule.
        //
        // Stremio in a browser is served from a public site, and this server answers on a
        // private address. Chrome treats that combination as a step from a public network into a
        // private one and refuses it unless the preflight says it is allowed, which it reports
        // in the network tab as a plain CORS error however correct the other headers are. Seen
        // exactly that way: our preflight answered 200 with the origin allowed, and every media
        // request was still blocked.
        //
        // Only ever an answer to a request that already carries the browser's own preflight for
        // this, and it grants nothing beyond reaching a server the viewer is running themselves.
        .allow_private_network(true)
        // A player reads these to learn the size and whether it may seek.
        .expose_headers([
            header::CONTENT_RANGE,
            header::ACCEPT_RANGES,
            header::CONTENT_LENGTH,
            header::CONTENT_TYPE,
        ]);

    let addon = Router::new()
        .route("/{api_key}/manifest.json", get(manifest))
        .route("/{api_key}/stream/{kind}/{id}", get(stream_list))
        .route("/{api_key}/play/{torrent_id}", get(play_movie).head(play_movie))
        .route(
            "/{api_key}/play/{torrent_id}/{season}/{episode}",
            get(play_episode).head(play_episode),
        )
        .layer(cors);

    let app = Router::new()
        .route("/", get(status))
        // Reachable by name as well: "/status" is what the interface links to and what
        // anyone would type.
        .route("/status", get(status))
        .route("/ui", get(ui_page))
        .route("/ui/setup", post(ui_setup))
        .route("/ui/login", post(ui_login))
        .route("/ui/logout", post(ui_logout))
        .route("/ui/shutdown", post(ui_shutdown))
        .route("/ui/save-common", post(ui_save_common))
        .route("/ui/save-retention", post(ui_save_retention))
        .route("/ui/save-engine", post(ui_save_engine))
        .route("/ui/test-notification", post(ui_test_notification))
        .route("/ui/save-toml", post(ui_save_toml))
        .route("/ui/downloads", get(ui_downloads))
        .route("/ui/downloads/keep", post(ui_set_keep))
        .route("/ui/downloads/watched", post(ui_set_watched))
        .route("/ui/downloads/delete", post(ui_delete_download))
        .route("/ui/downloads/refresh-tracker", post(ui_refresh_tracker))
        .route("/ui/downloads/dry-run", post(ui_dry_run))
        .route("/ui/downloads/sweep-now", post(ui_sweep_now))
        .route("/ui/save-network", post(ui_save_network))
        .merge(addon)
        .with_state(state.clone());

    // Kept so the HTTPS hostname can be recorded once the TLS listener is up; the
    // router has consumed the state by then.
    let state_for_https = state;

    let addr: std::net::SocketAddr = format!("{}:{}", cfg.server.listen_addr, cfg.server.port)
        .parse()
        .context("server.listen_addr / server.port")?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("cannot bind {addr}"))?;

    // HTTPS runs alongside, not instead: the plain port stays the convenient way to
    // reach the settings from this machine, while anything else on the network needs
    // TLS to be allowed to talk to us at all.
    let https = start_https(&cfg, app.clone()).await;
    *state_for_https.https_host.write().await = https.clone();

    crate::note(&format!(
        "\n  beállítások:     http://localhost:{}/ui",
        cfg.server.port
    ));
    match &https {
        Some(host) => crate::note(&format!(
            "  addon manifest:  https://{host}:{}/{}/manifest.json  <- ez kell a tv-re",
            cfg.network.https_port, cfg.auth.api_key
        )),
        None => crate::note(&format!(
            "  addon manifest:  http://{}:{}/{}/manifest.json  (csak ezen a gépen: más \
             eszközhöz HTTPS kell)",
            host_for_display(&cfg),
            cfg.server.port,
            cfg.auth.api_key
        )),
    }
    crate::note("");

    // With connect info, so a login can tell whether it came from this machine.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .context("serve")?;
    Ok(())
}

/// Brings up the TLS listener, returning the hostname clients should use.
///
/// Every failure here is a warning rather than an error: the server is still useful on
/// this machine over plain HTTP, and taking it down entirely because a certificate
/// could not be fetched would remove the very interface needed to fix the setting that
/// is wrong.
pub(crate) async fn start_https(cfg: &Config, app: Router) -> Option<String> {
    if !cfg.network.enable_https {
        tracing::info!("https is switched off in the configuration");
        return None;
    }
    let host = match cfg.network.https_host() {
        Some(host) => host,
        None => {
            tracing::warn!(
                "https is on but network.host_ip is not a LAN address, so no certificate \
                 hostname can be built"
            );
            return None;
        }
    };

    let cache = crate::tls::Cache::new(&cfg.network.cert_cache_dir);
    let cert = match crate::tls::obtain(
        &cfg.network.cert_provider_url,
        &cfg.network.cert_key_url,
        &cache,
        crate::state::now(),
        cfg.network.renew_margin_secs(),
    )
    .await
    {
        Ok(cert) => cert,
        Err(e) => {
            tracing::warn!(error = %e, "https not started: no certificate");
            return None;
        }
    };

    let tls = match crate::tls::rustls_config(&cert) {
        Ok(config) => axum_server::tls_rustls::RustlsConfig::from_config(config),
        Err(e) => {
            tracing::warn!(error = %e, "https not started: unusable certificate");
            return None;
        }
    };

    let addr: std::net::SocketAddr =
        match format!("{}:{}", cfg.server.listen_addr, cfg.network.https_port).parse() {
            Ok(addr) => addr,
            Err(e) => {
                tracing::warn!(error = %e, "https not started: bad address");
                return None;
            }
        };

    let serving = tls.clone();
    tokio::spawn(async move {
        if let Err(e) = axum_server::bind_rustls(addr, serving)
            .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
            .await
        {
            tracing::error!(error = %e, "the https listener stopped");
        }
    });

    // Reloaded in place rather than by restarting: a renewal must not interrupt a film.
    crate::tls::spawn_renewal(
        tls,
        cfg.network.cert_provider_url.clone(),
        cfg.network.cert_key_url.clone(),
        cfg.network.cert_cache_dir.clone(),
        cfg.network.renew_margin_secs(),
    );

    match cert.expires_in(crate::state::now()) {
        Some(secs) => tracing::info!(
            host = %host,
            port = cfg.network.https_port,
            certificate_valid_days = secs / 86_400,
            "https listening"
        ),
        None => tracing::info!(host = %host, port = cfg.network.https_port, "https listening"),
    }
    Some(host)
}

pub(crate) fn host_for_display(cfg: &Config) -> String {
    if cfg.network.host_ip.is_empty() {
        "localhost".to_string()
    } else {
        cfg.network.host_ip.clone()
    }
}

/// 32 hex characters from the operating system's CSPRNG.
///
/// This key is the only thing protecting the stream URLs, so it has to be
/// unguessable. A time or address based value would not be, and a failure to read
/// real randomness is treated as fatal rather than silently downgraded.
pub(crate) fn random_key() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("the OS random source must be available");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn authorised(cfg: &Config, key: &str) -> bool {
    // An empty configured key would otherwise authorise an empty path segment.
    !cfg.auth.api_key.is_empty()
        && key.len() == cfg.auth.api_key.len()
        && key == cfg.auth.api_key
}
