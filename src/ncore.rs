//! nCore client.
//!
//! Search goes through nCore's JSON endpoint (`/torrents.php` with `jsons=true`).
//!
//! The JSON is loosely typed: numbers arrive quoted or unquoted, and fields that
//! do not apply to a given hit arrive as `false` rather than being omitted. A
//! strict struct turns that into a hard failure that kills a whole page of
//! results, so every field is extracted leniently and an unusable entry is
//! skipped with a warning instead of aborting the search.
//!
//! Session handling mirrors what a private tracker needs: the cookie jar holds
//! the session, and any request redirected to the login page triggers exactly
//! one re-login and retry.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use url::Url;

const BASE_URL: &str = "https://ncore.pro";
const LOGIN_PATH: &str = "/login.php";
const TORRENTS_PATH: &str = "/torrents.php";
const HITNRUN_PATH: &str = "/hitnrun.php";

/// One open seeding obligation, as the tracker reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitAndRun {
    /// The tracker's torrent id. The list identifies torrents this way and never by
    /// info hash, which is why a local download has to remember its id.
    pub torrent_id: String,
    pub name: String,
    /// Seeding time still owed, in seconds. None when the page did not say.
    pub remaining_secs: Option<u64>,
    /// Sent and fetched for this torrent, in bytes, by the tracker's own accounting.
    ///
    /// This is the figure that matters. A client counts only what it has done since it
    /// last started, whereas the tracker counts everything it has been told, and it is
    /// the tracker that decides whether an obligation has been met.
    pub uploaded_bytes: u64,
    pub downloaded_bytes: u64,
    /// Exactly as printed, so no rounding here can disagree with their page.
    pub ratio: String,
}

/// Reads the obligations out of the page.
///
/// An empty list and an unreadable page must not look the same. An empty list means
/// nothing is owed and everything may be deleted; an unreadable page means the answer
/// is unknown, and deleting on an unknown answer is how a seeding obligation gets
/// broken. So the container has to be found before an empty result is believed, and
/// its absence is an error.
pub fn parse_hit_and_run(html: &str) -> Result<Vec<HitAndRun>> {
    // The rows live in a container; the row divs themselves alternate class names, so
    // the name cell is the reliable anchor.
    if !html.contains("hnr_torrents") && !html.contains("hnr_tname") {
        bail!("the hit and run page has no torrent list; not logged in, or nCore changed the page");
    }

    let mut out = Vec::new();
    for block in html.split("hnr_tname").skip(1) {
        // Stop at the next row so a missing cell cannot borrow the following row's.
        let block = block.split("hnr_all").next().unwrap_or(block);

        let Some(torrent_id) = detail_link_id(block) else {
            continue;
        };
        let name = attribute(block, "title").unwrap_or_default();
        let remaining_secs = cell_text(block, "hnr_ttimespent")
            .as_deref()
            .and_then(parse_hungarian_duration);

        out.push(HitAndRun {
            torrent_id,
            name: decode_entities(&name),
            remaining_secs,
            uploaded_bytes: cell_text(block, "hnr_tup")
                .as_deref()
                .and_then(parse_size)
                .unwrap_or(0),
            downloaded_bytes: cell_text(block, "hnr_tdown")
                .as_deref()
                .and_then(parse_size)
                .unwrap_or(0),
            ratio: cell_text(block, "hnr_tratio").unwrap_or_default(),
        });
    }

    // The same torrent can be linked more than once in a row; keep the first of each.
    out.dedup_by(|a, b| a.torrent_id == b.torrent_id);
    Ok(out)
}

/// The torrent id out of a details link.
///
/// The separator before `id=` is looked for rather than assumed: nCore writes the
/// query as `&id=` in some places and entity-encoded as `&amp;id=` in others, and a
/// literal match on either one alone silently finds nothing.
fn detail_link_id(block: &str) -> Option<String> {
    let rest = block.split_once("action=details")?.1;
    let rest = rest.split_once("id=")?.1;
    let id: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if id.is_empty() { None } else { Some(id) }
}

