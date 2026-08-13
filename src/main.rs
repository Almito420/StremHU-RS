// No console window on Windows.
//
// This is a server that runs all evening in the background. A command prompt that pops up
// on starting it and has to stay open for as long as it runs is not something anyone wants
// on their desktop, and closing it by reflex would stop the server mid-film. The window is
// gone entirely: the interface is the way to look at it, and the interface has a button to
// stop it.
//
// Nothing is lost from a terminal. Console handles are inherited, so `stremhu-rs config`
// run from PowerShell still prints there; only a terminal that was never there cannot be
// written to, and for that case there is a log file and, for a failure to start, a dialog.
//
// Not applied to the test binary, whose whole output is its report.
#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

//! stremhu-rs
//!
//! A self-hosted torrent streaming server for nCore, in the shape of a Stremio
//! addon. The torrent engine is libtorrent, reached through a small C ABI shim,
//! because that is the only engine proven to work with this tracker and the only
//! one exposing `set_piece_deadline`, which is what keeps the front of the file
//! contiguous while streaming.
//!
//! Commands:
//!   stremhu-rs config                       show the effective configuration
//!   stremhu-rs search <query> [page] [miben] nCore search
//!
//! Nothing is hardcoded: every tunable lives in the TOML config.

use anyhow::{Context, Result, bail};

mod addon;
mod alerts;
mod bithumen;
mod app;
mod config;
mod disk;
mod engine;
mod http;
mod library;
mod maintenance;
mod media;
mod ncore;
mod play;
mod series;
mod state;
mod ui;
mod ui_downloads;
mod stream_policy;
mod stremio;
mod tls;
mod tmdb;
mod tracker;
mod webui;

/// Whether there is a console to write to.
///
/// True when a terminal started us, false when Explorer did: a console is inherited from
/// the parent, and a window-subsystem program is not given one of its own. This is what
/// decides whether the log goes to the screen or to a file, and it is asked rather than
/// assumed because a message written to a handle nobody holds is a message lost.
#[cfg(windows)]
fn has_console() -> bool {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetConsoleWindow() -> *mut std::ffi::c_void;
    }
    !unsafe { GetConsoleWindow() }.is_null()
}

#[cfg(not(windows))]
fn has_console() -> bool {
    true
}

/// Where the log goes when there is no console.
///
/// In `logs`, beside the rotated one, rather than loose in the install folder: two files
/// whose names differ only by an extension are exactly what a folder somebody has to look
/// through does not need.
fn log_file_path() -> std::path::PathBuf {
    config::base_dir().join("logs").join("stremhu-rs.log")
}

static LOG_FILE: std::sync::OnceLock<std::sync::Mutex<std::fs::File>> = std::sync::OnceLock::new();

/// Writes each log line to the file, one writer at a time.
///
/// The lock is taken per line rather than held, and a poisoned lock is used anyway: a panic
/// in some other thread is exactly when the log matters most, and refusing to write it then
/// would hide the reason.
struct FileLog;

impl std::io::Write for FileLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match LOG_FILE.get() {
            Some(lock) => {
                let mut file = lock.lock().unwrap_or_else(|e| e.into_inner());
                file.write(buf)
            }
            // Nothing to write to. Reporting success is deliberate: a logging failure must
            // not turn into a failure of whatever was being logged about.
            None => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(lock) = LOG_FILE.get() {
            let mut file = lock.lock().unwrap_or_else(|e| e.into_inner());
            file.flush()?;
        }
        Ok(())
    }
}

/// Opens the log file, starting a fresh one once it has grown past a few megabytes.
///
/// Rotation is one generation deep, `stremhu-rs.log.old`: enough to still have yesterday's
/// evening after today's, and not a mechanism that quietly fills a disk it was meant to be
/// reporting on.
fn open_log_file() -> Option<std::fs::File> {
    const MAX_BYTES: u64 = 4 * 1024 * 1024;
    let path = log_file_path();
    // The folder may not exist yet: logging is set up before the configuration is read, so
    // this can be the first thing the program writes anywhere.
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > MAX_BYTES {
        let _ = std::fs::rename(&path, path.with_extension("log.old"));
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()
}

/// Tells the user why the server did not start, when there is no console to tell them in.
///
/// A window-subsystem program that fails silently is indistinguishable from one that
/// started, and the difference matters: the next thing the user does is open the interface
/// and find nothing there.
#[cfg(windows)]
fn error_dialog(message: &str) {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn MessageBoxW(
            hwnd: *mut std::ffi::c_void,
            text: *const u16,
            caption: *const u16,
            kind: u32,
        ) -> i32;
    }
    const MB_ICONERROR: u32 = 0x0000_0010;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    // Bound to names: the pointers have to outlive the call.
    let text = wide(message);
    let caption = wide("stremhu-rs");
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_ICONERROR,
        );
    }
}

