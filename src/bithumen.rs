//! BitHUmen, the second tracker.
//!
//! Only ever asked when nCore came back with nothing. That is the whole point of it being
//! here: nCore is the account with fifteen years of history on it, and a search that finds a
//! film there needs no second opinion. What BitHUmen is for is the title nCore does not have.
//!
//! The site is a TBDev-style tracker, so unlike nCore there is no JSON API: the search results
//! are an HTML table and have to be read as one. The selectors below are the ones the original
//! StremHU uses against the same pages (`#torrenttable` rows, the `download.php/` link, the
//! `details.php?id=` link, the IMDb link, the category link), so this is not guesswork about
//! the markup; what is new is that the size is found by pattern rather than by column number,
//! because a column index is the thing that breaks silently when a tracker adds a column.
//!
//! Two rules are deliberate and worth stating, because both protect the account:
//!
//!   * an unreadable page is an error, never an empty result. "Nothing is owed" and "I could
//!     not read the answer" must not look the same to the sweep.
//!   * a download URL is only fetched if it is on this tracker's own host, so a crafted URL
//!     cannot make us send this session's cookies somewhere else.

use anyhow::{Context, Result, bail};
use reqwest::Url;
use tokio::sync::Mutex;

use crate::ncore::{decode_entities, parse_size, strip_tags};
use crate::tracker::{Torrent, Tracker};

const BASE_URL: &str = "https://bithumen.be";
const LOGIN_PATH: &str = "/takelogin.php";
const LOGIN_PAGE: &str = "/login.php";
const BROWSE_PATH: &str = "/browse.php";
const HITNRUN_PATH: &str = "/hitnrun.php";
const INDEX_PATH: &str = "/index.php";

/// The categories the original maps, and what they say about a release.
///
/// Only used when the release name itself does not say. A name like
/// `Some.Film.2019.1080p.BluRay.x264-GROUP` needs none of this; a bare Hungarian title from an
/// old SD upload needs all of it.
fn category_hint(cat_id: &str) -> &'static str {
    match cat_id {
        // Hungarian audio, high definition.
        "31" | "35" => "hd_hun",
        // Foreign audio, high definition.
        "28" | "36" => "hd",
        // Hungarian audio, standard definition.
        "23" | "32" => "sd_hun",
        // Foreign audio, standard definition.
        "3" | "33" => "sd",
        _ => "",
    }
}

pub struct BithumenClient {
    http: reqwest::Client,
    base: Url,
    username: String,
    password: String,
    /// The account's own user id, needed for the hit-and-run page. Read once from the status
    /// bar and kept: it does not change, and asking for it per sweep is a request that says
    /// nothing new to the tracker.
    user_id: Mutex<Option<String>>,
}

