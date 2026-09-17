//! What every request handler shares, and how it reaches the rest of the program.
//!
//! Split out of the server so the handlers can be read without wading through state
//! plumbing, and so the maintenance sweep's view of the world lives next to the state it
//! reads rather than in the middle of the HTTP layer.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::RwLock;

use crate::config::Config;
use crate::library::Library;
use crate::ncore::NcoreClient;


pub(crate) struct AppState {
    pub(crate) lib: Arc<Library>,
    /// Behind locks because the web interface can change credentials while the server
    /// runs, and the clients then have to be rebuilt rather than restarted.
    pub(crate) ncore: RwLock<NcoreClient>,
    /// The second tracker. None while it is switched off or has no credentials, and then it is
    /// never contacted: not for a search, not for a login.
    pub(crate) bithumen: RwLock<Option<crate::bithumen::BithumenClient>>,
    /// None when no API key is configured; TMDB ids cannot be resolved then.
    pub(crate) tmdb: RwLock<Option<crate::tmdb::TmdbClient>>,
    /// One shared handle, not a copy per owner. The library holds this same lock, so a
    /// setting saved in the interface is in force everywhere by the next tick.
    pub(crate) cfg: crate::config::Shared,
    pub(crate) cfg_path: std::path::PathBuf,
    /// Bumped on every save so the background loops know to re-read the configuration
    /// without cloning it on every pass.
    pub(crate) cfg_generation: Arc<std::sync::atomic::AtomicU64>,
    /// The message from the last action, waiting for the page that follows it.
    ///
    /// Held here rather than passed through the address bar. An action finishes by sending the
    /// browser back to the page it came from, which leaves the address exactly as it was and
    /// makes a refresh harmless; the sentence about what happened has to survive that trip
    /// somehow, and the address is the one place it must not travel, because anything put
    /// there stays there and gets repeated on every reload.
    ///
    /// Keyed by the session, taken by the first page that asks, and dropped. One at a time per
    /// session is enough: nobody performs two actions before reading the answer to the first.
    pub(crate) flash: tokio::sync::Mutex<std::collections::HashMap<String, String>>,
    /// How many requests have arrived, ever.
    ///
    /// Only the heartbeat reads it, and only to answer one question: when somebody says the
    /// television stopped working, did its requests reach this program at all? A silent log
    /// cannot tell "nobody asked" from "asked and got nothing", and those two have completely
    /// different causes.
    pub(crate) requests: std::sync::atomic::AtomicU64,
    /// The recommended catalogue, rebuilt once a day. Empty until the first build, so a
    /// viewer who never browses costs nothing.
    pub(crate) catalog: crate::catalog::Cache,
    /// Search results by title, kept for a few minutes so an evening on one series does not
    /// walk the ladder again for every episode.
    pub(crate) searches: tokio::sync::Mutex<std::collections::HashMap<String, CachedSearch>>,
    pub(crate) ui: crate::webui::Ui,
    /// What was downloaded and how much of it was watched. Survives restarts.
    pub(crate) store: Arc<crate::state::Store>,
    /// The hostname the TLS listener came up as, or None when HTTPS is not running.
    /// Read by the settings page so it offers a URL that actually works.
    pub(crate) https_host: RwLock<Option<String>>,
    /// The last look at the download folders. None until the first check runs.
    pub(crate) disks: RwLock<Option<crate::disk::Report>>,
    /// The last answer from the tracker about open seeding obligations, with the time
    /// it was fetched. Cached deliberately: this is a private tracker, and asking it
    /// once per page view would be unnecessary traffic against the account.
    pub(crate) owed: RwLock<OwedSnapshot>,
    /// The second tracker's answer, as torrent ids and when they were read. None while it has
    /// never been asked.
    ///
    /// Kept for the same reason as the first tracker's: so the page can say what the sweep
    /// would do rather than a cautious guess. Without it a BitHUmen download read as "seeding
    /// needed" for ever, including after the tracker had let it go, and a page that disagrees
    /// with the behaviour is worse than a page that says nothing.
    pub(crate) owed_bithumen: RwLock<Option<(crate::state::Unix, Vec<String>)>>,
    /// When each kind of warning was last pushed, so a repeated condition is reported
    /// without being reported on every request.
    pub(crate) last_notice: RwLock<HashMap<String, crate::state::Unix>>,
}

/// A source the stream list offered.
///
/// The size travels with the URL because the disk a download goes to depends on it, and the
/// tracker tells us the size at search time. Asking libtorrent instead would mean adding the
/// torrent first, which is after the folder has been chosen.
#[derive(Debug, Clone)]
pub(crate) struct Source {
    /// Which tracker offered it, and therefore which session can fetch the `.torrent`.
    pub(crate) tracker: crate::tracker::Tracker,
    pub(crate) download_url: String,
    pub(crate) size_bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct OwedSnapshot {
    pub(crate) fetched_at: Option<crate::state::Unix>,
    pub(crate) entries: Vec<crate::ncore::HitAndRun>,
    pub(crate) error: Option<String>,
}

/// Gives the sweep access to the tracker, the running torrents and the disk.
pub(crate) struct ServerWorld {
    pub(crate) state: Arc<AppState>,
}

impl crate::maintenance::World for ServerWorld {
    async fn settings(&self) -> crate::config::Maintenance {
        self.state.cfg.read().await.maintenance.clone()
    }

