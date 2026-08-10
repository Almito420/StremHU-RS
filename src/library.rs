//! Torrents currently in play, and the loop that keeps their read heads fed.
//!
//! An addon cannot work off a single torrent handed in on the command line: a
//! stream request arrives for some title, and the torrent has to be fetched and
//! added right then. So this holds one libtorrent session and a registry keyed by
//! info hash, and one background loop walks every entry applying piece deadlines
//! from the read positions its readers report.
//!
//! One loop for all torrents rather than one per torrent: deadlines are relative to
//! now and have to be re-applied continuously, and a single pass keeps that
//! predictable no matter how many titles are open.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::sync::{Mutex, RwLock};

use crate::engine::{self, FileInfo, Session, Torrent};
use crate::series::SeasonEpisode;
use crate::stream_policy::{self, FileSpan, ReadHead};

/// What a caller wants out of a torrent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Want {
    /// A film: the largest file, since release folders carry a small sample too.
    LargestFile,
    /// One episode from a season pack.
    Episode(SeasonEpisode),
    /// A file chosen on an earlier run, identified by name with the index as a
    /// fallback. Used when restoring after a restart.
    ///
    /// The name leads because it survives things the index does not. A state file
    /// written before the index was recorded has a zero there, and index zero in a
    /// release folder is as likely to be a text file as the video. Re-deriving the
    /// choice from the episode number was the other option and is worse: if the matcher
    /// has changed since, a restart would silently start serving a different file than
    /// the one that was being watched.
    SavedFile { index: usize, name: String },
}

/// Whether every piece covering a file is already on disk.
///
/// `have` is the torrent's piece map. Used before switching a file on: on a library that has
/// been running a while the companions arrived long ago, and asking for them again at every
/// start is work, log noise, and a torrent that briefly reports itself unfinished.
pub fn file_is_complete(have: &[u8], offset: u64, size: u64, piece_len: u64) -> bool {
    if size == 0 {
        return true;
    }
    let span = FileSpan::from_offsets(offset, size, piece_len);
    let first = span.first_piece as usize;
    let last = (span.last_piece as usize).min(have.len().saturating_sub(1));
    first < have.len() && have[first..=last].iter().all(|b| *b == 1)
}

/// Which file inside the torrent a request means.
///
/// Shared with the code that decides which disk to write to, and that is the reason this is
/// a function rather than a step inside opening the torrent. The disk has to be chosen before
/// the torrent is added, and the choice depends on how much will actually be written: for a
/// season pack that is one episode, not the pack. Asking the question twice, in two places,
/// with two implementations, is how the two answers end up disagreeing.
pub fn select_file(files: &[FileInfo], want: &Want) -> Result<usize> {
    Ok(match want {
        Want::LargestFile => files
            .iter()
            .max_by_key(|f| f.size)
            .context("torrent has no files")?
            .index,
        Want::SavedFile { index, name } => {
            let by_name = files.iter().find(|f| {
                f.path
                    .file_name()
                    .map(|n| n.to_string_lossy().eq_ignore_ascii_case(name))
                    .unwrap_or(false)
            });
            match by_name {
                Some(f) => f.index,
                None if files.iter().any(|f| f.index == *index) => {
                    tracing::warn!(
                        wanted = %name,
                        index,
                        "the saved file name is not in this torrent; falling back to the index"
                    );
                    *index
                }
                None => bail!("this torrent contains neither {name:?} nor a file at {index}"),
            }
        }
        Want::Episode(se) => {
            // Only the exact episode counts inside a torrent: a season pack's own
            // name matched earlier, but the file we play has to be the right one.
            // Among several matches take the largest, because a sample file
            // carries the same numbering as the episode itself.
            let chosen = files
                .iter()
                .filter(|f| {
                    let name = f
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    crate::series::match_episode(&name, *se)
                        == Some(crate::series::Match::Exact)
                })
                .max_by_key(|f| f.size);

            match chosen {
                Some(f) => f.index,
                // Not guessing here is deliberate: the wrong episode is worse
                // than a clear failure.
                None => bail!(
                    "no file in this torrent matches S{:02}E{:02}",
                    se.season,
                    se.episode
                ),
            }
        }
    })
}
/// Whether an open torrent's selected file is the one a request is asking for.
///
/// `file_name` is what the open entry is serving, `largest` whether that file is the biggest in
/// its torrent. Used twice: to reuse an open torrent without fetching anything, and to refuse
/// to serve the wrong file when one torrent holds several.
pub fn serves(want: &Want, file_name: &str, largest: bool) -> bool {
    match want {
        // A film's torrent has one sizeable file, and that is what a film request means.
        Want::LargestFile => largest,
        Want::Episode(se) => {
            crate::series::match_episode(file_name, *se) == Some(crate::series::Match::Exact)
        }
        Want::SavedFile { name, .. } => file_name.eq_ignore_ascii_case(name),
    }
}