impl BithumenClient {
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
            user_id: Mutex::new(None),
        })
    }

    /// Whether there is an account to log in with at all.
    pub fn configured(&self) -> bool {
        !self.username.trim().is_empty() && !self.password.is_empty()
    }

    pub async fn login(&self) -> Result<()> {
        if !self.configured() {
            bail!("bithumen.username or bithumen.password is empty");
        }
        let url = self.base.join(LOGIN_PATH)?;
        let res = self
            .http
            .post(url)
            .form(&[
                ("username", self.username.as_str()),
                ("password", self.password.as_str()),
                ("returnto", "/"),
            ])
            .send()
            .await
            .context("posting the login form")?;

        if res.url().path().contains(LOGIN_PAGE) {
            bail!("BitHUmen login rejected (check bithumen.username / bithumen.password)");
        }

        // Asked rather than assumed. A TBDev site answers a failed login with a page that is
        // not the login page either, so the only reliable proof of a session is a page that
        // offers to end it.
        let index = self.base.join(INDEX_PATH)?;
        let body = self
            .http
            .get(index)
            .send()
            .await
            .context("fetching the index page after login")?
            .text()
            .await
            .context("reading the index page")?;
        if !looks_logged_in(&body) {
            bail!("BitHUmen login did not take: the site still answers as a guest");
        }
        tracing::info!("BitHUmen login ok");
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

        tracing::warn!("BitHUmen session expired, logging in again");
        self.login().await?;

        let res = self
            .http
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("GET {url} (after relogin)"))?;
        if Self::is_session_error(&url, &res) {
            bail!("BitHUmen still redirects to the login page after re-login");
        }
        Ok(res)
    }

    fn is_session_error(requested: &Url, res: &reqwest::Response) -> bool {
        res.url().path().contains(LOGIN_PAGE) && !requested.path().contains(LOGIN_PAGE)
    }

    /// One page of search results. `page` is 1-based here and 0-based on the site.
    ///
    /// The same free-text field takes an IMDb id and a title, which is what makes the two
    /// search plans work the same way here as on nCore.
    pub async fn search(&self, query: &str, page: u32) -> Result<Vec<Torrent>> {
        let mut url = self.base.join(BROWSE_PATH)?;
        url.query_pairs_mut()
            .append_pair("genre", "0")
            .append_pair("search", query)
            .append_pair("page", &page.max(1).saturating_sub(1).to_string());

        let body = self.get(url).await?.text().await.context("reading body")?;
        parse_browse(&body, &self.base)
    }

    /// The tracker's own list of torrents that still owe seeding, by torrent id.
    ///
    /// No figures come with it. BitHUmen's page carries the names and the links but not the
    /// per-torrent transfer totals, so there is nothing here to run the ratio arithmetic on:
    /// for these torrents the answer is the list itself plus the flat seeding time from the
    /// settings, which is the cautious side.
    pub async fn hit_and_run_ids(&self) -> Result<Vec<String>> {
        let user_id = self.user_id().await?;
        let mut url = self.base.join(HITNRUN_PATH)?;
        url.query_pairs_mut()
            .append_pair("id", &user_id)
            .append_pair("hnr", "1");

        let body = self
            .get(url)
            .await?
            .text()
            .await
            .context("reading the hit and run page")?;
        parse_hit_and_run_ids(&body)
    }

    async fn user_id(&self) -> Result<String> {
        if let Some(id) = self.user_id.lock().await.clone() {
            return Ok(id);
        }
        let body = self
            .get(self.base.join(INDEX_PATH)?)
            .await?
            .text()
            .await
            .context("reading the index page")?;
        let id = parse_user_id(&body).context(
            "cannot find the account's own user id on the BitHUmen index page; not logged in, \
             or the page changed",
        )?;
        *self.user_id.lock().await = Some(id.clone());
        Ok(id)
    }

    pub async fn download_torrent(&self, download_url: &str) -> Result<Vec<u8>> {
        let url = Url::parse(download_url)
            .or_else(|_| self.base.join(download_url))
            .with_context(|| format!("bad download url {download_url}"))?;
        // This session's cookies go with the request, so the host has to be ours.
        if url.host_str() != self.base.host_str() {
            bail!(
                "refusing to fetch a torrent from {} with the BitHUmen session",
                url.host_str().unwrap_or("an unknown host")
            );
        }

        let res = self.get(url).await?;
        let status = res.status();
        let bytes = res.bytes().await.context("reading torrent bytes")?;
        if !status.is_success() {
            bail!("BitHUmen returned {status} for the torrent file");
        }
        if !bytes.starts_with(b"d") {
            bail!("response does not look like a .torrent file");
        }
        Ok(bytes.to_vec())
    }
}

/// A page that offers to log out is a page we are logged in on.
fn looks_logged_in(html: &str) -> bool {
    html.contains("logout.php") || html.contains("userdetails.php?id=")
}

/// The account's own user id, from the status bar link.
pub fn parse_user_id(html: &str) -> Option<String> {
    let rest = html.split_once("userdetails.php?id=")?.1;
    let id: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if id.is_empty() { None } else { Some(id) }
}