    async fn torrent_files_dir(&self) -> String {
        self.state.cfg.read().await.storage.torrent_files_dir.clone()
    }

    /// Asks the trackers, and caches nCore's answer for the interface to show.
    ///
    /// nCore failing abandons the whole round, as before: it is the tracker almost everything
    /// came from. BitHUmen failing does not, but then it is not reported as asked either, and
    /// its downloads are left alone for this round — which is the same rule, applied to the
    /// tracker it belongs to.
    async fn owed(&self) -> Result<crate::maintenance::Owed> {
        let mut owed = crate::maintenance::Owed::default();
        // Only the trackers something on the disk actually came from. A site we hold nothing
        // from cannot change any outcome here, and the sweep runs every evening: asking it
        // anyway would be a daily login and page fetch against a private account for nothing.
        let ask = crate::maintenance::trackers_to_ask(&self.state.store.items().await);

        if ask.contains(&crate::tracker::Tracker::Ncore) {
            let entries = self.state.refresh_owed().await?;
            owed.asked.push(crate::tracker::Tracker::Ncore);
            owed.keys.extend(
                entries
                    .into_iter()
                    .map(|e| crate::tracker::Tracker::Ncore.owed_key(&e.torrent_id)),
            );
        }

        if ask.contains(&crate::tracker::Tracker::Bithumen) {
            match self.state.bithumen.read().await.as_ref() {
                Some(client) => match client.hit_and_run().await {
                    Ok(entries) => {
                        owed.asked.push(crate::tracker::Tracker::Bithumen);
                        owed.keys.extend(
                            entries
                                .iter()
                                .map(|(id, _)| crate::tracker::Tracker::Bithumen.owed_key(id)),
                        );
                        self.state.store_bithumen_answer(entries).await;
                    }
                    Err(e) => tracing::warn!(
                        error = %e,
                        "could not read BitHUmen's hit and run list; its downloads stay put"
                    ),
                },
                // Downloads from a tracker that is now switched off. Nothing here can say
                // whether they still owe seeding, so the sweep leaves them alone.
                None => tracing::warn!(
                    "there are BitHUmen downloads but the tracker is switched off;                      they stay put until it is switched back on"
                ),
            }
        }
        Ok(owed)
    }

    async fn streaming_hashes(&self) -> Vec<String> {
        self.state.lib.streaming_hashes().await
    }

    async fn delete_downloads(&self, items: &[crate::state::Item]) -> Vec<String> {
        self.state.delete_downloads(items).await
    }

    async fn check_disk_space(&self) {
        self.state.check_disk_space().await;
    }

    async fn disk_lines(&self) -> Vec<String> {
        self.state
            .disks
            .read()
            .await
            .as_ref()
            .map(|r| r.lines.clone())
            .unwrap_or_default()
    }

    async fn notify(&self, message: &str) {
        // Not throttled: this is one message a day about a job that ran, not a warning about a
        // condition that persists.
        self.state.notify(message).await;
    }
}

/// Frees room for a download that will not fit, before anything else is decided.
///
/// Returns whether a round ran. The caller does not need the answer, but a test does, and so
/// does the log line that explains why a stream took a few seconds longer to start than usual.
///
/// The round is the ordinary one, with the ordinary rules: an obligation to the tracker still
/// outranks free space, and a file being watched is still never touched. What this changes is
/// only when it happens. Deleting in the evening what could have been deleted at noon is what
/// pushes a download onto the second disk, or refuses it, while the first disk is full of files
/// that had already served their time.
pub(crate) async fn make_room_for(state: &Arc<AppState>, dir: &str, needed: u64) -> bool {
    let cfg = state.config().await;
    let Ok(space) = crate::disk::space_for(std::path::Path::new(dir)) else {
        // Unreadable folder: the caller's own check reports that properly a moment later.
        return false;
    };
    let since = crate::state::now().saturating_sub(state.store.last_sweep_at().await);
    if !crate::maintenance::sweep_before_download(
        &cfg.maintenance,
        space.free_bytes,
        needed,
        since,
    ) {
        return false;
    }

    tracing::warn!(
        folder = dir,
        free = space.free_bytes,
        needed,
        "not enough room; running a deletion round before the download"
    );
    // Written down before the round rather than after, so a second request arriving while this
    // one is still working the tracker does not start its own.
    state.store.set_last_sweep_at(crate::state::now()).await;
    let world = ServerWorld {
        state: state.clone(),
    };
    crate::maintenance::run_once(
        &world,
        &state.store,
        "Kevés a hely, takarítás a letöltés előtt",
    )
    .await;
    true
}

/// Builds the recommended catalogue when it is due, and says so when it could not.
///
/// Due means either never built or a day old. Called at startup and again from the daily
/// sweep, which covers the machine that is left running for a week: the startup path never
/// fires and the sweep is what keeps the shelf current.
///
/// On failure, exactly one retry an hour later, and a notification. Not an hourly loop: a
/// tracker that is down stays down for longer than an hour, and a message every hour until it
/// comes back is a worse problem than a stale catalogue.
pub(crate) async fn build_catalog_if_due(state: &Arc<AppState>, trigger: &str) {
    if !state.catalog.is_stale(crate::state::now()).await {
        return;
    }
    match crate::catalog::refresh(state).await {
        Ok(()) => tracing::info!(trigger, "recommended catalogue built"),
        Err(e) => {
            let message = format!("Az Ajánló katalógus nem készült el: {e:#}");
            tracing::warn!("{message}");
            if state.config().await.maintenance.notify_problems {
                state.notify_occasionally("catalog", &message).await;
            }
            // One retry, and only one. Spawned so a slow tracker cannot hold up a startup or a
            // sweep, and the flag is left unset so the retry still sees the work as due.
            let state = state.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                if !state.catalog.is_stale(crate::state::now()).await {
                    return;
                }
                match crate::catalog::refresh(&state).await {
                    Ok(()) => tracing::info!("the recommended catalogue was built on the retry"),
                    Err(e) => tracing::warn!(
                        error = %format!("{e:#}"),
                        "the recommended catalogue failed again; waiting for the next round"
                    ),
                }
            });
        }
    }
}

