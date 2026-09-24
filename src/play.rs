//! Serving a file over HTTP while it is still downloading.
//!
//! The feedback loop lives here: every response body reports where its reader has got to,
//! and the library's deadline loop aims the download just ahead of it.

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::app::*;
use crate::config::Config;
use crate::library::{Entry, Want};
use crate::parse_range;
use crate::series::SeasonEpisode;

use crate::http::authorised;

pub(crate) async fn play_movie(
    State(state): State<Arc<AppState>>,
    Path((api_key, torrent_id)): Path<(String, String)>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    play(state, api_key, torrent_id, Want::LargestFile, method, headers).await
}

pub(crate) async fn play_episode(
    State(state): State<Arc<AppState>>,
    Path((api_key, torrent_id, season, episode)): Path<(String, String, u32, u32)>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    play(
        state,
        api_key,
        torrent_id,
        Want::Episode(SeasonEpisode { season, episode }),
        method,
        headers,
    )
    .await
}

pub(crate) async fn play(
    state: Arc<AppState>,
    api_key: String,
    torrent_id: String,
    want: Want,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let cfg = state.config().await;
    if !authorised(&cfg, &api_key) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    // The cold-start clock. Every stage below reports how long it took, because "playback
    // took ten seconds" is not a finding and "the tracker fetch took 1.75 of them" is.
    let began = std::time::Instant::now();
    let Some(source) = state.source_for(&torrent_id).await else {
        // Only reachable if a play URL is opened without the stream list having been
        // fetched first, for example a stale bookmark after a restart.
        tracing::warn!(torrent_id = %torrent_id, "no cached source; ask for the stream list first");
        return (
            StatusCode::NOT_FOUND,
            "unknown torrent id; open the title in Stremio again\n",
        )
            .into_response();
    };

    // Already playing this, or played it before: serve it and touch nothing else. A player
    // opens a new request for every seek and every time its buffer drains, and each one used to
    // pay for a .torrent download from the tracker plus a measurement of both disks before the
    // first byte went out.
    if let Some(entry) = state.already_open(&torrent_id, &want).await {
        return range_response(state, entry, &cfg, method, headers);
    }

    // The .torrent already on the disk, when there is one.
    //
    // Every torrent that has been played is written into the torrents folder, and the second
    // episode of a season pack is the same torrent as the first. Asking the tracker for a file
    // we are already holding is a network round trip in front of a waiting viewer, and it is
    // the largest single item in a cold start before the first piece is even requested:
    // measured at 1.75 seconds on a live start against nCore.
    let cached = local_torrent_for(&state, &torrent_id).await;
    let from_disk = cached.is_some();
    let looked_on_disk = began.elapsed();
    // Fetched with the session that belongs to the site the link came from. A download URL
    // carries an account's passkey, so the two are not interchangeable: asking nCore for a
    // BitHUmen link would send one account's cookies to the other site and get a login page
    // back for the trouble.
    let fetched = if let Some(bytes) = cached {
        Ok(bytes)
    } else {
        match source.tracker {
        crate::tracker::Tracker::Ncore => {
            state
                .ncore
                .read()
                .await
                .download_torrent(&source.download_url)
                .await
        }
        crate::tracker::Tracker::Bithumen => match state.bithumen.read().await.as_ref() {
            Some(client) => client.download_torrent(&source.download_url).await,
            None => Err(anyhow::anyhow!(
                "BitHUmen is switched off, so this stream cannot be fetched"
            )),
        },
        }
    };
    let bytes = match fetched {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                error = %e,
                tracker = source.tracker.id(),
                "could not fetch the .torrent"
            );
            return (
                StatusCode::BAD_GATEWAY,
                format!("{}: {e}\n", source.tracker.label()),
            )
                .into_response();
        }
    };

    let have_torrent = began.elapsed();

    // How much will actually be written, which is not the size of the torrent.
    //
    // This was the cause of a real failure: a complete-series pack of 1.33 TiB was refused for
    // want of room on a disk with 127 GiB free, when the episode being watched is five
    // gigabytes and the pack is never downloaded. The tracker's size is the whole torrent; what
    // matters is the one file plus whatever small companions come with it. Reading the .torrent
    // here is what makes that knowable before the torrent is opened, because opening it is
    // already telling libtorrent where to write.
    let parsed = crate::engine::parse_torrent(&bytes);
    let parsed_hash = parsed.as_ref().ok().map(|t| t.info_hash.clone());
    let needed = match &parsed {
        Ok(info) => match info.files() {
            Ok(files) => match crate::library::select_file(&files, &want) {
                Ok(selected) => {
                    let companions = crate::stream_policy::extras_worth_completing(
                        &files.iter().map(|f| f.size).collect::<Vec<u64>>(),
                        &[selected],
                        cfg.torrent.complete_extras_below_bytes,
                    );
                    let extra: u64 = companions.iter().map(|i| files[*i].size).sum();
                    files[selected].size.saturating_add(extra)
                }
                Err(e) => {
                    // No file in there matches what was asked for. Saying so now is better than
                    // choosing a disk for a download that cannot happen.
                    tracing::warn!(error = %e, "this torrent has nothing to play");
                    return (StatusCode::UNPROCESSABLE_ENTITY, format!("{e}\n")).into_response();
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "could not read the file list; using the torrent size");
                source.size_bytes
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "could not read the .torrent; using the torrent size");
            source.size_bytes
        }
    };

    // A torrent already on the disk keeps the folder it was started in. libtorrent holds one
    // save path per torrent, so a second episode of a pack cannot be sent to the other disk
    // however full the first one is: it would be written where the pack already lives, and the
    // failure would arrive as a write error from inside the engine rather than as an answer.
    // Asked here instead, against the folder that will really be used.
    let existing_dir = match &parsed_hash {
        Some(hash) => state.lib.save_dir_for(hash).await,
        None => None,
    };

    // Before any of that: if this will not fit, clear up first. Falling over to the second disk
    // or refusing the request are both answers to a full disk, and neither is the right one
    // while the disk holds files that have already served their seeding time. For a torrent
    // that is already here there is no second disk to fall over to, so this is the only answer
    // there is.
    if cfg.maintenance.sweep_when_full {
        let target = existing_dir
            .clone()
            .unwrap_or_else(|| cfg.torrent.save_path.clone());
        crate::app::make_room_for(&state, &target, needed).await;
    }

    if let Some(existing) = existing_dir
        && let Err(e) = crate::disk::space_at_least(&existing, needed) {
            let message = format!("Nincs hely a torrent saját mappájában: {e}");
            tracing::error!("{message}");
            if cfg.maintenance.notify_disk {
                state.notify_occasionally("no-room-existing", &message).await;
            }
            return (StatusCode::INSUFFICIENT_STORAGE, format!("{message}
")).into_response();
        }

    // Which disk, decided here so the answer can be acted on: a fall-over to the second disk
    // and a refusal for want of room are both things the owner should hear about at the
    // moment they happen, not in tomorrow's report.
    let save_dir = match crate::disk::choose(
        &cfg.torrent.save_path,
        &cfg.torrent.save_path_secondary,
        needed,
    ) {
        Ok(dir) => {
            let dir = dir.to_string_lossy().to_string();
            if dir != cfg.torrent.save_path {
                let message = format!(
                    "Az elsődleges lemez megtelt, a letöltés a másodlagosra megy: {dir}"
                );
                tracing::warn!("{message}");
                if cfg.maintenance.notify_disk {
                    state.notify_occasionally("secondary", &message).await;
                }
            }
            dir
        }
        Err(e) => {
            let message = format!("Nincs hely a letöltéshez: {e}");
            tracing::error!("{message}");
            // A player retries a refused stream several times, and each retry is not news.
            if cfg.maintenance.notify_disk {
                state.notify_occasionally("no-room", &message).await;
            }
            return (StatusCode::INSUFFICIENT_STORAGE, format!("{message}
")).into_response();
        }
    };

    // The library answers with the record's key, not the info hash: one torrent can serve
    // several files, so the key carries the file index too. What goes into the record is the
    // hash itself, from the entry, or the key ends up with the index in it twice.
    let disks_decided = began.elapsed();

    let (_key, entry) = match state.lib.add(&bytes, want, &save_dir).await {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!(error = %e, "could not open the torrent");
            return (StatusCode::UNPROCESSABLE_ENTITY, format!("{e}\n")).into_response();
        }
    };

    let opened = began.elapsed();

    // A new download is when the free space actually changes, so this is when it is worth
    // looking. The warning is rate limited, so it cannot turn into a message per film.
    state.check_disk_space().await;

    // Kept so the torrent can be restored after a restart without asking the tracker
    // for the file again, and so deletion has something to clean up.
    let torrent_file = save_torrent_file(&cfg, &entry.info_hash, &bytes);

    state
        .store
        .upsert(crate::state::Item {
            info_hash: entry.info_hash.clone(),
            // The tracker's own id, without the play prefix: this is what its hit-and-run list
            // is keyed by, and the tracker beside it is what says whose list to look on.
            ncore_torrent_id: crate::tracker::Tracker::from_play_id(&torrent_id).1.to_string(),
            tracker: source.tracker.id().to_string(),
            title: entry.file_name.clone(),
            file_name: entry.file_name.clone(),
            file_index: entry.selected,
            // More than one file in the torrent and only one of them selected: the
            // torrent will never be a complete seed, which matters when deciding whether
            // the tracker's obligation for it can ever clear. Partial download means the
            // same thing for a different reason — not even the one file is finished.
            partial: cfg.pieces.partial_download
                || (entry.files.len() > 1 && !cfg.ncore.requires_full_download),
            file_len: entry.file_len,
            save_path: entry.file_path.to_string_lossy().to_string(),
            torrent_file,
            added_at: crate::state::now(),
            ..Default::default()
        })
        .await;

    tracing::info!(
        file = %entry.file_name,
        from_disk,
        looked_on_disk_ms = looked_on_disk.as_millis() as u64,
        torrent_ms = have_torrent.saturating_sub(looked_on_disk).as_millis() as u64,
        disks_ms = disks_decided.saturating_sub(have_torrent).as_millis() as u64,
        engine_ms = opened.saturating_sub(disks_decided).as_millis() as u64,
        total_ms = began.elapsed().as_millis() as u64,
        "cold start"
    );
    range_response(state, entry, &cfg, method, headers)
}

/// The `.torrent` for this tracker id, read from the folder we saved it in.
///
/// The records already know the info hash for anything that has been played, and the file is
/// named by that hash, so a torrent we have handled before needs nothing from the network. A
/// season pack is the case this exists for: every episode after the first is the same torrent,
/// and every one of them used to pay for its own download from the tracker.
///
/// Anything unreadable or that does not look like a torrent falls through to the tracker, so a
/// truncated file costs one fetch rather than a playback.
async fn local_torrent_for(state: &Arc<AppState>, torrent_id: &str) -> Option<Vec<u8>> {
    let dir = state.config().await.storage.torrent_files_dir.clone();
    for key in state.store.keys_for_tracker_id(torrent_id).await {
        let hash = key.split(':').next().unwrap_or_default();
        if hash.is_empty() {
            continue;
        }
        let path = std::path::Path::new(&dir).join(format!("{hash}.torrent"));
        let Ok(bytes) = tokio::fs::read(&path).await else {
            continue;
        };
        // A bencoded torrent starts with a dictionary marker. Anything else is a leftover or a
        // half-written file, and handing it to the engine would fail later and less clearly.
        if !bytes.starts_with(b"d") {
            tracing::warn!(path = %path.display(), "the saved torrent is not readable; asking the tracker");
            continue;
        }
        tracing::debug!(path = %path.display(), "using the saved .torrent instead of asking the tracker");
        return Some(bytes);
    }
    None
}

/// Writes the `.torrent` next to the others, named by info hash. Returns the path, or
/// an empty string when it could not be written: failing to cache the file must not
/// stop the film from playing.
pub(crate) fn save_torrent_file(cfg: &Config, info_hash: &str, bytes: &[u8]) -> String {
    let dir = std::path::Path::new(&cfg.storage.torrent_files_dir);
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::warn!(dir = %dir.display(), error = %e, "cannot create the .torrent folder");
        return String::new();
    }
    let path = dir.join(format!("{info_hash}.torrent"));
    if path.exists() {
        return path.to_string_lossy().to_string();
    }
    match std::fs::write(&path, bytes) {
        Ok(()) => path.to_string_lossy().to_string(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "cannot save the .torrent");
            String::new()
        }
    }
}

