//! Configuration.
//!
//! Everything tunable lives in one TOML file: nothing is baked into the code, and
//! the same values are editable from the web UI, which writes the file back. The
//! shared handle is an `RwLock` so a settings change takes effect without a
//! restart.
//!
//! Every field has a serde default, so an older or hand-trimmed file still loads
//! and simply picks up defaults for whatever is missing. That matters because the
//! file is meant to be edited by hand as well as by the UI.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// The folder the program keeps everything in: the one the executable sits in.
///
/// The install is then one folder. Copy it elsewhere and it takes its downloads, its
/// records and its certificate with it; delete it and nothing is left behind anywhere
/// else on the machine. Nothing is written to the registry, to AppData or to a
/// hidden per-user directory.
///
/// Resolved once. The fallback is the working directory, which only comes up if the
/// executable's own path cannot be read.
pub fn base_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."))
    })
    .clone()
}

/// A default path inside the install folder, with forward slashes so the config file
/// stays readable: a Windows path in TOML would otherwise need every separator doubled.
fn in_base(name: &str) -> String {
    base_dir()
        .join(name)
        .to_string_lossy()
        .replace('\\', "/")
}

pub type Shared = Arc<RwLock<Config>>;

/// Whether a file found in a folder above is one of ours.
///
/// Checked by its sections rather than by parsing: every field has a serde default, so
/// almost any TOML would parse as a valid configuration, and accepting one would mean
/// overwriting somebody else's file the first time a setting is saved.
fn looks_like_our_config(path: &Path) -> bool {
    match std::fs::read_to_string(path) {
        Ok(text) => text.contains("[ncore]") || text.contains("[maintenance]"),
        Err(_) => false,
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub server: Server,
    pub storage: Storage,
    pub streaming: Streaming,
    pub torrent: Torrent,
    pub pieces: Pieces,
    pub maintenance: Maintenance,
    pub ncore: Ncore,
    pub filters: Filters,
    pub tmdb: Tmdb,
    pub network: Network,
    pub auth: Auth,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Server {
    /// Address the HTTP server binds to.
    pub listen_addr: String,
    pub port: u16,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0".into(),
            port: 3080,
        }
    }
}

/// Where the server keeps its own records. Two files and a folder, all removable by
/// hand: nothing is hidden in a registry or a database.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Storage {
    /// What was downloaded, how much of it was watched, and what is marked to keep.
    pub state_path: String,
    /// Saved `.torrent` files, kept so a torrent can be re-added after a restart
    /// without going back to the tracker for it.
    pub torrent_files_dir: String,
    /// How often the state file is written when something changed. Range requests
    /// arrive for every megabyte of video, so this is batched rather than immediate.
    pub flush_interval_secs: u64,
    /// How often each open torrent's resume data is saved.
    ///
    /// Resume data is what lets a restart skip re-reading and re-hashing finished files.
    /// Losing the last minute of it costs nothing, so this is not written more often than
    /// it is worth: the point is that a restart does not cost a full pass over a 17 GB
    /// file, not that it is accurate to the second.
    pub resume_save_interval_secs: u64,
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            state_path: in_base("state.json"),
            torrent_files_dir: in_base("torrents"),
            flush_interval_secs: 15,
            resume_save_interval_secs: 60,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Streaming {
    /// How much is read and sent at a time.
    pub chunk_size_bytes: u64,
    /// Give up on a range if the pieces do not arrive within this long.
    pub piece_wait_timeout_secs: u64,
    /// How often piece states are re-checked while waiting.
    pub piece_poll_interval_ms: u64,
    /// Content type sent for video responses.
    pub content_type: String,
}

