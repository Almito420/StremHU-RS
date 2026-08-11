//! Admin web interface: first-run setup, login, and settings.
//!
//! The reference implementation hashes admin passwords with Argon2, so this does the
//! same rather than inventing something. There is one admin, which is why the
//! password lives in the config file as a hash and there is no user table.
//!
//! Settings are editable two ways on purpose. A form covers what gets changed often,
//! and a raw TOML editor covers everything else, so no setting is reachable only by
//! editing the file by hand. The editor parses before saving, so a syntax error
//! cannot destroy a working configuration.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use tokio::sync::Mutex;

/// How long a login lasts when it came from somewhere else on the network. Short
/// enough that a browser left open on a shared machine does not stay authorised.
const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);

/// How long a login from this machine lasts.
///
/// A session from the loopback address can only have come from a browser on the
/// machine that already holds the configuration file, the downloads and the private
/// key. Expiring it every twelve hours protects nothing that is not already in the
/// hands of whoever is sitting there, and it costs a password every day. A session
/// from any other address keeps the short life, because that could be anyone.
///
/// A year rather than never: the token still disappears eventually, and it goes on
/// every restart regardless, since sessions are only held in memory.
const LOCAL_SESSION_TTL: Duration = Duration::from_secs(365 * 24 * 3600);
const COOKIE: &str = "stremhu_session";

/// Whether a login came from the machine the server runs on.
///
/// Determined from the connection's own peer address, not from a header: a `Host` or
/// `X-Forwarded-For` value is written by the client and could simply claim to be
/// localhost.
pub fn is_local_peer(peer: Option<std::net::SocketAddr>) -> bool {
    match peer {
        Some(addr) => addr.ip().is_loopback(),
        // No connection information at all: assume the cautious answer.
        None => false,
    }
}

pub fn hash_password(password: &str) -> Result<String> {
    if password.len() < 8 {
        bail!("the password must be at least 8 characters");
    }
    // The salt comes from the OS random source directly. Argon2's own generator is
    // behind a feature flag in this version, and there is no reason to depend on it
    // when the same entropy source is already in use for the API key.
    let mut salt_bytes = [0u8; 16];
    getrandom::fill(&mut salt_bytes).expect("the OS random source must be available");
    let salt = SaltString::encode_b64(&salt_bytes)
        .map_err(|e| anyhow::anyhow!("encoding the salt failed: {e}"))?;

    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("hashing failed: {e}"))?;
    Ok(hash.to_string())
}