/// The torrent ids on the hit-and-run page.
///
/// An empty page and an unreadable one are told apart before anything is believed: without the
/// marker this returns an error, because "nothing is owed" is the one answer that lets the
/// sweep delete.
pub fn parse_hit_and_run_ids(html: &str) -> Result<Vec<String>> {
    if !looks_logged_in(html) {
        bail!("the BitHUmen hit and run page came back as a guest page; not logged in");
    }
    let mut out: Vec<String> = Vec::new();
    for block in html.split("details.php?id=").skip(1) {
        let id: String = block.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !id.is_empty() && !out.contains(&id) {
            out.push(id);
        }
    }
    Ok(out)
}

/// Reads the search results out of the browse page.
///
/// Returns an empty list only when the page really is an empty result: either the results table
/// is there with no rows in it, or the site said so in words. Anything else is an error.
pub fn parse_browse(html: &str, base: &Url) -> Result<Vec<Torrent>> {
    let has_table = html.contains("torrenttable");
    if !has_table {
        if looks_like_no_results(html) {
            return Ok(Vec::new());
        }
        let head: String = html.chars().take(400).collect();
        bail!(
            "the BitHUmen browse page has no results table and does not say it is empty; \
             not logged in, or the page changed. Body starts: {head}"
        );
    }

    let mut out = Vec::new();
    for row in html.split("<tr").skip(1) {
        // Rows without a download link are the header and the pager.
        if !row.contains("download.php/") {
            continue;
        }
        let row = row.split("</tr>").next().unwrap_or(row);
        let links = hrefs(row);

        let Some(download) = links.iter().find(|h| h.contains("download.php/")) else {
            continue;
        };
        let Some(details) = links.iter().find(|h| h.contains("details.php?id=")) else {
            continue;
        };
        let torrent_id: String = details
            .split_once("details.php?id=")
            .map(|(_, rest)| rest.chars().take_while(|c| c.is_ascii_digit()).collect())
            .unwrap_or_default();
        if torrent_id.is_empty() {
            continue;
        }

        let download_url = base
            .join(&decode_entities(download))
            .map(|u| u.to_string())
            .ok();

        let imdb_id = links
            .iter()
            .find(|h| h.contains("imdb.com/title/"))
            .and_then(|h| imdb_id_from(h));

        let category = links
            .iter()
            .find_map(|h| h.rsplit_once("?cat=").map(|(_, id)| id))
            .map(|id| category_hint(id.trim()))
            .unwrap_or("")
            .to_string();

        let cells = cells(row);
        let title = anchor_text(row, "details.php?id=")
            .or_else(|| name_from_download_path(download))
            .map(|t| decode_entities(&t));
        let (seeders, leechers) = swarm(&cells);

        out.push(Torrent {
            tracker: Tracker::Bithumen,
            torrent_id,
            seeders,
            leechers,
            size_bytes: cells.iter().filter_map(|c| parse_size(c)).max().unwrap_or(0),
            download_url,
            category,
            imdb_id,
            title,
        });
    }
    Ok(out)
}

/// The site's own way of saying there was nothing.
fn looks_like_no_results(html: &str) -> bool {
    let lower = html.to_lowercase();
    ["nincs találat", "nincs a keresésnek", "nincs ilyen torrent", "no torrents"]
        .iter()
        .any(|needle| lower.contains(needle))
}

/// Every `href` value in a fragment.
fn hrefs(row: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in row.split("href=\"").skip(1) {
        if let Some(value) = part.split('"').next() {
            out.push(value.to_string());
        }
    }
    out
}

/// Every table cell in a row, tags stripped and entities decoded.
fn cells(row: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in row.split("<td").skip(1) {
        let Some((_, inner)) = part.split_once('>') else {
            continue;
        };
        let inner = inner.split("</td>").next().unwrap_or(inner);
        out.push(decode_entities(strip_tags(inner).trim()));
    }
    out
}