#[cfg(not(windows))]
fn error_dialog(message: &str) {
    eprintln!("{message}");
}

/// Says something to whoever is watching, wherever that is.
///
/// The startup lines carry the addon URL and the settings URL, which are the two things
/// somebody actually needs from a start. On a terminal they go to the screen; started by
/// double-clicking there is no screen, and they would be lost, so they also go to the log
/// file. That file is then a record of starts rather than an empty file that gives no way to
/// tell a server that came up from one that never did.
pub fn note(line: &str) {
    println!("{line}");
    if let Some(lock) = LOG_FILE.get() {
        use std::io::Write;
        let mut file = lock.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(file, "{line}");
    }
}

/// Turns logging on and picks how much of it.
///
/// Nothing is logged unless asked for. A media server that runs all evening should not be
/// narrating every piece it fetches, and it should not be writing a file nobody asked for
/// either: with no switch there is no log file at all. The one exception is a failure to
/// start, which is always written, because a server that never came up has to leave a reason
/// behind and there may be no window to leave it in.
///
/// Accepted forms, following the convention other tools use:
///   (nothing)        no log
///   --log            the ordinary running commentary
///   --log debug      or --log=debug, and likewise trace, info, warn, error, off
///   -v               same as --log
///
/// With `--log` and a terminal the log goes to the screen; with `--log` and no terminal, to
/// `stremhu-rs.log` beside the executable.
///
/// `RUST_LOG` still wins if it is set, because anyone who sets it means it.
fn init_logging(args: &[String]) {
    let mut level: Option<&str> = None;

    for (i, arg) in args.iter().enumerate() {
        if arg == "--log" || arg == "-v" {
            // A level may follow, but the next word could equally be a command.
            level = Some(match args.get(i + 1).map(String::as_str) {
                Some(next) if is_log_level(next) => next,
                _ => "info",
            });
        } else if let Some(rest) = arg.strip_prefix("--log=") {
            level = Some(if is_log_level(rest) { rest } else { "info" });
        }
    }

    let from_env = std::env::var("RUST_LOG")
        .ok()
        .filter(|v| !v.trim().is_empty());
    // Whether anybody asked for logging at all. Without this there is no file and no output:
    // "off" is the answer to a question that was not asked.
    let asked = level.is_some() || from_env.is_some();
    // Errors are never filtered out, whatever was asked for.
    //
    // Not for the log: with no switch there is still no file and no output, because there is no
    // console to write to and no file is opened. It is for the layer below, which turns an error
    // into a notification. A filter of "off" would stop the events reaching it, and then the one
    // evening something breaks unattended would be the evening nothing is sent.
    let filter = match from_env {
        Some(v) => v,
        None => format!("stremhu_rs={}", level.unwrap_or("error").replace("off", "error")),
    };
    let builder = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::new(
        filter,
    ));

    // A terminal is the place for it when there is one, and no file is created.
    // The error-catching layer rides along with whatever the fmt layer is doing.
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    if has_console() || !asked {
        builder.finish().with(alerts::ErrorLayer).init();
        return;
    }
    match open_log_file() {
        Some(file) => {
            let _ = LOG_FILE.set(std::sync::Mutex::new(file));
            // No colour codes in a file that will be read in Notepad.
            builder
                .with_ansi(false)
                .with_writer(|| FileLog)
                .finish()
                .with(alerts::ErrorLayer)
                .init();
            note(&format!(
                "\n=== stremhu-rs indul {} ===",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
            ));
        }
        None => builder.finish().with(alerts::ErrorLayer).init(),
    }
}

/// Writes down why the server did not start, whatever the logging switches said.
///
/// The one thing that is always recorded. Everything else is silent by default because
/// nobody asked for a commentary, but a server that failed to come up and left nothing
/// behind cannot be diagnosed at all, and with no console there is nowhere else for it to
/// go. Appended to the same file `--log` uses, so there is one place to look.
fn log_startup_failure(message: &str) {
    use std::io::Write;
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    if let Some(lock) = LOG_FILE.get() {
        let mut file = lock.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(file, "\n=== {stamp} nem indult el ===\n{message}");
        return;
    }
    if let Some(mut file) = open_log_file() {
        let _ = writeln!(file, "\n=== {stamp} nem indult el ===\n{message}");
    }
}