/// False for any malformed stored hash, so a corrupted config cannot accidentally
/// let everyone in.
pub fn verify_password(stored_hash: &str, password: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored_hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// Signed tokens rather than a table of live ones.
///
/// The first version kept sessions in memory, which meant every restart logged the admin
/// out. During a day of development that is a login per restart, and the reference
/// implementation does not behave that way at all: its interface authenticates with a
/// key that simply persists.
///
/// So a token now carries its own facts and a signature over them: when it was issued and
/// whether the login was local. The server keeps no list, a restart changes nothing, and
/// the secret in the configuration is what makes a token ours. Rotating that secret
/// invalidates every token at once, which is the one operation a stateless scheme needs
/// to keep.
///
/// A revocation list is kept for logout only, so pressing log out takes effect
/// immediately rather than at the token's expiry.
#[derive(Default)]
pub struct Sessions {
    secret: Mutex<Vec<u8>>,
    revoked: Mutex<HashMap<String, Instant>>,
}

impl Sessions {
    /// The signing secret comes from the configuration so tokens survive a restart.
    pub async fn set_secret(&self, secret: &str) {
        *self.secret.lock().await = secret.as_bytes().to_vec();
    }

    /// `local` means the login came from the machine itself, which earns a long life.
    ///
    /// The nonce is what makes two tokens distinct. Without it, two logins in the same
    /// second produce the same string, and logging out of one browser would silently log
    /// out the other.
    pub async fn create(&self, local: bool) -> String {
        let issued = unix_now();
        let mut nonce = [0u8; 8];
        getrandom::fill(&mut nonce).expect("the OS random source must be available");
        let nonce: String = nonce.iter().map(|b| format!("{b:02x}")).collect();

        let payload = format!("{issued}.{}.{nonce}", if local { 1 } else { 0 });
        let signature = self.sign(payload.as_bytes()).await;
        format!("{payload}.{signature}")
    }

    pub fn ttl(local: bool) -> Duration {
        if local { LOCAL_SESSION_TTL } else { SESSION_TTL }
    }

    pub async fn is_valid(&self, token: &str) -> bool {
        let Some((payload, signature)) = token.rsplit_once('.') else {
            return false;
        };
        // Constant-time-ish comparison of equal-length hex strings; a mismatch tells an
        // attacker nothing about how far it got.
        let expected = self.sign(payload.as_bytes()).await;
        if !constant_time_eq(signature.as_bytes(), expected.as_bytes()) {
            return false;
        }
        // issued.local.nonce
        let mut parts = payload.split('.');
        let Some(issued) = parts.next() else {
            return false;
        };
        let Some(local) = parts.next() else {
            return false;
        };
        let Ok(issued) = issued.parse::<u64>() else {
            return false;
        };
        if self.revoked.lock().await.contains_key(token) {
            return false;
        }

        let ttl = Self::ttl(local == "1").as_secs();
        let age = unix_now().saturating_sub(issued);
        // A token from the future is a clock change or a forgery attempt; either way it
        // is not something to honour.
        issued <= unix_now() && age < ttl
    }

    /// Logging out has to take effect at once, so the token is remembered as spent until
    /// it would have expired anyway.
    pub async fn destroy(&self, token: &str) {
        if token.is_empty() {
            return;
        }
        let mut revoked = self.revoked.lock().await;
        let now = Instant::now();
        revoked.retain(|_, until| *until > now);
        revoked.insert(token.to_string(), now + LOCAL_SESSION_TTL);
    }

    async fn sign(&self, payload: &[u8]) -> String {
        let secret = self.secret.lock().await.clone();
        hmac_sha256_hex(&secret, payload)
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// HMAC-SHA256, written out rather than pulled in.
///
/// It is the construction from RFC 2104 and it is ten lines: pad the key to the hash's
/// block size, hash the message under one padding, then hash that under another. Written
/// here it is checked against the published test vectors below, which is worth more than
/// trusting a wrapper whose API changes between releases.
fn hmac_sha256_hex(secret: &[u8], payload: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    const BLOCK: usize = 64;
    let mut key = [0u8; BLOCK];
    if secret.len() > BLOCK {
        // A key longer than the block is replaced by its own hash, per the spec.
        let digest = Sha256::digest(secret);
        key[..digest.len()].copy_from_slice(&digest);
    } else {
        key[..secret.len()].copy_from_slice(secret);
    }

    let mut inner = Sha256::new();
    inner.update(key.iter().map(|b| b ^ 0x36).collect::<Vec<u8>>());
    inner.update(payload);
    let inner = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(key.iter().map(|b| b ^ 0x5c).collect::<Vec<u8>>());
    outer.update(inner);

    outer
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A random secret, for when the configuration has none yet.
pub fn random_secret() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the OS random source must be available");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Reads our session cookie out of a Cookie header.
pub fn session_from_cookies(header: Option<&str>) -> String {
    let Some(header) = header else {
        return String::new();
    };
    for part in header.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix(COOKIE).and_then(|r| r.strip_prefix('=')) {
            return value.to_string();
        }
    }
    String::new()
}

/// HttpOnly so page scripts cannot read it, SameSite=Lax so another site cannot use
/// it, and no Secure flag because the panel is reachable over plain HTTP on a LAN.
///
/// `Max-Age` matches the server-side life, so the browser stops sending a token at the
/// same moment the server stops honouring it. A mismatch there produces the confusing
/// case of a browser that believes it is logged in and a server that disagrees.
pub fn session_cookie(token: &str, local: bool) -> String {
    format!(
        "{COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        Sessions::ttl(local).as_secs()
    )
}

pub fn clear_cookie() -> String {
    format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

/// Parses and validates a whole configuration from TOML text.
///
/// Returning the parsed value rather than writing it means a syntax error is caught
/// before the file on disk is touched.
pub fn parse_config(text: &str) -> Result<crate::config::Config> {
    toml::from_str(text).context("the configuration is not valid TOML")
}

pub struct Ui {
    pub sessions: Arc<Sessions>,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            sessions: Arc::new(Sessions::default()),
        }
    }
}

/// The whole interface, inline. No external assets: the server may have no internet
/// access, and one file cannot get out of step with itself.
pub fn page(state: PageState) -> String {
    let body = match state {
        PageState::Setup => SETUP_FORM.to_string(),
        PageState::Login { error } => LOGIN_FORM.replace("{{error}}", &error_block(error)),
        PageState::Settings {
            toml_text,
            message,
            engine,
            retention,
            network,
        } => network
            .fill(&engine.fill(&retention.fill(SETTINGS_PAGE)))
            .replace("{{toml}}", &html_escape(&toml_text))
            .replace("{{message}}", &message_block(message)),
        PageState::Downloads {
            groups,
            tracker_note,
            history,
            message,
        } => DOWNLOADS_PAGE
            .replace("{{groups}}", &render_groups(&groups))
            .replace("{{history}}", &render_history(&history))
            .replace("{{tracker_note}}", &html_escape(&tracker_note))
            .replace("{{message}}", &message_block(message)),
    };
    SHELL.replace("{{body}}", &body)
}

/// One row per download, one fact per column.
///
/// Everything used to sit in the title cell, which made the page unreadable: the size,
/// the age, the transfer figures and the verdict all ran together in a single block of
/// text, so nothing could be compared between rows. Each figure now has its own column
/// so the eye can go down a column and see which download is the large one, which has
/// given anything back, and which is about to be removed.
/// One torrent, with the files served out of it.
///
/// A torrent is the unit the tracker deals in and the unit the seeding obligation belongs to, so
/// it is the unit the page is organised by. A pack with eight episodes was eight rows that had to
/// be read one by one to work out they were the same download; now it is one line that opens.
pub struct TorrentGroup {
    /// The torrent's info hash, which is what the grouping is by.
    pub hash: String,
    /// The release, as the folder on disk names it.
    pub title: String,
    /// The one-line summary: how many files, how many watched, how much room.
    pub summary: String,
    /// Whether the tracker still wants seeding on this torrent, ready to display.
    pub owed_label: String,
    pub owed_class: &'static str,
    pub owed_detail: String,
    /// Open by default when something in it needs attention.
    pub open: bool,
    pub rows: Vec<DownloadRow>,
}

fn render_groups(groups: &[TorrentGroup]) -> String {
    if groups.is_empty() {
        return "<p class=\"note\">Még nincs letöltés.</p>".into();
    }
    let mut out = String::new();
    for group in groups {
        out.push_str(&format!(
            "<details class=\"grp\"{open}>\n\
             <summary><span class=\"gt\">{title}</span>\
             <span class=\"gs\">{summary}</span>\
             <span class=\"go {owed_class}\">{owed_label}{owed_detail}</span></summary>\n\
             <div class=\"scroll\"><table class=\"grid\">\n\
             <tr><th>Fájl</th><th class=\"num\">Méret</th><th class=\"num\">Letöltve</th>\
             <th class=\"num\">Visszaseedelve</th><th class=\"num\">Arány</th>\
             <th>Seed kötelezettség</th><th>Megnézve</th><th>Törlés</th><th></th></tr>\n\
             {rows}</table></div></details>\n",
            open = if group.open { " open" } else { "" },
            title = html_escape(&group.title),
            summary = html_escape(&group.summary),
            owed_class = group.owed_class,
            owed_label = html_escape(&group.owed_label),
            owed_detail = if group.owed_detail.is_empty() {
                String::new()
            } else {
                format!("<span class=\"note\"> {}</span>", html_escape(&group.owed_detail))
            },
            rows = render_rows(&group.rows),
        ));
    }
    out
}

fn render_rows(rows: &[DownloadRow]) -> String {
    if rows.is_empty() {
        return "<tr><td colspan=\"9\" class=\"note\">Nincs kiszolgált fájl.</td></tr>".into();
    }
    let mut out = String::new();
    for row in rows {

        let age = if row.figures_age.is_empty() {
            String::new()
        } else {
            format!("<div class=\"note\">{}</div>", html_escape(&row.figures_age))
        };
        out.push_str(&format!(
            r#"<tr>
  <td class="c-title"><div class="tt">{title}</div>{pack}</td>
  <td class="num">{size}</td>
  <td class="num">{downloaded}</td>
  <td class="num up">{uploaded}</td>
  <td class="num">{ratio}{age}</td>
  <td class="owed-cell {owed_class}">{owed_label}{owed_detail}</td>
  <td>{watched}</td>
  <td title="{verdict_full}">{verdict_short}<div class="note">{added}</div></td>
  <td class="actions">
    <form method="post" action="/ui/downloads/keep">
      <input type="hidden" name="key" value="{hash}">
      <input type="hidden" name="keep" value="{next_keep}">
      <button type="submit" class="ghost">{keep_label}</button>
    </form>
    <form method="post" action="/ui/downloads/watched">
      <input type="hidden" name="key" value="{hash}">
      <input type="hidden" name="watched" value="{next_watched}">
      <button type="submit" class="ghost">{watched_label}</button>
    </form>
    <form method="post" action="/ui/downloads/delete"
          onsubmit="return confirm('Töröljük ezt a letöltést és a hozzá tartozó adatot?')">
      <input type="hidden" name="key" value="{hash}">
      <button type="submit" class="danger">Törlés</button>
    </form>
  </td>
</tr>"#,
            title = html_escape(&row.title),
            owed_class = row.owed_class,
            owed_label = html_escape(&row.owed_label),
            owed_detail = if row.owed_detail.is_empty() {
                String::new()
            } else {
                format!("<div class=\"note\">{}</div>", html_escape(&row.owed_detail))
            },
            size = html_escape(&row.size),
            downloaded = html_escape(&row.downloaded),
            uploaded = html_escape(&row.uploaded),
            ratio = html_escape(&row.ratio),
            age = age,
            watched = html_escape(&row.watched),
            verdict_full = html_escape(&row.verdict),
            verdict_short = html_escape(&row.verdict_short),
            added = html_escape(&row.added),
            hash = html_escape(&row.key),
            next_keep = if row.keep { "0" } else { "1" },
            next_watched = if row.watched_by_hand { "0" } else { "1" },
            watched_label = if row.watched_by_hand {
                "Mégsem néztem"
            } else {
                "Megnéztem"
            },
            pack = if row.pack_summary.is_empty() {
                String::new()
            } else {
                format!("<div class=\"note\">{}</div>", html_escape(&row.pack_summary))
            },
            keep_label = if row.keep { "Mégse" } else { "Megtartás" },
        ));
    }
    out
}

pub enum PageState {
    /// No admin password yet.
    Setup,
    Login {
        error: Option<String>,
    },
    Settings {
        toml_text: String,
        message: Option<String>,
        engine: EngineView,
        retention: RetentionView,
        network: NetworkView,
    },
    Downloads {
        groups: Vec<TorrentGroup>,
        /// When the tracker's list was last read, and what it said.
        tracker_note: String,
        /// What was watched and when, newest first.
        history: Vec<(String, String)>,
        message: Option<String>,
    },
}

/// One download as the interface shows it.
///
/// Everything here is already formatted for reading. The point of the page is that a
/// person can see why something is still on the disk without inspecting anything, so
/// the verdict travels with the row.
pub struct DownloadRow {
    /// Which record this row is: `info_hash:file_index`. What the buttons post.
    pub key: String,
    /// Whether this file was marked watched by hand, which the button has to reflect.
    pub watched_by_hand: bool,
    /// For a torrent serving more than one file: how many, and how many are watched.
    ///
    /// A season pack is one torrent and many episodes, and the useful question about it is not
    /// answered by any single row: how much of this pack is still here, and how much of it have
    /// I finished. Empty for a torrent with one served file, where the row says everything.
    pub pack_summary: String,
    pub title: String,
    pub size: String,
    pub added: String,
    pub watched: String,
    /// Whether the tracker still wants seeding: "igen", "nem", or a question mark when the
    /// list has not been read. Three states rather than two, because an empty cell was being
    /// read as "nothing owed" when what it meant was "nobody has asked".
    pub owed_label: String,
    /// Which colour that is: red for owed, green for clear, grey for unknown.
    pub owed_class: &'static str,
    /// The detail under it: how much seeding is left, or when the answer came from.
    pub owed_detail: String,
    /// What the tracker counted, each in its own column.
    pub downloaded: String,
    pub uploaded: String,
    pub ratio: String,
    /// How old those three figures are, shown once under the ratio rather than repeated.
    pub figures_age: String,
    pub keep: bool,
    /// Why the sweep will or will not remove this.
    pub verdict: String,
    /// Short form of the verdict for the column, with the full reason as a tooltip.
    pub verdict_short: String,
}

fn render_history(entries: &[(String, String)]) -> String {
    if entries.is_empty() {
        return "<p class=\"note\">Még nem játszottunk le semmit.</p>".into();
    }
    let mut out = String::new();
    for (when, title) in entries {
        out.push_str(&format!(
            "<div class=\"kv\"><span>{}</span><b>{}</b></div>",
            html_escape(when),
            html_escape(title)
        ));
    }
    out
}

/// Bytes as something readable. Binary units, as trackers and players use.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

/// A duration as a person would say it.
pub fn human_duration(secs: u64) -> String {
    if secs == 0 {
        return "0 perc".into();
    }
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let minutes = (secs % 3_600) / 60;
    if days > 0 {
        return format!("{days} nap {hours} óra");
    }
    if hours > 0 {
        return format!("{hours} óra {minutes} perc");
    }
    format!("{minutes} perc")
}

/// How long ago, from a count of seconds.
pub fn human_ago(secs: u64) -> String {
    if secs < 90 {
        return "épp most".into();
    }
    format!("{}", human_duration(secs))
}

/// The retention settings as the form shows them.
///
/// Days and minutes, not seconds: these are numbers someone has to reason about when
/// deciding how long a film should survive, and 1209600 is not such a number. The
/// configuration file keeps seconds, and the conversion happens here and on save.
pub struct RetentionView {
    pub keep_seed_days: u64,
    pub retention_days: u64,
    /// `HH:MM`, for a time input.
    pub sweep_at: String,
    pub watched_position_percent: u8,
    pub watched_min_served_percent: u8,
    pub hit_and_run: bool,
    pub require_watched: bool,
    pub enable_deletion: bool,
    pub sweep_on_start: bool,
    pub sweep_when_full: bool,
    pub space_saving: bool,
    pub notify_sweep: bool,
    pub notify_disk: bool,
    pub notify_problems: bool,
    /// When the sweep will next happen, in words.
    pub next_run: String,
    /// Where a low-space warning is sent, empty for nowhere.
    pub notify_webhook_url: String,
}

impl RetentionView {
    pub fn from_config(m: &crate::config::Maintenance) -> Self {
        Self::new(m, "")
    }

    /// `last_sweep_date` decides whether today's run is still ahead.
    pub fn new(m: &crate::config::Maintenance, last_sweep_date: &str) -> Self {
        // Normalised through the parser, so a malformed value in the file shows up as
        // the time that will actually be used rather than as itself.
        let (hour, minute) = m.sweep_time();
        Self {
            keep_seed_days: m.keep_seed_seconds / 86_400,
            retention_days: m.cache_retention_seconds / 86_400,
            sweep_at: format!("{hour:02}:{minute:02}"),
            watched_position_percent: m.watched_position_percent,
            watched_min_served_percent: m.watched_min_served_percent,
            hit_and_run: m.hit_and_run,
            require_watched: m.require_watched,
            enable_deletion: m.enable_deletion,
            sweep_on_start: m.sweep_on_start,
            sweep_when_full: m.sweep_when_full,
            space_saving: m.space_saving,
            notify_sweep: m.notify_sweep,
            notify_disk: m.notify_disk,
            notify_problems: m.notify_problems,
            next_run: crate::maintenance::next_run_label(m, last_sweep_date),
            notify_webhook_url: m.notify_webhook_url.clone(),
        }
    }

    fn fill(&self, template: &str) -> String {
        template
            .replace("{{keep_seed_days}}", &self.keep_seed_days.to_string())
            .replace("{{retention_days}}", &self.retention_days.to_string())
            .replace("{{sweep_at}}", &html_escape(&self.sweep_at))
            .replace(
                "{{watched_position_percent}}",
                &self.watched_position_percent.to_string(),
            )
            .replace(
                "{{watched_min_served_percent}}",
                &self.watched_min_served_percent.to_string(),
            )
            .replace("{{hit_and_run}}", checked(self.hit_and_run))
            .replace("{{require_watched}}", checked(self.require_watched))
            .replace("{{enable_deletion}}", checked(self.enable_deletion))
            .replace("{{sweep_on_start}}", checked(self.sweep_on_start))
            .replace("{{sweep_when_full}}", checked(self.sweep_when_full))
            .replace("{{space_saving}}", checked(self.space_saving))
            .replace("{{notify_sweep}}", checked(self.notify_sweep))
            .replace("{{notify_disk}}", checked(self.notify_disk))
            .replace("{{notify_problems}}", checked(self.notify_problems))
            .replace("{{next_run}}", &html_escape(&self.next_run))
            .replace(
                "{{notify_webhook_url}}",
                &html_escape(&self.notify_webhook_url),
            )
    }
}

/// The engine and disk numbers as the form shows them.
///
/// Bytes become gibibytes for the same reason seconds become days above: 53687091200 is not
/// a number anybody decides on. The file keeps bytes.
pub struct EngineView {
    pub max_active_torrents: i32,
    pub complete_extras_below_mb: u64,
    pub global_connections_limit: u32,
    pub connections_while_streaming: u32,
    pub warn_below_free_gb: u64,
    pub warn_below_free_percent: u64,
    pub partial_download: bool,
}

impl EngineView {
    pub fn new(
        t: &crate::config::Torrent,
        m: &crate::config::Maintenance,
        p: &crate::config::Pieces,
    ) -> Self {
        Self {
            max_active_torrents: t.max_active_torrents,
            complete_extras_below_mb: t.complete_extras_below_bytes / (1024 * 1024),
            global_connections_limit: t.global_connections_limit,
            connections_while_streaming: t.connections_while_streaming,
            // Rounded up, so a threshold set below one gibibyte shows as one rather than as
            // nothing at all.
            warn_below_free_gb: m.warn_below_free_bytes.div_ceil(1024 * 1024 * 1024),
            warn_below_free_percent: m.warn_below_free_percent,
            partial_download: p.partial_download,
        }
    }

    fn fill(&self, template: &str) -> String {
        template
            .replace(
                "{{max_active_torrents}}",
                &self.max_active_torrents.to_string(),
            )
            .replace(
                "{{complete_extras_below_mb}}",
                &self.complete_extras_below_mb.to_string(),
            )
            .replace(
                "{{global_connections_limit}}",
                &self.global_connections_limit.to_string(),
            )
            .replace(
                "{{connections_while_streaming}}",
                &self.connections_while_streaming.to_string(),
            )
            .replace(
                "{{warn_below_free_gb}}",
                &self.warn_below_free_gb.to_string(),
            )
            .replace(
                "{{warn_below_free_percent}}",
                &self.warn_below_free_percent.to_string(),
            )
            .replace("{{partial_download}}", checked(self.partial_download))
    }
}

/// A count from the form, kept within bounds, falling back to the current value.
///
/// Out of range is treated as a typo rather than as an instruction: ten connections in total
/// would stall every stream, and a mistyped box must not be able to do that.
pub fn count_or_current(input: &str, current: u32, min: u32, max: u32) -> u32 {
    match input.trim().parse::<u32>() {
        Ok(n) if (min..=max).contains(&n) => n,
        _ => current,
    }
}

/// The active-torrent limit, where `-1` means no limit and is deliberately allowed through.
pub fn active_limit_or_current(input: &str, current: i32) -> i32 {
    match input.trim().parse::<i32>() {
        Ok(-1) => -1,
        Ok(n) if (1..=100_000).contains(&n) => n,
        _ => current,
    }
}

fn checked(on: bool) -> &'static str {
    if on { " checked" } else { "" }
}

/// Days back to seconds, refusing a value that would delete everything immediately.
///
/// A blank or nonsense field keeps the current setting instead of silently becoming
/// zero: zero retention with deletion enabled would wipe the library on the next
/// sweep, which is not something a mistyped form should be able to do.
pub fn days_to_seconds(input: &str, current: u64) -> u64 {
    match input.trim().parse::<u64>() {
        Ok(days) if days >= 1 => days.saturating_mul(86_400),
        _ => current,
    }
}

/// A `HH:MM` from the form, keeping the current value when it is not a valid time.
pub fn sweep_time_or_current(input: &str, current: &str) -> String {
    match crate::config::parse_hhmm(input) {
        Some((h, m)) => format!("{h:02}:{m:02}"),
        None => current.to_string(),
    }
}

/// A percentage from the form. Clamped to 1..=100: zero would mean every download
/// counts as watched the moment it is opened.
pub fn percent_or_current(input: &str, current: u8) -> u8 {
    match input.trim().parse::<u32>() {
        Ok(p) if (1..=100).contains(&p) => p as u8,
        _ => current,
    }
}

fn error_block(error: Option<String>) -> String {
    match error {
        Some(e) => format!("<p class=\"err\">{}</p>", html_escape(&e)),
        None => String::new(),
    }
}

fn message_block(message: Option<String>) -> String {
    match message {
        Some(m) => format!("<p class=\"ok\">{}</p>", html_escape(&m)),
        None => String::new(),
    }
}

/// Escapes text before it goes into HTML. The configuration contains values a user
/// typed, so it must never be interpolated raw.
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// What the browser is left looking at after the server has been asked to stop.
///
/// Not a redirect: there will be nothing to redirect to a moment from now.
pub fn stopped_page() -> String {
    let body = "<h1>A szerver leáll</h1>\
        <p class=\"sub\">Az állapot és a folytatási adatok ki vannak írva.</p>\
        <div class=\"card\"><p>Ez az oldal már nem frissül. Újraindítás: \
        indítsd el megint a stremhu-rs programot.</p></div>";
    SHELL.replace("{{body}}", body)
}

const SHELL: &str = include_str!("templates/shell.html");

const SETUP_FORM: &str = include_str!("templates/setup.html");

const LOGIN_FORM: &str = include_str!("templates/login.html");

/// What the settings page needs to say about reaching the server from elsewhere.
pub struct NetworkView {
    /// The URL to paste into Stremio, already complete with the key.
    pub addon_url: String,
    /// Whether that URL will work from another device.
    pub reachable_elsewhere: bool,
    /// One sentence on the state of HTTPS.
    pub https_state: String,
    pub host_ip: String,
    pub https_port: String,
    pub enable_https: bool,
    /// The handful of live facts worth a glance, as ready-to-place rows.
    ///
    /// A separate status page turned out to be the wrong shape: it duplicated what the
    /// settings already say and put the two or three genuinely live facts behind another
    /// click. They belong at the top of the page somebody already has open.
    pub live_rows: Vec<(String, String)>,
}

impl NetworkView {
    fn fill(&self, template: &str) -> String {
        template
            .replace("{{addon_url}}", &html_escape(&self.addon_url))
            .replace("{{https_state}}", &html_escape(&self.https_state))
            .replace("{{host_ip}}", &html_escape(&self.host_ip))
            .replace("{{https_port}}", &html_escape(&self.https_port))
            .replace("{{enable_https}}", checked(self.enable_https))
            .replace("{{live}}", &self.render_live())
            .replace(
                "{{addon_note}}",
                if self.reachable_elsewhere {
                    "A televízión és a hálózat bármelyik eszközén működik."
                } else {
                    "Csak ezen a gépen működik. Más eszközhöz HTTPS kell, mert a Stremio \
                     böngészőben fut, és a böngésző nem tölt be sima HTTP-t olyan címről, \
                     ami nem a saját gép."
                },
            )
    }

    fn render_live(&self) -> String {
        let mut out = String::new();
        for (label, value) in &self.live_rows {
            out.push_str(&format!(
                "<div class=\"kv\"><span>{}</span><b>{}</b></div>",
                html_escape(label),
                html_escape(value)
            ));
        }
        out
    }
}

const DOWNLOADS_PAGE: &str = include_str!("templates/downloads.html");

const SETTINGS_PAGE: &str = include_str!("templates/settings.html");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_round_trips_through_argon2() {
        let hash = hash_password("correct horse battery").expect("hashes");
        assert!(verify_password(&hash, "correct horse battery"));
        assert!(!verify_password(&hash, "wrong"));
        // Argon2 output carries its algorithm and parameters.
        assert!(hash.starts_with("$argon2"), "got: {hash}");
    }

    #[test]
    fn the_same_password_hashes_differently_each_time() {
        // A per-password salt means two admins with one password do not share a hash.
        let a = hash_password("same password").expect("hashes");
        let b = hash_password("same password").expect("hashes");
        assert_ne!(a, b);
    }

    #[test]
    fn short_passwords_are_refused() {
        assert!(hash_password("short").is_err());
        assert!(hash_password("12345678").is_ok());
    }

    /// A damaged or empty hash must never verify: the alternative is a config file
    /// mishap turning into open access.
    #[test]
    fn a_malformed_stored_hash_never_verifies() {
        assert!(!verify_password("", "anything"));
        assert!(!verify_password("not-a-hash", "anything"));
        assert!(!verify_password("$argon2id$broken", "anything"));
    }

    /// Checked against RFC 4231's published vectors. Writing the construction out is only
    /// defensible if it is verified against the specification's own answers.
    #[test]
    fn hmac_matches_the_published_test_vectors() {
        // RFC 4231, test case 1.
        assert_eq!(
            hmac_sha256_hex(&[0x0b; 20], b"Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        // Test case 2.
        assert_eq!(
            hmac_sha256_hex(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Test case 6: a key longer than the block, which the spec says to hash first.
        assert_eq!(
            hmac_sha256_hex(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            ),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    /// The failure that prompted all this: an in-memory session meant every restart logged
    /// the admin out. A signed token has to keep working when the server forgets everything
    /// except the secret from its configuration file.
    #[tokio::test]
    async fn a_token_survives_a_restart() {
        let before = Sessions::default();
        before.set_secret("a secret from the config file").await;
        let token = before.create(true).await;
        assert!(before.is_valid(&token).await);

        // A new process, holding nothing but the same secret.
        let after = Sessions::default();
        after.set_secret("a secret from the config file").await;
        assert!(
            after.is_valid(&token).await,
            "a restart must not log the admin out"
        );

        // Rotating the secret is the way to invalidate everything at once.
        let rotated = Sessions::default();
        rotated.set_secret("a different secret").await;
        assert!(!rotated.is_valid(&token).await);
    }

    /// A token nobody signed, or one edited after the fact, must not be accepted.
    #[tokio::test]
    async fn a_forged_or_edited_token_is_refused() {
        let s = Sessions::default();
        s.set_secret("secret").await;
        let token = s.create(false).await;

        assert!(!s.is_valid("").await);
        assert!(!s.is_valid("nonsense").await);
        assert!(!s.is_valid(&format!("{token}x")).await);
        // Claiming a local login, which lasts far longer, without a matching signature.
        let (issued, _) = token.split_once('.').expect("shaped as issued.local.sig");
        assert!(!s.is_valid(&format!("{issued}.1.deadbeef")).await);
        // An issue time in the future is a clock change or a forgery; either way not ours.
        let future = unix_now() + 86_400;
        assert!(!s.is_valid(&format!("{future}.0.deadbeef")).await);
    }

    #[tokio::test]
    async fn logging_out_takes_effect_at_once() {
        let s = Sessions::default();
        s.set_secret("secret").await;
        let token = s.create(true).await;
        assert!(s.is_valid(&token).await);
        s.destroy(&token).await;
        assert!(
            !s.is_valid(&token).await,
            "a signed token still has to be revocable"
        );
    }

    #[tokio::test]
    async fn sessions_are_accepted_only_while_they_live() {
        let s = Sessions::default();
        s.set_secret("secret").await;
        let token = s.create(false).await;
        assert!(s.is_valid(&token).await);
        assert!(!s.is_valid("some other token").await);
        assert!(!s.is_valid("").await);

        s.destroy(&token).await;
        assert!(!s.is_valid(&token).await, "logout must invalidate it");
    }

    #[tokio::test]
    async fn two_sessions_get_different_tokens() {
        let s = Sessions::default();
        assert_ne!(s.create(false).await, s.create(false).await);
    }

    /// A login at the machine itself should not have to be repeated daily; one from
    /// elsewhere on the network should.
    #[test]
    fn a_login_from_this_machine_lasts_and_one_from_elsewhere_does_not() {
        let local = Sessions::ttl(true);
        let remote = Sessions::ttl(false);
        assert!(
            local >= Duration::from_secs(300 * 24 * 3600),
            "a local login has to survive months"
        );
        assert_eq!(remote, Duration::from_secs(12 * 3600));
        assert!(local > remote);
    }

    /// Judged from the connection, never from a header, because a header saying
    /// localhost proves nothing.
    #[test]
    fn only_a_loopback_connection_counts_as_local() {
        let local: std::net::SocketAddr = "127.0.0.1:51234".parse().unwrap();
        let local_v6: std::net::SocketAddr = "[::1]:51234".parse().unwrap();
        let lan: std::net::SocketAddr = "192.168.0.20:51234".parse().unwrap();
        let far: std::net::SocketAddr = "8.8.8.8:443".parse().unwrap();

        assert!(is_local_peer(Some(local)));
        assert!(is_local_peer(Some(local_v6)));
        assert!(!is_local_peer(Some(lan)));
        assert!(!is_local_peer(Some(far)));
        // No connection information: take the cautious answer.
        assert!(!is_local_peer(None));
    }

    /// The browser must stop sending the token exactly when the server stops accepting
    /// it, or a session looks alive on one side and dead on the other.
    #[test]
    fn the_cookie_lifetime_matches_the_server_side_one() {
        for local in [true, false] {
            let c = session_cookie("tok", local);
            let expected = format!("Max-Age={}", Sessions::ttl(local).as_secs());
            assert!(c.contains(&expected), "got: {c}");
        }
    }

    #[test]
    fn the_session_cookie_is_read_out_of_a_header() {
        assert_eq!(session_from_cookies(Some("stremhu_session=abc123")), "abc123");
        assert_eq!(
            session_from_cookies(Some("other=1; stremhu_session=abc123; more=2")),
            "abc123"
        );
        assert_eq!(session_from_cookies(Some("other=1")), "");
        assert_eq!(session_from_cookies(None), "");
    }

    #[test]
    fn the_cookie_is_not_readable_by_scripts() {
        let c = session_cookie("tok", false);
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("SameSite=Lax"));
        assert!(clear_cookie().contains("Max-Age=0"));
    }

    #[test]
    fn broken_toml_is_rejected_before_anything_is_written() {
        assert!(parse_config("[server]\nport = 3080").is_ok());
        assert!(parse_config("[server\nport = ").is_err());
        // A wrong type has to fail too, not silently fall back to a default.
        assert!(parse_config("[server]\nport = \"not a number\"").is_err());
    }

    /// Configuration values are user input, so they cannot be dropped into HTML raw.
    #[test]
    fn config_text_is_escaped_for_display() {
        let out = html_escape(r#"</textarea><script>alert(1)</script>"#);
        assert!(!out.contains("<script"));
        assert!(!out.contains("</textarea>"));
        assert!(out.contains("&lt;script&gt;"));
    }

    fn network_view() -> NetworkView {
        NetworkView {
            addon_url: "https://192-168-1-100.local-ip.medicmobile.org:3443/key/manifest.json".into(),
            reachable_elsewhere: true,
            https_state: "HTTPS is running.".into(),
            host_ip: "192.168.1.100".into(),
            https_port: "3443".into(),
            enable_https: true,
            live_rows: vec![
                ("Most játszik".into(), "semmi".into()),
                ("Nyitott torrentek".into(), "2 db, seedelnek".into()),
            ],
        }
    }

    fn retention_view() -> RetentionView {
        RetentionView::from_config(&crate::config::Maintenance::default())
    }

    fn engine_view() -> EngineView {
        EngineView::new(
            &crate::config::Torrent::default(),
            &crate::config::Maintenance::default(),
            &crate::config::Pieces::default(),
        )
    }

    #[test]
    fn the_settings_page_embeds_the_escaped_config() {
        let html = page(PageState::Settings {
            toml_text: "password = \"a<b\"".into(),
            message: None,
            network: network_view(),
            engine: engine_view(),
            retention: retention_view(),
        });
        assert!(html.contains("a&lt;b"));
        assert!(!html.contains("a<b"));
    }

    /// The form has to show days, because that is the unit the setting is thought
    /// about in; the file keeps seconds.
    #[test]
    fn retention_is_shown_in_days_and_minutes() {
        let v = retention_view();
        assert_eq!(v.keep_seed_days, 10);
        assert_eq!(v.retention_days, 14);
        assert_eq!(v.sweep_at, "20:00");
        assert_eq!(v.watched_position_percent, 90);
        assert!(v.hit_and_run);
        assert!(v.require_watched);
        assert!(!v.enable_deletion, "deletion is off by default");
    }

    #[test]
    fn the_retention_form_is_prefilled_and_reflects_the_switches() {
        let html = page(PageState::Settings {
            toml_text: String::new(),
            message: None,
            network: network_view(),
            engine: engine_view(),
            retention: retention_view(),
        });
        assert!(html.contains("/ui/save-retention"));
        assert!(html.contains(r#"name="keep_seed_days" type="number" min="1" value="10""#));
        assert!(html.contains(r#"name="retention_days" type="number" min="1" value="14""#));
        assert!(html.contains(r#"name="sweep_at" type="time" value="20:00""#));
        assert!(html.contains(r#"name="hit_and_run" checked"#));
        assert!(html.contains(r#"name="require_watched" checked"#));
        // Off must render without the attribute, or the box would show as ticked.
        assert!(html.contains(r#"name="enable_deletion">"#));
        // No placeholder may survive into the page.
        assert!(!html.contains("{{"), "unsubstituted placeholder left in the page");
    }

    /// Every box the save handlers read has to exist on the page.
    ///
    /// This is here because it went wrong: the webhook field was added to the retention
    /// handler and not to the template, which does not fail to compile and does not leave a
    /// placeholder behind for the check above to catch. What it does is make the form
    /// unpostable, so every setting on that card silently stopped saving. The page-side
    /// check and this one are opposite directions of the same requirement, and both are
    /// needed: one catches a field on the page with nothing behind it, the other a field
    /// behind the page that is not on it.
    ///
    /// The list tracks the `Form` structs in ui.rs. A field added there and not here is not
    /// caught, but a field added there and not to the template is, which is the mistake that
    /// actually happens.
    #[test]
    fn every_field_the_handlers_read_has_a_box_on_the_page() {
        let expected = [
            // save-network
            "host_ip",
            "https_port",
            "enable_https",
            // save-common
            "ncore_username",
            "ncore_password",
            "tmdb_api_key",
            "tmdb_language",
            // save-retention
            "keep_seed_days",
            "retention_days",
            "sweep_at",
            "watched_position_percent",
            "watched_min_served_percent",
            "notify_webhook_url",
            "hit_and_run",
            "require_watched",
            "enable_deletion",
            "sweep_on_start",
            "sweep_when_full",
            "space_saving",
            "notify_sweep",
            "notify_disk",
            "notify_problems",
            // save-engine
            "max_active_torrents",
            "complete_extras_below_mb",
            "global_connections_limit",
            "connections_while_streaming",
            "warn_below_free_gb",
            "warn_below_free_percent",
            "partial_download",
            // save-toml
            "toml",
        ];
        for name in expected {
            assert!(
                SETTINGS_PAGE.contains(&format!("name=\"{name}\"")),
                "no input named {name} on the settings page"
            );
        }
    }

    #[test]
    fn the_engine_form_is_prefilled_from_the_configuration() {
        let html = page(PageState::Settings {
            toml_text: String::new(),
            message: None,
            network: network_view(),
            engine: engine_view(),
            retention: retention_view(),
        });
        assert!(html.contains("/ui/save-engine"));
        // The defaults: 200 active torrents, 200 connections in total, 50 for a stream,
        // and a threshold of one gibibyte shown as 1 rather than as 1073741824.
        assert!(html.contains(r#"name="max_active_torrents""#));
        assert!(html.contains(r#"value="200""#));
        assert!(html.contains(r#"value="50""#));
        assert!(html.contains(r#"name="warn_below_free_gb" type="number" min="1" max="10000""#));
        assert!(html.contains(r#"value="1""#), "the threshold shows in gibibytes");
        assert!(!html.contains("{{"), "unsubstituted placeholder left in the page");
        // Off by default, and the box has to show that rather than the other way round: a
        // checkbox drawn ticked while the setting is off turns itself on at the next save.
        assert!(
            html.contains(r#"name="partial_download">"#),
            "partial download must be unticked when it is off"
        );
    }

    /// And ticked when it is on, or the page would be lying about a setting that changes how
    /// much of a film ends up on the disk.
    #[test]
    fn partial_download_shows_as_ticked_when_it_is_on() {
        let html = page(PageState::Settings {
            toml_text: String::new(),
            message: None,
            network: network_view(),
            engine: EngineView::new(
                &crate::config::Torrent::default(),
                &crate::config::Maintenance::default(),
                &crate::config::Pieces {
                    partial_download: true,
                    ..Default::default()
                },
            ),
            retention: retention_view(),
        });
        assert!(html.contains(r#"name="partial_download" checked>"#));
    }

    /// Bytes to gibibytes and back has to survive the round trip, and a threshold set below
    /// a gibibyte must not display as zero: saving that back would mean no warning at all.
    #[test]
    fn the_free_space_threshold_round_trips_through_gibibytes() {
        let mut m = crate::config::Maintenance::default();
        let p = crate::config::Pieces::default();
        assert_eq!(
            EngineView::new(&crate::config::Torrent::default(), &m, &p).warn_below_free_gb,
            1
        );

        m.warn_below_free_bytes = 50 * 1024 * 1024 * 1024;
        assert_eq!(
            EngineView::new(&crate::config::Torrent::default(), &m, &p).warn_below_free_gb,
            50
        );

        m.warn_below_free_bytes = 100 * 1024 * 1024;
        assert_eq!(
            EngineView::new(&crate::config::Torrent::default(), &m, &p).warn_below_free_gb,
            1,
            "rounded up, never to zero"
        );
    }

    /// `-1` is a real value here and has to pass through; nonsense keeps what is set.
    #[test]
    fn the_active_torrent_limit_accepts_unlimited_and_refuses_nonsense() {
        assert_eq!(active_limit_or_current("200", 5), 200);
        assert_eq!(active_limit_or_current("-1", 5), -1);
        assert_eq!(active_limit_or_current("", 200), 200);
        assert_eq!(active_limit_or_current("0", 200), 200, "zero would pause everything");
        assert_eq!(active_limit_or_current("-7", 200), 200);
        assert_eq!(active_limit_or_current("lots", 200), 200);
    }

    #[test]
    fn a_connection_count_outside_the_range_keeps_the_current_one() {
        assert_eq!(count_or_current("300", 200, 10, 10_000), 300);
        assert_eq!(count_or_current("1", 200, 10, 10_000), 200, "too few to stream");
        assert_eq!(count_or_current("99999", 200, 10, 10_000), 200);
        assert_eq!(count_or_current("", 200, 10, 10_000), 200);
    }

    /// The page has to say the server can be stopped, because there is no longer a window
    /// to close: the executable runs without a console.
    #[test]
    fn the_settings_page_offers_to_stop_the_server() {
        assert!(SETTINGS_PAGE.contains("/ui/shutdown"));
        let stopped = stopped_page();
        assert!(stopped.contains("A szerver le"));
        assert!(!stopped.contains("{{"));
    }

    /// A blank or nonsense duration must keep the current one. Zero retention with
    /// deletion enabled would wipe the library at the next sweep.
    #[test]
    fn a_bad_duration_keeps_the_current_value() {
        assert_eq!(days_to_seconds("14", 999), 14 * 86_400);
        assert_eq!(days_to_seconds(" 3 ", 999), 3 * 86_400);
        assert_eq!(days_to_seconds("", 1_209_600), 1_209_600);
        assert_eq!(days_to_seconds("0", 1_209_600), 1_209_600);
        assert_eq!(days_to_seconds("-5", 1_209_600), 1_209_600);
        assert_eq!(days_to_seconds("soon", 1_209_600), 1_209_600);
        // Absurd input must not wrap around into a small number.
        assert_eq!(days_to_seconds(&u64::MAX.to_string(), 1), u64::MAX);
    }

    #[test]
    fn a_bad_sweep_time_keeps_the_current_one() {
        assert_eq!(sweep_time_or_current("20:00", "03:00"), "20:00");
        assert_eq!(sweep_time_or_current("7:5", "03:00"), "07:05", "normalised");
        assert_eq!(sweep_time_or_current("", "20:00"), "20:00");
        assert_eq!(sweep_time_or_current("25:00", "20:00"), "20:00");
    }

    /// Zero would mark everything watched the moment it is opened, which would let a
    /// download be deleted after a glance at it.
    #[test]
    fn a_watched_percentage_cannot_be_zero_or_over_a_hundred() {
        assert_eq!(percent_or_current("90", 50), 90);
        assert_eq!(percent_or_current("1", 90), 1);
        assert_eq!(percent_or_current("100", 90), 100);
        assert_eq!(percent_or_current("0", 90), 90);
        assert_eq!(percent_or_current("101", 90), 90);
        assert_eq!(percent_or_current("", 90), 90);
        assert_eq!(percent_or_current("most of it", 90), 90);
    }

    /// One torrent's worth of rows, as the page takes them.
    fn group(rows: Vec<DownloadRow>) -> TorrentGroup {
        TorrentGroup {
            hash: "abc123".into(),
            title: "A hegyi doktor S19".into(),
            summary: "2 fájl, 1 megnézve, 964.42 MiB".into(),
            owed_label: "igen".into(),
            owed_class: "owed-yes",
            owed_detail: "még 36 óra 5 perc".into(),
            open: true,
            rows,
        }
    }

    fn row(keep: bool) -> DownloadRow {
        DownloadRow {
            key: "abc123:1".into(),
            watched_by_hand: false,
            pack_summary: "ez a torrent 3 fájlt szolgál ki, 2 megnézve".into(),
            title: "A hegyi doktor S19E08".into(),
            size: "482.71 MiB".into(),
            added: "23 hours 0 minutes ago".into(),
            watched: "yes".into(),
            owed_label: "igen".into(),
            owed_class: "owed-yes",
            owed_detail: "még 36 óra 5 perc".into(),
            downloaded: "482.71 MiB".into(),
            uploaded: "4.75 MiB".into(),
            ratio: "0.000".into(),
            figures_age: "2 hours 0 minutes ago".into(),
            keep,
            verdict: "kept: the tracker still expects seeding".into(),
            verdict_short: "seeding owed".into(),
        }
    }

    #[test]
    fn a_download_row_shows_why_it_is_still_there() {
        let html = page(PageState::Downloads {
            groups: vec![group(vec![row(false)])],
            history: vec![("2026-08-08 19:30".into(), "Soulm8te.2026.2160p".into())],
            tracker_note: "2 open obligation(s), read just now.".into(),
            message: None,
        });
        assert!(html.contains("A hegyi doktor S19E08"));
        assert!(html.contains("kept: the tracker still expects seeding"));
        // Its own cell, coloured by the answer: red still owes, green is clear, grey is
        // "we have not asked". The word is there as well, so the colour is not the only
        // thing carrying it.
        assert!(html.contains(r#"<td class="owed-cell owed-yes">igen"#));
        assert!(html.contains("még 36 óra 5 perc"));
        // Each figure in its own cell, so a column can be read down.
        assert!(html.contains(r#"<td class="num">482.71 MiB</td>"#));
        assert!(html.contains(r#"<td class="num up">4.75 MiB</td>"#));

        // As many cells as the header has columns, or the table is misaligned.
        let header_cols = html.matches("<th").count();
        let body_cols = html
            .split("<tr>")
            .find(|part| part.contains("class=\"c-title\""))
            .map(|part| part.matches("<td").count())
            .expect("a data row");
        assert_eq!(body_cols, header_cols, "every column needs a cell");
        assert!(html.contains("2 open obligation(s)"));
        // Both actions have to be reachable for every row.
        assert!(html.contains("/ui/downloads/delete"));
        assert!(html.contains("/ui/downloads/keep"));
        assert!(!html.contains("{{"), "unsubstituted placeholder");
    }

    /// The keep button has to toggle, not just set: otherwise a kept item can never
    /// be released from the page.
    #[test]
    fn the_keep_button_toggles() {
        let not_kept = page(PageState::Downloads {
            groups: vec![group(vec![row(false)])],
            history: vec![("2026-08-08 19:30".into(), "Soulm8te.2026.2160p".into())],
            tracker_note: String::new(),
            message: None,
        });
        assert!(not_kept.contains(r#"name="keep" value="1""#));
        assert!(not_kept.contains(">Megtartás<"));

        let kept = page(PageState::Downloads {
            groups: vec![group(vec![row(true)])],
            history: vec![("2026-08-08 19:30".into(), "Soulm8te.2026.2160p".into())],
            tracker_note: String::new(),
            message: None,
        });
        assert!(kept.contains(r#"name="keep" value="0""#));
        assert!(kept.contains(">Mégse<"));
    }

    /// A release name is user-supplied text and lands in an HTML attribute.
    #[test]
    fn a_row_escapes_its_title_and_hash() {
        let mut r = row(false);
        r.title = "<script>alert(1)</script>".into();
        r.key = "\" onmouseover=\"evil()".into();
        let html = page(PageState::Downloads {
            groups: vec![group(vec![r])],
            history: vec![("2026-08-08 19:30".into(), "Soulm8te.2026.2160p".into())],
            tracker_note: String::new(),
            message: None,
        });
        assert!(!html.contains("<script>alert"));
        assert!(!html.contains("onmouseover=\"evil()"));
    }

    #[test]
    fn an_empty_library_says_so() {
        let html = page(PageState::Downloads {
            groups: Vec::new(),
            history: vec![("2026-08-08 19:30".into(), "Soulm8te.2026.2160p".into())],
            tracker_note: "The tracker has not been asked yet in this session.".into(),
            message: None,
        });
        assert!(html.contains("Még nincs letöltés."));
    }

    #[test]
    fn sizes_read_the_way_a_tracker_writes_them() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.00 KiB");
        assert_eq!(human_size(506_166_968), "482.72 MiB");
        assert_eq!(human_size(52_143_000_000), "48.56 GiB");
    }

    #[test]
    fn durations_read_as_words() {
        assert_eq!(human_duration(0), "0 perc");
        assert_eq!(human_duration(300), "5 perc");
        assert_eq!(human_duration(36 * 3600 + 300), "1 nap 12 óra");
        assert_eq!(human_duration(2 * 3600 + 600), "2 óra 10 perc");
        assert_eq!(human_ago(30), "épp most");
        assert_eq!(human_ago(3 * 86_400), "3 nap 0 óra");
    }

    #[test]
    fn setup_and_login_render_their_own_forms() {
        assert!(page(PageState::Setup).contains("/ui/setup"));
        let login = page(PageState::Login {
            error: Some("bad login".into()),
        });
        assert!(login.contains("/ui/login"));
        assert!(login.contains("bad login"));
    }
}
