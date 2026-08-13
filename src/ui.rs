//! The admin interface's request handlers.
//!
//! Rendering lives in `webui`; this is the part that reads the request, changes something,
//! and decides which page to send back.

use std::sync::Arc;

use axum::extract::{Form, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::app::*;
use crate::config::Config;


/// Which page the interface should show, given whether an admin exists and whether
/// this request carries a live session.
pub(crate) async fn ui_state(state: &AppState, cookies: Option<&str>) -> UiAccess {
    let cfg = state.config().await;
    if cfg.auth.password_hash.trim().is_empty() {
        return UiAccess::NeedsSetup;
    }
    let token = crate::webui::session_from_cookies(cookies);
    if state.ui.sessions.is_valid(&token).await {
        UiAccess::LoggedIn
    } else {
        UiAccess::NeedsLogin
    }
}

pub(crate) enum UiAccess {
    NeedsSetup,
    NeedsLogin,
    LoggedIn,
}

pub(crate) fn cookie_header(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::COOKIE).and_then(|v| v.to_str().ok())
}

pub(crate) fn html(body: String) -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

pub(crate) async fn settings_page(state: &AppState, message: Option<String>) -> Response {
    let cfg = state.config().await;
    let toml_text = toml::to_string_pretty(&cfg).unwrap_or_else(|e| format!("# {e}"));
    let last_sweep = state.store.last_sweep_date().await;
    html(crate::webui::page(crate::webui::PageState::Settings {
        toml_text,
        message,
        engine: crate::webui::EngineView::new(&cfg.torrent, &cfg.maintenance, &cfg.pieces),
        retention: crate::webui::RetentionView::new(&cfg.maintenance, &last_sweep),
        network: network_view(state, &cfg).await,
        bithumen_enabled: cfg.bithumen.enabled,
    }))
}

/// The addon URL and what to say about reaching it.
pub(crate) async fn network_view(state: &AppState, cfg: &Config) -> crate::webui::NetworkView {
    let https_live = *state.https_host.read().await != None;
    let host = cfg.network.https_host();

    // The URL offered is the one that will actually work. Showing the HTTPS address
    // while the TLS listener failed to start would send someone to the television with
    // a link that cannot connect, and the reason would not be visible there.
    let (addon_url, reachable_elsewhere) = match (&host, https_live) {
        (Some(host), true) => (
            format!(
                "https://{host}:{}/{}/manifest.json",
                cfg.network.https_port, cfg.auth.api_key
            ),
            true,
        ),
        _ => (
            format!(
                "http://localhost:{}/{}/manifest.json",
                cfg.server.port, cfg.auth.api_key
            ),
            false,
        ),
    };

    let https_state = match (&host, https_live) {
        (Some(host), true) => {
            let cert = crate::tls::Cache::new(&cfg.network.cert_cache_dir).load();
            match cert.and_then(|c| c.expires_in(crate::state::now())) {
                Some(secs) if secs > 0 => format!(
                    "A HTTPS fut, neve {host}. A tanúsítvány még {} napig érvényes, és lejárat előtt \
                     {} nappal magától megújul.",
                    secs / 86_400,
                    cfg.network.cert_renew_margin_days
                ),
                _ => format!("A HTTPS fut, neve {host}, de a tanúsítvány lejáratát nem tudtuk kiolvasni."),
            }
        }
        (Some(_), false) => {
            "A HTTPS be van állítva, de nem indult el. A napló megmondja, miért; jellemzően vagy \
             nem volt internet a tanúsítvány letöltésekor, vagy a port foglalt."
                .to_string()
        }
        (None, _) if !cfg.network.enable_https => "A HTTPS ki van kapcsolva.".to_string(),
        (None, _) => "A HTTPS addig nem indul, amíg fentebb nincs megadva a gép hálózati címe."
            .to_string(),
    };

    crate::webui::NetworkView {
        addon_url,
        reachable_elsewhere,
        https_state,
        host_ip: cfg.network.host_ip.clone(),
        https_port: cfg.network.https_port.to_string(),
        enable_https: cfg.network.enable_https,
        live_rows: live_rows(state, cfg).await,
    }
}