fn is_log_level(text: &str) -> bool {
    matches!(
        text.to_ascii_lowercase().as_str(),
        "off" | "error" | "warn" | "info" | "debug" | "trace"
    )
}

/// Removes the logging switches so what remains is the command and its own arguments.
///
/// A level word is only removed directly after the switch. `search error` is a search for
/// the word "error" and has to survive.
fn strip_log_flags(raw: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let arg = raw[i].as_str();
        if arg == "--log" || arg == "-v" {
            let level_follows = raw.get(i + 1).map(|a| is_log_level(a)).unwrap_or(false);
            i += if level_follows { 2 } else { 1 };
            continue;
        }
        if arg.starts_with("--log=") {
            i += 1;
            continue;
        }
        out.push(raw[i].clone());
        i += 1;
    }
    out
}

#[tokio::main]
async fn main() -> Result<()> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    init_logging(&raw);

    // The logging switches are not commands, so they are dropped before the command is
    // read; otherwise `stremhu-rs --log` would look like an unknown command. A level word
    // is only dropped when it directly follows the switch: `search error` is a search for
    // the word "error" and must survive.
    let mut args = strip_log_flags(&raw).into_iter();
    let command = args.next().unwrap_or_default();
    // No arguments means the server. That is what double-clicking the executable does,
    // and it is the only thing anyone wants from it day to day; the other commands are
    // for looking into things from a terminal.
    let double_clicked = command.is_empty();
    let command = if double_clicked { "serve".to_string() } else { command };

    let result = run(&command, &mut args).await;

    // Started from a terminal, an error is on screen already. Started by double-clicking
    // there is no screen, and a server that exits without a word looks exactly like one
    // that started, so the reason is put in a dialog and in the log file.
    if let Err(e) = &result {
        log_startup_failure(&format!("{e:#}"));
        if !has_console() {
            error_dialog(&format!(
                "A stremhu-rs nem indult el.\n\n{e:#}\n\nA részletek itt: {}",
                log_file_path().display()
            ));
        }
        let _ = double_clicked;
    }
    result
}

async fn run(command: &str, args: &mut impl Iterator<Item = String>) -> Result<()> {
    match command {
        "config" => {
            let path = config::Config::path_from_env();
            let cfg = config::Config::load(&path)?;
            println!("config: {}\n", path.display());
            print!("{}", toml::to_string_pretty(&cfg)?);
            Ok(())
        }
        "search" => {
            let query = args.next().context("usage: search <query> [page] [miben]")?;
            let page: u32 = match args.next() {
                Some(v) => v.parse().context("page must be a number")?,
                None => 1,
            };
            // nCore answers a free-text query on `miben=imdb` with the whole
            // catalogue rather than an error, so the field follows the query shape.
            let miben = args
                .next()
                .unwrap_or_else(|| ncore::search_field_for(&query).to_string());
            search(&query, page, &miben).await
        }
        // The addon server: Stremio talks to it, and it opens torrents on demand.
        "serve" => http::serve().await,
        "tmdb" => {
            let kind = args.next().context("usage: tmdb <tv|movie> <id>")?;
            let id = args.next().context("usage: tmdb <tv|movie> <id>")?;
            tmdb_probe(&kind, &id).await
        }
        "measure" => {
            let torrent = args
                .next()
                .context("usage: measure <torrent-file> [seconds]")?;
            let seconds: u64 = match args.next() {
                Some(v) => v.parse().context("seconds must be a number")?,
                None => 60,
            };
            measure(&torrent, seconds).await
        }
        other => bail!(
            "unknown command {other:?}; expected no arguments to run the server, or one of \
             `serve`, `config`, `search <query> [page] [miben]`, `tmdb <tv|movie> <id>`, \
             `measure <torrent-file> [seconds]`"
        ),
    }
}