/// The text of one `div` identified by its class, tags stripped.
///
/// The value sits inside nested markup — the tracker wraps it in a `span` — so the
/// cell is taken whole and flattened rather than read up to the next tag.
fn cell_text(block: &str, class: &str) -> Option<String> {
    let rest = block.split_once(class)?.1;
    let rest = rest.split_once('>')?.1;
    let inner = rest.split("</div>").next()?;
    Some(strip_tags(inner).trim().to_string())
}

/// The value of a double-quoted HTML attribute.
fn attribute(block: &str, name: &str) -> Option<String> {
    let marker = format!("{name}=\"");
    let rest = block.split_once(marker.as_str())?.1;
    Some(rest.split('"').next()?.to_string())
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            other if !in_tag => out.push(other),
            _ => {}
        }
    }
    out
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

/// `482.71 MiB`, `48.56 GiB`, `0 B`, as the tracker writes transfer figures.
///
/// Binary units, which is what the suffixes say and what the tracker means. Reading
/// `GiB` as 10^9 would understate a 4K release by seven percent.
fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text.find(|c: char| c.is_ascii_alphabetic())?;
    let (number, unit) = text.split_at(split);
    let value: f64 = number.trim().replace(',', ".").parse().ok()?;
    let multiplier: f64 = match unit.trim().to_ascii_uppercase().as_str() {
        "B" => 1.0,
        "KIB" | "KB" => 1024.0,
        "MIB" | "MB" => 1024.0 * 1024.0,
        "GIB" | "GB" => 1024.0 * 1024.0 * 1024.0,
        "TIB" | "TB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    let bytes = value * multiplier;
    if bytes < 0.0 {
        return None;
    }
    Some(bytes as u64)
}

/// `36ó 5p` and the like, as the tracker writes remaining seed time. Days appear as
/// `n`, so `2n 3ó` is handled too.
fn parse_hungarian_duration(text: &str) -> Option<u64> {
    let mut total = 0u64;
    let mut number = String::new();
    let mut found = false;

    for c in text.chars() {
        if c.is_ascii_digit() {
            number.push(c);
            continue;
        }
        if number.is_empty() {
            continue;
        }
        let value: u64 = number.parse().unwrap_or(0);
        number.clear();
        let unit = match c {
            'n' => Some(86_400),
            'ó' | 'o' | 'h' => Some(3_600),
            'p' | 'm' => Some(60),
            _ => None,
        };
        if let Some(secs) = unit {
            total = total.saturating_add(value.saturating_mul(secs));
            found = true;
        }
    }

    if found { Some(total) } else { None }
}

/// Values of the `miben` query parameter, i.e. which field nCore matches against.
///
/// Beware: nCore does not reject a malformed query. Passing free text with
/// `miben=imdb` returns the *entire* catalogue unfiltered rather than an error,
/// so the field has to match the shape of the query.
pub const SEARCH_BY_IMDB: &str = "imdb";
pub const SEARCH_BY_NAME: &str = "name";