/// The few facts that change while the server runs.
///
/// Kept short on purpose. Everything else about the server is a setting, and settings are
/// already on this page; what is worth a glance is whether something is playing, whether
/// anything is being seeded, and when the next deletion happens.
pub(crate) async fn live_rows(state: &AppState, cfg: &Config) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    let open = state.lib.open().await;

    let mut playing: Vec<String> = Vec::new();
    let mut down_rate = 0i64;
    let mut up_rate = 0i64;
    for (_, entry) in &open {
        let stats = entry.stats();
        down_rate += stats.download_rate as i64;
        up_rate += stats.upload_rate as i64;
        if !entry.reader_positions().await.is_empty() {
            playing.push(entry.file_name.clone());
        }
    }

    rows.push((
        "Most játszik".into(),
        if playing.is_empty() {
            "semmi".to_string()
        } else {
            playing.join(", ")
        },
    ));
    rows.push((
        "Nyitott torrentek".into(),
        format!("{} db, seedelnek", open.len()),
    ));
    rows.push((
        "Sebesség".into(),
        format!(
            "le {:.1} MB/s, fel {:.1} MB/s",
            down_rate as f64 / 1e6,
            up_rate as f64 / 1e6
        ),
    ));

    let last_sweep = state.store.last_sweep_date().await;
    rows.push((
        "Törlés".into(),
        if cfg.maintenance.enable_deletion {
            crate::maintenance::next_run_label(&cfg.maintenance, &last_sweep)
        } else {
            "kikapcsolva".to_string()
        },
    ));

    // Disks. Shown always rather than only when short, because "how much room is left" is a
    // thing somebody checks before starting a 60 GB film, not only after a warning.
    match state.disks.read().await.clone() {
        Some(report) => {
            for line in &report.lines {
                rows.push((
                    if report.low { "Lemez, kevés a hely".into() } else { "Lemez".into() },
                    line.clone(),
                ));
            }
        }
        None => rows.push(("Lemez".into(), "még nem mértük".into())),
    }

    let owed = state.owed.read().await.clone();
    rows.push((
        "Seedelési kötelezettség".into(),
        match (&owed.fetched_at, &owed.error) {
            (None, _) => "még nem kérdeztük meg".to_string(),
            (Some(_), Some(_)) => "nem tudjuk, a tracker nem válaszolt".to_string(),
            (Some(_), None) => format!("{} db nyitott", owed.entries.len()),
        },
    ));

    rows
}

#[derive(serde::Deserialize)]
pub(crate) struct NetworkForm {
    host_ip: String,
    https_port: String,
    #[serde(default)]
    enable_https: Option<String>,
}

pub(crate) async fn ui_save_network(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<NetworkForm>,
) -> Response {
    if let Some(page) = require_login(&state, cookie_header(&headers)).await {
        return page;
    }

    let mut cfg = state.config().await;
    let trimmed = form.host_ip.trim();
    // Refused rather than accepted and left broken: a malformed address means no
    // certificate hostname, and the failure would only show up on the television.
    if !trimmed.is_empty() && crate::tls::local_ip_host(trimmed, &cfg.network.cert_domain).is_err() {
        return settings_page(
            &state,
            Some(format!("{trimmed:?} nem olyan cím, mint a 192.168.1.100.")),
        )
        .await;
    }
    cfg.network.host_ip = trimmed.to_string();
    if let Ok(port) = form.https_port.trim().parse::<u16>() {
        if port > 0 {
            cfg.network.https_port = port;
        }
    }
    cfg.network.enable_https = form.enable_https.is_some();

    let message = match state.apply_config(cfg).await {
        Ok(()) => "Mentve. A HTTPS módosítás újraindítás után lép életbe.".to_string(),
        Err(e) => format!("Nem sikerült menteni: {e}"),
    };
    settings_page(&state, Some(message)).await
}