/// The one measurement the whole engine choice rests on: with deadlines driving
/// the download, does the contiguous run of completed pieces grow from the start of
/// the file? Scattered pieces are useless to a player.
///
/// Baseline to beat, measured through the qBittorrent API on this same torrent:
/// 30 seconds, +4.4% of the torrent downloaded, contiguous front stuck at 8 pieces.
async fn measure(torrent_path: &str, seconds: u64) -> Result<()> {
    let path = clean_arg(torrent_path);
    let mut cfg = config::Config::load(&config::Config::path_from_env())?;
    cfg.apply_env_overrides();

    println!("libtorrent {}", engine::libtorrent_version());

    let bytes = std::fs::read(&path).with_context(|| format!("reading {path}"))?;
    let session = engine::Session::new(engine::SessionSettings::from_config(&cfg.torrent))?;
    let torrent = session.add_torrent(&bytes, &cfg.torrent.save_path)?;
    println!("info hash: {}", torrent.info_hash);

    let files = torrent.files()?;
    let piece_len = torrent.piece_length()?;
    println!("pieces: {} of {} bytes\nfiles:", torrent.num_pieces()?, piece_len);
    for f in &files {
        println!("  [{}] {:>8.2} GB  {}", f.index, f.size as f64 / 1e9, f.path.display());
    }

    // The largest file: release folders carry a small sample that index 0 may hit.
    let wanted = files
        .iter()
        .max_by_key(|f| f.size)
        .context("torrent has no files")?
        .clone();
    torrent.select_only_file(wanted.index)?;
    torrent.set_max_connections(cfg.torrent.connections_while_streaming)?;
    torrent.resume()?;

    let span = stream_policy::FileSpan::from_offsets(wanted.offset, wanted.size, piece_len);
    let policy = cfg.pieces.to_policy(piece_len);
    println!(
        "\nserving file [{}], pieces {}..{}, prefetch window {} pieces\n",
        wanted.index, span.first_piece, span.last_piece, policy.prefetch_pieces
    );

    // A player starts at byte 0, so that is where the read head sits.
    let head = stream_policy::ReadHead {
        span,
        piece: span.first_piece,
    };
    let mut active: std::collections::BTreeSet<u32> = Default::default();

    println!("{:>4}  {:>10}  {:>9}  {:>10}  {:>6}", "s", "front", "done", "rate", "peers");
    for t in 1..=seconds {
        // Re-apply the plan every second: deadlines are relative to now, so they
        // have to be refreshed or they expire and stop influencing the order.
        // Pieces already on disk are skipped, so the window has to be planned against
        // the current bitmap rather than a fixed range.
        let plan = stream_policy::plan(&policy, &[head], &torrent.have_pieces()?, &active);
        for piece in &plan.reset {
            torrent.reset_piece_deadline(*piece)?;
        }
        for (piece, ms) in &plan.set {
            torrent.set_piece_deadline(*piece, *ms)?;
        }
        active = plan.set.keys().copied().collect();

        if let Some(err) = session.pump_alerts() {
            println!("  libtorrent error: {err}");
        }

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        let have = torrent.have_pieces()?;
        let front = engine::contiguous_from(&have, span.first_piece);
        let stats = torrent.stats()?;
        println!(
            "{t:>4}  {:>10}  {:>9}  {:>10}  {:>6}",
            format!("{front} pc"),
            format!("{:.2} GB", stats.total_done as f64 / 1e9),
            format!("{:.1} MB/s", stats.download_rate as f64 / 1e6),
            format!("{}/{}", stats.num_seeds, stats.num_peers),
        );
    }

    let have = torrent.have_pieces()?;
    let front = engine::contiguous_from(&have, span.first_piece);
    println!(
        "\nfinal: {front} contiguous piece(s) = {:.1} MB playable from the start",
        (front as u64 * piece_len) as f64 / 1e6
    );
    Ok(())
}

/// Shows what a TMDB id resolves to, which is what decides how nCore gets searched:
/// an IMDb id allows an exact match, and its absence forces the name path.
async fn tmdb_probe(kind: &str, id: &str) -> Result<()> {
    let mut cfg = config::Config::load(&config::Config::path_from_env())?;
    cfg.apply_env_overrides();

    let client = tmdb::TmdbClient::new(&cfg.tmdb.api_key, &cfg.tmdb.language)?;
    let title = match kind {
        "tv" | "series" => client.series(id).await?,
        "movie" | "film" => client.movie(id).await?,
        other => bail!("unknown kind {other:?}; expected tv or movie"),
    };

    println!("\nname          : {}", title.name);
    println!("original_name : {}", title.original_name);
    println!("year          : {:?}", title.year);
    match &title.imdb_id {
        Some(imdb) => println!("imdb_id       : {imdb}  -> exact nCore search by imdb"),
        None => println!("imdb_id       : none  -> nCore has to be searched by name"),
    }
    println!("search terms  : {:?}", title.search_terms());
    Ok(())
}