/// The text of the first anchor whose href contains `needle`.
fn anchor_text(row: &str, needle: &str) -> Option<String> {
    let after_href = row.split_once(needle)?.1;
    let after_tag = after_href.split_once('>')?.1;
    let inner = after_tag.split("</a>").next()?;
    let text = strip_tags(inner).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

/// `tt1234567` out of an IMDb link.
fn imdb_id_from(href: &str) -> Option<String> {
    let rest = href.split_once("imdb.com/title/")?.1;
    let id: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    if id.starts_with("tt") && id.len() > 2 {
        Some(id)
    } else {
        None
    }
}

/// The release name from `download.php/12345/Some.Release.torrent`.
fn name_from_download_path(href: &str) -> Option<String> {
    let last = href.rsplit('/').next()?;
    let name = last.strip_suffix(".torrent").unwrap_or(last);
    let name = name.replace('+', " ");
    if name.is_empty() { None } else { Some(name) }
}

/// Seeders and leechers out of the row's cells.
///
/// The original reads them from columns seven and eight. Those are used when they hold numbers,
/// and when they do not — a column added, a layout changed — the last two whole numbers in the
/// row are taken instead, which is where a TBDev table keeps them. Guessing zero seeders would
/// be worse than either: the ranking drops a release with none.
fn swarm(cells: &[String]) -> (u64, u64) {
    let numeric = |i: usize| -> Option<u64> {
        cells
            .get(i)
            .and_then(|c| c.replace([' ', ',', '\u{a0}'], "").parse::<u64>().ok())
    };
    if let (Some(s), Some(l)) = (numeric(7), numeric(8)) {
        return (s, l);
    }
    let numbers: Vec<u64> = cells
        .iter()
        .filter_map(|c| c.replace([' ', ',', '\u{a0}'], "").parse::<u64>().ok())
        .collect();
    match numbers.len() {
        0 => (0, 0),
        1 => (numbers[0], 0),
        n => (numbers[n - 2], numbers[n - 1]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row shaped like the site's, with the markup the original's selectors describe:
    /// the category link, the download link, the IMDb link, the details link, then the
    /// figures. `1.37<br>GB` is how a TBDev table writes a size, and it has to survive
    /// having its tags taken out.
    fn page(rows: &str) -> String {
        format!(
            "<html><body><div id=\"status\">\
             <a href=\"/userdetails.php?id=4242\">valaki</a> \
             <a href=\"logout.php\">kilépés</a></div>\
             <table id=\"torrenttable\"><tbody>\
             <tr><td>Kategória</td><td>Név</td><td>Ki</td><td>Mikor</td><td>Méret</td>\
             <td>Le</td><td>Fel</td><td>Seed</td><td>Leech</td></tr>\
             {rows}</tbody></table></body></html>"
        )
    }

    fn row(id: &str, name: &str, cat: &str, size: &str, seed: &str, leech: &str) -> String {
        format!(
            "<tr class=\"rowfollow\">\
             <td><a href=\"?cat={cat}\"><img src=\"pic.gif\"></a></td>\
             <td><a href=\"details.php?id={id}\"><b>{name}</b></a> \
             <a href=\"download.php/{id}/{name}.torrent\">letöltés</a> \
             <a href=\"https://www.imdb.com/title/tt1951266/\">imdb</a></td>\
             <td>valaki</td><td>2026-08-01</td><td>{size}</td>\
             <td>12</td><td>3</td><td>{seed}</td><td>{leech}</td></tr>"
        )
    }

    #[test]
    fn a_search_result_row_is_read_whole() {
        let html = page(&row(
            "98765",
            "The.Hunger.Games.Mockingjay.Part.2.2015.1080p.BluRay.x264-GROUP",
            "28",
            "8.42<br>GB",
            "37",
            "4",
        ));
        let base = Url::parse(BASE_URL).unwrap();
        let found = parse_browse(&html, &base).expect("readable page");
        assert_eq!(found.len(), 1);

        let t = &found[0];
        assert_eq!(t.tracker, Tracker::Bithumen);
        assert_eq!(t.torrent_id, "98765");
        assert_eq!(t.seeders, 37);
        assert_eq!(t.leechers, 4);
        assert_eq!(t.imdb_id.as_deref(), Some("tt1951266"));
        assert_eq!(
            t.title.as_deref(),
            Some("The.Hunger.Games.Mockingjay.Part.2.2015.1080p.BluRay.x264-GROUP")
        );
        // Binary units, and the `<br>` between the number and the unit must not defeat it.
        assert_eq!(t.size_bytes, (8.42 * 1024.0 * 1024.0 * 1024.0) as u64);
        assert_eq!(
            t.download_url.as_deref(),
            Some(
                "https://bithumen.be/download.php/98765/The.Hunger.Games.Mockingjay.Part.2.2015.1080p.BluRay.x264-GROUP.torrent"
            )
        );
        // A high-definition foreign-audio category, which is what tells an old upload's
        // resolution when its name does not.
        assert_eq!(t.category, "hd");
    }

    /// The table with no rows in it is an answer: nothing was found. An error here would send
    /// the search on to nowhere, and a hit that does not exist is not worth inventing.
    #[test]
    fn an_empty_table_is_an_empty_result() {
        let html = page("");
        let found = parse_browse(&html, &Url::parse(BASE_URL).unwrap()).expect("readable");
        assert!(found.is_empty());
    }

    /// And a page that is not the results page at all is an error, not an empty result. This is
    /// the difference that protects the account: an unreadable answer must never be read as
    /// "there is nothing here".
    #[test]
    fn a_guest_page_is_an_error() {
        let err = parse_browse("<html><body>Belépés</body></html>", &Url::parse(BASE_URL).unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("not logged in"), "got: {err}");

        // Unless it says in words that there was nothing, which some searches answer with.
        let empty = parse_browse(
            "<html><body>Nincs találat a keresésre.</body></html>",
            &Url::parse(BASE_URL).unwrap(),
        )
        .expect("a stated empty result");
        assert!(empty.is_empty());
    }

    /// The same rule on the hit-and-run page, and this one decides deletions.
    #[test]
    fn the_hit_and_run_page_has_to_be_a_real_page() {
        let ids = parse_hit_and_run_ids(
            "<html><a href=\"logout.php\">ki</a><table><tr><td>\
             <a href=\"/details.php?id=111\">egy</a></td></tr><tr><td>\
             <a href=\"/details.php?id=222\">kettő</a></td></tr>\
             <tr><td><a href=\"/details.php?id=111\">egy megint</a></td></tr></table></html>",
        )
        .expect("readable");
        assert_eq!(ids, vec!["111", "222"], "each torrent once");

        // Logged out: unknown, not empty.
        assert!(parse_hit_and_run_ids("<html>Belépés</html>").is_err());

        // Logged in with nothing owed is a genuine empty list.
        let none = parse_hit_and_run_ids("<html><a href=\"logout.php\">ki</a>Nincs</html>")
            .expect("readable");
        assert!(none.is_empty());
    }

    #[test]
    fn the_user_id_comes_from_the_status_bar() {
        assert_eq!(
            parse_user_id("<div id=\"status\"><a href=\"/userdetails.php?id=4242\">n</a></div>"),
            Some("4242".to_string())
        );
        assert_eq!(parse_user_id("<html>semmi</html>"), None);
    }

    /// A column added to the table must not turn into zero seeders: a release with none is
    /// dropped from the list, so a parsing slip would look like a tracker with no swarm.
    #[test]
    fn the_swarm_survives_an_extra_column() {
        // Seven and eight hold the numbers: taken as they are.
        let normal: Vec<String> = ["k", "név", "ki", "mikor", "1.5 GB", "12", "3", "37", "4"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(swarm(&normal), (37, 4));

        // A column inserted, so those two are no longer numbers. The last two whole numbers
        // in the row are the swarm, which is where the table keeps them.
        let shifted: Vec<String> = ["k", "név", "új", "ki", "mikor", "1.5 GB", "megjegyzés", "x", "37", "4"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(swarm(&shifted), (37, 4));
    }
}
