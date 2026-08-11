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

    let bytes = match state
        .ncore
        .read()
        .await
        .download_torrent(&source.download_url)
        .await
    {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "could not fetch the .torrent");
            return (StatusCode::BAD_GATEWAY, format!("nCore: {e}\n")).into_response();
        }
    };

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

    if let Some(existing) = existing_dir {
        if let Err(e) = crate::disk::space_at_least(&existing, needed) {
            let message = format!("Nincs hely a torrent saját mappájában: {e}");
            tracing::error!("{message}");
            if cfg.maintenance.notify_disk {
                state.notify_occasionally("no-room-existing", &message).await;
            }
            return (StatusCode::INSUFFICIENT_STORAGE, format!("{message}
")).into_response();
        }
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
    let (_key, entry) = match state.lib.add(&bytes, want, &save_dir).await {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!(error = %e, "could not open the torrent");
            return (StatusCode::UNPROCESSABLE_ENTITY, format!("{e}\n")).into_response();
        }
    };

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
            ncore_torrent_id: torrent_id.clone(),
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

    range_response(state, entry, &cfg, method, headers)
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
    // Wait before opening: right after a torrent is added the file may not exist.
    wait_for(entry, start, start, timeout, poll).await?;
    let mut file = tokio::fs::File::open(&entry.file_path)
        .await
        .with_context(|| format!("opening {}", entry.file_path.display()))?;
    file.seek(std::io::SeekFrom::Start(start))
        .await
        .context("seek")?;

    let mut offset = start;
    let mut buf = vec![0u8; chunk as usize];

    while offset <= end {
        let want = chunk.min(end - offset + 1);

        // Report the position before waiting, so the deadline window is already
        // aimed here while the pieces are still on their way.
        entry.advance_reader(reader_id, entry.piece_of(offset)).await;
        wait_for(entry, offset, offset + want - 1, timeout, poll).await?;

        let slice = &mut buf[..want as usize];
        file.read_exact(slice)
            .await
            .with_context(|| format!("reading {want} bytes at {offset}"))?;

        if tx.send(Ok(Bytes::copy_from_slice(slice))).await.is_err() {
            // Normal: the player seeked or stopped.
            tracing::debug!(offset, "reader closed the connection");
            return Ok(());
        }
        // Counted only once the bytes are actually on their way to the player, which
        // is the whole basis for deciding later that this was watched.
        store
            .record_served(key, offset, want, crate::state::now())
            .await;
        offset += want;
    }
    Ok(())
}

pub(crate) async fn wait_for(
    entry: &Arc<Entry>,
    from: u64,
    to: u64,
    timeout: std::time::Duration,
    poll: std::time::Duration,
) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    let mut logged = false;

    loop {
        if entry.ready(from, to).await {
            return Ok(());
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
        tokio::time::sleep(poll).await;
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