/// Says out loud, every few minutes, that the program is alive and whether anybody is asking
/// it for anything.
///
/// This exists because of a real evening: the television stopped being served, the same
/// address worked from a browser on the same network, and the log for the whole period was
/// empty. Empty because nothing was wrong that this program noticed, and an idle server writes
/// nothing at all — so there was no way to tell whether the requests were arriving and failing
/// or never arriving. A counted heartbeat separates those two, which is the first question and
/// the one that decides where to look next.
pub(crate) fn spawn_heartbeat(state: Arc<AppState>) {
    const EVERY: std::time::Duration = std::time::Duration::from_secs(300);
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let mut previous = 0u64;
        loop {
            tokio::time::sleep(EVERY).await;
            let total = state.requests.load(std::sync::atomic::Ordering::Relaxed);
            report(&state, started.elapsed().as_secs() / 60, total, total - previous).await;
            previous = total;
        }
    });
}

/// One line saying where the program's memory has gone, broken down by what is holding it.
///
/// The engine is a library inside this process, so nothing the operating system reports can
/// separate its memory from ours; asking it directly is the only way. That distinction is the
/// whole point of this line. If the engine's own buffers are large, the remedy is its buffer
/// settings. If they are small while the process is large, the memory is in mapped file pages
/// or the system's cache, and the remedy is the write mode instead. Without the breakdown the
/// two look identical and there is nothing to do but guess.
pub(crate) async fn report(state: &Arc<AppState>, uptime_min: u64, total: u64, since: u64) {
    let (_, working_set, _) = crate::alerts::usage(0, std::time::Duration::from_secs(1));

    // Ours: what this program is holding on its own account.
    let torrents = state.lib.open().await.len();
    let searches = state.searches.lock().await.len();
    let sources = state.store.source_count().await;
    let catalog = state.catalog.rows(crate::catalog::Kind::Film).await.len()
        + state.catalog.rows(crate::catalog::Kind::Series).await.len();

    // The engine's: asked of it, because it cannot be measured from outside.
    let engine = tokio::task::spawn_blocking({
        let lib = state.lib.clone();
        move || lib.engine_stats()
    })
    .await
    .ok()
    .flatten();

    let mb = |bytes: i64| -> i64 {
        if bytes < 0 { -1 } else { bytes / (1024 * 1024) }
    };

    match engine {
        Some(e) => tracing::info!(
            uptime_min,
            requests_total = total,
            requests_since = since,
            process_mb = working_set / (1024 * 1024),
            engine_buffers_mb = mb(e.disk_buffer_bytes),
            engine_queued_write_mb = mb(e.queued_write_bytes),
            engine_disk_jobs = e.queued_disk_jobs,
            engine_blocked_jobs = e.blocked_disk_jobs,
            engine_threads = e.running_threads,
            engine_peers = e.peers_connected,
            engine_downloading = e.downloading_torrents,
            engine_seeding = e.seeding_torrents,
            engine_checking = e.checking_torrents,
            ours_torrents = torrents,
            ours_searches = searches,
            ours_sources = sources,
            ours_catalog = catalog,
            "heartbeat"
        ),
        None => tracing::info!(
            uptime_min,
            requests_total = total,
            requests_since = since,
            process_mb = working_set / (1024 * 1024),
            ours_torrents = torrents,
            ours_searches = searches,
            ours_sources = sources,
            ours_catalog = catalog,
            "heartbeat (the engine did not answer)"
        ),
    }
}

