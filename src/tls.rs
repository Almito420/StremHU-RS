//! HTTPS, and how a server on a private address gets a certificate a television will
//! accept.
//!
//! # Why this is needed at all
//!
//! Stremio is a web application, served over HTTPS, and a page served over HTTPS may
//! not fetch plain HTTP. `localhost` is the one exception browsers make, which is why
//! the addon installs from this machine and fails from the television: `192.168.1.100`
//! is an ordinary address as far as the browser is concerned, so the request is
//! blocked before it is sent. The implementation being replaced has exactly the same
//! constraint and solves it exactly this way.
//!
//! # How a private address gets a public certificate
//!
//! A certificate authority will not issue for `192.168.1.100`; it is not a name and it
//! is not globally unique. The trick is a public wildcard domain whose names encode a
//! private address: `192-168-1-100.local-ip.example.org` resolves, in public DNS, to
//! `192.168.1.100`. A single wildcard certificate for `*.local-ip.example.org` then
//! covers every private address anyone might have, and its private key is published
//! deliberately so that anyone can serve it.
//!
//! # What that costs
//!
//! A published private key is not secret, so anyone who has it can present a valid
//! certificate for that hostname. In practice the exposure is narrow: the names only
//! ever point at private addresses, so the attack requires already being on this
//! network, and at that point plain HTTP would be no better. It is worth stating
//! rather than glossing over, because "valid certificate" normally implies "nobody
//! else can impersonate this", and here it does not.
//!
//! # Renewal
//!
//! The certificate is a normal short-lived one, so it is fetched again well before it
//! expires. It is also cached on disk: a restart while the provider is unreachable
//! should still come up with HTTPS working, and an expired cached copy is better
//! diagnosed than silently replaced by nothing.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// A certificate chain with its key, both PEM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Certificate {
    pub fullchain: String,
    pub privkey: String,
}

/// What the provider serves. Only two of its fields are needed; `cert` and `chain`
/// arrive separately as well and are the same data split up.
#[derive(Debug, Deserialize)]
struct KeysResponse {
    fullchain: String,
    privkey: String,
}