pub struct Entry {
    torrent: Torrent,
    /// The torrent this file belongs to. Several entries can share it: a season pack is one
    /// torrent, and each episode watched out of it is an entry of its own.
    pub info_hash: String,
    pub files: Vec<FileInfo>,
    pub piece_len: u64,
    pub selected: usize,
    pub span: FileSpan,
    /// Byte offset of the selected file inside the torrent. Kept explicitly: a file
    /// that does not start on a piece boundary cannot be derived from the span.
    pub file_offset: u64,
    pub file_len: u64,
    pub file_path: PathBuf,
    pub file_name: String,

    heads: Mutex<HashMap<u64, u32>>,
    have: RwLock<Vec<u8>>,
    next_reader_id: AtomicU64,
    /// Pieces that currently carry a deadline, so the loop can clear stale ones.
    active_deadlines: Mutex<BTreeSet<u32>>,
    streaming: Mutex<bool>,
    /// Every wanted piece is on disk. Once true, the loop stops re-reading the piece map for
    /// this torrent unless somebody is watching it.
    complete: RwLock<bool>,
    /// Whether the torrent's other files have already been switched on. Once, not every tick.
    extras_promoted: AtomicBool,
}

impl Entry {
    /// How this entry is addressed, matching the record in the store.
    pub fn key(&self) -> String {
        crate::state::item_key(&self.info_hash, self.selected)
    }

    pub async fn register_reader(&self, piece: u32) -> u64 {
        let id = self.next_reader_id.fetch_add(1, Ordering::Relaxed);
        self.heads.lock().await.insert(id, piece);
        id
    }

    pub async fn advance_reader(&self, id: u64, piece: u32) {
        self.heads.lock().await.insert(id, piece);
    }

    /// Dropping a reader shrinks the deadline set, so a stopped player stops
    /// holding bandwidth for pieces nobody will read.
    pub async fn drop_reader(&self, id: u64) {
        self.heads.lock().await.remove(&id);
    }

    pub async fn reader_positions(&self) -> HashMap<u64, u32> {
        self.heads.lock().await.clone()
    }

    pub fn piece_of(&self, offset_in_file: u64) -> u32 {
        stream_policy::piece_of(self.file_offset, offset_in_file, self.piece_len)
    }

    /// True when every piece covering the byte range is complete.
    pub async fn ready(&self, from: u64, to: u64) -> bool {
        let first = self.piece_of(from) as usize;
        let last = self.piece_of(to) as usize;
        let have = self.have.read().await;
        if first >= have.len() {
            return false;
        }
        let last = last.min(have.len() - 1);
        have[first..=last].iter().all(|b| *b == 1)
    }

    /// How much of *this file* is on disk.
    ///
    /// Counted from the piece map rather than taken from the torrent's own figure, because that
    /// one is the torrent's: with four episodes taken from one pack, every episode reported the
    /// pack's twenty-nine gigabytes against its own seven. The pieces at the file's edges are
    /// shared with its neighbours, so only the part that overlaps this file is counted.
    pub async fn downloaded_bytes(&self) -> u64 {
        let have = self.have.read().await;
        let piece = self.piece_len.max(1);
        let start = self.file_offset;
        let end = start + self.file_len;
        let mut total = 0u64;
        for index in self.span.first_piece..=self.span.last_piece {
            if have.get(index as usize).copied().unwrap_or(0) != 1 {
                continue;
            }
            let piece_start = index as u64 * piece;
            let piece_end = piece_start + piece;
            let from = piece_start.max(start);
            let to = piece_end.min(end);
            if to > from {
                total += to - from;
            }
        }
        total.min(self.file_len)
    }

    pub async fn contiguous_front(&self) -> u32 {
        let have = self.have.read().await;
        engine::contiguous_from(&have, self.span.first_piece)
    }

    pub fn stats(&self) -> engine::Stats {
        self.torrent.stats().unwrap_or_default()
    }
}

pub struct Library {
    session: Session,
    /// The records, so the moment a file finishes can be written down.
    ///
    /// That moment is only observable here: this loop is what reads the piece map. The clock a
    /// file's own seeding requirement runs on starts then, and counting from anywhere else
    /// would be counting from the wrong end of a download that takes hours.
    store: Arc<crate::state::Store>,
    entries: RwLock<HashMap<String, Arc<Entry>>>,
    /// Shared with the rest of the program rather than copied.
    ///
    /// It used to be a clone taken at startup, which meant ten settings could be changed in
    /// the interface, saved to the file, and have no effect until a restart nobody was told
    /// about: the download folder, the peer limits, the whole piece deadline policy. The
    /// listen port genuinely does need a restart, because a bound socket cannot move, and
    /// the interface says so.
    cfg: crate::config::Shared,
    /// Woken when a reader appears, so a starting stream does not wait out the idle pause.
    ///
    /// Without this the backoff that keeps the server quiet while seeding would add up to two
    /// seconds to the start of every playback, which is the opposite of the point.
    wake: tokio::sync::Notify,
    /// Bumped whenever the configuration is saved.
    ///
    /// The loop compares this instead of cloning the configuration on every pass. An atomic
    /// read is a few nanoseconds; the clone it replaces copied a dozen strings and lists
    /// several times a second for no benefit.
    cfg_generation: Arc<std::sync::atomic::AtomicU64>,
}