/// Keeps the recommended catalogue current: once at startup, and once a day after that.
///
/// Two triggers, and the second exists because of the first. A machine that is restarted every
/// few days is served entirely by the startup build; a machine left running for a fortnight
/// would never build again, so the daily one picks it up at the same hour the sweep runs. That
/// hour is deliberate rather than convenient: it is already the time of day this program does
/// its housekeeping, and it is not a time anybody is likely to be watching.
pub(crate) fn spawn_catalog_builder(state: Arc<AppState>) {
    tokio::spawn(async move {
        // After the torrents are back and the first announce has gone out. The catalogue is
        // the least urgent thing this program does and must not compete with a start.
        tokio::time::sleep(std::time::Duration::from_secs(20)).await;
        build_catalog_if_due(&state, "startup").await;

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            let cfg = state.config().await;
            let (hour, minute) = cfg.maintenance.sweep_time();
            let now = chrono::Local::now();
            use chrono::Timelike;
            if (now.hour(), now.minute()) < (hour, minute) {
                continue;
            }
            // `is_stale` inside this is what makes the minute-by-minute check harmless: past
            // the appointed time it is asked sixty times an hour and answers no until the
            // catalogue is actually a day old.
            build_catalog_if_due(&state, "daily").await;
        }
    });
}

/// The BitHUmen client, or None when the tracker must not be contacted at all.
///
/// Switched off or without credentials means no client, and no client means no request: not a
/// search, not even a login. A private site has nothing to gain from an unauthenticated visit
/// from an address that also holds a real account.
pub(crate) fn bithumen_client(
    cfg: &crate::config::Bithumen,
) -> Option<crate::bithumen::BithumenClient> {
    if !cfg.enabled || cfg.username.trim().is_empty() || cfg.password.is_empty() {
        return None;
    }
    match crate::bithumen::BithumenClient::new(&cfg.username, &cfg.password) {
        Ok(client) => Some(client),
        Err(e) => {
            tracing::warn!(error = %e, "cannot build the BitHUmen client");
            None
        }
    }
}

impl AppState {
    /// Reads the second tracker's list, if there is anything of ours on it to read about.
    ///
    /// Called from the button on the downloads page, so the traffic is something the owner
    /// asked for; it is still gated on actually holding a download from there, because a page
    /// view must not turn into a visit to a site we took nothing from.
    pub(crate) async fn refresh_owed_bithumen(&self) -> Option<Result<usize>> {
        let items = self.store.items().await;
        if !crate::maintenance::trackers_to_ask(&items)
            .contains(&crate::tracker::Tracker::Bithumen)
        {
            return None;
        }
        let guard = self.bithumen.read().await;
        let client = guard.as_ref()?;
        match client.hit_and_run().await {
            Ok(entries) => Some(Ok(self.store_bithumen_answer(entries).await)),
            Err(e) => Some(Err(e)),
        }
    }

    /// Writes down what BitHUmen said, in memory and in the state file. Returns how many
    /// obligations are open.
    ///
    /// Recorded against the downloads, not only kept in memory, for the same reason nCore's
    /// answer is: the memory is gone at the next restart, and until the evening sweep runs the
    /// page would have nothing to show but "not asked". The record survives, so it can say
    /// what was true when it was read and how long ago that was.
    ///
    /// The remaining time comes with it, the site's own "Hátravan" column, but no transfer
    /// totals: there is nothing to run the ratio arithmetic on, so those downloads fall back to
    /// the flat seeding time, which is the cautious side.
    async fn store_bithumen_answer(&self, entries: Vec<(String, Option<u64>)>) -> usize {
        let count = entries.len();
        let now = crate::state::now();
        self.store
            .record_obligations(crate::tracker::Tracker::Bithumen, &entries, now)
            .await;
        let _ = self.store.flush().await;
        *self.owed_bithumen.write().await =
            Some((now, entries.into_iter().map(|(id, _)| id).collect()));
        count
    }

    /// A snapshot, so no handler holds the lock while doing network work.
    pub(crate) async fn config(&self) -> Config {
        self.cfg.read().await.clone()
    }

    /// Persists a new configuration and rebuilds what depends on it.
    pub(crate) async fn apply_config(&self, mut new: Config) -> Result<()> {
        new.apply_env_overrides();
        new.save(&self.cfg_path)?;

        let ncore = NcoreClient::new(&new.ncore.username, &new.ncore.password)?;
        if !new.ncore.username.is_empty() {
            // Not fatal: wrong credentials should show up as a failed search, not as a
            // refusal to save the settings page.
            if let Err(e) = ncore.login().await {
                tracing::warn!(error = %e, "nCore login failed with the new credentials");
            }
        }
        let tmdb = crate::tmdb::TmdbClient::new(&new.tmdb.api_key, &new.tmdb.language).ok();
        // Rebuilt from the saved settings, so switching the second tracker on or off in the
        // interface takes effect without a restart — including switching it off, which has to
        // drop the session rather than leave it usable.
        let bithumen = bithumen_client(&new.bithumen);

        *self.bithumen.write().await = bithumen;
        *self.ncore.write().await = ncore;
        *self.tmdb.write().await = tmdb;
        *self.cfg.write().await = new;
        self.cfg_generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::info!("configuration saved and clients rebuilt");
        Ok(())
    }
}