async fn search(query: &str, page: u32, miben: &str) -> Result<()> {
    let mut cfg = config::Config::load(&config::Config::path_from_env())?;
    cfg.apply_env_overrides();

    let client = ncore::NcoreClient::new(&cfg.ncore.username, &cfg.ncore.password)?;
    client.login().await?;

    let result = client.search(miben, query, page).await?;
    println!(
        "\nmiben={miben}  mire={query}\n{} of {} hit(s), page {}{}\n",
        result.torrents.len(),
        result.total_results,
        page,
        match result.next_page {
            Some(n) => format!(", next page {n}"),
            None => ", last page".to_string(),
        }
    );
    for t in &result.torrents {
        println!(
            "  id={:<10} seeders={:<5} dl={:<3} category={:<14} imdb={:<12} {}",
            t.torrent_id,
            t.seeders,
            if t.download_url.is_some() { "yes" } else { "NO" },
            t.category,
            t.imdb_id.as_deref().unwrap_or("-"),
            t.title.as_deref().unwrap_or("")
        );
    }
    if result.torrents.is_empty() {
        println!("  (no results)");
    }
    Ok(())
}

/// Parses a single-range `bytes=` value into an inclusive (start, end) pair.
/// Returns None when the header is malformed or unsatisfiable, which the caller
/// turns into a 416.
pub fn parse_range(value: &str, total: u64) -> Option<(u64, u64)> {
    if total == 0 {
        return None;
    }
    let spec = value.strip_prefix("bytes=")?.trim();
    if spec.contains(',') {
        // Multi-range needs a multipart body; players never ask for it.
        return None;
    }
    let (raw_start, raw_end) = spec.split_once('-')?;
    let last = total - 1;

    let (start, end) = match (raw_start.trim(), raw_end.trim()) {
        ("", "") => return None,
        // Suffix form: the final N bytes.
        ("", n) => {
            let n: u64 = n.parse().ok()?;
            if n == 0 {
                return None;
            }
            (total.saturating_sub(n), last)
        }
        (s, "") => (s.parse().ok()?, last),
        (s, e) => {
            let start: u64 = s.parse().ok()?;
            let end: u64 = e.parse().ok()?;
            (start, end.min(last))
        }
    };

    if start > end || start > last {
        return None;
    }
    Some((start, end))
}

/// Strips wrappers a shell copy-paste tends to leave behind.
pub fn clean_arg(raw: &str) -> String {
    let trimmed = raw.trim();
    let trimmed = trimmed
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(trimmed);
    trimmed.trim_matches('"').trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::{clean_arg, parse_range};

    #[test]
    fn full_and_open_ended() {
        assert_eq!(parse_range("bytes=0-", 100), Some((0, 99)));
        assert_eq!(parse_range("bytes=10-19", 100), Some((10, 19)));
        assert_eq!(parse_range("bytes=90-", 100), Some((90, 99)));
    }

    #[test]
    fn end_is_clamped_to_the_file() {
        assert_eq!(parse_range("bytes=10-1000", 100), Some((10, 99)));
    }

    #[test]
    fn suffix_form() {
        assert_eq!(parse_range("bytes=-10", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=-500", 100), Some((0, 99)));
        assert_eq!(parse_range("bytes=-0", 100), None);
    }

    #[test]
    fn rejects_unsatisfiable_and_malformed() {
        assert_eq!(parse_range("bytes=100-", 100), None);
        assert_eq!(parse_range("bytes=50-10", 100), None);
        assert_eq!(parse_range("bytes=-", 100), None);
        assert_eq!(parse_range("bytes=0-10,20-30", 100), None);
        assert_eq!(parse_range("items=0-10", 100), None);
        assert_eq!(parse_range("bytes=0-", 0), None);
    }

    #[test]
    fn strips_copy_paste_wrappers() {
        assert_eq!(clean_arg("<magnet:?xt=urn:btih:abc>"), "magnet:?xt=urn:btih:abc");
        assert_eq!(clean_arg("  magnet:?xt=urn:btih:abc  "), "magnet:?xt=urn:btih:abc");
        assert_eq!(clean_arg("\"D:/a b.torrent\""), "D:/a b.torrent");
        assert_eq!(clean_arg("D:/plain.torrent"), "D:/plain.torrent");
        assert_eq!(clean_arg("<half"), "<half");
    }
}





