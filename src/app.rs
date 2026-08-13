//! What every request handler shares, and how it reaches the rest of the program.
//!
//! Split out of the server so the handlers can be read without wading through state
//! plumbing, and so the maintenance sweep's view of the world lives next to the state it
//! reads rather than in the middle of the HTTP layer.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::{Mutex, RwLock};

use crate::config::Config;
use crate::library::Library;
use crate::ncore::NcoreClient;

/// nCore download URLs carry the account passkey, so they cannot be rebuilt from a
/// torrent id alone. Stremio always asks for the stream list before playing, so the
/// list handler records what it found and the play handler looks it up here.
pub(crate) const SOURCE_CACHE_LIMIT: usize = 512;

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
    /// What the stream list found, kept so the play handler can act on it.
    pub(crate) sources: Mutex<HashMap<String, Source>>,
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

        let entries = self.state.refresh_owed().await?;
        owed.asked.push(crate::tracker::Tracker::Ncore);
        owed.keys.extend(
            entries
                .into_iter()
                .map(|e| crate::tracker::Tracker::Ncore.owed_key(&e.torrent_id)),
        );

        if let Some(client) = self.state.bithumen.read().await.as_ref() {
            match client.hit_and_run_ids().await {
                Ok(ids) => {
                    owed.asked.push(crate::tracker::Tracker::Bithumen);
                    owed.keys.extend(
                        ids.iter()
                            .map(|id| crate::tracker::Tracker::Bithumen.owed_key(id)),
                    );
                }
                Err(e) => tracing::warn!(
                    error = %e,
                    "could not read BitHUmen's hit and run list; its downloads stay put"
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

/// How nCore should be searched for a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SearchPlan {
    /// Exact match on an IMDb id: no false positives.
    Imdb(String),
    /// Titles to try in order, used when the work has no IMDb entry at all. Many
    /// Hungarian series are in that position, which is why this path exists.
    Names(Vec<String>),
}

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
        let mut map = self.sources.lock().await;
        if map.len() >= SOURCE_CACHE_LIMIT {
            map.clear();
        }
        map.insert(
            tracker.play_id(torrent_id),
            Source {
                tracker,
                download_url: download_url.to_string(),
                size_bytes,
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

    pub(crate) async fn source_for(&self, torrent_id: &str) -> Option<Source> {
        self.sources.lock().await.get(torrent_id).cloned()
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
                self.store.record_obligations(&owed, now).await;
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
            if let Some(at) = last.get(kind) {
                if now.saturating_sub(*at) < INTERVAL {
                    return;
                }
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
            if others == 0 && !item.torrent_file.is_empty() {
                if let Err(e) = std::fs::remove_file(&item.torrent_file) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        tracing::warn!(
                            path = %item.torrent_file,
                            error = %e,
                            "could not remove the .torrent"
                        );
                    }
                }
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
            if let Err(e) = std::fs::remove_file(&item.torrent_file) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(path = %item.torrent_file, error = %e, "could not remove the .torrent");
                }
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
    // Every half minute, and a problem has to hold for ten of those before it is mentioned.
    const INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
    const NEEDED: usize = 10;
    // Two thirds of one core, and one gigabyte of privately held memory. Measured on this
    // machine: idle is a fraction of a percent and under thirty megabytes, and downloading four
    // episodes at once while writing twenty-four gigabytes came to seventy-one megabytes. A
    // gigabyte is therefore more than an order of magnitude clear of normal working, which is
    // where a threshold for "something is wrong" belongs.
    const CPU_LIMIT: f64 = 0.66;
    const RSS_LIMIT: u64 = 1024 * 1024 * 1024;

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

            if let Some(text) =
                crate::alerts::sustained_problem(&samples, CPU_LIMIT, RSS_LIMIT, NEEDED)
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