/// Everything known about a title that a tracker can be asked with.
///
/// Both handles at once, not one or the other. The IMDb id is the exact one and is tried
/// first, but a search that finds nothing on it is not the end: nCore carries no IMDb id at
/// all on many Hungarian uploads, and the title still finds them. The old shape chose one of
/// the two up front and could never fall back, which is why an IMDb search that came up short
/// simply lost.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SearchPlan {
    /// Exact match, when the work has an IMDb entry: no false positives.
    pub(crate) imdb: Option<String>,
    /// Titles to try in order. Many Hungarian series have no IMDb entry at all, and for the
    /// rest these are the second chance when the id finds nothing.
    pub(crate) names: Vec<String>,
}

impl SearchPlan {
    pub(crate) fn is_empty(&self) -> bool {
        self.imdb.is_none() && self.names.is_empty()
    }

    /// What this plan is filed under in the search cache. The title, not the episode: the
    /// whole series comes back in one answer and every episode is served from it.
    pub(crate) fn cache_key(&self) -> String {
        match &self.imdb {
            Some(imdb) => format!("imdb:{imdb}"),
            None => format!("name:{}", self.names.join("|").to_lowercase()),
        }
    }
}

/// A search result kept for a moment, so a series being watched is not searched for again on
/// every episode.
pub(crate) struct CachedSearch {
    pub(crate) torrents: Vec<crate::tracker::Torrent>,
    /// Refreshed on every hit, not set once. An evening spent on one series keeps its list
    /// warm for the whole evening; a title opened once and abandoned falls out on its own.
    pub(crate) touched: std::time::Instant,
}

/// How long a search result stays usable without being asked for again.
///
/// Ten minutes, refreshed on every use. This exists because the ladder is no longer one
/// request: a title that is genuinely absent walks four rungs, and Stremio asks again for
/// every episode. It is short enough that a release uploaded during the evening is found by
/// the time the next episode is reached.
const SEARCH_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(600);

/// How many titles are kept. A household watches a handful of things at once.
const SEARCH_CACHE_LIMIT: usize = 64;


impl AppState {
    /// Keyed by the play id, which carries the tracker: both sites number their torrents
    /// from one, so `12345` on its own would let one tracker's cached URL answer for the
    /// other's release.
    pub(crate) async fn remember_source(
        &self,
        tracker: crate::tracker::Tracker,
        torrent_id: &str,
        download_url: &str,
        size_bytes: u64,
    ) {
        self.store
            .remember_source(
                &tracker.play_id(torrent_id),
                tracker.id(),
                download_url,
                size_bytes,
            )
            .await;
    }

    /// Leaves a message for the page the browser is about to be sent to.
    pub(crate) async fn set_flash(&self, session: &str, message: impl Into<String>) {
        if session.is_empty() {
            return;
        }
        self.flash
            .lock()
            .await
            .insert(session.to_string(), message.into());
    }

    /// Takes that message, once.
    pub(crate) async fn take_flash(&self, session: &str) -> Option<String> {
        if session.is_empty() {
            return None;
        }
        self.flash.lock().await.remove(session)
    }

    /// The remembered result for this plan, when it is still warm.
    ///
    /// Only whole-title results are cached, so this is keyed by the plan and not by the
    /// episode. The ranking and filtering happen after this returns, which means a setting
    /// changed in the interface takes effect at once rather than waiting for the entry to age
    /// out.
    pub(crate) async fn cached_search(
        &self,
        plan: &SearchPlan,
    ) -> Option<Vec<crate::tracker::Torrent>> {
        let mut map = self.searches.lock().await;
        let key = plan.cache_key();
        let entry = map.get_mut(&key)?;
        if entry.touched.elapsed() > SEARCH_CACHE_TTL {
            map.remove(&key);
            return None;
        }
        entry.touched = std::time::Instant::now();
        Some(entry.torrents.clone())
    }

    /// Remembers a search that actually found something.
    ///
    /// An empty result is deliberately not kept. "Nothing was found" is the one answer worth
    /// asking again for, because the reason may simply be that nobody had uploaded it yet.
    pub(crate) async fn cache_search(
        &self,
        plan: &SearchPlan,
        torrents: &[crate::tracker::Torrent],
    ) {
        if torrents.is_empty() {
            return;
        }
        let mut map = self.searches.lock().await;
        // Added to what is there, not put in its place.
        //
        // A series is searched for one episode at a time, and different episodes can be
        // answered by different trackers. Replacing would mean each answer threw away the one
        // before it, so watching two episodes in a row would search twice for the same series
        // and end up remembering only half of what it had found.
        let key = plan.cache_key();
        if let Some(existing) = map.get_mut(&key) {
            let known: std::collections::HashSet<String> = existing
                .torrents
                .iter()
                .map(|t| t.tracker.owed_key(&t.torrent_id))
                .collect();
            existing.torrents.extend(
                torrents
                    .iter()
                    .filter(|t| !known.contains(&t.tracker.owed_key(&t.torrent_id)))
                    .cloned(),
            );
            existing.touched = std::time::Instant::now();
            return;
        }
        // Oldest first, one at a time. Emptying the map would make every open series pay for a
        // fresh ladder at the same moment.
        while map.len() >= SEARCH_CACHE_LIMIT {
            let Some(oldest) = map
                .iter()
                .min_by_key(|(_, e)| e.touched)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            map.remove(&oldest);
        }
        map.insert(
            plan.cache_key(),
            CachedSearch {
                torrents: torrents.to_vec(),
                touched: std::time::Instant::now(),
            },
        );
    }