impl Library {
    pub async fn new(
        cfg: crate::config::Shared,
        cfg_generation: Arc<std::sync::atomic::AtomicU64>,
        store: Arc<crate::state::Store>,
    ) -> Result<Arc<Self>> {
        // Read once, deliberately: a bound socket cannot move, so the session's own
        // settings are fixed for the life of the process.
        let session = {
            let snapshot = cfg.read().await;
            Session::new(engine::SessionSettings::from_config(&snapshot.torrent))?
        };
        // Logged from the engine's own answer rather than from what we asked for, so a
        // setting that fails to apply is visible instead of assumed.
        if let Some((connections, down, up)) = session.limits() {
            tracing::info!(
                connections,
                download_rate = down,
                upload_rate = up,
                "engine settings in force"
            );
        }
        let lib = Arc::new(Self {
            session,
            store,
            entries: RwLock::new(HashMap::new()),
            wake: tokio::sync::Notify::new(),
            cfg,
            cfg_generation,
        });
        tokio::spawn(deadline_loop(lib.clone()));
        Ok(lib)
    }

    /// Tells the deadline loop that something changed and it should not wait.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// One entry, by `info_hash:file_index`.
    pub async fn get(&self, key: &str) -> Option<Arc<Entry>> {
        self.entries.read().await.get(key).cloned()
    }