impl Default for Streaming {
    fn default() -> Self {
        Self {
            chunk_size_bytes: 1_048_576,
            piece_wait_timeout_secs: 120,
            piece_poll_interval_ms: 400,
            content_type: "video/x-matroska".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Torrent {
    /// Where downloads go. Everything goes here while it has room.
    pub save_path: String,
    /// Where they go once the first folder is full.
    ///
    /// The order is the rule: the primary is used until a download will not fit, and only
    /// then is this one looked at, so what is where stays predictable and the second disk is
    /// not woken up on every request just to be compared. Empty means there is no second
    /// folder, and a full primary is then an error rather than a silent move.
    pub save_path_secondary: String,
    /// Inbound BitTorrent port, announced to the tracker so peers can reach us.
    /// 0 lets libtorrent pick one. Not 6881, because the container being replaced
    /// still publishes that port on this machine.
    pub listen_port: u16,
    /// Peers across the whole client, all torrents together.
    pub global_connections_limit: u32,
    /// Peers allowed on one torrent while a stream is active; more peers fill the
    /// read head faster.
    pub connections_while_streaming: u32,
    /// Peers allowed on one torrent when it is only seeding.
    pub connections_while_idle: u32,
    /// Download rate cap in bytes per second, 0 for unlimited.
    pub download_limit_bytes: i32,
    /// Upload rate cap in bytes per second, 0 for unlimited. Worth leaving at 0 on a
    /// private tracker, where upload is what keeps the account in good standing.
    pub upload_limit_bytes: i32,
    /// Ask the router to open the listen port. Off: the port is forwarded by hand or
    /// not at all, and the tracker still works without it.
    pub enable_upnp_and_natpmp: bool,
    /// Once the wanted file is on disk, also fetch whatever else the torrent holds, as long
    /// as the rest comes to no more than this.
    ///
    /// A film's torrent usually carries a sample and an nfo beside the film. Leaving those out
    /// costs about one percent of the torrent and means it can never become a complete seed:
    /// the tracker reports 98.94% for ever, and seeding time cannot change that. Picking them
    /// up afterwards costs ninety megabytes and makes the torrent whole.
    ///
    /// The limit is what keeps this from undoing the one-episode rule: the other nine episodes
    /// of a season pack are gigabytes, far above any sensible value here, so a pack stays
    /// partial. Zero switches the whole thing off.
    pub complete_extras_below_bytes: u64,
    /// How many torrents may be active at once, downloading and seeding together.
    ///
    /// libtorrent defaults to three downloads and five seeds and **pauses the rest**, which
    /// on a library of any size means torrents that quietly stop seeding and stop paying off
    /// their obligation. `-1`, the default, removes the limit entirely; a plain number is a
    /// limit. Peers are capped separately, per torrent and for the client as a whole, so
    /// nothing here can let the engine run away with the connection count.
    pub max_active_torrents: i32,
}

impl Default for Torrent {
    fn default() -> Self {
        Self {
            save_path: in_base("downloads"),
            save_path_secondary: String::new(),
            listen_port: 6890,
            // The values the implementation being replaced actually runs with.
            global_connections_limit: 200,
            connections_while_streaming: 50,
            connections_while_idle: 20,
            download_limit_bytes: 0,
            upload_limit_bytes: 0,
            enable_upnp_and_natpmp: false,
            // Enough for a sample and an nfo, never enough for another episode.
            complete_extras_below_bytes: 512 * 1024 * 1024,
            max_active_torrents: -1,
        }
    }
}

/// Piece deadline window. This is what makes playback start: see stream_policy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Pieces {
    /// How much video to fetch ahead of the playhead, in bytes.
    ///
    /// A quantity of film rather than a count of pieces, because a piece is not a unit of
    /// time: on the releases here the piece size ranges from half a megabyte to sixteen, so
    /// a fixed piece count meant anything from two megabytes of readahead to sixty-four. At
    /// 4K bitrates the small end was under a second of video, which is what made a 4K stream
    /// start slowly and then stall.
    ///
    /// 64 MB is roughly eight seconds of a 4K remux and a couple of minutes of a 1080p
    /// episode. Raising it buys more cushion at the cost of fetching further ahead of what
    /// the viewer may actually watch.
    pub readahead_bytes: u64,
    /// Deadline for the piece being read right now. 0 means "as soon as possible".
    pub head_deadline_ms: u32,
    /// Base for every other piece in the window.
    pub base_deadline_ms: u32,
    /// Added per piece of distance from the read position.
    pub deadline_step_ms: u32,
    /// Also pin the file's first and last piece, where containers keep the header
    /// and the seek index.
    pub pin_file_edges: bool,

    /// Priority for the selected file's pieces. Zero would mean it never completes.
    pub idle_priority: u8,
}

impl Default for Pieces {
    fn default() -> Self {
        Self {
            // The strategy of the implementation being replaced, which is proven on
            // this tracker: a 1 MB critical step, four of them in the window, the read
            // head due immediately and everything behind it at 2s + 1s per piece.
            readahead_bytes: 64 * 1024 * 1024,
            head_deadline_ms: 0,
            base_deadline_ms: 2000,
            deadline_step_ms: 1000,
            pin_file_edges: true,
            idle_priority: 4,
        }
    }
}

impl Pieces {
    /// The window depends on the torrent's piece size, so the policy is built per
    /// torrent rather than once for the process.
    pub fn to_policy(&self, piece_size: u64) -> crate::stream_policy::Policy {
        crate::stream_policy::Policy {
            prefetch_pieces: crate::stream_policy::prefetch_for_piece_size(
                piece_size,
                self.readahead_bytes,
            ),
            head_deadline_ms: self.head_deadline_ms,
            base_deadline_ms: self.base_deadline_ms,
            deadline_step_ms: self.deadline_step_ms,
            pin_file_edges: self.pin_file_edges,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Maintenance {
    /// Respect hit-and-run rules: never delete something still owing seed time.
    pub hit_and_run: bool,
    /// Minimum seeding time before a torrent may be removed.
    pub keep_seed_seconds: u64,
    /// Delete downloaded data older than this.
    pub cache_retention_seconds: u64,
    /// Time of day the sweep runs, local time, `HH:MM`. Once a day rather than on an
    /// interval: deleting is not urgent, and a fixed evening time means it never
    /// happens while someone is likely to be watching.
    pub sweep_at: String,
    /// How far into the file a viewer must have reached for it to count as watched.
    /// 90 is where end credits usually start, and is what media servers use.
    pub watched_position_percent: u8,
    /// How much of the file must actually have been delivered. Guards against a seek
    /// to the end registering as a complete viewing: jumping to the credits serves a
    /// few megabytes, not half the film.
    pub watched_min_served_percent: u8,
    /// Only ever delete something that has been played at least once. Anything
    /// downloaded but never watched is left alone, however old it is, so a title
    /// queued for later cannot disappear before it is seen. The cost is that a
    /// mistaken download is never cleaned up on its own.
    pub require_watched: bool,
    /// For a download where files were deliberately skipped, decide on our own seeding
    /// time instead of waiting for the tracker's list to clear.
    ///
    /// One episode taken from a season pack is never a complete torrent, so it cannot
    /// become a finished seed and the tracker's obligation for it may never clear. Waiting
    /// for something that cannot happen would keep the episode forever.
    ///
    /// Measured before switching this on: on this tracker a partially fetched torrent is
    /// still reported as `Seed` and its remaining time does count down, so the ordinary
    /// rule works and this is a safety net rather than a fix. It stays off by default
    /// because the tracker's own answer is the better one wherever it is available.
    pub partial_uses_local_seed_time: bool,
    /// Safety switch: nothing is ever deleted while this is false.
    pub enable_deletion: bool,
    /// Warn when a download folder has less than this free.
    pub warn_below_free_bytes: u64,
    /// And warn when it has less than this share of the volume left, which catches a large
    /// disk that is nearly full while its absolute figure still looks comfortable.
    pub warn_below_free_percent: u64,
    /// Where to send a notification when space runs low. Empty means nowhere.
    ///
    /// A plain HTTP POST with the message as the body, which is what a phone notification
    /// service such as ntfy accepts. Empty by default and deliberately so: this is a
    /// private tracker setup, and no message should leave this machine unless it was asked
    /// for. With nothing set, the warning still appears in the interface and the log.
    pub notify_webhook_url: String,
    /// Work out the seeding requirement from the tracker's published formula instead of the
    /// flat `keep_seed_seconds`.
    ///
    /// The formula is the tracker's own: `(1 - ratio) * (48h + 0.4h per downloaded GB)`, minus
    /// the time already seeded. On a seven gigabyte episode with nothing given back that comes
    /// to about fifty-one hours, where the flat setting asks for ten days. Following the rule
    /// that will actually be applied to the account is both safer and less wasteful than
    /// guessing high.
    pub use_tracker_seed_rule: bool,
    /// Added on top of whatever the formula asks for.
    ///
    /// The tracker recomputes these figures only when the client announces, every thirty to
    /// forty minutes, and it closes the month two to three hours before it ends. Deleting at
    /// the exact minute the requirement is met would be trusting a figure that is up to an
    /// hour old.
    pub seed_safety_margin_hours: u64,
    /// For a torrent we deliberately left files out of, require that as much has been given
    /// back as was taken instead of allowing seeding time to satisfy it.
    ///
    /// Off, because the tracker was watched doing the opposite. The rules define Status Seed as
    /// having the torrent at 100%, which would make every pack of ours a Leech for ever, but
    /// what the tracker actually goes on is what the client announces: with the unwanted files
    /// deselected, libtorrent reports nothing left to fetch, and the tracker showed our
    /// deliberately partial torrent as **Seed** with its remaining time counting down
    /// (54h43m to 45h54m over one evening). So seeding time does accrue for a pack we hold one
    /// episode of, and an episode can pay off its torrent's debt on its own.
    ///
    /// Left as a setting because it is the one thing here that rests on an observation of the
    /// tracker's behaviour rather than on its published rules. Turning it on is the cautious
    /// reading: nothing is deleted from a pack until as much has been given back as was taken.
    pub partial_requires_ratio_one: bool,
}

impl Default for Maintenance {
    fn default() -> Self {
        Self {
            // The values the implementation being replaced actually runs with.
            hit_and_run: true,
            keep_seed_seconds: 864_000,
            cache_retention_seconds: 1_209_600,
            sweep_at: "20:00".into(),
            watched_position_percent: 90,
            watched_min_served_percent: 50,
            require_watched: true,
            partial_uses_local_seed_time: false,
            // Deletion stays off until explicitly enabled: losing seeding data on a
            // private tracker is not recoverable by re-downloading for free.
            enable_deletion: false,
            warn_below_free_bytes: 1024 * 1024 * 1024,
            warn_below_free_percent: 5,
            notify_webhook_url: String::new(),
            use_tracker_seed_rule: true,
            seed_safety_margin_hours: 6,
            partial_requires_ratio_one: false,
        }
    }
}

/// How much seeding the tracker still wants, by its own published formula.
///
/// From nCore's wiki (Ratio-free, hit'n'run), which states the rule as
///
/// ```text
/// remaining = (1 - ratio) * (48 + 0.4 * downloaded_GB) - hours_seeded
/// ```
///
/// with the obligation arising on any torrent from which at least 5% or at least 200 MB was
/// taken, whichever came first, and satisfiable either by returning as much as was taken
/// (ratio 1.0) or by seeding for that long. Both are per torrent.
///
/// Returned in seconds, zero meaning nothing is owed. Written out here rather than left to the
/// tracker's own answer because the tracker only recomputes on announce, every thirty to forty
/// minutes, and a sweep that runs in between would otherwise be working from a stale figure.
///
/// `downloaded` and `uploaded` are the tracker's figures for this torrent, in bytes.
pub fn seed_time_still_owed(downloaded: u64, uploaded: u64, seeded_secs: u64) -> u64 {
    // No obligation below both thresholds: 5% of the torrent cannot be known from these two
    // numbers, so the 200 MB floor is the one that can be checked, and it is the one that
    // triggers first on anything worth streaming.
    const OBLIGATION_FLOOR_BYTES: u64 = 200 * 1024 * 1024;
    if downloaded < OBLIGATION_FLOOR_BYTES {
        return 0;
    }
    let ratio = uploaded as f64 / downloaded.max(1) as f64;
    if ratio >= 1.0 {
        // Given back as much as was taken. That satisfies it on its own, at any time.
        return 0;
    }
    let gib = downloaded as f64 / 1_073_741_824.0;
    let required_hours = (1.0 - ratio) * (48.0 + 0.4 * gib);
    let required_secs = (required_hours * 3600.0).max(0.0) as u64;
    required_secs.saturating_sub(seeded_secs)
}

/// How much seeding one file of a torrent still owes on its own account.
///
/// The same formula as the torrent's, with the torrent's ratio but only this file's size:
/// `(1 - ratio) * (48h + 0.4h per this file's GB)`, minus how long this file has been finished.
///
/// This is the arithmetic behind rotating a pack. The tracker keeps one debt per torrent, so a
/// file's own share is not something it recognises; what makes the idea work is that the clock
/// keeps running on whatever is left. Paying a file's share and then letting it go means the
/// disk holds one episode instead of ten, while the torrent goes on paying with the one that
/// stayed. The torrent's whole debt is still settled before the last file is allowed to go, so
/// nothing here can turn into a hit and run.
pub fn file_seed_time_still_owed(
    file_bytes: u64,
    torrent_downloaded: u64,
    torrent_uploaded: u64,
    file_seeded_secs: u64,
) -> u64 {
    const OBLIGATION_FLOOR_BYTES: u64 = 200 * 1024 * 1024;
    if torrent_downloaded < OBLIGATION_FLOOR_BYTES {
        return 0;
    }
    let ratio = torrent_uploaded as f64 / torrent_downloaded.max(1) as f64;
    if ratio >= 1.0 {
        return 0;
    }
    let gib = file_bytes as f64 / 1_073_741_824.0;
    let required = ((1.0 - ratio) * (48.0 + 0.4 * gib) * 3600.0).max(0.0) as u64;
    required.saturating_sub(file_seeded_secs)
}

/// What the sweep knows about one download when deciding its fate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    /// Marked to keep in the interface.
    pub kept: bool,
    /// Someone got to the credits.
    pub watched: bool,
    /// The tracker itself lists this torrent as still owing seed time.
    pub owed_to_tracker: bool,
    /// The tracker was asked, it has figures for this torrent, and it says nothing is owed.
    ///
    /// All three parts matter. `!owed_to_tracker` on its own is also what an unanswered question
    /// looks like, and the answer must not predate the newest file taken from the torrent.
    ///
    /// And the tracker has to actually know the torrent. Measured on a real one: four episodes
    /// pulled from a pack forty minutes earlier, fourteen gigabytes in all, and the tracker's own
    /// page listed it nowhere and reported zero downloaded and zero uploaded against it. Absence
    /// from the hit-and-run list meant "no opinion yet", not "nothing owed", and treating the two
    /// alike is how an account collects its first hit and run. Its figures are the proof that it
    /// has seen the torrent at all, so without them there is no clear answer to act on.
    pub tracker_says_clear: bool,
    /// Files inside the torrent were deliberately skipped, so it can never be a complete
    /// seed. One episode out of a season pack is the usual case.
    pub partial: bool,
    /// A player is reading it right now.
    pub streaming: bool,
    /// How long the torrent has been seeding, across every file taken from it.
    pub seeded_secs: u64,
    /// Whether this file is the one holding the torrent open. The last file of a torrent is
    /// held to the torrent's whole debt; the others only to their own share.
    pub is_keeper: bool,
    /// This file's size, and how long it has been finished.
    pub file_bytes: u64,
    pub file_seeded_secs: u64,
    /// The tracker's own figures for the torrent, and whether they are known at all.
    ///
    /// Per torrent, because that is what the obligation is attached to: every episode taken
    /// out of one pack shares one debt.
    pub figures_known: bool,
    pub tracker_downloaded: u64,
    pub tracker_uploaded: u64,
}

/// The decision, with the reason. The reason is what gets logged and shown in the
/// interface, so "why is this still here" never needs guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Delete,
    Keep(&'static str),
}

impl Maintenance {
    /// Whether the automatic sweep may remove this download.
    ///
    /// Ordered so the most protective rule answers first, and every rule can veto.
    pub fn verdict(&self, c: &Candidate) -> Verdict {
        if !self.enable_deletion {
            return Verdict::Keep("az automatikus törlés ki van kapcsolva");
        }
        // Never touch something a player is reading, whatever the clock says. The
        // sweep runs in the evening, which is exactly when someone is watching.
        if c.streaming {
            return Verdict::Keep("épp játszik");
        }
        if c.kept {
            return Verdict::Keep("megtartásra jelölve");
        }
        if self.require_watched && !c.watched {
            return Verdict::Keep("még nem néztük meg");
        }
        // The tracker's own answer outranks our arithmetic, in both directions.
        //
        // Our formula exists to bridge the gap between announces, not to argue with a fresh
        // answer. It works from figures that are up to forty minutes old and it cannot see
        // everything the tracker counts: a torrent showing 48.56 GiB downloaded, nothing given
        // back and eighteen hours of seeding computes to fifty more hours owed, while the
        // tracker had already taken it off the list. Believing the arithmetic there means
        // keeping a download for two more days for no reason at all.
        if self.hit_and_run && c.tracker_says_clear {
            return Verdict::Delete;
        }
        if self.hit_and_run && !c.is_keeper {
            // Not the file holding the torrent open, so the tracker's list is not this file's
            // business: whatever is still owed goes on being paid by what stays behind. What
            // this file has to have done is its own share.
            let owed = file_seed_time_still_owed(
                c.file_bytes,
                c.tracker_downloaded,
                c.tracker_uploaded,
                c.file_seeded_secs,
            );
            if !c.figures_known {
                // Without the tracker's figures the share cannot be worked out, so the flat
                // setting stands in.
                if c.file_seeded_secs < self.keep_seed_seconds {
                    return Verdict::Keep("még nem seedeltünk eleget ezzel a fájllal");
                }
            } else if owed > 0 {
                return Verdict::Keep("ennek a fájlnak még hátravan a seedelése");
            }
            return Verdict::Delete;
        }
        if self.hit_and_run {
            // What the tracker itself says, first. Its list is the thing that decides whether
            // this becomes a hit and run, so being on it is the end of the discussion.
            let trust_tracker = !(c.partial && self.partial_uses_local_seed_time);
            if c.owed_to_tracker && trust_tracker {
                return Verdict::Keep("a tracker szerint még seedelni kell");
            }

            // A pack we hold one episode of: by the published definition of Status it is a
            // Leech for ever, but the tracker was watched treating it as a Seed and counting
            // its remaining time down, because what it goes on is the client's announce and
            // libtorrent reports nothing left once the unwanted files are deselected. So time
            // is allowed to settle the debt here too, and this branch is the cautious reading
            // for anyone who would rather not rely on that.
            if c.partial && !self.partial_requires_ratio_one {
                // Explicitly allowed to fall back on time instead.
            } else if c.partial {
                if !c.figures_known {
                    return Verdict::Keep("a trackertől még nincs adat erről a torrentről");
                }
                if c.tracker_uploaded < c.tracker_downloaded {
                    return Verdict::Keep("még nem osztottuk vissza a letöltött mennyiséget");
                }
            }

            if self.use_tracker_seed_rule && c.figures_known {
                // The tracker's own arithmetic, plus a margin: it recomputes on announce every
                // thirty to forty minutes, and it closes the month two to three hours early.
                let owed = seed_time_still_owed(
                    c.tracker_downloaded,
                    c.tracker_uploaded,
                    c.seeded_secs,
                );
                if owed > 0 {
                    return Verdict::Keep("a seedelési idő még nem telt le");
                }
                if c.seeded_secs < self.seed_safety_margin_hours * 3600 {
                    return Verdict::Keep("a ráhagyás ideje még nem telt le");
                }
            } else if c.seeded_secs < self.keep_seed_seconds {
                // No figures, so the flat setting decides. Deliberately the cautious branch:
                // without the tracker's numbers there is no way to work out what is owed.
                return Verdict::Keep("még nem seedeltünk eleget");
            }
        }
        Verdict::Delete
    }

    /// The configured sweep time as (hour, minute), falling back to 20:00 when the
    /// value is unusable, so a bad string cannot stop the sweep from ever running.
    pub fn sweep_time(&self) -> (u32, u32) {
        parse_hhmm(&self.sweep_at).unwrap_or((20, 0))
    }
}

/// `HH:MM` in 24-hour form.
pub fn parse_hhmm(text: &str) -> Option<(u32, u32)> {
    let (h, m) = text.trim().split_once(':')?;
    let hour: u32 = h.trim().parse().ok()?;
    let minute: u32 = m.trim().parse().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    Some((hour, minute))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Ncore {
    pub username: String,
    pub password: String,
    /// Some indexers need the whole torrent, not just the selected file.
    pub requires_full_download: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Filters {
    pub min_seeders: u64,
    /// Return only the single best match instead of every hit.
    pub only_best_match: bool,
    /// Most wanted first. Anything not listed is offered after everything that is,
    /// rather than hidden: a preference is not a filter.
    ///
    /// The names are the short ids from the media table: `2160p`, `1080p`, `720p`,
    /// `480p`, `bluray`, `remux`, `web-dl`, `webrip`, `bdrip`, `hdtv`, `hun`, `eng`.
    pub resolution_order: Vec<String>,
    pub source_order: Vec<String>,
    /// Spoken language, which is what `hun` and `eng` mean here.
    pub language_order: Vec<String>,
    /// Which of the three matters most when they disagree. First in this list wins.
    pub priority: Vec<String>,
}

impl Default for Filters {
    fn default() -> Self {
        Self {
            min_seeders: 1,
            only_best_match: false,
            resolution_order: vec!["2160p".into(), "1080p".into(), "720p".into()],
            source_order: vec!["remux".into(), "bluray".into(), "web-dl".into()],
            language_order: vec!["hun".into(), "eng".into()],
            // Language first: a film in the wrong language is not worth watching at any
            // resolution, whereas a lower resolution is merely worse.
            priority: vec!["language".into(), "resolution".into(), "source".into()],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Tmdb {
    pub api_key: String,
    pub language: String,
}

impl Default for Tmdb {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            language: "hu-HU".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Network {

    /// LAN address the clients reach us on.
    pub host_ip: String,
    /// Serve HTTPS as well as HTTP. Required for anything other than this machine:
    /// Stremio runs in a browser, and a page served over HTTPS may not fetch plain
    /// HTTP from an address that is not localhost.
    pub enable_https: bool,
    /// Port the HTTPS listener uses. Separate from the HTTP one because a single port
    /// cannot serve both.
    pub https_port: u16,
    /// Wildcard domain whose names encode private addresses, so `192.168.1.100` is
    /// reachable as `192-168-1-100.<domain>` with a publicly trusted certificate.
    pub cert_domain: String,
    /// Where the certificate and its deliberately public key are published.
    ///
    /// Two shapes work. Left alone, this URL is expected to return JSON holding both
    /// the chain and the key. If a provider serves two plain PEM files instead, put
    /// the chain here and the key in `cert_key_url`.
    pub cert_provider_url: String,
    /// Only for a provider that serves the key as a separate PEM file. Empty means the
    /// provider returns both in one JSON response.
    pub cert_key_url: String,
    /// Local copy, so a restart without internet still comes up with HTTPS.
    pub cert_cache_dir: String,
    /// Renew this many days before expiry. Early renewal costs nothing; running out
    /// takes the television offline.
    pub cert_renew_margin_days: u64,
    /// Name in front of the server when something else terminates TLS.
    pub reverse_proxy_domain: String,
}

impl Default for Network {
    fn default() -> Self {
        Self {
            host_ip: String::new(),
            enable_https: true,
            https_port: 3443,
            // The service the implementation being replaced uses, and which this
            // machine is already reachable through.
            cert_domain: "local-ip.medicmobile.org".into(),
            cert_provider_url: "https://local-ip.medicmobile.org/keys".into(),
            cert_key_url: String::new(),
            cert_cache_dir: in_base("certs"),
            cert_renew_margin_days: 21,
            reverse_proxy_domain: String::new(),
        }
    }
}

impl Network {
    /// Hostname the clients should use, or None when HTTPS is not set up.
    pub fn https_host(&self) -> Option<String> {
        if !self.enable_https {
            return None;
        }
        if !self.reverse_proxy_domain.trim().is_empty() {
            return Some(self.reverse_proxy_domain.trim().to_string());
        }
        crate::tls::local_ip_host(&self.host_ip, &self.cert_domain).ok()
    }

    pub fn renew_margin_secs(&self) -> i64 {
        (self.cert_renew_margin_days.max(1) * 86_400) as i64
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Auth {
    pub username: String,
    /// Never a plaintext password; filled on first setup.
    pub password_hash: String,
    pub session_secret: String,
    /// Opaque key the clients put in stream URLs.
    pub api_key: String,
}

impl Default for Auth {
    fn default() -> Self {
        Self {
            username: "admin".into(),
            password_hash: String::new(),
            session_secret: String::new(),
            api_key: String::new(),
        }
    }
}

impl Config {
    /// Loads the file, creating it with defaults when it does not exist yet.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            let cfg = Self::default();
            cfg.save(path)
                .with_context(|| format!("creating {}", path.display()))?;
            tracing::info!(path = %path.display(), "wrote a default config");
            return Ok(cfg);
        }

        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: Self = toml::from_str(crate::state::strip_bom(&text))
            .with_context(|| format!("parsing {}", path.display()))?;
        tracing::info!(path = %path.display(), "config loaded");
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("serialising config")?;
        // Written via a temporary file so a crash mid-write cannot leave a
        // truncated config behind.
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    /// Which configuration file this run uses.
    ///
    /// `STREMHU_CONFIG` wins. Otherwise it is `config.toml` beside the executable, and
    /// failing that the same name in a folder above it.
    ///
    /// The search upwards is there for one specific mistake with real consequences. A build
    /// leaves a second copy of the executable in `target/release`, and run from there, with
    /// nothing beside it, it would write itself a brand new configuration: no tracker
    /// account, a different key, an empty record of what is on the disk, and a second server
    /// competing for the same ports. Finding the install's own file instead is the difference
    /// between running the same server and quietly starting a stranger.
    ///
    /// A file found above is only accepted if it is recognisably ours, so an unrelated
    /// `config.toml` belonging to some other program cannot be picked up and then written to.
    pub fn path_from_env() -> PathBuf {
        if let Ok(from_env) = std::env::var("STREMHU_CONFIG") {
            if !from_env.trim().is_empty() {
                return PathBuf::from(from_env);
            }
        }

        let beside = base_dir().join("config.toml");
        if beside.is_file() {
            return beside;
        }

        let mut dir = base_dir();
        for _ in 0..3 {
            let Some(parent) = dir.parent().map(Path::to_path_buf) else {
                break;
            };
            let candidate = parent.join("config.toml");
            if candidate.is_file() && looks_like_our_config(&candidate) {
                return candidate;
            }
            dir = parent;
        }

        // Nothing found: a fresh install, and the file will be created beside the executable.
        beside
    }

    /// Environment overrides for the two secrets, so they can stay out of the file
    /// if that is preferred. Everything else is config-only by design.
    pub fn apply_env_overrides(&mut self) {
        if let Ok(v) = std::env::var("NCORE_USERNAME") {
            if !v.is_empty() {
                self.ncore.username = v;
            }
        }
        if let Ok(v) = std::env::var("NCORE_PASSWORD") {
            if !v.is_empty() {
                self.ncore.password = v;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `config.toml` found in a folder above is only used if it is ours. The alternative
    /// is a run from `target/release` adopting, and then overwriting, an unrelated file.
    #[test]
    fn only_our_own_config_is_adopted_from_a_folder_above() {
        let dir = std::env::temp_dir().join("stremhu-config-sniff");
        let _ = std::fs::create_dir_all(&dir);

        let ours = dir.join("ours.toml");
        std::fs::write(&ours, "[ncore]\nusername = \"someone\"\n").expect("writes");
        assert!(looks_like_our_config(&ours));

        let theirs = dir.join("theirs.toml");
        std::fs::write(&theirs, "[build]\ntarget = \"x86\"\n").expect("writes");
        assert!(!looks_like_our_config(&theirs));

        assert!(!looks_like_our_config(&dir.join("absent.toml")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The environment wins over everything, including a file sitting beside the executable.
    #[test]
    fn the_environment_variable_wins() {
        // Serialised through the same variable other tests do not touch, and restored after.
        let before = std::env::var("STREMHU_CONFIG").ok();
        unsafe { std::env::set_var("STREMHU_CONFIG", "X:/somewhere/else.toml") };
        assert_eq!(Config::path_from_env(), PathBuf::from("X:/somewhere/else.toml"));
        // An empty value is treated as not set, not as an empty filename.
        unsafe { std::env::set_var("STREMHU_CONFIG", "  ") };
        assert_ne!(Config::path_from_env(), PathBuf::from("  "));
        match before {
            Some(v) => unsafe { std::env::set_var("STREMHU_CONFIG", v) },
            None => unsafe { std::env::remove_var("STREMHU_CONFIG") },
        }
    }

    #[test]
    fn defaults_round_trip_through_toml() {
        let cfg = Config::default();
        let text = toml::to_string_pretty(&cfg).expect("serialises");
        let back: Config = toml::from_str(&text).expect("parses");
        assert_eq!(cfg, back);
    }

    /// A hand-trimmed file must still load; this is why every field has a default.
    #[test]
    fn a_partial_file_fills_in_defaults() {
        let text = r#"
            [server]
            port = 9999

            [maintenance]
            cache_retention_seconds = 60
        "#;
        let cfg: Config = toml::from_str(text).expect("parses");
        assert_eq!(cfg.server.port, 9999);
        assert_eq!(cfg.maintenance.cache_retention_seconds, 60);
        // Untouched values keep their defaults.
        assert_eq!(cfg.server.listen_addr, "0.0.0.0");
        assert!(cfg.maintenance.hit_and_run);
        assert_eq!(cfg.streaming.chunk_size_bytes, 1_048_576);
        assert_eq!(cfg.filters.resolution_order[0], "2160p");
    }

    #[test]
    fn an_empty_file_is_all_defaults() {
        let cfg: Config = toml::from_str("").expect("parses");
        assert_eq!(cfg, Config::default());
    }

    /// A film has to arrive complete, and a season pack must fetch only the chosen
    /// episode. File selection covers both, so piece-level starvation stays off.
    #[test]
    fn the_selected_file_downloads_in_full_by_default() {
        let p = Config::default().pieces;
        assert!(p.idle_priority > 0, "priority zero would never complete");
    }

    /// A default that names a quality the media table does not know would order
    /// nothing, silently.
    #[test]
    fn the_default_orders_name_real_qualities() {
        let f = Filters::default();
        let known = crate::media::known_ids();
        for list in [&f.resolution_order, &f.source_order, &f.language_order] {
            for id in list {
                assert!(
                    known.contains(&id.as_str()),
                    "{id:?} is not a known quality id"
                );
            }
        }
        // And the priority list may only name aspects that exist.
        for aspect in &f.priority {
            assert!(
                ["language", "resolution", "source"].contains(&aspect.as_str()),
                "{aspect:?} is not an orderable aspect"
            );
        }
    }

    #[test]
    fn deletion_is_off_until_asked_for() {
        // Losing seeding data on a private tracker is not free to undo.
        assert!(!Config::default().maintenance.enable_deletion);
    }

    /// The retention numbers of the implementation being replaced.
    #[test]
    fn retention_matches_the_implementation_being_replaced() {
        let m = Config::default().maintenance;
        assert_eq!(m.keep_seed_seconds, 10 * 24 * 3600, "ten days of seeding");
        assert_eq!(
            m.cache_retention_seconds,
            14 * 24 * 3600,
            "fourteen days of retention"
        );
        assert!(m.hit_and_run);
    }

    fn deleting() -> Maintenance {
        Maintenance {
            enable_deletion: true,
            ..Maintenance::default()
        }
    }

    /// An old, watched, long-seeded download the tracker no longer wants: the only
    /// case that actually deletes.
    fn ripe() -> Candidate {
        Candidate {
            kept: false,
            watched: true,
            owed_to_tracker: false,
            // Not answered either way, so the arithmetic below is what decides. The tests that
            // exercise the tracker's own answer set this themselves.
            tracker_says_clear: false,
            partial: false,
            streaming: false,
            seeded_secs: 11 * 24 * 3600,
            // One file, so it is the one holding the torrent open and the torrent's whole debt
            // is what it has to answer for.
            is_keeper: true,
            file_bytes: 7 * 1024 * 1024 * 1024,
            file_seeded_secs: 11 * 24 * 3600,
            // Seven gigabytes taken and seven given back: ratio 1.0, which satisfies the
            // obligation on its own by the tracker's own rules.
            figures_known: true,
            tracker_downloaded: 7 * 1024 * 1024 * 1024,
            tracker_uploaded: 7 * 1024 * 1024 * 1024,
        }
    }

    /// A tracker that has never heard of a torrent is not a tracker saying the debt is paid.
    ///
    /// The numbers are from the real case: four episodes taken from a pack forty minutes
    /// earlier, and the tracker's page carried no row for it at all, so nothing downloaded and
    /// nothing uploaded were recorded against it. The rule that trusts a clear answer would have
    /// deleted all four, which is precisely how a hit and run happens.
    #[test]
    fn silence_from_the_tracker_is_not_a_clear_answer() {
        let fresh = Candidate {
            // No figures, because the torrent never appeared on the tracker's page.
            figures_known: false,
            tracker_downloaded: 0,
            tracker_uploaded: 0,
            tracker_says_clear: false,
            owed_to_tracker: false,
            seeded_secs: 40 * 60,
            file_seeded_secs: 40 * 60,
            is_keeper: true,
            ..ripe()
        };
        assert_eq!(
            deleting().verdict(&fresh),
            Verdict::Keep("még nem seedeltünk eleget"),
            "with nothing known, the flat setting is what protects it"
        );

        // The same file as a non-keeper is held by its own flat fallback.
        let sibling = Candidate {
            is_keeper: false,
            ..fresh
        };
        assert_eq!(
            deleting().verdict(&sibling),
            Verdict::Keep("még nem seedeltünk eleget ezzel a fájllal")
        );
    }

    /// The tracker's own answer settles it, and this is the case that made the rule.
    ///
    /// Real figures from the account: 48.56 GiB counted as downloaded on that torrent, nothing
    /// given back, eighteen hours of seeding. The formula computes fifty more hours owed, while
    /// the tracker had already taken the torrent off its list. The tracker is the thing that
    /// decides whether this becomes a hit and run, so its answer wins.
    #[test]
    fn a_clear_answer_from_the_tracker_settles_it() {
        const GIB: u64 = 1024 * 1024 * 1024;
        let watched_and_seeded = Candidate {
            tracker_downloaded: 48 * GIB + 573 * GIB / 1024,
            tracker_uploaded: 0,
            seeded_secs: 18 * 3600,
            file_seeded_secs: 18 * 3600,
            ..ripe()
        };

        // Our own arithmetic, on its own, would hold it for another two days.
        assert!(
            seed_time_still_owed(watched_and_seeded.tracker_downloaded, 0, 18 * 3600) > 40 * 3600
        );
        assert_eq!(
            deleting().verdict(&watched_and_seeded),
            Verdict::Keep("a seedelési idő még nem telt le"),
            "without an answer from the tracker the arithmetic is all there is"
        );

        // With the answer, it goes.
        let answered = Candidate {
            tracker_says_clear: true,
            ..watched_and_seeded
        };
        assert_eq!(deleting().verdict(&answered), Verdict::Delete);

        // A clear answer does not override the things that are not about seeding.
        assert_eq!(
            deleting().verdict(&Candidate { watched: false, ..answered }),
            Verdict::Keep("még nem néztük meg")
        );
        assert_eq!(
            deleting().verdict(&Candidate { kept: true, ..answered }),
            Verdict::Keep("megtartásra jelölve")
        );
        assert_eq!(
            deleting().verdict(&Candidate { streaming: true, ..answered }),
            Verdict::Keep("épp játszik")
        );
    }

    /// Rotating a pack: a file that has served its own share goes, while the newest one stays to
    /// carry on paying the torrent's debt. This is the whole point of the arrangement, so the
    /// numbers are the real ones from a season pack.
    #[test]
    fn a_file_that_served_its_own_share_can_go_while_the_keeper_stays() {
        const GIB: u64 = 1024 * 1024 * 1024;
        // Two episodes taken from one pack: the tracker counts 15 GiB against the torrent.
        let base = Candidate {
            watched: true,
            partial: true,
            figures_known: true,
            tracker_downloaded: 15 * GIB,
            tracker_uploaded: 0,
            // The torrent has been seeding for two days, so its own debt is not settled yet:
            // 48 + 0.4 * 15 = 54 hours.
            seeded_secs: 48 * 3600,
            owed_to_tracker: true,
            ..ripe()
        };

        // The first episode, finished 52 hours ago. Its own share is 48 + 0.4 * 7.5 = 51 hours.
        let served = Candidate {
            is_keeper: false,
            file_bytes: 15 * GIB / 2,
            file_seeded_secs: 52 * 3600,
            ..base
        };
        assert_eq!(
            deleting().verdict(&served),
            Verdict::Delete,
            "its own share is served, and the pack goes on paying with what is left"
        );

        // The same file ten hours in: not yet.
        let early = Candidate {
            file_seeded_secs: 10 * 3600,
            ..served
        };
        assert_eq!(
            deleting().verdict(&early),
            Verdict::Keep("ennek a fájlnak még hátravan a seedelése")
        );

        // The keeper, with the same numbers, answers for the whole torrent instead, and the
        // tracker still lists it.
        let keeper = Candidate {
            is_keeper: true,
            file_seeded_secs: 52 * 3600,
            ..served
        };
        assert_eq!(
            deleting().verdict(&keeper),
            Verdict::Keep("a tracker szerint még seedelni kell"),
            "the last file is never taken while the torrent still owes"
        );
    }

    /// A file's own share is smaller than the torrent's whole debt, which is what makes the
    /// disk saving possible, and it shrinks as the ratio rises exactly as the tracker's does.
    #[test]
    fn a_files_share_follows_its_own_size() {
        const GIB: u64 = 1024 * 1024 * 1024;
        // 7 GiB file on a torrent that has taken 70 GiB: the file owes 48 + 2.8 = 50.8 hours,
        // the torrent owes 48 + 28 = 76 hours.
        assert_eq!(file_seed_time_still_owed(7 * GIB, 70 * GIB, 0, 0) / 3600, 50);
        assert_eq!(seed_time_still_owed(70 * GIB, 0, 0) / 3600, 76);

        // Half given back halves both.
        assert_eq!(file_seed_time_still_owed(7 * GIB, 70 * GIB, 35 * GIB, 0) / 3600, 25);
        // Given back in full, nothing is owed by anything.
        assert_eq!(file_seed_time_still_owed(7 * GIB, 70 * GIB, 70 * GIB, 0), 0);
        // Below the floor there is no obligation to share out.
        assert_eq!(file_seed_time_still_owed(7 * GIB, 100 * 1024 * 1024, 0, 0), 0);
    }

    /// The tracker's own formula, with its own published numbers.
    ///
    /// From the wiki: the obligation arises above 200 MB, is satisfied outright at ratio 1.0,
    /// and otherwise wants `(1 - ratio) * (48h + 0.4h per downloaded GB)` of seeding.
    #[test]
    fn the_seeding_requirement_follows_the_trackers_formula() {
        const GIB: u64 = 1024 * 1024 * 1024;

        // Nothing given back on a 7 GiB episode: 48 + 0.4 * 7 = 50.8 hours.
        let owed = seed_time_still_owed(7 * GIB, 0, 0);
        assert_eq!(owed / 3600, 50, "about fifty-one hours");

        // Half given back halves the requirement.
        let half = seed_time_still_owed(7 * GIB, 7 * GIB / 2, 0);
        assert_eq!(half / 3600, 25);

        // Given back in full: nothing owed, whatever the clock says.
        assert_eq!(seed_time_still_owed(7 * GIB, 7 * GIB, 0), 0);
        assert_eq!(seed_time_still_owed(7 * GIB, 8 * GIB, 0), 0);

        // Time already seeded counts against it, and it never goes below zero.
        assert_eq!(seed_time_still_owed(7 * GIB, 0, 40 * 3600) / 3600, 10);
        assert_eq!(seed_time_still_owed(7 * GIB, 0, 100 * 3600), 0);

        // Below the 200 MB floor there is no obligation at all.
        assert_eq!(seed_time_still_owed(100 * 1024 * 1024, 0, 0), 0);
        assert_eq!(seed_time_still_owed(0, 0, 0), 0);

        // A large download asks for proportionally more: 48 + 0.4 * 26 = 58.4 hours.
        assert_eq!(seed_time_still_owed(26 * GIB, 0, 0) / 3600, 58);
    }

    /// A download whose time is not up yet is kept, and the reason says which rule held it.
    #[test]
    fn the_formula_holds_a_download_that_has_not_seeded_long_enough() {
        const GIB: u64 = 1024 * 1024 * 1024;
        let c = Candidate {
            tracker_uploaded: 0,
            seeded_secs: 10 * 3600,
            ..ripe()
        };
        assert_eq!(
            deleting().verdict(&c),
            Verdict::Keep("a seedelési idő még nem telt le")
        );

        // Past the requirement and past the margin: deletable.
        let done = Candidate {
            tracker_uploaded: 0,
            tracker_downloaded: 7 * GIB,
            seeded_secs: 60 * 3600,
            ..ripe()
        };
        assert_eq!(deleting().verdict(&done), Verdict::Delete);

        // Met on paper but inside the safety margin, which exists because the tracker's
        // figures are up to forty minutes old and it closes the month hours early.
        let fresh = Candidate {
            tracker_uploaded: 7 * GIB,
            seeded_secs: 3600,
            ..ripe()
        };
        assert_eq!(
            deleting().verdict(&fresh),
            Verdict::Keep("a ráhagyás ideje még nem telt le")
        );
    }

    /// By default a pack's debt is settled the same way any other is: by time or by ratio.
    /// This is the observed behaviour, not the published rule, and the test says which.
    #[test]
    fn by_default_time_settles_a_partial_torrents_debt_too() {
        const GIB: u64 = 1024 * 1024 * 1024;
        let c = Candidate {
            partial: true,
            tracker_downloaded: 7 * GIB,
            tracker_uploaded: 0,
            // Past the 50.8 hours the formula asks for, and past the margin.
            seeded_secs: 60 * 3600,
            ..ripe()
        };
        assert_eq!(deleting().verdict(&c), Verdict::Delete);

        // And short of it, it is held.
        let young = Candidate {
            seeded_secs: 10 * 3600,
            ..c
        };
        assert_eq!(
            deleting().verdict(&young),
            Verdict::Keep("a seedelési idő még nem telt le")
        );
    }

    /// One episode out of a pack never makes the torrent 100%, so the tracker lists it as
    /// Leech for ever. For those the requirement is the other one the rules give: give back as
    /// much as was taken.
    #[test]
    fn a_partial_torrent_has_to_give_back_what_it_took() {
        const GIB: u64 = 1024 * 1024 * 1024;
        // The cautious reading, which is a setting rather than the default: the tracker was
        // watched counting a partial torrent's seeding time down, so by default time settles
        // this debt like any other.
        let deleting = || Maintenance {
            enable_deletion: true,
            partial_requires_ratio_one: true,
            ..Maintenance::default()
        };
        let c = Candidate {
            partial: true,
            tracker_downloaded: 7 * GIB,
            tracker_uploaded: 3 * GIB,
            seeded_secs: 400 * 24 * 3600,
            ..ripe()
        };
        assert_eq!(
            deleting().verdict(&c),
            Verdict::Keep("még nem osztottuk vissza a letöltött mennyiséget"),
            "no amount of time settles a partial torrent's debt"
        );

        // Given back in full, it can go.
        let repaid = Candidate {
            tracker_uploaded: 7 * GIB,
            ..c
        };
        assert_eq!(deleting().verdict(&repaid), Verdict::Delete);

        // And with no figures at all, nothing is assumed.
        let unknown = Candidate {
            figures_known: false,
            ..c
        };
        assert_eq!(
            deleting().verdict(&unknown),
            Verdict::Keep("a trackertől még nincs adat erről a torrentről")
        );
    }

    #[test]
    fn an_old_watched_and_fully_seeded_download_is_deletable() {
        assert_eq!(deleting().verdict(&ripe()), Verdict::Delete);
    }

    #[test]
    fn something_never_watched_is_never_deleted() {
        let c = Candidate {
            watched: false,
            // Ancient and long since seeded, but never played.
            seeded_secs: 400 * 24 * 3600,
            ..ripe()
        };
        assert_eq!(deleting().verdict(&c), Verdict::Keep("még nem néztük meg"));
    }

    /// The tracker's own answer outranks every local clock: this is what protects the
    /// account.
    #[test]
    fn an_obligation_the_tracker_reports_blocks_deletion() {
        let c = Candidate {
            owed_to_tracker: true,
            seeded_secs: 400 * 24 * 3600,
            ..ripe()
        };
        assert_eq!(
            deleting().verdict(&c),
            Verdict::Keep("a tracker szerint még seedelni kell")
        );
    }

    /// With no figures from the tracker there is nothing to compute a requirement from, so the
    /// flat setting decides and it decides cautiously.
    #[test]
    fn without_tracker_figures_the_flat_setting_blocks_deletion() {
        // Nine days seeded against a ten day setting.
        let c = Candidate {
            figures_known: false,
            seeded_secs: 9 * 24 * 3600,
            ..ripe()
        };
        assert_eq!(
            deleting().verdict(&c),
            Verdict::Keep("még nem seedeltünk eleget")
        );

        // Past it, and the retention window too, it can go.
        let older = Candidate {
            seeded_secs: 11 * 24 * 3600,
            ..c
        };
        assert_eq!(deleting().verdict(&older), Verdict::Delete);
    }

    /// The retention window is no longer a condition on deleting a download: what decides is
    /// the seeding obligation, which is the rule the account is actually judged by. The setting
    /// stays for the one job it still has, clearing away `.torrent` files nothing refers to.
    #[test]
    fn the_retention_window_no_longer_holds_a_download() {
        let m = deleting();
        assert_eq!(m.cache_retention_seconds, 14 * 24 * 3600, "the setting is still there");
        // A download that has met its seeding obligation goes at once, however new it is.
        let fresh = Candidate {
            seeded_secs: 60 * 3600,
            file_seeded_secs: 60 * 3600,
            ..ripe()
        };
        assert_eq!(m.verdict(&fresh), Verdict::Delete);
    }

    #[test]
    fn a_kept_item_survives_everything() {
        let c = Candidate { kept: true, ..ripe() };
        assert_eq!(deleting().verdict(&c), Verdict::Keep("megtartásra jelölve"));
    }

    /// The sweep runs in the evening, which is when someone is watching.
    #[test]
    fn something_being_played_is_never_deleted_mid_stream() {
        let c = Candidate {
            streaming: true,
            kept: false,
            ..ripe()
        };
        assert_eq!(
            deleting().verdict(&c),
            Verdict::Keep("épp játszik")
        );
    }

    #[test]
    fn the_master_switch_blocks_everything() {
        let m = Maintenance::default(); // enable_deletion is false
        assert_eq!(
            m.verdict(&ripe()),
            Verdict::Keep("az automatikus törlés ki van kapcsolva")
        );
    }

    /// One episode out of a season pack can never be a complete seed, so the tracker's
    /// obligation for it may never clear. Waiting for something that cannot happen would
    /// keep the episode forever, which is what the setting exists to avoid.
    #[test]
    fn a_partial_download_can_fall_back_to_our_own_seeding_time() {
        let stuck = Candidate {
            owed_to_tracker: true,
            partial: true,
            ..ripe()
        };
        // Off by default: the tracker's answer is the better one where it works, and
        // measuring this tracker showed that it does work.
        assert_eq!(
            deleting().verdict(&stuck),
            Verdict::Keep("a tracker szerint még seedelni kell")
        );

        let m = Maintenance {
            partial_uses_local_seed_time: true,
            ..deleting()
        };
        assert_eq!(m.verdict(&stuck), Verdict::Delete);

        // A complete torrent still obeys the tracker, whatever the setting says: there the
        // obligation can be waited out, so waiting is the right thing.
        let complete = Candidate {
            partial: false,
            ..stuck
        };
        assert_eq!(
            m.verdict(&complete),
            Verdict::Keep("a tracker szerint még seedelni kell")
        );

        // With the tracker's formula switched off, the flat setting is what has to be served,
        // even for a partial one.
        let flat = Maintenance {
            use_tracker_seed_rule: false,
            ..m.clone()
        };
        let young = Candidate {
            seeded_secs: 2 * 24 * 3600,
            ..stuck
        };
        assert_eq!(
            flat.verdict(&young),
            Verdict::Keep("még nem seedeltünk eleget")
        );
    }

    /// Turning hit-and-run off lets a watched, expired download go without waiting
    /// out the seed time or asking the tracker.
    #[test]
    fn without_hit_and_run_only_age_and_watching_matter() {
        let m = Maintenance {
            hit_and_run: false,
            ..deleting()
        };
        let c = Candidate {
            owed_to_tracker: true,
            seeded_secs: 0,
            ..ripe()
        };
        assert_eq!(m.verdict(&c), Verdict::Delete);
        assert_eq!(
            m.verdict(&Candidate { watched: false, ..c }),
            Verdict::Keep("még nem néztük meg")
        );
    }

    /// The default is the evening time that was asked for, and a broken value must
    /// not stop the sweep from running at all.
    #[test]
    fn the_sweep_runs_in_the_evening_by_default() {
        assert_eq!(Maintenance::default().sweep_time(), (20, 0));
        let bad = Maintenance {
            sweep_at: "not a time".into(),
            ..Maintenance::default()
        };
        assert_eq!(bad.sweep_time(), (20, 0));
    }

    #[test]
    fn sweep_times_are_parsed_and_validated() {
        assert_eq!(parse_hhmm("20:00"), Some((20, 0)));
        assert_eq!(parse_hhmm("03:05"), Some((3, 5)));
        assert_eq!(parse_hhmm(" 7:30 "), Some((7, 30)));
        assert_eq!(parse_hhmm("00:00"), Some((0, 0)));
        assert_eq!(parse_hhmm("23:59"), Some((23, 59)));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("20:60"), None);
        assert_eq!(parse_hhmm("2000"), None);
        assert_eq!(parse_hhmm(""), None);
    }

    /// The window follows the torrent's piece size, so it must be built per torrent.
    #[test]
    fn the_piece_window_is_derived_from_the_piece_size() {
        let p = Config::default().pieces;
        // 64 MB of readahead: 32 pieces of 2 MiB, 128 of half a megabyte. The same amount
        // of film either way, which is what a viewer notices.
        assert_eq!(p.to_policy(2 * 1024 * 1024).prefetch_pieces, 32);
        assert_eq!(p.to_policy(512 * 1024).prefetch_pieces, 128);
        let policy = p.to_policy(2 * 1024 * 1024);
        assert_eq!(policy.head_deadline_ms, 0, "the head is due now");
        assert_eq!(policy.base_deadline_ms, 2000);
        assert_eq!(policy.deadline_step_ms, 1000);
    }

    /// The qBittorrent section is gone; a file still carrying it must not fail to
    /// load, because the user's own config has one.
    #[test]
    fn a_leftover_qbittorrent_section_is_ignored() {
        let text = r#"
            [qbittorrent]
            url = "http://localhost:8080"
            save_path = "D:/stremhu-rs/downloads"

            [torrent]
            save_path = "D:/stremhu-rs/downloads"
        "#;
        let cfg: Config = toml::from_str(text).expect("unknown sections are ignored");
        assert_eq!(cfg.torrent.save_path, "D:/stremhu-rs/downloads");
    }

    #[test]
    fn save_then_load_preserves_edits() {
        let dir = std::env::temp_dir().join("stremhu-rs-cfg-test");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("config.toml");
        let _ = std::fs::remove_file(&path);

        let mut cfg = Config::default();
        cfg.server.port = 4321;
        cfg.ncore.username = "someone".into();
        cfg.filters.min_seeders = 7;
        cfg.save(&path).expect("saves");

        let back = Config::load(&path).expect("loads");
        assert_eq!(back.server.port, 4321);
        assert_eq!(back.ncore.username, "someone");
        assert_eq!(back.filters.min_seeders, 7);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_creates_a_default_file_when_missing() {
        let dir = std::env::temp_dir().join("stremhu-rs-cfg-test-new");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");

        let cfg = Config::load(&path).expect("creates and loads");
        assert!(path.exists(), "the file has to be written out");
        assert_eq!(cfg, Config::default());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