    /// The open torrent for this request, if there already is one.
    ///
    /// The point of this is what it avoids. Without it every single range request re-fetched the
    /// .torrent from nCore before serving a byte: a network round trip and up to a quarter of a
    /// megabyte in front of every seek and every rebuffer, and dozens of downloads from the
    /// tracker for one film. The records already hold what is needed to recognise the torrent
    /// by the tracker's own id, and the torrent is already open, so a repeat request needs
    /// nothing from the network, nothing from the disks and no parsing.
    ///
    /// Works after a restart as well, because the records are on disk and the torrents are
    /// reopened from them at startup.
    pub(crate) async fn already_open(
        &self,
        torrent_id: &str,
        want: &crate::library::Want,
    ) -> Option<std::sync::Arc<crate::library::Entry>> {
        for key in self.store.keys_for_tracker_id(torrent_id).await {
            // Not open is not a reason to stop looking: a season pack can have several
            // records under one tracker id, and only one of them is what was asked for.
            let Some(entry) = self.lib.get(&key).await else {
                continue;
            };
            let largest = entry
                .files
                .iter()
                .max_by_key(|f| f.size)
                .map(|f| f.index == entry.selected)
                .unwrap_or(true);
            if crate::library::serves(want, &entry.file_name, largest) {
                return Some(entry);
            }
        }
        None
    }

    /// Where a play id can be fetched from.
    ///
    /// Read out of the store, so it survives a restart. Stremio opens a remembered play URL
    /// without re-asking for the stream list, and every one of those used to fail after an
    /// update because this lived only in memory.
    pub(crate) async fn source_for(&self, torrent_id: &str) -> Option<Source> {
        let stored = self.store.source(torrent_id).await?;
        Some(Source {
            tracker: crate::tracker::Tracker::from_id(&stored.tracker),
            download_url: stored.download_url,
            size_bytes: stored.size_bytes,
        })
    }

    /// Fetches the tracker's list of open seeding obligations and caches it.
    ///
    /// The cache is what the interface shows, so a page view costs nothing. The error
    /// is cached too: "we could not ask" is information, and it is the state in which
    /// nothing may be deleted.
    pub(crate) async fn refresh_owed(&self) -> Result<Vec<crate::ncore::HitAndRun>> {
        // The full list first, because it is what tells us which torrents the tracker knows at
        // all. Its figures are the evidence behind a "nothing owed" answer; without them the
        // short list's silence could equally mean the tracker has not got round to the torrent.
        //
        // A failure here is not fatal: the short list below is what decides deletions, and
        // recording fewer figures only makes the rules more cautious.
        match self.ncore.read().await.hit_and_run_all().await {
            Ok(all) => {
                let now = crate::state::now();
                for e in &all {
                    self.store
                        .record_tracker_figures(
                            crate::tracker::Tracker::Ncore,
                            &e.torrent_id,
                            e.uploaded_bytes,
                            e.downloaded_bytes,
                            &e.ratio,
                            now,
                        )
                        .await;
                }
                tracing::info!(count = all.len(), "the tracker's full activity list was read");
            }
            Err(e) => tracing::warn!(error = %e, "could not read the tracker's full list"),
        }

        let result = self.ncore.read().await.hit_and_run().await;
        let mut snapshot = self.owed.write().await;
        snapshot.fetched_at = Some(crate::state::now());
        match result {
            Ok(entries) => {
                snapshot.entries = entries.clone();
                snapshot.error = None;
                drop(snapshot);
                // The same page carries each torrent's transfer figures, so one request
                // answers both "may I delete this" and "how much have I given back".
                let now = crate::state::now();
                for e in &entries {
                    self.store
                        .record_tracker_figures(
                            crate::tracker::Tracker::Ncore,
                            &e.torrent_id,
                            e.uploaded_bytes,
                            e.downloaded_bytes,
                            &e.ratio,
                            now,
                        )
                        .await;
                }
                // Recorded against every download, not only the ones on the list: what the
                // interface has to show is which torrents still owe seeding, and for that the
                // answer "no" has to be stored too.
                let owed: Vec<(String, Option<u64>)> = entries
                    .iter()
                    .map(|e| (e.torrent_id.clone(), e.remaining_secs))
                    .collect();
                self.store
                    .record_obligations(crate::tracker::Tracker::Ncore, &owed, now)
                    .await;
                let _ = self.store.flush().await;
                Ok(entries)
            }
            Err(e) => {
                // The previous list is kept: a stale answer still protects torrents,
                // whereas an empty one would expose them.
                snapshot.error = Some(e.to_string());
                Err(e)
            }
        }
    }