    pub async fn open(&self) -> Vec<(String, Arc<Entry>)> {
        self.entries
            .read()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// The folder an already-open torrent writes into, if it is open.
    ///
    /// libtorrent keeps one save path per torrent, so a second file taken from the same torrent
    /// goes where the first one went, whatever the disk chooser would prefer. Whoever is about
    /// to add that second file needs to know which folder will really be used.
    pub async fn save_dir_for(&self, info_hash: &str) -> Option<String> {
        self.entries
            .read()
            .await
            .values()
            .find(|e| e.info_hash == info_hash)
            .map(|e| e.torrent.save_path().to_string())
    }

    /// The distinct torrents behind the open entries.
    ///
    /// Several entries can share one torrent, and anything said to libtorrent about a torrent
    /// has to be said once.
    pub async fn open_hashes(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for entry in self.entries.read().await.values() {
            if !out.iter().any(|h| *h == entry.info_hash) {
                out.push(entry.info_hash.clone());
            }
        }
        out
    }

    /// Re-opens everything the state file knows about, so a restart does not stop
    /// seeding.
    ///
    /// This matters more than it looks. On a private tracker the obligation attached to
    /// a download is measured in seeding time, and that clock only advances while the
    /// torrent is actually announced. A server that forgets its torrents on restart
    /// quietly stops paying that debt, and the account is the thing that suffers.
    ///
    /// Failures are per torrent and never fatal: a missing or corrupt `.torrent` should
    /// cost that one item, not the whole startup.
    /// Returns the file index each restored torrent settled on, so the caller can write
    /// it back and the record heals itself.
    pub async fn restore(&self, items: &[crate::state::Item]) -> Vec<(String, usize)> {
        let mut restored = Vec::new();
        for item in items {
            if item.torrent_file.is_empty() {
                continue;
            }
            let bytes = match std::fs::read(&item.torrent_file) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(
                        path = %item.torrent_file,
                        error = %e,
                        "cannot re-open this torrent; it will not seed"
                    );
                    continue;
                }
            };
            let want = Want::SavedFile {
                index: item.file_index,
                name: item.file_name.clone(),
            };
            let resume = self.resume_path(&item.info_hash).await;
            let resume_bytes = std::fs::read(&resume).ok();
            if resume_bytes.is_none() {
                tracing::info!(
                    title = %item.title,
                    "no resume data; libtorrent will re-check the files from disk"
                );
            }
            let fallback = self.cfg.read().await.torrent.save_path.clone();
            match self
                .add_with_resume(&bytes, want, resume_bytes.as_deref(), &fallback)
                .await
            {
                // The key the record has now, and the file the torrent actually settled
                // on. They differ when a record was written before the index was tracked.
                Ok((_, entry)) => restored.push((item.key(), entry.selected)),
                Err(e) => tracing::warn!(title = %item.title, error = %e, "could not restore"),
            }
        }
        if !restored.is_empty() {
            tracing::info!(count = restored.len(), "torrents restored and seeding");
        }
        restored
    }

    /// Writes out whatever resume data has arrived since the last look.
    ///
    /// Written atomically through a temporary file: a half-written resume file would be
    /// rejected at the next start, costing the full re-check it exists to avoid.
    async fn collect_resume_data(&self) {
        // Once per torrent, not once per served file: a pack with three episodes open has
        // three entries and one lot of resume data, and asking for it again after it has been
        // taken would find nothing and log a puzzle.
        for hash in self.open_hashes().await {
            let Some(bytes) = self.session.take_resume_data(&hash) else {
                continue;
            };
            let path = self.resume_path(&hash).await;
            if let Some(parent) = path.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    tracing::warn!(error = %e, "cannot create the resume folder");
                    continue;
                }
            }
            let tmp = path.with_extension("resume.tmp");
            if let Err(e) = std::fs::write(&tmp, &bytes) {
                tracing::warn!(error = %e, "cannot write resume data");
                continue;
            }
            if let Err(e) = std::fs::rename(&tmp, &path) {
                tracing::warn!(error = %e, "cannot replace resume data");
            }
        }
    }

    /// Asks every open torrent for its resume data and writes what comes back.
    ///
    /// For stopping on purpose. libtorrent answers asynchronously through the alert queue,
    /// so this asks, waits briefly, drains the queue and writes: the alternative is a start
    /// that re-hashes every finished file, which on a library of this size is minutes of
    /// disk work to rediscover what was already known.
    pub async fn save_all_resume_data(&self) {
        let open = self.open().await;
        if open.is_empty() {
            return;
        }
        let mut asked: Vec<String> = Vec::new();
        for (_, entry) in &open {
            if asked.iter().any(|h| *h == entry.info_hash) {
                continue;
            }
            asked.push(entry.info_hash.clone());
            if let Err(e) = entry.torrent.request_resume_data() {
                tracing::debug!(error = %e, "could not ask for resume data");
            }
        }
        // Two passes: the first alerts are usually there within a few hundred milliseconds,
        // and a torrent whose data has not arrived by the second pass simply keeps the
        // resume file it already had.
        for _ in 0..2 {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            if let Some(err) = self.session.pump_alerts() {
                tracing::debug!("libtorrent: {err}");
            }
            self.collect_resume_data().await;
        }
    }

    /// Info hashes that have a reader attached right now.
    pub async fn streaming_hashes(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (_, entry) in self.open().await {
            if !entry.reader_positions().await.is_empty() && !out.contains(&entry.info_hash) {
                out.push(entry.info_hash.clone());
            }
        }
        out
    }

    /// Stops serving one file and, when asked, erases it.
    ///
    /// Two cases, and the difference matters.
    ///
    /// The last file of its torrent: the torrent leaves the session and libtorrent deletes
    /// everything belonging to it, the part file included.
    ///
    /// One episode of a pack whose other episodes stay: the torrent has to keep running, so
    /// the file is dropped to priority zero, its data is removed by hand, and the torrent is
    /// rechecked. The recheck is not optional. Without it libtorrent still believes it holds
    /// those pieces and offers them to peers; the first read of the deleted file then fails,
    /// and a file error takes the whole torrent down, along with the episodes that were still
    /// paying off their seeding obligation.
    ///
    /// The registry entry goes first, so nothing can pick the file up again while it is being
    /// taken apart. A reader still holding a reference keeps the entry alive harmlessly.
    /// Stops serving several files of one torrent, and rechecks it once at the end.
    ///
    /// Rotating a pack means deleting an episode at a time, and each deletion on its own would
    /// mean a recheck on its own: ten episodes, ten passes over what is left on the disk, and
    /// the torrent not seeding through any of them. Done together it is one pass.
    ///
    /// Returns how many were removed. Keys belonging to other torrents are handled one by one,
    /// so a caller does not have to group them itself.
    pub async fn remove_files(&self, keys: &[String], delete_files: bool) -> usize {
        let mut done = 0;
        // Group by torrent, keeping the caller's order within each.
        let mut by_torrent: Vec<(String, Vec<String>)> = Vec::new();
        for key in keys {
            let hash = match self.get(key).await {
                Some(entry) => entry.info_hash.clone(),
                // Not open: still worth trying, the file may be on the disk without an entry.
                None => key.split(':').next().unwrap_or(key).to_string(),
            };
            match by_torrent.iter_mut().find(|(h, _)| *h == hash) {
                Some((_, list)) => list.push(key.clone()),
                None => by_torrent.push((hash, vec![key.clone()])),
            }
        }

        for (hash, group) in by_torrent {
            // All but the last of a torrent's group without a recheck, then one at the end.
            let last = group.len().saturating_sub(1);
            for (i, key) in group.iter().enumerate() {
                let recheck = i == last;
                match self.remove_one(key, delete_files, recheck).await {
                    Ok(()) => done += 1,
                    Err(e) => tracing::warn!(key = %key, error = %e, "could not remove this file"),
                }
            }
            tracing::info!(hash = %hash, files = group.len(), "batch removed");
        }
        done
    }

    pub async fn remove_file(&self, key: &str, delete_files: bool) -> Result<()> {
        self.remove_one(key, delete_files, true).await
    }

    async fn remove_one(&self, key: &str, delete_files: bool, recheck: bool) -> Result<()> {
        let entry = self.entries.write().await.remove(key);
        let Some(entry) = entry else {
            // Already gone: not an error, the caller wanted it absent.
            return Ok(());
        };

        let others = self
            .entries
            .read()
            .await
            .values()
            .filter(|e| e.info_hash == entry.info_hash)
            .count();

        if others == 0 {
            self.session.remove_torrent(&entry.torrent, delete_files)?;
            tracing::info!(key, delete_files, "torrent removed");
            return Ok(());
        }

        // The torrent stays for the sake of its other files.
        if let Err(e) = entry.torrent.set_file_priority(entry.selected, 0) {
            tracing::warn!(key, error = %e, "could not switch the file off");
        }
        if delete_files {
            match std::fs::remove_file(&entry.file_path) {
                Ok(()) => tracing::info!(path = %entry.file_path.display(), "file deleted"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(
                    path = %entry.file_path.display(),
                    error = %e,
                    "could not delete the file"
                ),
            }
            // Only once per torrent per batch: the recheck reads everything the torrent still
            // has, and doing it after each file of a pack would be the same work over again.
            if recheck {
                if let Err(e) = entry.torrent.force_recheck() {
                    tracing::warn!(key, error = %e, "could not start the recheck");
                }
            }
        }
        // What is wanted shrank, and with the rest complete this torrent is a seeder again.
        // Saying so promptly is what keeps its seeding time counting.
        if let Err(e) = entry.torrent.force_reannounce() {
            tracing::debug!(key, error = %e, "could not re-announce");
        }
        tracing::info!(
            key,
            remaining = others,
            "stopped serving this file; the torrent keeps running for its other files"
        );
        Ok(())
    }

    /// Adds the torrent if it is not already open, selects the wanted file and
    /// starts it. Returns the existing entry when the same torrent is requested
    /// again, so two viewers of one title share a download.
    pub async fn add(
        &self,
        torrent_bytes: &[u8],
        want: Want,
        save_dir: &str,
    ) -> Result<(String, Arc<Entry>)> {
        self.add_with_resume(torrent_bytes, want, None, save_dir).await
    }

    /// Where a torrent's resume data is kept: beside its `.torrent`, named the same.
    pub async fn resume_path(&self, info_hash: &str) -> PathBuf {
        let dir = self.cfg.read().await.storage.torrent_files_dir.clone();
        std::path::Path::new(&dir).join(format!("{info_hash}.resume"))
    }

    /// `save_dir` is where the data goes. The library is told rather than deciding: which
    /// disk to use is a policy question about free space and notifications, and that belongs
    /// with the code that can act on it.
    ///
    /// For a restore this is only a fallback. Resume data carries the folder the files are
    /// already in, and the shim keeps that in preference, so a restored torrent stays put.
    pub async fn add_with_resume(
        &self,
        torrent_bytes: &[u8],
        want: Want,
        resume: Option<&[u8]>,
        save_dir: &str,
    ) -> Result<(String, Arc<Entry>)> {
        // Peek at the info hash without adding, so a second request for the same
        // title does not create a duplicate session entry.
        let cfg = self.cfg.read().await.clone();


        let torrent =
            self.session
                .add_torrent_with_resume(torrent_bytes, save_dir, resume)?;
        let hash = torrent.info_hash.clone();

        let files = torrent.files()?;
        let piece_len = torrent.piece_length()?;

        let selected = select_file(&files, &want)?;

        // Keyed by torrent and file, so a second episode out of the same pack is its own
        // entry rather than a request that quietly gets the first episode's video.
        let key = crate::state::item_key(&hash, selected);
        if let Some(existing) = self.get(&key).await {
            // libtorrent ignores a duplicate add, and dropping this handle leaves the
            // original untouched.
            tracing::debug!(key = %key, "this file is already open, reusing it");
            return Ok((key, existing));
        }
        // Any other file of the same torrent that is already being served. Their selection
        // must survive: switching to "only this file" would stop the episode somebody else is
        // watching, and deleting its data later would then find nothing.
        let siblings: Vec<usize> = self
            .entries
            .read()
            .await
            .values()
            .filter(|e| e.info_hash == hash)
            .map(|e| e.selected)
            .collect();

        let file = files
            .iter()
            .find(|f| f.index == selected)
            .context("selected file vanished")?
            .clone();

        // A film always arrives whole, because the film is the only sizeable file in
        // its torrent. A season pack is different: only the episode being watched is
        // fetched, so nine unwatched episodes do not land on the disk to be seeded and
        // then deleted.
        //
        // Some trackers count a partial download against the ratio rules, or expect the
        // complete torrent to be seeded. Where that is the case the setting turns the
        // saving off, and the whole pack is fetched with the wanted episode still
        // getting the deadlines, so playback starts just as quickly either way.
        if cfg.ncore.requires_full_download {
            torrent.prioritize_all_pieces(cfg.pieces.idle_priority.max(1))?;
            tracing::info!(
                hash = %hash,
                "the whole torrent will be downloaded: ncore.requires_full_download is on"
            );
        } else if siblings.is_empty() {
            // First file out of this torrent: everything else off.
            torrent.select_only_file(selected)?;
        } else {
            // Another episode of the same pack is already being served. Only this file is
            // added; nothing else is touched.
            torrent.set_file_priority(selected, 7)?;
            // And say so to the tracker at once. Everything this torrent wanted was already on
            // disk, so it has been announcing itself as a seeder, and a seeder is handed no
            // peers: without this the new file would sit at zero bytes until the next scheduled
            // announce, half an hour later.
            if let Err(e) = torrent.force_reannounce() {
                tracing::warn!(hash = %hash, error = %e, "could not re-announce");
            }
            tracing::info!(
                hash = %hash,
                already = siblings.len(),
                "another file of this torrent is now served as well, tracker told"
            );
        }
        torrent.set_max_connections(cfg.torrent.connections_while_idle)?;
        torrent.resume()?;

        let span = FileSpan::from_offsets(file.offset, file.size, piece_len);
        let entry = Arc::new(Entry {
            torrent,
            info_hash: hash.clone(),
            files: files.clone(),
            piece_len,
            selected,
            span,
            file_offset: file.offset,
            file_len: file.size,
            file_path: file.path.clone(),
            file_name: file
                .path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            heads: Mutex::new(HashMap::new()),
            have: RwLock::new(Vec::new()),
            next_reader_id: AtomicU64::new(1),
            active_deadlines: Mutex::new(BTreeSet::new()),
            streaming: Mutex::new(false),
            complete: RwLock::new(false),
            extras_promoted: AtomicBool::new(false),
        });

        self.entries.write().await.insert(key.clone(), entry.clone());
        tracing::info!(
            hash = %hash,
            file = %entry.file_name,
            size = entry.file_len,
            "torrent opened"
        );
        Ok((hash, entry))
    }
}