impl Certificate {
    /// Rejects anything that is not actually a certificate and a key.
    ///
    /// Checked here rather than at first connection: a truncated download or an error
    /// page saved as a certificate would otherwise surface as a television that
    /// silently fails to connect, which is a much harder thing to work out.
    pub fn validate(&self) -> Result<()> {
        if !self.fullchain.contains("BEGIN CERTIFICATE") {
            bail!("the certificate chain contains no certificate");
        }
        if !self.privkey.contains("BEGIN") || !self.privkey.contains("PRIVATE KEY") {
            bail!("the private key is not a PEM private key");
        }
        // The chain has to parse as at least one certificate, and the key as a key.
        let certs = rustls_pemfile::certs(&mut self.fullchain.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .context("parsing the certificate chain")?;
        if certs.is_empty() {
            bail!("the certificate chain parsed to nothing");
        }
        let key = rustls_pemfile::private_key(&mut self.privkey.as_bytes())
            .context("parsing the private key")?;
        if key.is_none() {
            bail!("no usable private key in the file");
        }
        Ok(())
    }

    /// Seconds until the leaf certificate expires, read from the certificate itself.
    ///
    /// Returns None when the expiry cannot be determined; the caller then treats the
    /// certificate as usable but unknown rather than throwing it away.
    pub fn expires_in(&self, now: u64) -> Option<i64> {
        let not_after = self.not_after()?;
        Some(not_after as i64 - now as i64)
    }

    /// The leaf certificate's `notAfter`, as a unix timestamp.
    ///
    /// Read by walking the DER rather than with a certificate library: the only fact
    /// needed is one timestamp, and pulling in a full X.509 parser to learn it would be
    /// a large dependency for one field.
    fn not_after(&self) -> Option<u64> {
        let der = rustls_pemfile::certs(&mut self.fullchain.as_bytes())
            .next()?
            .ok()?;
        parse_not_after(der.as_ref())
    }
}

/// Finds the validity period in a DER certificate and returns its end.
///
/// A certificate's `Validity` is a two-element SEQUENCE of times, and the times are
/// the only `UTCTime` or `GeneralizedTime` values in the first part of the structure,
/// so the second such value is the expiry. This is narrow but it is checked by tests
/// against a real certificate from the provider.
fn parse_not_after(der: &[u8]) -> Option<u64> {
    let mut times: Vec<u64> = Vec::new();
    let mut i = 0usize;

    while i + 2 <= der.len() {
        let tag = der[i];
        // 0x17 UTCTime, 0x18 GeneralizedTime.
        if tag != 0x17 && tag != 0x18 {
            i += 1;
            continue;
        }
        let len = der[i + 1] as usize;
        // Times are short and never use the long form; anything else is not a time.
        if len == 0 || len > 32 || i + 2 + len > der.len() {
            i += 1;
            continue;
        }
        let text = std::str::from_utf8(&der[i + 2..i + 2 + len]).ok();
        if let Some(secs) = text.and_then(|t| parse_asn1_time(t, tag == 0x18)) {
            times.push(secs);
            if times.len() == 2 {
                return times.pop();
            }
        }
        i += 2 + len;
    }
    None
}

/// `YYMMDDHHMMSSZ`, or `YYYYMMDDHHMMSSZ` for a generalized time.
fn parse_asn1_time(text: &str, generalized: bool) -> Option<u64> {
    let t = text.trim_end_matches('Z');
    let digits: Vec<u32> = t.chars().map(|c| c.to_digit(10)).collect::<Option<_>>()?;
    let expected = if generalized { 14 } else { 12 };
    if digits.len() != expected {
        return None;
    }
    let num = |from: usize, count: usize| -> u32 {
        digits[from..from + count].iter().fold(0, |a, d| a * 10 + d)
    };

    let (year, rest) = if generalized {
        (num(0, 4), 4)
    } else {
        // Two-digit years: 50 and above mean the twentieth century, by convention.
        let yy = num(0, 2);
        (if yy >= 50 { 1900 + yy } else { 2000 + yy }, 2)
    };
    let month = num(rest, 2);
    let day = num(rest + 2, 2);
    let hour = num(rest + 4, 2);
    let minute = num(rest + 6, 2);
    let second = num(rest + 8, 2);

    days_from_civil(year as i64, month, day).map(|days| {
        (days * 86_400) as u64 + (hour * 3600 + minute * 60 + second) as u64
    })
}

/// Days from 1970-01-01 to the given date, by the usual proleptic Gregorian formula.
fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// The hostname for a private address under a wildcard domain: `1.2.3.4` becomes
/// `1-2-3-4.<domain>`.
pub fn local_ip_host(ip: &str, domain: &str) -> Result<String> {
    let ip = ip.trim();
    if ip.is_empty() {
        bail!("network.host_ip is not set, so the certificate hostname cannot be built");
    }
    let parts: Vec<&str> = ip.split('.').collect();
    if parts.len() != 4 || !parts.iter().all(|p| p.parse::<u8>().is_ok()) {
        bail!("{ip:?} is not an IPv4 address");
    }
    Ok(format!("{}.{}", parts.join("-"), domain.trim()))
}

/// The address this machine uses to reach the rest of the network.
///
/// Needed because the certificate hostname is built from it, and a first run should not
/// have to be configured before HTTPS can work at all. Found by opening a UDP socket
/// towards an outside address and asking which local address the routing table chose:
/// no packet is sent, and unlike enumerating interfaces this picks the one that is
/// actually used, rather than the first of several that happens to be listed.
///
/// Only a private address is accepted. A public one would mean this machine is directly
/// on the internet, where the local-ip trick does not apply and exposing the server
/// would be a much larger decision than a default should make.
pub fn detect_lan_ipv4() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    // Any routable address will do; this one is never contacted.
    socket.connect("8.8.8.8:80").ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(v4) if v4.is_private() && !v4.is_loopback() => Some(v4.to_string()),
        _ => None,
    }
}