/// True for `tt` followed by digits, which is the only shape [`SEARCH_BY_IMDB`]
/// can be used with safely.
pub fn is_imdb_id(query: &str) -> bool {
    let q = query.trim();
    match q.strip_prefix("tt") {
        Some(digits) => !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

/// Picks the search field from the shape of the query.
pub fn search_field_for(query: &str) -> &'static str {
    if is_imdb_id(query) {
        SEARCH_BY_IMDB
    } else {
        SEARCH_BY_NAME
    }
}

pub struct NcoreClient {
    http: reqwest::Client,
    base: Url,
    username: String,
    password: String,
}

#[derive(Debug, Clone)]
pub struct NcoreTorrent {
    pub torrent_id: String,
    pub seeders: u64,
    pub leechers: u64,
    /// Total size in bytes, as the tracker reports it. Zero when it did not say.
    /// Worth having before anything is downloaded: it is what tells a 4K remux from a
    /// re-encode of the same resolution.
    pub size_bytes: u64,
    /// None when nCore sent `false` here, which happens for hits that cannot be
    /// downloaded. Such an entry is still worth showing, just not streamable.
    pub download_url: Option<String>,
    pub category: String,
    pub imdb_id: Option<String>,
    pub title: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SearchPage {
    pub torrents: Vec<NcoreTorrent>,
    pub total_results: u64,
    /// None when the current page was the last one.
    pub next_page: Option<u32>,
}

impl NcoreClient {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .cookie_store(true)
            .user_agent(concat!("stremhu-rs/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building http client")?;

        Ok(Self {
            http,
            base: Url::parse(BASE_URL).expect("BASE_URL is a valid url"),
            username: username.into(),
            password: password.into(),
        })
    }


    pub async fn login(&self) -> Result<()> {
        let url = self.base.join(LOGIN_PATH)?;
        let res = self
            .http
            .post(url)
            .form(&[("nev", &self.username), ("pass", &self.password)])
            .send()
            .await
            .context("posting login form")?;

        // A successful login redirects away from the login page. Staying there
        // means the credentials were rejected.
        if res.url().path().contains(LOGIN_PATH) {
            bail!("nCore login rejected (check NCORE_USERNAME / NCORE_PASSWORD)");
        }
        tracing::info!("nCore login ok");
        Ok(())
    }

    /// GET that recovers from an expired session exactly once.
    async fn get(&self, url: Url) -> Result<reqwest::Response> {
        let res = self
            .http
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;

        if !Self::is_session_error(&url, &res) {
            return Ok(res);
        }

        tracing::warn!("session expired, logging in again");
        self.login().await?;

        let res = self
            .http
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("GET {url} (after relogin)"))?;

        if Self::is_session_error(&url, &res) {
            bail!("nCore still redirects to the login page after re-login");
        }
        Ok(res)
    }

    /// We landed on the login page without having asked for it.
    fn is_session_error(requested: &Url, res: &reqwest::Response) -> bool {
        res.url().path().contains(LOGIN_PATH) && !requested.path().contains(LOGIN_PATH)
    }

    /// Searches nCore, best-seeded first. `page` is 1-based. `miben` selects which
    /// field is matched; use [`SEARCH_BY_IMDB`] for an IMDb id.
    pub async fn search(&self, miben: &str, query: &str, page: u32) -> Result<SearchPage> {
        let url = search_url(&self.base, miben, query, page)?;
        let body = self.get(url).await?.text().await.context("reading body")?;

        // No hits is served as an HTML page, not as JSON, so a parse failure is
        // not automatically an error.
        let raw: RawSearch = match serde_json::from_str(&body) {
            Ok(raw) => raw,
            Err(e) => {
                if looks_like_no_results(&body) {
                    return Ok(SearchPage {
                        torrents: Vec::new(),
                        total_results: 0,
                        next_page: None,
                    });
                }
                // Surface a slice of what actually arrived; guessing at the shape
                // of this API has already cost us once.
                let head: String = body.chars().take(400).collect();
                return Err(e).with_context(|| {
                    format!("nCore returned neither JSON nor a known error page; body starts: {head}")
                });
            }
        };

        let total_results = raw.total_results;
        let mut torrents = Vec::with_capacity(raw.results.len());
        for (i, value) in raw.results.iter().enumerate() {
            match torrent_from_value(value) {
                Some(t) => torrents.push(t),
                None => tracing::warn!(index = i, "skipping a result with no usable torrent id"),
            }
        }

        let per_page = raw.perpage.max(1);
        let last_page = total_results.div_ceil(per_page);

        Ok(SearchPage {
            torrents,
            total_results,
            next_page: if u64::from(page) < last_page {
                Some(page + 1)
            } else {
                None
            },
        })
    }

    /// The tracker's own list of torrents that still owe seed time.
    ///
    /// This is the authoritative answer to "may I delete this yet", and it beats any
    /// local timer: the tracker knows what it is owed, a local clock only knows when
    /// a file appeared.
    ///
    /// The page comes in two forms and both are needed. `showall=false` is the short list, the
    /// obligations still open: measured, ten entries. `showall=true` is everything the tracker
    /// has a record of: measured on the same account and minute, a hundred and forty-eight.
    ///
    /// The difference between them is the information that was missing. A torrent on neither
    /// list is one the tracker has not processed yet, and its absence from the short list says
    /// nothing; a torrent on the long list but not the short one has genuinely settled its debt.
    /// Reading only the short list made those two look the same, which is either a download kept
    /// for no reason or one deleted while it still owed.
    pub async fn hit_and_run(&self) -> Result<Vec<HitAndRun>> {
        self.hit_and_run_list(false).await
    }

    /// Everything the tracker has a record of, settled or not.
    pub async fn hit_and_run_all(&self) -> Result<Vec<HitAndRun>> {
        self.hit_and_run_list(true).await
    }

    async fn hit_and_run_list(&self, show_all: bool) -> Result<Vec<HitAndRun>> {
        let mut url = self.base.join(HITNRUN_PATH)?;
        url.query_pairs_mut()
            .append_pair("showall", if show_all { "true" } else { "false" });
        let body = self
            .get(url)
            .await?
            .text()
            .await
            .context("reading the hit and run page")?;
        parse_hit_and_run(&body)
    }

    /// Fetches the .torrent bytes for a search hit.
    pub async fn download_torrent(&self, download_url: &str) -> Result<Vec<u8>> {
        let url = Url::parse(download_url)
            .or_else(|_| self.base.join(download_url))
            .with_context(|| format!("bad download url {download_url}"))?;

        let res = self.get(url).await?;
        let status = res.status();
        let bytes = res.bytes().await.context("reading torrent bytes")?;

        if !status.is_success() {
            bail!("nCore returned {status} for the torrent file");
        }
        // A bencoded torrent starts with a dictionary marker; anything else means
        // we were handed an HTML page instead of a file.
        if !bytes.starts_with(b"d") {
            bail!("response does not look like a .torrent file");
        }
        Ok(bytes.to_vec())
    }
}

/// Builds the search URL.
///
/// Split out so the encoding can be asserted in a test: an accented Hungarian title
/// has to reach nCore as correct UTF-8 percent-encoding, and getting that wrong is
/// invisible until searches quietly return nothing.
fn search_url(base: &Url, miben: &str, query: &str, page: u32) -> Result<Url> {
    let mut url = base.join(TORRENTS_PATH)?;
    url.query_pairs_mut()
        .append_pair("oldal", &page.max(1).to_string())
        .append_pair("miben", miben)
        .append_pair("mire", query)
        .append_pair("miszerint", "seeders")
        .append_pair("hogyan", "DESC")
        .append_pair("jsons", "true");
    Ok(url)
}

fn looks_like_no_results(body: &str) -> bool {
    body.contains("lista_mini_error") || body.contains("Nincs találat")
}

/// Builds a torrent from one JSON entry. Only the id is mandatory: everything
/// else may legitimately be absent or `false`.
fn torrent_from_value(v: &Value) -> Option<NcoreTorrent> {
    let torrent_id = loose_string(v.get("torrent_id"))?;
    Some(NcoreTorrent {
        torrent_id,
        seeders: loose_u64(v.get("seeders")),
        leechers: loose_u64(v.get("leechers")),
        size_bytes: loose_u64(v.get("size")),
        download_url: loose_string(v.get("download_url")),
        category: loose_string(v.get("category")).unwrap_or_default(),
        imdb_id: loose_string(v.get("imdb_id")),
        // nCore has used more than one key for the display name over time, so try
        // the ones that appear in practice rather than insisting on one.
        title: loose_string(v.get("release_name"))
            .or_else(|| loose_string(v.get("torrent_name")))
            .or_else(|| loose_string(v.get("name"))),
    })
}

/// Strings, numbers and nothing else. `false`, `null` and containers become None,
/// which is how nCore signals "not applicable".
fn loose_string(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        Value::String(_) => None,
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn loose_u64(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

#[derive(Debug, Deserialize)]
struct RawSearch {
    /// Kept untyped on purpose: one odd entry must not fail the whole page.
    #[serde(default)]
    results: Vec<Value>,
    #[serde(default, deserialize_with = "number_from_any")]
    total_results: u64,
    #[serde(default, deserialize_with = "number_from_any")]
    perpage: u64,
}

/// nCore is inconsistent about quoting numbers, so accept both forms.
fn number_from_any<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    match Value::deserialize(d)? {
        Value::Number(n) => Ok(n.as_u64().unwrap_or(0)),
        Value::String(s) => Ok(s.trim().parse().unwrap_or(0)),
        _ => Ok(0),
    }
}

#[cfg(test)]
mod hitnrun_tests {
    use super::*;

    /// Captured from the live page, so the parser is tested against the markup that
    /// actually exists rather than an assumption about it.
    const REAL_PAGE: &str = r#"
 	<div class="hnr_torrents">
		<div class="hnr_all2" onmouseover="this.className='hnr_hl'" onmouseout="this.className='hnr_all2'">
			<div class="hnr_tname">
				<a href="torrents.php?action=details&id=4207293" onclick="torrent(4207293); return false;" title="A hegyi doktor - Újra rendel S19E08"><nobr>A hegyi doktor - Újra rendel S19E08</nobr></a>
			</div>
			<div class="hnr_tstart">23 órája</div>
			<div class="hnr_tlastactive">10 perce</div>
			<div class="hnr_tseed"><span class="stopped">Seed</span></div>
			<div class="hnr_tup">0 B</div>
			<div class="hnr_tdown">482.71 MiB</div>
			<div class="hnr_ttimespent"><span class="stopped">36ó 5p</span></div>
			<div class="hnr_tratio"><span class="stopped">0.000</span></div>
		</div>
		<div class="hnr_all" onmouseover="this.className='hnr_hl'" onmouseout="this.className='hnr_all'">
			<div class="hnr_tname">
				<a href="torrents.php?action=details&id=3055839" onclick="torrent(3055839); return false;" title="The.Hunger.Games.Mockingjay.Part.1.2014.2160p.UHD.HDR.BluRay"><nobr>The.Hunger.Games...</nobr></a>
			</div>
			<div class="hnr_tstart">3 napja</div>
			<div class="hnr_tlastactive">27 perce</div>
			<div class="hnr_tseed"><span class="stopped">Seed</span></div>
			<div class="hnr_tup">0 B</div>
			<div class="hnr_tdown">48.56 GiB</div>
			<div class="hnr_ttimespent"><span class="stopped">16ó 14p</span></div>
			<div class="hnr_tratio"><span class="stopped">0.000</span></div>
		</div>
	</div>
"#;

    #[test]
    fn the_live_page_yields_both_obligations() {
        let list = parse_hit_and_run(REAL_PAGE).expect("parses");
        assert_eq!(list.len(), 2);

        assert_eq!(list[0].torrent_id, "4207293");
        assert_eq!(list[0].name, "A hegyi doktor - Újra rendel S19E08");
        assert_eq!(
            list[0].remaining_secs,
            Some(36 * 3600 + 5 * 60),
            "36ó 5p of seeding still owed"
        );
        // The tracker's own transfer figures, which are what decide the obligation.
        assert_eq!(list[0].uploaded_bytes, 0);
        assert_eq!(list[0].downloaded_bytes, 506_158_120, "482.71 MiB");
        assert_eq!(list[0].ratio, "0.000");

        assert_eq!(list[1].torrent_id, "3055839");
        assert_eq!(list[1].remaining_secs, Some(16 * 3600 + 14 * 60));
        assert!(list[1].name.starts_with("The.Hunger.Games"));
        assert_eq!(list[1].downloaded_bytes, 52_140_902_973, "48.56 GiB");
    }

    /// Binary units, because that is what the suffix says. Reading GiB as 10^9 would
    /// understate a 4K release by seven percent.
    #[test]
    fn transfer_figures_parse_as_binary_units() {
        assert_eq!(parse_size("0 B"), Some(0));
        assert_eq!(parse_size("482.71 MiB"), Some(506_158_120));
        assert_eq!(parse_size("48.56 GiB"), Some(52_140_902_973));
        assert_eq!(parse_size("1 KiB"), Some(1024));
        assert_eq!(parse_size("  16.95 GiB  "), Some(18_199_923_916));
        // A comma as the decimal mark, which a Hungarian locale can produce.
        assert_eq!(parse_size("1,50 GiB"), Some(1_610_612_736));
        // Anything unrecognised must be absent rather than a wrong number.
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("-"), None);
        assert_eq!(parse_size("0.000"), None, "a ratio is not a size");
        assert_eq!(parse_size("12 potatoes"), None);
    }

    /// The id must survive the entity-encoded ampersand nCore sometimes emits.
    #[test]
    fn an_escaped_ampersand_in_the_link_still_yields_the_id() {
        let html = r#"<div class="hnr_torrents"><div class="hnr_tname">
            <a href="torrents.php?action=details&amp;id=999111" title="Thing"></a></div></div>"#;
        let list = parse_hit_and_run(html).expect("parses");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].torrent_id, "999111");
    }

    /// Nothing owed is a real answer, and it has to be distinguishable from a broken
    /// page, because it permits deletion.
    #[test]
    fn an_empty_list_inside_a_real_page_is_an_empty_answer() {
        let html = r#"<div class="hnr_torrents"><div style="clear:both;"></div></div>"#;
        assert_eq!(parse_hit_and_run(html).expect("parses").len(), 0);
    }

    /// The case that must never be read as "nothing is owed": if nCore redesigns the
    /// page or the session has expired, deleting would break a seeding obligation.
    #[test]
    fn a_page_without_the_list_is_an_error_not_an_empty_answer() {
        assert!(parse_hit_and_run("<html><body>Belépés</body></html>").is_err());
        assert!(parse_hit_and_run("").is_err());
        assert!(parse_hit_and_run("<div class=\"box_torrent_all\"></div>").is_err());
    }

    /// A row missing its remaining-time cell must still protect the torrent; only the
    /// remaining time is unknown.
    #[test]
    fn a_row_without_a_duration_still_counts_as_an_obligation() {
        let html = r#"<div class="hnr_torrents"><div class="hnr_tname">
            <a href="torrents.php?action=details&id=1234" title="No time cell"></a></div></div>"#;
        let list = parse_hit_and_run(html).expect("parses");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].remaining_secs, None);
    }

    #[test]
    fn hungarian_durations_parse() {
        assert_eq!(parse_hungarian_duration("36ó 5p"), Some(36 * 3600 + 300));
        assert_eq!(parse_hungarian_duration("16ó 14p"), Some(16 * 3600 + 840));
        assert_eq!(parse_hungarian_duration("2n 3ó"), Some(2 * 86_400 + 3 * 3600));
        assert_eq!(parse_hungarian_duration("45p"), Some(2_700));
        assert_eq!(parse_hungarian_duration("0.000"), None, "a ratio is not a time");
        assert_eq!(parse_hungarian_duration(""), None);
        assert_eq!(parse_hungarian_duration("--"), None);
    }

    #[test]
    fn tags_and_entities_are_cleaned_up() {
        assert_eq!(strip_tags("<span class=\"x\">36ó 5p</span>"), "36ó 5p");
        assert_eq!(decode_entities("Tom &amp; Jerry"), "Tom & Jerry");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact shape that broke the first implementation: `download_url: false`.
    #[test]
    fn tolerates_false_instead_of_a_string() {
        let json = r#"{
            "results": [
                {"torrent_id": 1, "seeders": 5, "download_url": false, "category": false, "imdb_id": false},
                {"torrent_id": "2", "seeders": "9", "download_url": "https://ncore.pro/x", "category": "hd_hun", "imdb_id": "tt1392170"}
            ],
            "total_results": "45",
            "perpage": 20
        }"#;
        let raw: RawSearch = serde_json::from_str(json).expect("parses");
        let parsed: Vec<_> = raw.results.iter().filter_map(torrent_from_value).collect();

        assert_eq!(parsed.len(), 2, "a `false` field must not drop the entry");
        assert_eq!(parsed[0].torrent_id, "1");
        assert_eq!(parsed[0].download_url, None);
        assert_eq!(parsed[0].category, "");
        assert_eq!(parsed[0].imdb_id, None);
        assert_eq!(parsed[1].torrent_id, "2");
        assert_eq!(parsed[1].seeders, 9);
        assert_eq!(parsed[1].download_url.as_deref(), Some("https://ncore.pro/x"));
    }

    #[test]
    fn accepts_quoted_and_unquoted_numbers() {
        let raw: RawSearch =
            serde_json::from_str(r#"{"total_results": "45", "perpage": 20}"#).expect("parses");
        assert_eq!(raw.total_results, 45);
        assert_eq!(raw.perpage, 20);
    }

    #[test]
    fn missing_fields_default() {
        let raw: RawSearch = serde_json::from_str("{}").expect("parses");
        assert!(raw.results.is_empty());
        assert_eq!(raw.total_results, 0);
        assert_eq!(raw.perpage, 0);
    }

    #[test]
    fn an_entry_without_an_id_is_skipped() {
        let v: Value = serde_json::from_str(r#"{"seeders": 3}"#).expect("parses");
        assert!(torrent_from_value(&v).is_none());
    }

    #[test]
    fn empty_strings_count_as_absent() {
        assert_eq!(loose_string(Some(&Value::String("   ".into()))), None);
        assert_eq!(loose_string(None), None);
        assert_eq!(loose_string(Some(&Value::Bool(false))), None);
        assert_eq!(loose_string(Some(&Value::Null)), None);
    }

    #[test]
    fn imdb_ids_are_recognised_by_shape() {
        assert!(is_imdb_id("tt1392170"));
        assert!(is_imdb_id("  tt0111161  "));
        assert!(!is_imdb_id("tt"));
        assert!(!is_imdb_id("exatlon"));
        assert!(!is_imdb_id("tt12a34"));
        assert!(!is_imdb_id(""));
        assert!(!is_imdb_id("1392170"));
    }

    #[test]
    fn search_field_follows_the_query_shape() {
        assert_eq!(search_field_for("tt1392170"), SEARCH_BY_IMDB);
        // Free text on `imdb` would return the entire catalogue unfiltered.
        assert_eq!(search_field_for("exatlon"), SEARCH_BY_NAME);
    }

    /// The server's job with accents is simply to transmit them correctly. nCore
    /// handles accented queries itself, so what matters is that `csatája` leaves here
    /// as UTF-8 percent-encoding and not as mangled bytes.
    #[test]
    fn an_accented_query_is_utf8_percent_encoded() {
        let base = Url::parse(BASE_URL).expect("base");
        let url = search_url(&base, SEARCH_BY_NAME, "Exek csatája", 1).expect("builds");
        let q = url.query().expect("has a query");

        // á is C3 A1 in UTF-8.
        assert!(q.contains("csat%C3%A1ja"), "got: {q}");
        assert!(q.contains("miben=name"));
        assert!(q.contains("jsons=true"));
        // The value must arrive as one parameter, not split on the space.
        let mire = url
            .query_pairs()
            .find(|(k, _)| k == "mire")
            .map(|(_, v)| v.to_string());
        assert_eq!(mire.as_deref(), Some("Exek csatája"));
    }

    #[test]
    fn the_page_number_is_never_zero() {
        let base = Url::parse(BASE_URL).expect("base");
        let url = search_url(&base, SEARCH_BY_IMDB, "tt123", 0).expect("builds");
        assert!(url.query().expect("query").contains("oldal=1"));
    }

    #[test]
    fn recognises_the_no_results_page() {
        assert!(looks_like_no_results(
            "<div class=\"lista_mini_error\">Nincs találat!</div>"
        ));
        assert!(!looks_like_no_results("<html>something else</html>"));
    }
}