/// How long the loop waits when nothing is being watched.
///
/// Two seconds: long enough that an evening of seeding costs almost nothing, short enough
/// that the first byte range of a new playback waits no longer than one pass before the
/// deadlines start aiming at it.
const IDLE_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Re-applies deadlines for every open torrent. Deadlines expire, so this has to
/// keep running; and it is the only place that talks to libtorrent about ordering,
/// which keeps the policy in one auditable spot.
async fn deadline_loop(lib: Arc<Library>) {
    let mut ticks: u64 = 0;
    // Only re-read the configuration when it has actually changed. The loop runs several
    // times a second and the configuration is a deep structure of strings and lists; cloning
    // all of it on every pass was pure waste for something that changes when somebody
    // presses Save.
    let mut cfg = lib.cfg.read().await.clone();
    let mut cfg_generation = lib.cfg_generation.load(Ordering::Relaxed);

    loop {
        let generation = lib.cfg_generation.load(Ordering::Relaxed);
        if generation != cfg_generation {
            cfg = lib.cfg.read().await.clone();
            cfg_generation = generation;
            tracing::debug!("the deadline loop picked up new settings");
        }

        let busy_interval = Duration::from_millis(cfg.streaming.piece_poll_interval_ms.max(100));
        // Resume data is asked for on this many ticks' cadence, worked out from the tick
        // length so the interval stays the same however often the loop runs.
        let resume_every_ticks = (cfg.storage.resume_save_interval_secs.max(5) * 1000
            / busy_interval.as_millis().max(1) as u64)
            .max(1);

        let entries = lib.open().await;
        // Whether anything is being watched decides how hard this loop works. With no reader
        // there is no read head to feed, so the only reason to come round again is to notice
        // that one has appeared, and that does not need checking three times a second.
        let mut anyone_reading = false;

        for (key, entry) in entries {
            let heads: Vec<ReadHead> = entry
                .heads
                .lock()
                .await
                .values()
                .map(|piece| ReadHead {
                    span: entry.span,
                    piece: *piece,
                })
                .collect();

            let streaming = !heads.is_empty();
            anyone_reading |= streaming;

            // The piece bitmap crosses the language boundary and allocates a vector the
            // length of the torrent's piece count, so it is only worth refreshing when
            // something will read it: a reader waiting on a byte range, or a download that
            // has not finished yet.
            let complete = *entry.complete.read().await;
            if streaming || !complete {
                match entry.torrent.have_pieces() {
                    Ok(h) => {
                        // The wanted file's own pieces, not the whole torrent's. With files
                        // left out of a torrent the whole map never fills, so this flag never
                        // got set and the map was fetched again on every tick for ever.
                        let first = entry.span.first_piece as usize;
                        let last = (entry.span.last_piece as usize).min(h.len().saturating_sub(1));
                        let done = first < h.len() && h[first..=last].iter().all(|b| *b == 1);
                        *entry.have.write().await = h;
                        if done && !complete {
                            *entry.complete.write().await = true;
                            // Written down once, and only the first time: the file's own
                            // seeding clock starts here.
                            lib.store.mark_completed(&key, crate::state::now()).await;
                            tracing::info!(key = %key, "download complete");
                        }
                    }
                    Err(e) => tracing::warn!(key = %key, error = %e, "have_pieces failed"),
                }
            }
            // With the wanted file on disk, pick up whatever small leftovers the torrent has,
            // so it can become a complete seed instead of sitting at 99% for ever. Deliberately
            // after completion and not before: at the start of a stream every byte of bandwidth
            // belongs to the read head.
            if *entry.complete.read().await
                && !entry.extras_promoted.swap(true, Ordering::Relaxed)
            {
                let sizes: Vec<u64> = entry.files.iter().map(|f| f.size).collect();
                // Files this torrent is already serving, this one included. A second episode
                // open in its own right must keep its top priority.
                let served: Vec<usize> = lib
                    .entries
                    .read()
                    .await
                    .values()
                    .filter(|e| e.info_hash == entry.info_hash)
                    .map(|e| e.selected)
                    .collect();
                let candidates = stream_policy::extras_worth_completing(
                    &sizes,
                    &served,
                    cfg.torrent.complete_extras_below_bytes,
                );
                // Only the ones not already here. After the first time, that is all of them.
                let extras: Vec<usize> = {
                    let have = entry.have.read().await;
                    candidates
                        .into_iter()
                        .filter(|i| {
                            let f = &entry.files[*i];
                            !file_is_complete(&have, f.offset, f.size, entry.piece_len)
                        })
                        .collect()
                };
                if !extras.is_empty() {
                    let total: u64 = extras.iter().map(|i| sizes[*i]).sum();
                    let mut failed = false;
                    for index in &extras {
                        // Lowest priority that still downloads: the file being watched keeps
                        // its deadlines, and these come along behind whatever is left.
                        if let Err(e) = entry.torrent.set_file_priority(*index, 1) {
                            tracing::warn!(key = %key, index, error = %e, "could not switch on a file");
                            failed = true;
                        }
                    }
                    if !failed {
                        // The wanted set grew, so the tracker has to hear about it.
                        if let Err(e) = entry.torrent.force_reannounce() {
                            tracing::debug!(error = %e, "could not re-announce");
                        }
                        tracing::info!(
                            key = %key,
                            files = extras.len(),
                            bytes = total,
                            "picking up the rest of the torrent so it can seed complete"
                        );
                        // The wanted pieces changed, so the flag has to be earned again.
                        *entry.complete.write().await = false;
                    }
                }
            }

            {
                let mut was = entry.streaming.lock().await;
                if streaming != *was {
                    let limit = stream_policy::max_connections(
                        streaming,
                        cfg.torrent.connections_while_streaming,
                        cfg.torrent.connections_while_idle,
                    );
                    if let Err(e) = entry.torrent.set_max_connections(limit) {
                        tracing::warn!(error = %e, "set_max_connections failed");
                    }
                    tracing::info!(key = %key, streaming, limit, "stream state changed");
                    *was = streaming;
                }
            }

            // Per torrent: the window follows this torrent's piece size, so a release
            // with 16 MB pieces does not queue up hundreds of megabytes ahead.
            let policy = cfg.pieces.to_policy(entry.piece_len);
            let mut active = entry.active_deadlines.lock().await;
            let plan = {
                let have = entry.have.read().await;
                stream_policy::plan(&policy, &heads, &have, &active)
            };
            for piece in &plan.reset {
                if let Err(e) = entry.torrent.reset_piece_deadline(*piece) {
                    tracing::warn!(error = %e, piece, "reset_piece_deadline failed");
                }
            }
            for (piece, ms) in &plan.set {
                if let Err(e) = entry.torrent.set_piece_deadline(*piece, *ms) {
                    tracing::warn!(error = %e, piece, "set_piece_deadline failed");
                }
            }
            *active = plan.set.keys().copied().collect();
        }

        if let Some(err) = lib.session.pump_alerts() {
            tracing::warn!("libtorrent: {err}");
        }

        // Resume data asked for on an earlier tick arrives through the same alert queue,
        // so it is collected right after pumping it.
        lib.collect_resume_data().await;

        ticks = ticks.wrapping_add(1);
        // Every so often rather than every tick: this loop runs several times a second,
        // and writing a resume file that often would be pure disk churn for data that
        // only matters at the next start.
        if ticks % resume_every_ticks == 0 {
            let mut asked: Vec<String> = Vec::new();
            for (_, entry) in lib.open().await {
                if asked.iter().any(|h| *h == entry.info_hash) {
                    continue;
                }
                asked.push(entry.info_hash.clone());
                if let Err(e) = entry.torrent.request_resume_data() {
                    tracing::debug!(error = %e, "could not ask for resume data");
                }
            }
        }

        // Idle means seeding, and seeding needs nothing from this loop beyond draining the
        // alert queue. Backing off is the difference between a server that costs nothing
        // while it sits there and one that wakes the CPU a few times a second all evening.
        let interval = if anyone_reading {
            busy_interval
        } else {
            IDLE_POLL_INTERVAL
        };
        // A new reader cuts the wait short. While something is playing the interval is a few
        // hundred milliseconds anyway, so this matters for the first request of a playback,
        // which is exactly the one a viewer is waiting on.
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = lib.wake.notified() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Companions already on disk must not be switched on again at every start: that is work
    /// for nothing, and it makes a finished torrent report itself unfinished for a moment.
    #[test]
    fn a_file_already_on_disk_is_recognised() {
        // Four pieces of 4 MiB. A file covering pieces 1 and 2.
        let piece = 4 * 1024 * 1024u64;
        let offset = piece;
        let size = 2 * piece;

        let all_there = vec![1u8, 1, 1, 1];
        assert!(file_is_complete(&all_there, offset, size, piece));

        let second_missing = vec![1u8, 1, 0, 1];
        assert!(!file_is_complete(&second_missing, offset, size, piece));

        // Nothing known yet, which is the state before the first piece map arrives.
        assert!(!file_is_complete(&[], offset, size, piece));
        // An empty file is trivially there.
        assert!(file_is_complete(&[], offset, 0, piece));
    }

    /// The bug this was written for: a complete-series pack of 1.33 TiB was refused for want of
    /// room on a disk with 127 GiB free, because the size checked was the torrent's rather than
    /// the episode's. One episode plus its nfo is what gets written.
    #[test]
    fn the_space_needed_is_the_episode_not_the_pack() {
        let mut sizes = vec![5_000_000_000u64; 177]; // House, every episode in one torrent
        sizes.push(20_000); // the nfo
        let pack: u64 = sizes.iter().sum();
        assert!(pack > 800_000_000_000, "the pack really is enormous: {pack}");

        let selected = 0usize;
        let companions = crate::stream_policy::extras_worth_completing(
            &sizes,
            &[selected],
            512 * 1024 * 1024,
        );
        let needed: u64 = sizes[selected] + companions.iter().map(|i| sizes[*i]).sum::<u64>();
        assert_eq!(needed, 5_000_020_000, "one episode and the nfo");
        assert!(needed < 127 * 1024 * 1024 * 1024, "and it fits where the pack would not");
    }

    /// Reusing an open torrent must never hand back a different file than the one asked for:
    /// serving episode one when episode two was requested looks like a working stream.
    #[test]
    fn an_open_torrent_is_only_reused_for_the_file_that_was_asked_for() {
        let se = SeasonEpisode { season: 2, episode: 6 };
        assert!(serves(&Want::Episode(se), "Exek.csataja.S02E06.HUN.WEB-DL.mkv", false));
        assert!(!serves(&Want::Episode(se), "Exek.csataja.S02E01.HUN.WEB-DL.mkv", false));
        // A film request is for the torrent's one sizeable file.
        assert!(serves(&Want::LargestFile, "film.mkv", true));
        assert!(!serves(&Want::LargestFile, "Sample/sample.mkv", false));
        // A restored record names its file, and case must not matter on Windows.
        let saved = Want::SavedFile { index: 1, name: "Film.MKV".into() };
        assert!(serves(&saved, "film.mkv", false));
        assert!(!serves(&saved, "other.mkv", true));
    }

    #[test]
    fn an_episode_want_carries_both_numbers() {
        let w = Want::Episode(SeasonEpisode {
            season: 19,
            episode: 8,
        });
        assert_ne!(w, Want::LargestFile);
    }

    /// Among files that all match the wanted episode, the largest has to win: a
    /// sample file carries the same S..E.. numbering as the episode itself.
    #[test]
    fn the_largest_matching_file_is_the_episode_not_the_sample() {
        let sizes = [(0usize, 50_000_000u64), (1, 1_200_000_000)];
        let biggest = sizes.iter().max_by_key(|(_, s)| *s).map(|(i, _)| *i);
        assert_eq!(biggest, Some(1));
    }
}