    /// Looks at the download folders, records what it found, and notifies when short.
    ///
    /// The result is kept so the interface can show it without touching the disks on every
    /// page view, and so the notification can be sent once per day rather than repeatedly:
    /// a warning that arrives every few minutes stops being read.
    pub(crate) async fn check_disk_space(&self) {
        let cfg = self.config().await;
        let report = crate::disk::report(
            &cfg.torrent.save_path,
            &cfg.torrent.save_path_secondary,
            cfg.maintenance.warn_below_free_bytes,
            cfg.maintenance.warn_below_free_percent,
        );

        for line in &report.lines {
            match report.low {
                true => tracing::warn!("disk: {line}"),
                false => tracing::info!("disk: {line}"),
            }
        }

        // At most one push every six hours while it stays low, and a fresh one the next time
        // it drops after recovering. This check runs whenever a download starts, and a
        // warning that arrives on every film stops being read; one that never arrives until
        // tomorrow evening is no warning at all.
        if report.low && cfg.maintenance.notify_disk {
            self.notify_occasionally("low-space", &report.summary).await;
        } else {
            // Recovered, so the next time it drops is news again.
            self.last_notice.write().await.remove("low-space");
        }
        *self.disks.write().await = Some(report);
    }

    /// Sends a message at most once every six hours per kind.
    ///
    /// The conditions worth a push are all conditions that persist: a full disk stays full, and
    /// a player asked to stream from it retries several times a second. Sending each one turns a
    /// warning into noise, and a warning nobody reads is the same as no warning.
    pub(crate) async fn notify_occasionally(&self, kind: &str, message: &str) {
        const INTERVAL: u64 = 6 * 3600;
        let now = crate::state::now();
        {
            let mut last = self.last_notice.write().await;
            if let Some(at) = last.get(kind)
                && now.saturating_sub(*at) < INTERVAL {
                    return;
                }
            last.insert(kind.to_string(), now);
        }
        self.notify(message).await;
    }

    /// Sends a message somewhere the owner will see it.
    ///
    /// Nothing leaves this machine unless a destination was configured. The interface and
    /// the log always carry the warning regardless, so an unset webhook loses nothing but
    /// the push.
    pub(crate) async fn notify(&self, message: &str) {
        let url = self.config().await.maintenance.notify_webhook_url;
        if url.trim().is_empty() {
            return;
        }
        let http = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "could not build the notification client");
                return;
            }
        };
        let url = url.trim();
        // Discord wants a JSON object with a `content` field and refuses a plain body with a
        // 400; ntfy and most of the others want exactly the plain body. Sending the wrong
        // shape fails silently from the owner's point of view, which for a warning about a
        // filling disk is the worst possible outcome, so the shape follows the destination.
        let request = if crate::disk::is_discord_webhook(url) {
            http.post(url).json(&serde_json::json!({
                "content": format!("**stremhu-rs**\n{message}")
            }))
        } else {
            http.post(url)
                .header("Title", "stremhu-rs")
                .body(message.to_string())
        };

        match request.send().await {
            Ok(res) if res.status().is_success() => tracing::info!("notification sent"),
            Ok(res) => tracing::warn!(status = %res.status(), "the notification was refused"),
            Err(e) => tracing::warn!(error = %e, "could not send the notification"),
        }
    }

    /// Removes several downloads, one recheck per torrent, and reports what actually went.
    pub(crate) async fn delete_downloads(&self, items: &[crate::state::Item]) -> Vec<String> {
        let keys: Vec<String> = items.iter().map(|i| i.key()).collect();
        self.lib.remove_files(&keys, true).await;

        // Read once. Asking per item meant cloning every record, every coverage map included,
        // once for each file being deleted.
        let survivors = self.store.items().await;

        // Whatever is no longer open counts as gone: remove_files logs its own failures, and a
        // file still being served is a file that did not leave the disk.
        let mut removed = Vec::new();
        for (item, key) in items.iter().zip(keys.iter()) {
            if self.lib.get(key).await.is_some() {
                continue;
            }
            removed.push(key.clone());
            // The .torrent is shared, so it only goes with the last file that needed it.
            let others = survivors
                .iter()
                .filter(|other| {
                    other.info_hash == item.info_hash && !keys.contains(&other.key())
                })
                .count();
            if others == 0 && !item.torrent_file.is_empty()
                && let Err(e) = std::fs::remove_file(&item.torrent_file)
                    && e.kind() != std::io::ErrorKind::NotFound {
                        tracing::warn!(
                            path = %item.torrent_file,
                            error = %e,
                            "could not remove the .torrent"
                        );
                    }
        }
        removed
    }

    /// Removes a download: out of the torrent session, off the disk, and its
    /// `.torrent` with it.
    pub(crate) async fn delete_download(&self, item: &crate::state::Item) -> Result<()> {
        self.lib.remove_file(&item.key(), true).await?;

        // The .torrent is shared by every file served out of it, so it only goes when the last
        // one does. Deleting it while a pack still has episodes on disk would mean those cannot
        // be re-opened after a restart, and they would stop seeding without anybody asking.
        let others = self
            .store
            .items()
            .await
            .iter()
            .filter(|other| other.info_hash == item.info_hash && other.key() != item.key())
            .count();
        if others > 0 {
            tracing::info!(
                key = %item.key(),
                remaining = others,
                "the .torrent stays: other files of this torrent are still here"
            );
            return Ok(());
        }

        if !item.torrent_file.is_empty() {
            // Not fatal: the data is what matters, and a leftover .torrent is litter
            // the folder sweep collects later.
            if let Err(e) = std::fs::remove_file(&item.torrent_file)
                && e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(path = %item.torrent_file, error = %e, "could not remove the .torrent");
                }
        }
        Ok(())
    }
}