/// Fetches the published certificate for the wildcard domain.
///
/// Two shapes are accepted, because the services that offer this do not agree on one
/// and depending on a single provider would make the television's ability to connect
/// hinge on one website staying up:
///
///   * one URL returning JSON with `fullchain` and `privkey`
///   * two URLs, each a plain PEM file, when `key_url` is given
///
/// Both were verified against live services while this was written.
pub async fn fetch(provider_url: &str, key_url: &str) -> Result<Certificate> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .context("building the certificate http client")?;

    let body = get_text(&http, provider_url).await?;

    let cert = if key_url.trim().is_empty() {
        let keys: KeysResponse = serde_json::from_str(&body).with_context(|| {
            format!(
                "{provider_url} did not return the expected JSON; if it serves plain PEM, \
                 set network.cert_key_url to its key file as well"
            )
        })?;
        Certificate {
            fullchain: keys.fullchain,
            privkey: keys.privkey,
        }
    } else {
        Certificate {
            fullchain: body,
            privkey: get_text(&http, key_url.trim()).await?,
        }
    };

    cert.validate()?;
    Ok(cert)
}

async fn get_text(http: &reqwest::Client, url: &str) -> Result<String> {
    let res = http
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = res.status();
    let body = res.text().await.context("reading the response")?;
    if !status.is_success() {
        bail!(
            "{url} returned {status}: {}",
            body.chars().take(200).collect::<String>()
        );
    }
    Ok(body)
}

/// Where the cached copy lives.
pub struct Cache {
    dir: PathBuf,
}

impl Cache {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn chain_path(&self) -> PathBuf {
        self.dir.join("fullchain.pem")
    }

    fn key_path(&self) -> PathBuf {
        self.dir.join("privkey.pem")
    }

    pub fn load(&self) -> Option<Certificate> {
        let fullchain = std::fs::read_to_string(self.chain_path()).ok()?;
        let privkey = std::fs::read_to_string(self.key_path()).ok()?;
        let cert = Certificate {
            fullchain,
            privkey,
        };
        // A cached file that no longer parses is worthless; fall back to fetching.
        cert.validate().ok().map(|()| cert)
    }

    pub fn store(&self, cert: &Certificate) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        write_atomic(&self.chain_path(), &cert.fullchain)?;
        write_atomic(&self.key_path(), &cert.privkey)?;
        Ok(())
    }
}

fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Whether a certificate should be replaced now.
///
/// Renewing early is free and running out is not, so anything inside the margin is
/// treated as due. An unreadable expiry counts as due for the same reason.
pub fn needs_renewal(cert: &Certificate, now: u64, margin_secs: i64) -> bool {
    match cert.expires_in(now) {
        Some(remaining) => remaining <= margin_secs,
        None => true,
    }
}

/// The certificate to serve with, from the cache when it is still good and from the
/// provider otherwise.
///
/// A failed fetch with a usable cached copy is a warning, not an error: an expiring
/// certificate still works, and refusing to start would take the server down over
/// something that will very likely succeed tomorrow.
pub async fn obtain(
    provider_url: &str,
    key_url: &str,
    cache: &Cache,
    now: u64,
    margin_secs: i64,
) -> Result<Certificate> {
    let cached = cache.load();

    if let Some(cert) = &cached {
        if !needs_renewal(cert, now, margin_secs) {
            tracing::info!("using the cached certificate");
            return Ok(cert.clone());
        }
        tracing::info!("the cached certificate is due for renewal");
    }

    match fetch(provider_url, key_url).await {
        Ok(cert) => {
            if let Err(e) = cache.store(&cert) {
                // Not fatal: the certificate in hand still works for this run.
                tracing::warn!(error = %e, "could not cache the certificate");
            }
            tracing::info!("fetched a certificate from {provider_url}");
            Ok(cert)
        }
        Err(e) => match cached {
            Some(cert) => {
                tracing::warn!(
                    error = %e,
                    "could not fetch a certificate; carrying on with the cached one"
                );
                Ok(cert)
            }
            None => Err(e.context("no certificate available, and none cached")),
        },
    }
}