pub(crate) async fn ui_page(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    match ui_state(&state, cookie_header(&headers)).await {
        UiAccess::NeedsSetup => html(crate::webui::page(crate::webui::PageState::Setup)),
        UiAccess::NeedsLogin => {
            html(crate::webui::page(crate::webui::PageState::Login { error: None }))
        }
        UiAccess::LoggedIn => settings_page(&state, None).await,
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct SetupForm {
    password: String,
    password2: String,
}

pub(crate) async fn ui_setup(
    State(state): State<Arc<AppState>>,
    Form(form): Form<SetupForm>,
) -> Response {
    let mut cfg = state.config().await;
    // Refuse to run setup twice: otherwise anyone reaching the port could replace the
    // admin password after it has been set.
    if !cfg.auth.password_hash.trim().is_empty() {
        return redirect("/ui");
    }
    if form.password != form.password2 {
        return html(crate::webui::page(crate::webui::PageState::Login {
            error: Some("the two passwords differ".into()),
        }));
    }
    let hash = match crate::webui::hash_password(&form.password) {
        Ok(h) => h,
        Err(e) => {
            return html(crate::webui::page(crate::webui::PageState::Login {
                error: Some(e.to_string()),
            }));
        }
    };

    cfg.auth.password_hash = hash;
    if let Err(e) = state.apply_config(cfg).await {
        return html(crate::webui::page(crate::webui::PageState::Login {
            error: Some(format!("could not save: {e}")),
        }));
    }
    redirect("/ui")
}

#[derive(serde::Deserialize)]
pub(crate) struct LoginForm {
    username: String,
    password: String,
}

pub(crate) async fn ui_login(
    State(state): State<Arc<AppState>>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    Form(form): Form<LoginForm>,
) -> Response {
    let cfg = state.config().await;
    let ok = form.username == cfg.auth.username
        && crate::webui::verify_password(&cfg.auth.password_hash, &form.password);

    if !ok {
        // One message for both a wrong name and a wrong password, so the form does not
        // reveal which half was right.
        tracing::warn!(user = %form.username, "failed admin login");
        return html(crate::webui::page(crate::webui::PageState::Login {
            error: Some("wrong username or password".into()),
        }));
    }

    // A login from this machine stays valid; one from elsewhere on the network expires
    // in twelve hours. Whoever is sitting at this machine already has the configuration
    // file and the downloads, so a daily password does not protect anything.
    let local = crate::webui::is_local_peer(Some(peer));
    let token = state.ui.sessions.create(local).await;
    tracing::info!(local, "admin logged in");
    (
        [
            (
                header::SET_COOKIE,
                crate::webui::session_cookie(&token, local),
            ),
            (header::LOCATION, "/ui".to_string()),
        ],
        StatusCode::SEE_OTHER,
    )
        .into_response()
}

pub(crate) async fn ui_logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let token = crate::webui::session_from_cookies(cookie_header(&headers));
    state.ui.sessions.destroy(&token).await;
    (
        [
            (header::SET_COOKIE, crate::webui::clear_cookie()),
            (header::LOCATION, "/ui".to_string()),
        ],
        StatusCode::SEE_OTHER,
    )
        .into_response()
}

pub(crate) fn redirect(to: &str) -> Response {
    ([(header::LOCATION, to.to_string())], StatusCode::SEE_OTHER).into_response()
}

#[derive(serde::Deserialize)]
pub(crate) struct CommonForm {
    ncore_username: String,
    ncore_password: String,
    #[serde(default)]
    bithumen_username: String,
    #[serde(default)]
    bithumen_password: String,
    #[serde(default)]
    bithumen_enabled: Option<String>,
    tmdb_api_key: String,
    tmdb_language: String,
}

pub(crate) async fn ui_save_common(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<CommonForm>,
) -> Response {
    if !matches!(
        ui_state(&state, cookie_header(&headers)).await,
        UiAccess::LoggedIn
    ) {
        return redirect("/ui");
    }

    let mut cfg = state.config().await;
    // A blank field means unchanged, so a password never has to be retyped just to
    // edit something else on the same form.
    for (value, target) in [
        (&form.ncore_username, &mut cfg.ncore.username),
        (&form.ncore_password, &mut cfg.ncore.password),
        (&form.bithumen_username, &mut cfg.bithumen.username),
        (&form.bithumen_password, &mut cfg.bithumen.password),
        (&form.tmdb_api_key, &mut cfg.tmdb.api_key),
        (&form.tmdb_language, &mut cfg.tmdb.language),
    ] {
        if !value.trim().is_empty() {
            *target = value.trim().to_string();
        }
    }
    cfg.bithumen.enabled = form.bithumen_enabled.is_some();

    let second = if !cfg.bithumen.enabled {
        "A BitHUmen ki van kapcsolva, tehát nem kérdezzük meg."
    } else if cfg.bithumen.username.trim().is_empty() || cfg.bithumen.password.is_empty() {
        "A BitHUmen be van kapcsolva, de hiányzik a fiók, tehát nem kérdezzük meg."
    } else {
        "A BitHUmen akkor kap kérdést, ha az nCore semmit nem hozott a címre."
    };
    let message = match state.apply_config(cfg).await {
        Ok(()) => Some(format!("Mentve. {second}")),
        Err(e) => Some(format!("Nem sikerült menteni: {e}")),
    };
    settings_page(&state, message).await
}

/// The retention form. Checkboxes are absent from the body when unticked, which is
/// how HTML posts them, so each one is an `Option` and absence means off.
#[derive(serde::Deserialize)]
pub(crate) struct RetentionForm {
    #[serde(default)]
    keep_seed_days: String,
    #[serde(default)]
    retention_days: String,
    #[serde(default)]
    sweep_at: String,
    #[serde(default)]
    watched_position_percent: String,
    #[serde(default)]
    watched_min_served_percent: String,
    #[serde(default)]
    notify_webhook_url: Option<String>,
    #[serde(default)]
    hit_and_run: Option<String>,
    #[serde(default)]
    require_watched: Option<String>,
    #[serde(default)]
    enable_deletion: Option<String>,
    #[serde(default)]
    sweep_on_start: Option<String>,
    #[serde(default)]
    sweep_when_full: Option<String>,
    #[serde(default)]
    space_saving: Option<String>,
    #[serde(default)]
    notify_sweep: Option<String>,
    #[serde(default)]
    notify_disk: Option<String>,
    #[serde(default)]
    notify_problems: Option<String>,
}

pub(crate) async fn ui_save_retention(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<RetentionForm>,
) -> Response {
    if !matches!(
        ui_state(&state, cookie_header(&headers)).await,
        UiAccess::LoggedIn
    ) {
        return redirect("/ui");
    }

    let mut cfg = state.config().await;
    let m = &mut cfg.maintenance;
    // A blank or unparseable duration keeps the current one. Zero retention with
    // deletion on would empty the library at the next sweep, and a mistyped box must
    // not be able to do that.
    m.keep_seed_seconds = crate::webui::days_to_seconds(&form.keep_seed_days, m.keep_seed_seconds);
    m.cache_retention_seconds =
        crate::webui::days_to_seconds(&form.retention_days, m.cache_retention_seconds);
    m.sweep_at = crate::webui::sweep_time_or_current(&form.sweep_at, &m.sweep_at);
    m.watched_position_percent = crate::webui::percent_or_current(
        &form.watched_position_percent,
        m.watched_position_percent,
    );
    m.watched_min_served_percent = crate::webui::percent_or_current(
        &form.watched_min_served_percent,
        m.watched_min_served_percent,
    );
    // Trimmed and taken as given: this is a URL somebody pasted, and rejecting it here would
    // mean guessing which services are legitimate. An absent field leaves it alone; an empty
    // one is somebody clearing it on purpose.
    if let Some(url) = &form.notify_webhook_url {
        m.notify_webhook_url = url.trim().to_string();
    }
    m.hit_and_run = form.hit_and_run.is_some();
    m.require_watched = form.require_watched.is_some();
    m.enable_deletion = form.enable_deletion.is_some();
    m.sweep_on_start = form.sweep_on_start.is_some();
    m.sweep_when_full = form.sweep_when_full.is_some();
    m.space_saving = form.space_saving.is_some();
    m.notify_sweep = form.notify_sweep.is_some();
    m.notify_disk = form.notify_disk.is_some();
    m.notify_problems = form.notify_problems.is_some();

    let summary = describe_retention(m);
    let message = match state.apply_config(cfg).await {
        Ok(()) => Some(summary),
        Err(e) => Some(format!("Nem sikerült menteni: {e}")),
    };
    settings_page(&state, message).await
}

/// Stops the server.
///
/// There is no console window to close, so this is the way it ends. What is on disk is
/// written out first, resume data included: without that, the next start would re-hash
/// every finished file, which on a library of this size is minutes of disk work for
/// nothing. Playback in progress does stop, hence the confirmation on the page.
pub(crate) async fn ui_shutdown(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if !matches!(
        ui_state(&state, cookie_header(&headers)).await,
        UiAccess::LoggedIn
    ) {
        return redirect("/ui");
    }

    // The reply has to reach the browser before the process goes away, so the exit happens
    // just after this response has been handed back rather than inside the handler.
    tokio::spawn(async move {
        state.lib.save_all_resume_data().await;
        if let Err(e) = state.store.flush().await {
            tracing::error!("state could not be written before stopping: {e:#}");
        }
        tracing::info!("stopping on request from the interface");
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        std::process::exit(0);
    });

    html(crate::webui::stopped_page())
}

/// Sends a message to whatever destination is configured, so it can be seen to work.
///
/// The same path a real warning takes, deliberately: a test that goes out through a different
/// door proves nothing about the door being tested. Not throttled, because pressing the button
/// is the request.
pub(crate) async fn ui_test_notification(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if let Some(page) = require_login(&state, cookie_header(&headers)).await {
        return page;
    }

    let cfg = state.config().await;
    if cfg.maintenance.notify_webhook_url.trim().is_empty() {
        return settings_page(
            &state,
            Some("Nincs megadva értesítési cím, tehát nincs hova küldeni.".into()),
        )
        .await;
    }

    // Measured now rather than taken from the last look, so the numbers in the message are the
    // numbers on the disks.
    state.check_disk_space().await;
    let disks = state
        .disks
        .read()
        .await
        .as_ref()
        .map(|r| r.lines.clone())
        .unwrap_or_default();
    let message = format!(
        "Próbaüzenet a stremhu-rs-től.
{}",
        if disks.is_empty() {
            "A lemezeket nem sikerült megmérni.".to_string()
        } else {
            disks.join("
")
        }
    );
    state.notify(&message).await;

    settings_page(
        &state,
        Some(format!(
            "Elküldve ide: {}. Ha nem érkezik meg, a napló megmondja mit válaszolt a másik oldal.",
            cfg.maintenance.notify_webhook_url
        )),
    )
    .await
}

/// The engine and disk form.
#[derive(serde::Deserialize)]
pub(crate) struct EngineForm {
    #[serde(default)]
    max_active_torrents: String,
    #[serde(default)]
    complete_extras_below_mb: String,
    #[serde(default)]
    global_connections_limit: String,
    #[serde(default)]
    connections_while_streaming: String,
    #[serde(default)]
    warn_below_free_gb: String,
    #[serde(default)]
    warn_below_free_percent: String,
    #[serde(default)]
    partial_download: Option<String>,
}

pub(crate) async fn ui_save_engine(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<EngineForm>,
) -> Response {
    if !matches!(
        ui_state(&state, cookie_header(&headers)).await,
        UiAccess::LoggedIn
    ) {
        return redirect("/ui");
    }

    let mut cfg = state.config().await;
    let t = &mut cfg.torrent;
    t.max_active_torrents =
        crate::webui::active_limit_or_current(&form.max_active_torrents, t.max_active_torrents);
    t.global_connections_limit = crate::webui::count_or_current(
        &form.global_connections_limit,
        t.global_connections_limit,
        10,
        10_000,
    );
    t.connections_while_streaming = crate::webui::count_or_current(
        &form.connections_while_streaming,
        t.connections_while_streaming,
        5,
        1_000,
    );
    // Zero is a real answer here: it switches off picking up the leftovers entirely.
    t.complete_extras_below_bytes = match form.complete_extras_below_mb.trim().parse::<u64>() {
        Ok(mb) if mb <= 1_000_000 => mb * 1024 * 1024,
        _ => t.complete_extras_below_bytes,
    };

    // The idle limit never rises above the streaming one: idle torrents holding more peers
    // than the file being watched is the wrong way round.
    t.connections_while_idle = t.connections_while_idle.min(t.connections_while_streaming);

    cfg.pieces.partial_download = form.partial_download.is_some();

    let m = &mut cfg.maintenance;
    let current_gb = m.warn_below_free_bytes.div_ceil(1024 * 1024 * 1024);
    let gb = crate::webui::count_or_current(
        &form.warn_below_free_gb,
        current_gb.min(u64::from(u32::MAX)) as u32,
        1,
        10_000,
    );
    m.warn_below_free_bytes = u64::from(gb) * 1024 * 1024 * 1024;
    m.warn_below_free_percent = u64::from(crate::webui::count_or_current(
        &form.warn_below_free_percent,
        m.warn_below_free_percent.min(u64::from(u32::MAX)) as u32,
        0,
        99,
    ));

    let active = match cfg.torrent.max_active_torrents {
        -1 => "korlátlan".to_string(),
        n => format!("{n} db"),
    };
    let letoltes = if cfg.pieces.partial_download {
        "csak a lejátszott rész"
    } else {
        "a teljes fájl"
    };
    let summary = format!(
        "Mentve. Aktív torrentek: {active}, kapcsolatok: {} összesen és {} egy streamre, \
         figyelmeztetés {} GiB vagy {}% szabad hely alatt, letöltés: {letoltes}. A \
         kapcsolatszámok újraindulás után lépnek életbe.",
        cfg.torrent.global_connections_limit,
        cfg.torrent.connections_while_streaming,
        cfg.maintenance.warn_below_free_bytes / (1024 * 1024 * 1024),
        cfg.maintenance.warn_below_free_percent,
    );
    let message = match state.apply_config(cfg).await {
        Ok(()) => Some(summary),
        Err(e) => Some(format!("Nem sikerült menteni: {e}")),
    };
    settings_page(&state, message).await
}

/// Says back in words what was just saved, so the effect is visible without having to
/// work it out from the numbers.
pub(crate) fn describe_retention(m: &crate::config::Maintenance) -> String {
    if !m.enable_deletion {
        return "Mentve. Az automatikus törlés ki van kapcsolva, tehát semmi nem törlődik.".into();
    }
    let mut conditions: Vec<String> = Vec::new();
    if m.require_watched {
        conditions.push(format!(
            "{}%-ig megnézve",
            m.watched_position_percent
        ));
    }
    if m.hit_and_run {
        conditions.push("nincs rajta a tracker listáján".into());
        conditions.push(format!("{} napig seedelt", m.keep_seed_seconds / 86_400));
    }
    conditions.push(format!(
        "{} napnál régebbi",
        m.cache_retention_seconds / 86_400
    ));
    let (hour, minute) = m.sweep_time();
    format!(
        "Mentve. Minden nap {hour:02}:{minute:02}-kor törli azt, ami {}.",
        conditions.join(", ")
    )
}

/// Returns a page to send instead when the caller is not logged in.
pub(crate) async fn require_login(state: &AppState, cookies: Option<&str>) -> Option<Response> {
    match ui_state(state, cookies).await {
        UiAccess::LoggedIn => None,
        _ => Some(redirect("/ui")),
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct TomlForm {
    toml: String,
}

pub(crate) async fn ui_save_toml(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<TomlForm>,
) -> Response {
    if !matches!(
        ui_state(&state, cookie_header(&headers)).await,
        UiAccess::LoggedIn
    ) {
        return redirect("/ui");
    }

    // Parsed before anything is written, so a typo cannot destroy a working file.
    let mut parsed = match crate::webui::parse_config(&form.toml) {
        Ok(c) => c,
        Err(e) => {
            // The rejected text is shown back so the typo can be fixed in place, but
            // the retention boxes come from the saved config, which is still intact.
            let saved = state.config().await;
            let network = network_view(&state, &saved).await;
            return html(crate::webui::page(crate::webui::PageState::Settings {
                toml_text: form.toml,
                message: Some(format!("Nem mentettük el: {e}")),
                engine: crate::webui::EngineView::new(&saved.torrent, &saved.maintenance, &saved.pieces),
                retention: crate::webui::RetentionView::from_config(&saved.maintenance),
                network,
                bithumen_enabled: saved.bithumen.enabled,
            }));
        }
    };

    // The admin hash and api key are not on this form; losing them by editing the
    // file in the browser would lock the owner out of their own server.
    let current = state.config().await;
    if parsed.auth.password_hash.trim().is_empty() {
        parsed.auth.password_hash = current.auth.password_hash.clone();
    }
    if parsed.auth.api_key.trim().is_empty() {
        parsed.auth.api_key = current.auth.api_key.clone();
    }

    let message = match state.apply_config(parsed).await {
        Ok(()) => Some("Saved. Listen ports need a restart to take effect.".to_string()),
        Err(e) => Some(format!("Nem sikerült menteni: {e}")),
    };
    settings_page(&state, message).await
}

/// What the server is doing right now, in words.
///
/// Deliberately readable rather than a data dump: the questions it has to answer are
/// "is it playing", "is it downloading", "will it delete anything tonight" and "is the
/// certificate fine", and a person should be able to read the answers off the page.
///
/// No login required, and nothing secret on it: no API key, no credentials, no paths.
/// It is the page to look at when something is wrong, which is exactly when being
/// locked out of it would be least welcome.
pub(crate) async fn status(State(state): State<Arc<AppState>>) -> Response {
    let cfg = state.config().await;
    let now = crate::state::now();
    let mut out = String::new();

    out.push_str(&format!(
        "stremhu-rs {}, libtorrent {}\n\n",
        env!("CARGO_PKG_VERSION"),
        crate::engine::libtorrent_version()
    ));

    // Playing now.
    out.push_str("playing now\n");
    let open = state.lib.open().await;
    let mut any_reader = false;
    for (hash, entry) in &open {
        let readers = entry.reader_positions().await;
        if readers.is_empty() {
            continue;
        }
        any_reader = true;
        let stats = entry.stats();
        let furthest = readers.values().copied().max().unwrap_or(0);
        let through = if entry.span.last_piece > entry.span.first_piece {
            let span = (entry.span.last_piece - entry.span.first_piece) as f64;
            ((furthest.saturating_sub(entry.span.first_piece)) as f64 / span * 100.0) as u64
        } else {
            0
        };
        out.push_str(&format!(
            "  {}\n    {} at {through}% - {:.1} MB/s from {} peers, {} of {} on disk\n",
            entry.file_name,
            &hash[..12.min(hash.len())],
            stats.download_rate as f64 / 1e6,
            stats.num_peers,
            crate::webui::human_size(entry.downloaded_bytes().await),
            crate::webui::human_size(entry.file_len),
        ));
    }
    if !any_reader {
        out.push_str("  nothing\n");
    }

    // Open but idle, which is what seeding looks like.
    out.push_str("\nopen torrents\n");
    if open.is_empty() {
        out.push_str("  none\n");
    }
    for (hash, entry) in &open {
        let stats = entry.stats();
        let front = entry.contiguous_front().await;
        out.push_str(&format!(
            "  {}  {}\n    {} of {} on disk, playable from the start for {}, \
             down {:.1} MB/s up {:.1} MB/s, {} peers\n",
            &hash[..12.min(hash.len())],
            entry.file_name,
            crate::webui::human_size(entry.downloaded_bytes().await),
            crate::webui::human_size(entry.file_len),
            crate::webui::human_size((front as u64 * entry.piece_len).min(entry.file_len)),
            stats.download_rate as f64 / 1e6,
            stats.upload_rate as f64 / 1e6,
            stats.num_peers,
        ));
    }

    // Retention.
    out.push_str("\ndeletion\n");
    let last_sweep = state.store.last_sweep_date().await;
    if cfg.maintenance.enable_deletion {
        out.push_str(&format!(
            "  runs {}\n",
            crate::maintenance::next_run_label(&cfg.maintenance, &last_sweep)
        ));
    } else {
        out.push_str("  switched off, nothing is ever removed automatically\n");
    }
    let items = state.store.items().await;
    let kept = items.iter().filter(|i| i.keep).count();
    let watched = items
        .iter()
        .filter(|i| {
            i.watched(
                cfg.maintenance.watched_position_percent,
                cfg.maintenance.watched_min_served_percent,
            )
        })
        .count();
    out.push_str(&format!(
        "  {} download(s) on record, {watched} watched, {kept} marked to keep\n",
        items.len()
    ));

    // Seeding obligations.
    let owed = state.owed.read().await.clone();
    out.push_str("\nseeding obligations\n");
    match (&owed.fetched_at, &owed.error) {
        (None, _) => out.push_str("  the tracker has not been asked yet\n"),
        (Some(at), Some(err)) => out.push_str(&format!(
            "  unknown: {err} (tried {})\n  nothing will be deleted while this is the case\n",
            crate::webui::human_ago(now.saturating_sub(*at))
        )),
        (Some(at), None) => {
            out.push_str(&format!(
                "  {} open, read {}\n",
                owed.entries.len(),
                crate::webui::human_ago(now.saturating_sub(*at))
            ));
            for e in &owed.entries {
                let remaining = e
                    .remaining_secs
                    .map(crate::webui::human_duration)
                    .unwrap_or_else(|| "unknown".to_string());
                out.push_str(&format!("    {} - {remaining} left\n", e.name));
            }
        }
    }

    // HTTPS.
    out.push_str("\nhttps\n");
    match state.https_host.read().await.clone() {
        Some(host) => {
            let cert = crate::tls::Cache::new(&cfg.network.cert_cache_dir).load();
            match cert.and_then(|c| c.expires_in(now)) {
                Some(secs) if secs > 0 => out.push_str(&format!(
                    "  serving as {host} on port {}\n  certificate good for {} more days, \
                     renews {} days before expiry\n",
                    cfg.network.https_port,
                    secs / 86_400,
                    cfg.network.cert_renew_margin_days
                )),
                _ => out.push_str(&format!(
                    "  serving as {host} on port {}, certificate expiry unknown\n",
                    cfg.network.https_port
                )),
            }
        }
        None if !cfg.network.enable_https => out.push_str("  switched off\n"),
        None => out.push_str("  not running; other devices cannot connect\n"),
    }

    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        out,
    )
        .into_response()
}