/// The content type for a file, from its name.
///
/// It used to be one configured value for everything, which said Matroska whatever was being
/// served. Most releases here are `.mkv` so it was right most of the time, and wrong in the way
/// that is hardest to diagnose: an older pack whose episodes are `.avi` was announced as
/// Matroska, and a player that believes the label and then finds `RIFF` gives up without
/// explaining itself. Measured on a real pack from this tracker, so this is not hypothetical.
///
/// The configured value stays as the answer for anything unrecognised.
pub(crate) fn content_type_for(file_name: &str, fallback: &str) -> String {
    let extension = file_name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "mkv" => "video/x-matroska",
        "mp4" | "m4v" | "mov" => "video/mp4",
        "avi" => "video/x-msvideo",
        "webm" => "video/webm",
        "ts" | "m2ts" | "mts" => "video/mp2t",
        "wmv" => "video/x-ms-wmv",
        "flv" => "video/x-flv",
        "mpg" | "mpeg" => "video/mpeg",
        _ => fallback,
    }
    .to_string()
}

pub(crate) fn range_response(
    state: Arc<AppState>,
    entry: Arc<Entry>,
    cfg: &Config,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let total = entry.file_len;

    let requested = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|raw| parse_range(raw, total));

    let range = match requested {
        Some(None) => {
            let mut res = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            res.headers_mut().insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{total}")).expect("ascii"),
            );
            return res;
        }
        Some(Some(r)) => Some(r),
        None => None,
    };

    let (start, end) = range.unwrap_or((0, total.saturating_sub(1)));
    let length = if total == 0 { 0 } else { end - start + 1 };

    let mut res = Response::builder()
        .status(if range.is_some() {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(header::ACCEPT_RANGES, "bytes")
        .header(
            header::CONTENT_TYPE,
            content_type_for(&entry.file_name, &cfg.streaming.content_type),
        )
        .header(header::CONTENT_LENGTH, length);

    if range.is_some() {
        res = res.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{total}"));
    }

    // HEAD is how a player probes the size; it must never wait for a piece.
    if method == Method::HEAD {
        return res.body(Body::empty()).expect("valid response");
    }

    tracing::info!(file = %entry.file_name, start, end, length, "serving range");
    let chunk = cfg.streaming.chunk_size_bytes.max(64 * 1024);
    let timeout = std::time::Duration::from_secs(cfg.streaming.piece_wait_timeout_secs);
    let poll = std::time::Duration::from_millis(cfg.streaming.piece_poll_interval_ms.max(50));

    res.body(body_for(state, entry, start, end, chunk, timeout, poll))
    .expect("valid response")
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn body_for(
    state: Arc<AppState>,
    entry: Arc<Entry>,
    start: u64,
    end: u64,
    chunk: u64,
    timeout: std::time::Duration,
    poll: std::time::Duration,
) -> Body {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);

    tokio::spawn(async move {
        // The record for this file of this torrent, which is what is watched, seeded and
        // deleted on its own.
        let key = entry.key();
        // A body request, rather than the size probe that precedes it, is what counts
        // as a viewing starting.
        state.store.record_play(&key, crate::state::now()).await;

        let reader_id = entry.register_reader(entry.piece_of(start)).await;
        // Deadlines are only applied by the background loop, so it has to know at once that
        // there is a read head to aim at rather than finding out on its next pass.
        state.lib.wake();
        let result = pump(
            &entry,
            reader_id,
            start,
            end,
            chunk,
            timeout,
            poll,
            &tx,
            &state.store,
            &key,
        )
        .await;
        entry.drop_reader(reader_id).await;

        if let Err(e) = result {
            tracing::warn!(error = %e, start, end, "stream aborted");
            let _ = tx.send(Err(std::io::Error::other(e.to_string()))).await;
        }
    });

    Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx))
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn pump(
    entry: &Arc<Entry>,
    reader_id: u64,
    start: u64,
    end: u64,
    chunk: u64,
    timeout: std::time::Duration,
    poll: std::time::Duration,
    tx: &tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    store: &crate::state::Store,
    key: &str,
) -> Result<()> {
    // How long the player waits for its first byte of this range.
    //
    // The stages before this are all timed and reported as "cold start", and they end where
    // the torrent is handed to the engine. Everything after that — finding a peer, asking for
    // the opening piece, getting it written and read back — was the one part of a slow start
    // nobody could put a number on. One line per range, at the moment it stops being a guess.
    let asked_at = std::time::Instant::now();
    // Wait before opening: right after a torrent is added the file may not exist.
    wait_for(entry, start, start, timeout, poll).await?;
    let mut file = tokio::fs::File::open(&entry.file_path)
        .await
        .with_context(|| format!("opening {}", entry.file_path.display()))?;
    file.seek(std::io::SeekFrom::Start(start))
        .await
        .context("seek")?;

    let mut offset = start;
    // What has gone out but has not been written into the record yet.
    //
    // Kept here and handed over in batches rather than one call per chunk, because each call
    // takes the write lock on the whole store. A second is the longest any of this waits, and
    // the only thing at stake in that second is how much of the file the record believes was
    // watched, which is read minutes later by the deletion round.
    let mut pending: Vec<(u64, u64)> = Vec::new();
    let mut last_recorded = std::time::Instant::now();

    while offset <= end {
        // Small pieces of the file until the first one has gone out, then the configured size.
        //
        // The configured chunk is a megabyte, and on a release with half-megabyte pieces that
        // means the very first read waits for two pieces before a single byte reaches the
        // player. Measured on a real start: the first piece arrived after two seconds and the
        // second after fourteen, so twelve of those seconds were spent holding a piece the
        // player already had and could have been parsing. Asking for a quarter of a megabyte
        // to begin with lets the header out as soon as it exists.
        let warming = !entry.started.load(std::sync::atomic::Ordering::Relaxed);
        let step = if warming { WARMUP_CHUNK.min(chunk) } else { chunk };
        let want = step.min(end - offset + 1);

        // Report the position before waiting, so the deadline window is already
        // aimed here while the pieces are still on their way.
        entry.advance_reader(reader_id, entry.piece_of(offset)).await;
        wait_for(entry, offset, offset + want - 1, timeout, poll).await?;

        // Read straight into the buffer that will be handed over, rather than into a scratch
        // one and copied out of it.
        //
        // Every chunk leaves this loop owned by the response, so it has to be its own
        // allocation either way; what was avoidable was doing the work twice. Reading into a
        // reused array and then copying it into the outgoing buffer meant a megabyte of
        // copying per chunk, seventy thousand times over a large film. `read_buf` fills the
        // uninitialised capacity directly, so there is no clearing beforehand either.
        let mut piece = bytes::BytesMut::with_capacity(want as usize);
        while (piece.len() as u64) < want {
            let read = file
                .read_buf(&mut piece)
                .await
                .with_context(|| format!("reading {want} bytes at {offset}"))?;
            if read == 0 {
                anyhow::bail!(
                    "the file ended at {} while {want} bytes were wanted at {offset}",
                    offset + piece.len() as u64
                );
            }
        }

        // The first chunk out is what ends the warm-up: from here the window widens to what
        // the configuration asks for, because the job changes from "finish one piece" to
        // "stay ahead of a reader".
        entry
            .started
            .store(true, std::sync::atomic::Ordering::Relaxed);

        if offset == start {
            tracing::info!(
                file = %entry.file_name,
                from = start,
                bytes = want,
                first_byte_ms = asked_at.elapsed().as_millis() as u64,
                "first bytes out"
            );
        }

        if tx.send(Ok(piece.freeze())).await.is_err() {
            // Normal: the player seeked or stopped. What it did receive still counts.
            tracing::debug!(offset, "reader closed the connection");
            store
                .record_served_many(key, &pending, crate::state::now())
                .await;
            return Ok(());
        }
        // Counted only once the bytes are actually on their way to the player, which
        // is the whole basis for deciding later that this was watched.
        pending.push((offset, want));
        if last_recorded.elapsed() >= RECORD_EVERY {
            store
                .record_served_many(key, &pending, crate::state::now())
                .await;
            pending.clear();
            last_recorded = std::time::Instant::now();
        }
        offset += want;
    }
    // Whatever is left, before this reader goes. A player that stops mid-file must still be
    // credited with what it did watch.
    store
        .record_served_many(key, &pending, crate::state::now())
        .await;
    Ok(())
}

