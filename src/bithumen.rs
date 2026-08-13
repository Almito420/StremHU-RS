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
/// The list lives on the account's own page, not on a page of its own.
///
/// The original StremHU asks `/hitnrun.php?id=<user>&hnr=1`; measured against the live site
/// that is a 404, and what answers is `/userdetails.php?id=<user>&hnr=1`, which is also the URL
/// the site's own interface produces. The `hnr=1` is what expands the section.
const USERDETAILS_PATH: &str = "/userdetails.php";
const INDEX_PATH: &str = "/index.php";

/// What the tracker itself says a release is, out of the category image.
///
/// The site writes it as the image's alt text — `Film/Hun/1080p`, `Sorozat/Hun/SD`,
/// `Film/Eng/720p`, `Film/Hun/DVD-R` — and that is worth more than the numeric category id the
/// original maps: it names the type, the audio language and the resolution in one string, and
/// it is right even for an old upload whose filename says none of them.
fn category_from_row(row: &str) -> String {
    // The first image in the row is the category one; the later ones are the download icon and
    // the cover, whose alt text is `info` or nothing.
    for part in row.split("<img").skip(1) {
        let tag = part.split_once('>').map(|(t, _)| t).unwrap_or(part);
        let Some(alt) = crate::ncore::attribute(tag, "alt") else {
            continue;
        };
        if alt.contains('/') {
            return crate::ncore::decode_entities(&alt);
        }
    }
    String::new()
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

    /// The tracker's own list of torrents that still owe seeding.
    ///
    /// Returns the torrent id and how long it still has to run, which the site does print: its
    /// table has a "Hátravan" column, measured on the live page as `23 óra 58 perc`. What it
    /// does not print is the per-torrent transfer totals, so there is nothing to run the ratio
    /// arithmetic on and these downloads fall back to the flat seeding time — the cautious side.
    pub async fn hit_and_run(&self) -> Result<Vec<(String, Option<u64>)>> {
        let user_id = self.user_id().await?;
        let mut url = self.base.join(USERDETAILS_PATH)?;
        url.query_pairs_mut()
            .append_pair("id", &user_id)
            .append_pair("hnr", "1");

        let body = self
            .get(url)
            .await?
            .text()
            .await
            .context("reading the hit and run page")?;
        parse_hit_and_run(&body)
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
///
/// Not a number. Measured on the live site the link is `userdetails.php?id=uWiYAMjoweY-`, so
/// anything up to the end of the value is the id; reading digits only found nothing at all and
/// the hit-and-run list could never be fetched.
pub fn parse_user_id(html: &str) -> Option<String> {
    let rest = html.split_once("userdetails.php?id=")?.1;
    let id: String = rest
        .chars()
        .take_while(|c| !matches!(c, '"' | '\'' | '&' | '#' | '<' | '>' | ' '))
        .collect();
    if id.is_empty() { None } else { Some(id) }
}

/// The hit-and-run list: the torrent id and how long it still has to run.
///
/// The list is a table inside the account's own page, under an `<a name="hnr">` anchor, with
/// the columns Típus, Név, Seed idő, Hátravan. Only that table is read; the page carries other
/// tables with torrent links in them, and a link from one of those would be recorded as an
/// obligation that does not exist.
///
/// An empty page and an unreadable one are told apart before anything is believed. Without the
/// section this returns an error, because "nothing is owed" is the one answer that lets the
/// sweep delete.
pub fn parse_hit_and_run(html: &str) -> Result<Vec<(String, Option<u64>)>> {
    if !looks_logged_in(html) {
        bail!("the BitHUmen hit and run page came back as a guest page; not logged in");
    }
    let Some(section) = hnr_section(html) else {
        bail!(
            "cannot find the Hit & Run section on the BitHUmen user page; the page changed,              and an unreadable answer is not an empty one"
        );
    };

    let mut out: Vec<(String, Option<u64>)> = Vec::new();
    for row in section.split("<tr").skip(1) {
        let cells = cells(row);
        let Some(id) = torrent_id(row) else {
            continue;
        };
        // The last column is what is left to seed. Absent is not zero: the caller keeps the
        // download either way, and a missing figure only means the page did not say.
        let remaining = cells
            .last()
            .and_then(|text| crate::ncore::parse_hungarian_duration(text));
        if !out.iter().any(|(known, _)| *known == id) {
            out.push((id, remaining));
        }
    }
    Ok(out)
}

/// The part of the user page that holds the hit-and-run table, and nothing else.
fn hnr_section(html: &str) -> Option<&str> {
    let anchor = html.find("name=\"hnr\"").or_else(|| html.find("name='hnr'"))?;
    let rest = &html[anchor..];
    let table = rest.find("<table")?;
    let rest = &rest[table..];
    // No nested tables inside this one, so the first close is the right one.
    Some(match rest.find("</table>") {
        Some(end) => &rest[..end],
        None => rest,
    })
}

/// A torrent id out of a `details.php?id=` link, ignoring `userdetails.php?id=`.
///
/// The two differ by three letters and both appear on the same page. Without this check the
/// account's own id was collected as a torrent id, and the account is not a torrent.
fn torrent_id(row: &str) -> Option<String> {
    for (i, _) in row.match_indices("details.php?id=") {
        if row[..i].ends_with("user") {
            continue;
        }
        let rest = &row[i + "details.php?id=".len()..];
        let id: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !id.is_empty() {
            return Some(id);
        }
    }
    None
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

        let cells = cells(row);
        // The name from the link's `title`, not from the text of the link. The visible text is
        // truncated with an ellipsis, and everything the quality is read from lives in the part
        // that was cut off. This is why a BitHUmen row showed no resolution, no source and no
        // audio: those tags were never in the string being parsed.
        let title = details_title(row)
            .or_else(|| anchor_text(row, "details.php?id="))
            .or_else(|| name_from_download_path(download))
            .map(|t| decode_entities(&t));
        let (seeders, leechers) = swarm(&cells);

        out.push(Torrent {
            tracker: Tracker::Bithumen,
            torrent_id,
            seeders,
            leechers,
            size_bytes: size_of_row(&cells),
            download_url,
            category: category_from_row(row),
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

/// Every `href` value in a fragment, however it is quoted.
///
/// Both quote styles and neither: the live page writes `href="details.php?id=1"` for the name
/// but `href='http://www.imdb.com/title/tt1951266/'` for the IMDb link, in the same cell. A
/// reader that only knew double quotes found no IMDb id on any row, which is the id the whole
/// search is keyed on.
fn hrefs(row: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in row.split("href=").skip(1) {
        let Some(first) = part.chars().next() else {
            continue;
        };
        let value = match first {
            '"' | '\'' => part[1..].split(first).next().unwrap_or(""),
            _ => part
                .split(|c: char| c.is_whitespace() || c == '>')
                .next()
                .unwrap_or(""),
        };
        if !value.is_empty() {
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

/// The full release name, out of the `title` attribute of the details link.
///
/// The attribute has to come from that one tag: the same cell carries a `title` on the download
/// icon, on the trailer link and on the Hungarian title, and any of those would be picked up by
/// a search for the first `title=` in the cell.
fn details_title(row: &str) -> Option<String> {
    for (i, _) in row.match_indices("details.php?id=") {
        if row[..i].ends_with("user") {
            continue;
        }
        let rest = &row[i..];
        // Only the plain link to the torrent. The same cell links to the same torrent four
        // times — `&trailer=1`, `&filelist=1`, `&dllist=1`, `&others=1` — and those carry
        // titles of their own: measured on the live page, a row whose name link had no title
        // came back called "Előzetes", the title of the trailer icon.
        let after_id = &rest["details.php?id=".len()..];
        let end = after_id
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after_id.len());
        if end == 0 || !matches!(after_id[end..].chars().next(), Some('"') | Some('\'')) {
            continue;
        }
        let tag = rest.split_once('>').map(|(t, _)| t).unwrap_or(rest);
        if let Some(title) = crate::ncore::attribute(tag, "title") {
            if !title.trim().is_empty() {
                return Some(title);
            }
        }
        // The plain link is the name, so if it has no title the visible text is the whole
        // name: the site only adds a title when it had to shorten it.
        return None;
    }
    None
}

/// The torrent's size out of the row.
///
/// Found by pattern and preferring the size column, because the cell holds more than the size:
/// the live page writes `<u>16.48 GiB</u>` and then an upload-ratio figure in the same cell, so
/// reading the cell as one number and unit found nothing at all and every BitHUmen row showed
/// no size.
fn size_of_row(cells: &[String]) -> u64 {
    const SIZE_COLUMN: usize = 5;
    if let Some(bytes) = cells.get(SIZE_COLUMN).and_then(|c| find_size(c)) {
        return bytes;
    }
    // A column added or removed: take the largest size-shaped value in the row rather than
    // trusting an index.
    cells.iter().filter_map(|c| find_size(c)).max().unwrap_or(0)
}

/// The first `12.3 GiB`-shaped value anywhere in a piece of text.
fn find_size(text: &str) -> Option<u64> {
    static PATTERN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)(\d+(?:[.,]\d+)?)\s*(TiB|GiB|MiB|KiB|TB|GB|MB|KB)")
            .expect("the size pattern compiles")
    });
    let m = PATTERN.captures(text)?;
    parse_size(&format!("{} {}", m.get(1)?.as_str(), m.get(2)?.as_str()))
}

/// Seeders and leechers out of the row's cells.
///
/// The original reads them from columns seven and eight. Those are used when they hold numbers,
/// and when they do not — a column added, a layout changed — the last two whole numbers in the
/// row are taken instead, which is where a TBDev table keeps them. Guessing zero seeders would
/// be worse than either: the ranking drops a release with none.
fn swarm(cells: &[String]) -> (u64, u64) {
    const SEEDERS: usize = 7;
    const LEECHERS: usize = 8;
    let whole = |i: usize| -> Option<u64> {
        cells
            .get(i)
            .and_then(|c| c.replace([' ', ',', '\u{a0}'], "").parse::<u64>().ok())
    };
    // The leecher cell is not a plain number: the live page writes `0 / 0`, real leechers and
    // all leechers. The first of the two is the one that matters, and taking the cell whole
    // failed, which then dragged the seeder count into the fallback with it.
    let first_number = |i: usize| -> Option<u64> {
        let cell = cells.get(i)?;
        let digits: String = cell
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(|c| c.is_ascii_digit())
            .collect();
        digits.parse().ok()
    };

    if let Some(seeders) = whole(SEEDERS) {
        return (seeders, first_number(LEECHERS).unwrap_or(0));
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

    /// A row shaped like the live page's, cell for cell.
    ///
    /// Every awkward detail here is one the real site has and an earlier version of this parser
    /// got wrong: the visible name is truncated with an ellipsis while the full one is in the
    /// `title` attribute (single-quoted), the size cell carries an upload-ratio figure after the
    /// size, the leecher cell is `0 / 0` rather than a number, and the same cell holds three more
    /// `details.php?id=` links and three more `title=` attributes.
    fn row() -> String {
        "<tr>\n\
         <td align=center class=cati style='padding: 0px'><a href=\"?cat=37\">\
         <img src=\"https://x/null.png\" alt=\"Film/Hun/1080p\" class='spr_c Scat_1080p_hun' \
         width=42 height=42 /></a></td>\n\
         <td align=left ><a href=\"details.php?id=1200182\" \
         title='The.Hunger.Games.Mockingjay.Part.2.2015.1080p.UHD.BluRay.DDP7.1.DoVi.HDR10.x265.HuN-TRiNiTY'>\
         <b>The.Hunger.Games.Mockingjay.Part.2.2015.1080p.UHD.BluRay.DDP...</b></a> \n\
         <a href=\"download.php/1200182/%5BbHUm_%231200182%5DThe_Hunger_Games.torrent\" \
         title=\"Letöltés\" style='border: none'><img src=\"https://x/null.png\" \
         class=\"spr_b Sgray-dl_icon\" width=16 height=15></a>\n\
         <div><a href='http://www.port.hu/valami' target='_blank' alt=\"info\">\
         <img src='https://x/null.png' class=\"spr_b Sgray-cover_icon\"></a>&nbsp;\
         <span title='Az éhezők viadala: A kiválasztott - Befejező rész'>Az éhezők viada...</span> \
         <a href='details.php?id=1200182&amp;trailer=1#trailer' title='Előzetes'><img></a> \
         <a href='http://www.imdb.com/title/tt1951266/' target='_blank'><b>[</b>imdb<b>]</b></a> \
         (<span><a href='browse.php?genre=1'>akció</a></span>) \
         <a href=\"details.php?id=1200182&amp;others=1#others\" title=\"7 további verzió\">\
         <img></a> </div></td>\n\
         <td align=right><b><a href=\"details.php?id=1200182&amp;filelist=1#filelist\">3</a></b></td>\n\
         <td align=right>0</td>\n\
         <td align=center><nobr>2025-10-20 12:05<br><font><img/> &times; 4</font></nobr></td>\n\
         <td align=center><u>16.48 GiB</u><br><nobr><font><img/> &times; 0.5</font></nobr></td>\n\
         <td align=center>45</td>\n\
         <td align=right><b><a href='details.php?id=1200182&amp;dllist=1#seeders'>\
         <font color=#000000>3</font></a></b></td>\n\
         <td align=right>0 / 0</td>\n\
         </tr>"
            .to_string()
    }

    fn browse_page(rows: &str) -> String {
        format!(
            "<html><body><div id=\"status\">\
             <a href=\"/userdetails.php?id=uWiYAMjoweY-\">valaki</a> \
             <a href=\"logout.php\">kilépés</a></div>\
             <table id=\"torrenttable\">\
             <tr><td class=\"colhead\">Típus</td><td class=\"colhead\">Név</td>\
             <td class=\"colhead\">Fileok</td><td class=\"colhead\">Komm.</td>\
             <td class=\"colhead\">Feltöltve</td><td class=\"colhead\">Méret</td>\
             <td class=\"colhead\">DLs</td><td class=\"colhead\">Seed</td>\
             <td class=\"colhead\">VL/Leech</td></tr>\
             {rows}</table></body></html>"
        )
    }

    #[test]
    fn a_search_result_row_is_read_whole() {
        let found =
            parse_browse(&browse_page(&row()), &Url::parse(BASE_URL).unwrap()).expect("readable");
        assert_eq!(found.len(), 1);
        let t = &found[0];

        assert_eq!(t.tracker, Tracker::Bithumen);
        assert_eq!(t.torrent_id, "1200182");
        // The full name, not the truncated one on screen. Everything the quality is read from
        // is in the part that was cut off.
        assert_eq!(
            t.title.as_deref(),
            Some(
                "The.Hunger.Games.Mockingjay.Part.2.2015.1080p.UHD.BluRay.DDP7.1.DoVi.HDR10.x265.HuN-TRiNiTY"
            )
        );
        // And it does produce the quality, which is the point of taking the full name.
        let listing = crate::media::listing(
            t.tracker.label(),
            t.title.as_deref().unwrap_or_default(),
            &t.category,
            t.seeders,
            t.leechers,
            t.size_bytes,
            false,
        );
        for expected in ["1080p", "Blu-ray", "Hun", "16.48 GiB"] {
            assert!(
                listing.name.contains(expected) || listing.description.contains(expected),
                "{expected} is missing from the listing: {} / {}",
                listing.name,
                listing.description
            );
        }

        assert_eq!(t.imdb_id.as_deref(), Some("tt1951266"));
        assert_eq!(t.seeders, 3, "the seeder column, not the download count");
        assert_eq!(t.leechers, 0);
        assert_eq!(t.size_bytes, (16.48 * 1024.0 * 1024.0 * 1024.0) as u64);
        assert_eq!(t.category, "Film/Hun/1080p");
        assert_eq!(
            t.download_url.as_deref(),
            Some(
                "https://bithumen.be/download.php/1200182/%5BbHUm_%231200182%5DThe_Hunger_Games.torrent"
            )
        );
    }

    /// A row whose name link has no `title` at all: the site only adds one when it had to
    /// shorten the name, so the visible text is then the whole name. Measured on the live page,
    /// reading "the first title in the cell" instead called seven of fifteen rows "Előzetes",
    /// which is the title of the trailer icon.
    #[test]
    fn a_row_without_a_shortened_name_still_gets_its_name() {
        let short = "<tr>             <td><a href=\"?cat=3\"><img alt=\"Film/Hun/SD\"></a></td>             <td><a href=\"details.php?id=345806\"><b>Some.Film.2015.BDRiP.x264.HuN-Hyperx</b></a>              <a href=\"download.php/345806/x.torrent\" title=\"Letöltés\"><img></a>             <div><a href='details.php?id=345806&amp;trailer=1#trailer' title='Előzetes'><img></a>              <a href='http://www.imdb.com/title/tt1951266/'>imdb</a></div></td>             <td>1</td><td>0</td><td>2015-01-01</td><td><u>1.52 GiB</u></td>             <td>10</td><td>36</td><td>0 / 0</td></tr>";
        let found =
            parse_browse(&browse_page(short), &Url::parse(BASE_URL).unwrap()).expect("readable");
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].title.as_deref(),
            Some("Some.Film.2015.BDRiP.x264.HuN-Hyperx"),
            "the name, not the trailer icon's title"
        );
        // And the category still carries the quality the name does not.
        assert_eq!(found[0].category, "Film/Hun/SD");
    }

    /// The table with no rows in it is an answer: nothing was found. An error here would send a
    /// search on to nowhere, and a hit that does not exist is not worth inventing.
    #[test]
    fn an_empty_table_is_an_empty_result() {
        let found =
            parse_browse(&browse_page(""), &Url::parse(BASE_URL).unwrap()).expect("readable");
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

        let empty = parse_browse(
            "<html><body>Nincs találat a keresésre.</body></html>",
            &Url::parse(BASE_URL).unwrap(),
        )
        .expect("a stated empty result");
        assert!(empty.is_empty());
    }

    /// The hit-and-run table as the live user page writes it: an anchor, a table with four
    /// columns, and the remaining time spelled out in words.
    ///
    /// The release name here is invented. The structure is the site's.
    fn user_page(hnr_rows: &str) -> String {
        format!(
            "<html><body><div id=\"status\">\
             <a href=\"/userdetails.php?id=uWiYAMjoweY-\">valaki</a> \
             <a href=\"logout.php\">kilépés</a></div>\
             <table><tr><td class='rowhead'>Torrent ajánlások</td>\
             <td><a href=\"/details.php?id=999999\">egy ajánlás, nem tartozás</a></td></tr>\
             <tr valign=\"top\"><td class=\"rowhead\"><a name=\"hnr\"></a>Hit &amp; Run<br>\
             <a class=\"sublink\" href=\"/userdetails.php?id=uWiYAMjoweY-\">[Elrejt]</a></td>\
             <td align=\"left\"><table class=\"main\" border=\"1\">\
             <tr><td class=\"colhead\"><span>Típus</span></td>\
             <td class=\"colhead\"><span>Név</span></td>\
             <td class=\"colhead\"><span>Seed idő</span></td>\
             <td class=\"colhead\"><span>Hátravan</span></td></tr>\
             {hnr_rows}</table></td></tr></table></body></html>"
        )
    }

    #[test]
    fn the_hit_and_run_list_gives_the_ids_and_the_time_left() {
        let page = user_page(
            "<tr><td style=\"padding: 0px\"><img alt=\"Sorozat/Hun/SD\"></td>\
             <td><a href=\"/details.php?id=1197963\" title=\"Some.Series.S04.HUN.WEB-DL-GROUP\">\
             <b>Some.Series.S04 ...</b></a></td>\
             <td align=\"center\">2 perc</td>\
             <td align=\"center\">23 óra 58 perc</td></tr>",
        );
        let owed = parse_hit_and_run(&page).expect("readable");
        assert_eq!(owed.len(), 1, "one obligation, and not the recommendation");
        assert_eq!(owed[0].0, "1197963");
        assert_eq!(owed[0].1, Some(23 * 3600 + 58 * 60));
    }

    /// Two ways this page can lie to the sweep, and both are guarded.
    #[test]
    fn the_hit_and_run_page_has_to_be_a_real_page() {
        // Logged out: unknown, not empty. Empty is the answer that permits a deletion.
        assert!(parse_hit_and_run("<html>Belépés</html>").is_err());
        // Logged in but the section is gone: the page changed, so the answer is unknown.
        assert!(parse_hit_and_run("<html><a href=\"logout.php\">ki</a>semmi</html>").is_err());
        // Logged in, section there, no rows: a genuine empty list.
        let none = parse_hit_and_run(&user_page("")).expect("readable");
        assert!(none.is_empty());
    }

    /// `userdetails.php?id=` contains `details.php?id=`, and the account is not a torrent. The
    /// account's own id is also not a number on this site, which is what stopped the list from
    /// ever being fetched.
    #[test]
    fn the_account_is_not_mistaken_for_a_torrent() {
        assert_eq!(
            parse_user_id("<div id=\"status\"><a href=\"/userdetails.php?id=uWiYAMjoweY-\">n</a>"),
            Some("uWiYAMjoweY-".to_string())
        );
        assert_eq!(parse_user_id("<html>semmi</html>"), None);

        assert_eq!(
            torrent_id("<a href=\"/userdetails.php?id=uWiYAMjoweY-\">én</a>"),
            None
        );
        assert_eq!(
            torrent_id("<a href=\"/userdetails.php?id=uWiYAMjoweY-\">én</a> \
                        <a href=\"/details.php?id=42\">torrent</a>"),
            Some("42".to_string())
        );
    }

    /// A column added to the table must not turn into zero seeders: a release with none is
    /// dropped from the list, so a parsing slip would look like a tracker with no swarm.
    #[test]
    fn the_swarm_survives_an_extra_column() {
        let normal: Vec<String> = ["", "név", "3", "0", "2025-10-20", "16.48 GiB", "45", "3", "0 / 0"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(swarm(&normal), (3, 0));

        // Shifted, so column seven is not a number any more.
        let shifted: Vec<String> = ["", "név", "új", "3", "0", "2025-10-20", "16.48 GiB", "x", "37", "4"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(swarm(&shifted), (37, 4));
    }

    /// The size lives in a cell with other figures in it, so it is found by shape.
    #[test]
    fn the_size_is_found_next_to_the_ratio_figure() {
        assert_eq!(
            find_size("16.48 GiB× 0.5"),
            Some((16.48 * 1024.0 * 1024.0 * 1024.0) as u64)
        );
        assert_eq!(find_size("1,37 GB"), Some((1.37 * 1024.0 * 1024.0 * 1024.0) as u64));
        assert_eq!(find_size("2025-10-20 12:05× 4"), None, "a date is not a size");
        assert_eq!(find_size("45"), None);
    }
}