/// Sends on whatever the program reported as an error.
///
/// Throttled per source, so a tracker that is unreachable for an hour is one message and not
/// sixty. The kind is the module that reported it: two different failures in two different
/// places are two different messages, which is what somebody reading them wants.
pub(crate) fn spawn_problem_reporter(
    state: Arc<AppState>,
    mut problems: tokio::sync::mpsc::UnboundedReceiver<crate::alerts::Problem>,
) {
    tokio::spawn(async move {
        while let Some(problem) = problems.recv().await {
            if !state.config().await.maintenance.notify_problems {
                continue;
            }
            let kind = format!("error:{}", problem.kind);
            state
                .notify_occasionally(&kind, &format!("Hiba: {}", problem.text))
                .await;
        }
    });
}

/// Watches what the process is using, and says so when a reading will not go away.
///
/// The failures that matter most are the ones that log nothing: a loop that will not end, a
/// wedged download, memory that only grows. Nothing reports those, so they are measured.
pub(crate) fn spawn_watchdog(state: Arc<AppState>) {
    // Every half minute, and a processor problem has to hold for ten of those before it is
    // mentioned, because a busy half minute is not news.
    const INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
    const NEEDED: usize = 10;
    // Memory is not treated the same way, and that is the point.
    //
    // Five minutes of agreement is right for processor use, where a spike means nothing. It is
    // wrong for memory: measured here, this program went from seventy-seven megabytes to almost
    // sixteen gigabytes in under three minutes, and the machine was down to two gigabytes free
    // before the watchdog had collected enough samples to be allowed to speak. The owner had to
    // notice it himself and stop the program. Two readings a minute apart is enough agreement
    // for a number that only ever climbs.
    const MEMORY_NEEDED: usize = 2;
    // Half the machine, and two gigabytes held.
    //
    // The processor limit used to be two thirds of one core, and that is what kept raising the
    // alarm: on this machine, sixteen threads, verifying a forty-gigabyte download reached 1.6
    // cores, which is four percent of what there is and exactly what the processor is for. A
    // limit has to know what it is running on, so it is taken as a share of everything
    // available. Half of it, sustained for five minutes, is a real problem on any machine;
    // one and a half cores is not.
    //
    // Two gigabytes for memory. Measured with the files no longer mapped into memory: idle is
    // under forty megabytes and a download in progress stays in the same range, so this is far
    // clear of normal working, which is where such a threshold belongs.
    const CPU_SHARE_OF_MACHINE: f64 = 0.5;
    const RSS_LIMIT: u64 = 2 * 1024 * 1024 * 1024;
    let cpu_limit = crate::alerts::cores() as f64 * CPU_SHARE_OF_MACHINE;

    tokio::spawn(async move {
        let mut samples: Vec<(f64, u64)> = Vec::new();
        // The first reading spans from process start, which says nothing about now, so it is
        // taken only to have something to measure the next one against.
        let (_, _, mut previous_cpu) = crate::alerts::usage(0, INTERVAL);

        loop {
            tokio::time::sleep(INTERVAL).await;
            let (share, rss, cpu) = crate::alerts::usage(previous_cpu, INTERVAL);
            previous_cpu = cpu;
            samples.push((share, rss));
            if samples.len() > NEEDED * 2 {
                samples.remove(0);
            }

            // Memory first and on its own terms. A short agreement is deliberate: by the time
            // a long one is satisfied the machine may have nothing left to give.
            let memory_problem = samples.len() >= MEMORY_NEEDED
                && samples
                    .iter()
                    .rev()
                    .take(MEMORY_NEEDED)
                    .all(|(_, held)| *held > RSS_LIMIT);
            if memory_problem {
                // What the program itself is holding, and what the engine says of its own
                // share, because the two lead to different remedies and the message is worth
                // nothing without the distinction.
                let engine = state
                    .lib
                    .engine_stats()
                    .map(|e| e.disk_buffer_bytes / (1024 * 1024))
                    .unwrap_or(-1);
                let text = format!(
                    "A program {} MB memóriát tart, ebből a torrentmotor saját puffere {} MB.",
                    rss / (1024 * 1024),
                    engine
                );
                tracing::warn!("{text}");
                if state.config().await.maintenance.notify_problems {
                    state.notify_occasionally("watchdog-memory", &text).await;
                }
                samples.clear();
                continue;
            }

            if let Some(text) =
                crate::alerts::sustained_problem(&samples, cpu_limit, u64::MAX, NEEDED)
            {
                tracing::warn!("{text}");
                if state.config().await.maintenance.notify_problems {
                    state.notify_occasionally("watchdog", &text).await;
                }
                // Start again, so the next message is about a new run rather than the same one.
                samples.clear();
            }
        }
    });
}