/// How long served bytes may wait before they are written into the record.
///
/// A second. The record is read by the deletion round, minutes later at the earliest, so this
/// is not information anybody needs sooner; and every write of it takes the lock that guards
/// the whole store, which at a megabyte a chunk was sixty times a second.
const RECORD_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// How much to hand over at a time before playback has started.
///
/// A quarter of a megabyte. Big enough to carry a container header, small enough that it rarely
/// spans more than one piece, which is the whole point: the first bytes should cost one piece
/// and not two.
const WARMUP_CHUNK: u64 = 256 * 1024;

pub(crate) async fn wait_for(
    entry: &Arc<Entry>,
    from: u64,
    to: u64,
    timeout: std::time::Duration,
    poll: std::time::Duration,
) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    let mut logged = false;
    // Short at first, then backing off to the configured interval.
    //
    // The configured interval is right for a wait that is going to be long: checking three
    // times a second for a piece that is thirty seconds away is pure wakeups. It is wrong for
    // the first moments of a wait, where the piece may land at any instant and the viewer is
    // sitting in front of a black screen: at four hundred milliseconds, a piece that arrived
    // just after a check was not noticed for most of half a second, every time. Starting at
    // twenty-five milliseconds and doubling costs a handful of extra checks and gives that
    // back.
    let mut interval = std::time::Duration::from_millis(25).min(poll);
    // And no point ever being slower than the map is refreshed while somebody is blocked on it.
    // The configured interval used to be the ceiling, so a reader that had been waiting a few
    // seconds was checking every four hundred milliseconds a copy that is refreshed every
    // hundred, and paid up to three hundred of those for nothing after the piece had landed.
    let ceiling = poll.min(crate::library::WAITING_POLL_INTERVAL);

    let began = std::time::Instant::now();
    // Set as soon as this reader is known to be blocked, and dropped when it is not. While it
    // is held the deadline loop re-reads the piece map briskly, because the only thing a
    // waiting reader can see is what that loop has put there.
    let mut blocked: Option<crate::library::Waiting> = None;
    loop {
        if entry.ready(from, to).await {
            if logged {
                tracing::info!(
                    file = %entry.file_name,
                    pieces = format!("{}..{}", entry.piece_of(from), entry.piece_of(to)),
                    waited_ms = began.elapsed().as_millis() as u64,
                    "pieces arrived"
                );
            }
            return Ok(());
        }
        if blocked.is_none() {
            blocked = Some(entry.begin_wait());
        }
        if !logged {
            tracing::info!(
                file = %entry.file_name,
                pieces = format!("{}..{}", entry.piece_of(from), entry.piece_of(to)),
                "waiting for pieces"
            );
            logged = true;
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "pieces {}..{} did not arrive within {}s",
                entry.piece_of(from),
                entry.piece_of(to),
                timeout.as_secs()
            );
        }
        tokio::time::sleep(interval).await;
        interval = (interval * 2).min(ceiling);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The case this was written for: a pack from this tracker whose episodes are `.avi`, served
    /// with a Matroska label. The bytes began with `RIFF` and the player gave up silently.
    #[test]
    fn the_content_type_follows_the_file() {
        let fallback = "video/x-matroska";
        assert_eq!(content_type_for("House.S01E06.avi", fallback), "video/x-msvideo");
        assert_eq!(content_type_for("film.mkv", fallback), "video/x-matroska");
        assert_eq!(content_type_for("film.MKV", fallback), "video/x-matroska");
        assert_eq!(content_type_for("film.mp4", fallback), "video/mp4");
        assert_eq!(content_type_for("recording.ts", fallback), "video/mp2t");
        // Anything unrecognised keeps the configured answer rather than guessing.
        assert_eq!(content_type_for("film.xyz", fallback), fallback);
        assert_eq!(content_type_for("no-extension", fallback), fallback);
    }
}