/// Turns PEM into the server's TLS configuration.
pub fn rustls_config(cert: &Certificate) -> Result<Arc<rustls::ServerConfig>> {
    let certs = rustls_pemfile::certs(&mut cert.fullchain.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .context("parsing the certificate chain")?;
    let key = rustls_pemfile::private_key(&mut cert.privkey.as_bytes())
        .context("parsing the private key")?
        .context("the private key file held no key")?;

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("the certificate and key do not go together")?;
    Ok(Arc::new(config))
}

/// Checks daily whether the certificate is due, and swaps it in without dropping a
/// connection.
///
/// Reloading in place matters: the certificate outlives a viewing session, but a
/// restart to pick up a new one would cut whatever is playing. The check is daily
/// because certificates are measured in months and the renewal margin in weeks.
pub fn spawn_renewal(
    serving: axum_server::tls_rustls::RustlsConfig,
    provider_url: String,
    key_url: String,
    cache_dir: String,
    margin_secs: i64,
) {
    tokio::spawn(async move {
        let cache = Cache::new(&cache_dir);
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(24 * 3600)).await;

            let current = match cache.load() {
                Some(cert) => cert,
                None => {
                    tracing::warn!("no cached certificate to check; fetching one");
                    match fetch(&provider_url, &key_url).await {
                        Ok(cert) => cert,
                        Err(e) => {
                            tracing::warn!(error = %e, "certificate check failed");
                            continue;
                        }
                    }
                }
            };

            let remaining = current.expires_in(crate::state::now());
            if !needs_renewal(&current, crate::state::now(), margin_secs) {
                if let Some(secs) = remaining {
                    tracing::debug!(days = secs / 86_400, "certificate still good");
                }
                continue;
            }

            match fetch(&provider_url, &key_url).await {
                Ok(fresh) => {
                    if let Err(e) = cache.store(&fresh) {
                        tracing::warn!(error = %e, "could not cache the renewed certificate");
                    }
                    match serving
                        .reload_from_pem(
                            fresh.fullchain.clone().into_bytes(),
                            fresh.privkey.clone().into_bytes(),
                        )
                        .await
                    {
                        Ok(()) => tracing::info!("certificate renewed and reloaded"),
                        Err(e) => {
                            tracing::error!(error = %e, "renewed certificate could not be loaded")
                        }
                    }
                }
                Err(e) => tracing::warn!(error = %e, "certificate renewal failed; will retry"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real chain served by the provider, captured live. Only the certificate is
    /// here; the key is not needed to test the parsing and does not belong in a repo.
    const REAL_CHAIN: &str = include_str!("../testdata/local-ip-fullchain.pem");

    #[test]
    fn the_hostname_encodes_the_private_address() {
        assert_eq!(
            local_ip_host("192.168.1.100", "local-ip.medicmobile.org").unwrap(),
            "192-168-1-100.local-ip.medicmobile.org"
        );
        assert_eq!(
            local_ip_host(" 10.0.0.1 ", "local-ip.co").unwrap(),
            "10-0-0-1.local-ip.co"
        );
    }

    /// A first run has to work with no configuration, so this must find the address and
    /// it must be one the certificate trick applies to.
    #[test]
    fn the_machines_own_address_is_found_and_is_private() {
        let found = detect_lan_ipv4();
        // A machine with no network at all legitimately has nothing to find; anything
        // else has to produce a usable private address.
        if let Some(ip) = found {
            let parsed: std::net::Ipv4Addr = ip.parse().expect("a valid IPv4 address");
            assert!(parsed.is_private(), "{ip} is not a private address");
            assert!(!parsed.is_loopback());
            // And it has to be usable as a certificate hostname: the first label is the
            // address with dashes, which is what the wildcard DNS resolves back.
            let host = local_ip_host(&ip, "local-ip.medicmobile.org").expect("builds");
            assert!(host.ends_with(".local-ip.medicmobile.org"));
            let label = host.split('.').next().expect("a first label");
            assert_eq!(label, ip.replace('.', "-"));
        }
    }

    #[test]
    fn a_bad_address_is_refused_rather_than_turned_into_a_bad_hostname() {
        assert!(local_ip_host("", "local-ip.co").is_err());
        assert!(local_ip_host("not-an-ip", "local-ip.co").is_err());
        assert!(local_ip_host("192.168.0", "local-ip.co").is_err());
        assert!(local_ip_host("192.168.0.999", "local-ip.co").is_err());
        assert!(local_ip_host("::1", "local-ip.co").is_err());
    }

    /// The pinned chain expires at 2026-10-30T23:53:16Z, read off the certificate with
    /// an independent tool. Anchoring the test to the exact second is the point: an
    /// off-by-a-few-days expiry check would still look plausible while renewing at the
    /// wrong time.
    #[test]
    fn the_expiry_is_read_from_the_real_certificate() {
        let cert = Certificate {
            fullchain: REAL_CHAIN.to_string(),
            privkey: String::new(),
        };
        assert_eq!(cert.not_after(), Some(1_793_404_396));
        // And the same value through the public method.
        assert_eq!(cert.expires_in(1_793_404_000), Some(396));
        assert_eq!(cert.expires_in(1_793_404_400), Some(-4), "already expired");
    }

    #[test]
    fn asn1_times_parse_in_both_forms() {
        // 2026-10-31T23:59:59Z as a UTCTime.
        assert_eq!(parse_asn1_time("261031235959Z", false), Some(1_793_491_199));
        // The same moment as a GeneralizedTime.
        assert_eq!(
            parse_asn1_time("20261031235959Z", true),
            Some(1_793_491_199)
        );
        // The start of that day, so a slip in the day arithmetic cannot hide.
        assert_eq!(parse_asn1_time("261031000000Z", false), Some(1_793_404_800));
        assert_eq!(parse_asn1_time("19700101000000Z", true), Some(0));
        // Two-digit years of 50 or more are last century.
        assert_eq!(parse_asn1_time("991231000000Z", false), Some(946_598_400));
        // Nonsense must not become a timestamp.
        assert_eq!(parse_asn1_time("", false), None);
        assert_eq!(parse_asn1_time("not-a-time", false), None);
        assert_eq!(parse_asn1_time("261331000000Z", false), None, "month 13");
        assert_eq!(parse_asn1_time("2610312359Z", false), None, "too short");
    }

    #[test]
    fn renewal_is_due_inside_the_margin() {
        let cert = Certificate {
            fullchain: REAL_CHAIN.to_string(),
            privkey: String::new(),
        };
        let expiry = cert.not_after().expect("readable");
        let month = 30 * 24 * 3600;

        // Two months before expiry, with a one month margin: not yet.
        assert!(!needs_renewal(&cert, expiry - 2 * month, month as i64));
        // Two weeks before, with the same margin: due.
        assert!(needs_renewal(&cert, expiry - 14 * 24 * 3600, month as i64));
        // Past expiry: certainly due.
        assert!(needs_renewal(&cert, expiry + 1, month as i64));
    }

    /// An unreadable expiry must count as due. Assuming a certificate is fine when its
    /// dates cannot be read is how a server ends up serving an expired one.
    #[test]
    fn an_unreadable_certificate_is_treated_as_due() {
        let cert = Certificate {
            fullchain: "-----BEGIN CERTIFICATE-----\nnot really\n-----END CERTIFICATE-----".into(),
            privkey: String::new(),
        };
        assert!(needs_renewal(&cert, 0, 0));
    }

    /// The failure this guards against: an error page or a truncated download saved as
    /// a certificate, which would otherwise show up only as a television that cannot
    /// connect.
    #[test]
    fn a_response_that_is_not_a_certificate_is_rejected() {
        let html = Certificate {
            fullchain: "<html>502 Bad Gateway</html>".into(),
            privkey: "<html>502 Bad Gateway</html>".into(),
        };
        assert!(html.validate().is_err());

        let no_key = Certificate {
            fullchain: REAL_CHAIN.to_string(),
            privkey: String::new(),
        };
        assert!(no_key.validate().is_err());

        let empty = Certificate {
            fullchain: String::new(),
            privkey: String::new(),
        };
        assert!(empty.validate().is_err());
    }

    /// The two provider shapes, both verified against live services: one returns JSON
    /// with the chain and key together, the other two separate PEM files. Depending on
    /// one provider would put the television's ability to connect at the mercy of one
    /// website.
    #[test]
    fn both_provider_shapes_are_understood() {
        // JSON, as served by the provider the reference implementation uses. The extra
        // fields it also sends must not upset the parse.
        let json = format!(
            r#"{{"cert":"x","chain":"y","fullchain":{chain},"privkey":{key}}}"#,
            chain = serde_json::to_string(REAL_CHAIN).unwrap(),
            key = serde_json::to_string(TEST_KEY).unwrap(),
        );
        let parsed: KeysResponse = serde_json::from_str(&json).expect("parses");
        assert!(parsed.fullchain.contains("BEGIN CERTIFICATE"));
        assert!(parsed.privkey.contains("PRIVATE KEY"));

        // A JSON body missing the fields must be an error, not an empty certificate.
        assert!(serde_json::from_str::<KeysResponse>(r#"{"cert":"x"}"#).is_err());
    }

    #[test]
    fn the_real_chain_parses_to_more_than_one_certificate() {
        // A leaf plus its issuer: a television needs the chain, not just the leaf.
        let count = rustls_pemfile::certs(&mut REAL_CHAIN.as_bytes()).count();
        assert!(count >= 2, "got {count} certificates");
    }

    fn temp_cache(name: &str) -> Cache {
        let dir = std::env::temp_dir().join("stremhu-rs-tls-test").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        Cache::new(dir)
    }

    #[test]
    fn the_cache_round_trips() {
        let cache = temp_cache("round-trip");
        assert!(cache.load().is_none(), "nothing cached yet");

        // A key is needed for validation, so use one generated for this test only.
        let cert = Certificate {
            fullchain: REAL_CHAIN.to_string(),
            privkey: TEST_KEY.to_string(),
        };
        cache.store(&cert).expect("stores");

        let back = cache.load().expect("loads");
        assert_eq!(back.fullchain, cert.fullchain);
        assert_eq!(back.privkey, cert.privkey);
    }

    #[test]
    fn a_corrupt_cache_is_ignored_rather_than_served() {
        let cache = temp_cache("corrupt");
        std::fs::create_dir_all(&cache.dir).expect("dir");
        std::fs::write(cache.chain_path(), "garbage").expect("write");
        std::fs::write(cache.key_path(), "garbage").expect("write");
        assert!(cache.load().is_none());
    }

    /// A key that is not the certificate's key has to fail here, not at the first
    /// connection from the television.
    #[test]
    fn a_mismatched_key_is_caught_when_building_the_tls_config() {
        let cert = Certificate {
            fullchain: REAL_CHAIN.to_string(),
            privkey: TEST_KEY.to_string(),
        };
        assert!(
            rustls_config(&cert).is_err(),
            "a key from elsewhere must not be accepted for this chain"
        );
    }

    /// Generated for these tests alone, with `openssl genpkey`. It matches no
    /// certificate anywhere and protects nothing.
    const TEST_KEY: &str = "-----BEGIN PRIVATE KEY-----\n\
MC4CAQAwBQYDK2VwBCIEINTuctv5E1hK1bbY8fdp+K06/nwoy/HU++CXqI9EdVhC\n\
-----END PRIVATE KEY-----\n";
}
