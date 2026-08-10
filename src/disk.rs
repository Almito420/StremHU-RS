//! Where a download goes, and what happens when the disks fill up.
//!
//! # The order is the rule
//!
//! There is a primary folder and a secondary one. Everything goes to the primary until a
//! download will not fit there, and only then is the secondary looked at. That is a
//! deliberate choice over "whichever has the most room": one disk fills before the other is
//! touched, so what is where stays predictable, and the second disk is not woken up on every
//! request just to be compared.
//!
//! The secondary is not even measured while the primary has room. Reading free space is a
//! call into the filesystem, and on a spun-down or network volume it can block; doing it
//! only when the answer can change the outcome keeps that cost out of the common path.
//!
//! # Running out
//!
//! Deletion here is time-based, so nothing stops the disks filling before anything becomes
//! old enough to remove. A server that quietly fails to write is the worst version of that,
//! so free space is checked once a day and said out loud while there is still time to act.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// How much room a folder's volume has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Space {
    pub free_bytes: u64,
    pub total_bytes: u64,
}

impl Space {
    /// Free space as a percentage of the whole volume, rounded down.
    pub fn free_percent(&self) -> u64 {
        if self.total_bytes == 0 {
            return 0;
        }
        (self.free_bytes as u128 * 100 / self.total_bytes as u128) as u64
    }
}

#[cfg(windows)]
mod sys {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    // Straight out of the Windows API rather than through a crate: it is one call with
    // three out-parameters, and a dependency for that is more moving parts than the thing
    // it would hide.
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(
            directory: *const u16,
            free_bytes_available_to_caller: *mut u64,
            total_bytes: *mut u64,
            total_free_bytes: *mut u64,
        ) -> i32;
    }

    /// Free and total bytes for the volume holding `path`.
    ///
    /// The free figure is the one available to this account, not the volume's raw free
    /// space: with a disk quota in force those differ, and the smaller one is what a write
    /// will actually be allowed to use.
    pub fn space(path: &Path) -> Option<(u64, u64)> {
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let mut available = 0u64;
        let mut total = 0u64;
        let mut free = 0u64;
        let ok = unsafe {
            GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut total, &mut free)
        };
        (ok != 0).then_some((available, total))
    }
}

#[cfg(not(windows))]
mod sys {
    use std::path::Path;
    /// Not implemented off Windows; the caller treats None as "unknown" and carries on.
    pub fn space(_path: &Path) -> Option<(u64, u64)> {
        None
    }
}

/// Free and total space for the volume a folder sits on.
///
/// The folder is created first when missing, because asking about a path that does not
/// exist yet reports the wrong volume or nothing at all, and a download folder named in the
/// configuration but not yet created is an ordinary first-run state.
pub fn space_for(dir: &Path) -> Result<Space> {
    if !dir.exists() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    let (free_bytes, total_bytes) = sys::space(dir)
        .with_context(|| format!("could not read the free space on {}", dir.display()))?;
    Ok(Space {
        free_bytes,
        total_bytes,
    })
}

/// Whether one named folder has room for this much, with no choosing involved.
///
/// For a torrent that is already open: it writes where it was started, so there is no decision
/// left to make, only a question to answer before the engine finds out the hard way.
pub fn space_at_least(dir: &str, needed_bytes: u64) -> Result<()> {
    if needed_bytes == 0 {
        return Ok(());
    }
    let path = PathBuf::from(dir);
    let space = space_for(&path)?;
    if space.free_bytes >= needed_bytes {
        return Ok(());
    }
    bail!(
        "{} needs {} and only {} is free",
        path.display(),
        crate::media::size_label(needed_bytes),
        crate::media::size_label(space.free_bytes)
    )
}

/// Which folder a download of this size should go to.
///
/// The primary unless the file will not fit, then the secondary. Nothing is measured beyond
/// what the decision needs: with room on the primary, the secondary is never touched.
///
/// `needed_bytes` of zero means the size is not known yet, and then only the primary's
/// existence matters. That happens when a torrent is added before its metadata has been
/// read, and choosing the primary is the right answer there: it is where things go unless
/// something says otherwise.
pub fn choose(primary: &str, secondary: &str, needed_bytes: u64) -> Result<PathBuf> {
    let primary = primary.trim();
    if primary.is_empty() {
        bail!("no download folder is configured");
    }
    let primary_dir = PathBuf::from(primary);

    let primary_space = space_for(&primary_dir);
    let primary_fits = match &primary_space {
        Ok(s) => needed_bytes == 0 || s.free_bytes >= needed_bytes,
        // Unreadable: use it anyway rather than moving a library to the other disk over a
        // failed measurement. A write that then fails is loud and recoverable; quietly
        // scattering files across two volumes is neither.
        Err(e) => {
            tracing::warn!(dir = %primary_dir.display(), error = %e, "could not measure the primary folder");
            true
        }
    };
    if primary_fits {
        return Ok(primary_dir);
    }

    let secondary = secondary.trim();
    if secondary.is_empty() {
        let free = primary_space.map(|s| s.free_bytes).unwrap_or(0);
        bail!(
            "a download of {} does not fit: {} has {} free and no second folder is configured",
            crate::media::size_label(needed_bytes),
            primary_dir.display(),
            crate::media::size_label(free)
        );
    }

    // Only now is the second disk worth asking about.
    let secondary_dir = PathBuf::from(secondary);
    match space_for(&secondary_dir) {
        Ok(s) if s.free_bytes >= needed_bytes => {
            tracing::info!(
                primary = %primary_dir.display(),
                secondary = %secondary_dir.display(),
                "the primary folder is full, writing to the secondary"
            );
            Ok(secondary_dir)
        }
        Ok(s) => bail!(
            "{} needs {}, and neither folder has room: {} free on {}, {} free on {}",
            crate::media::size_label(needed_bytes),
            crate::media::size_label(needed_bytes),
            crate::media::size_label(primary_space.map(|p| p.free_bytes).unwrap_or(0)),
            primary_dir.display(),
            crate::media::size_label(s.free_bytes),
            secondary_dir.display()
        ),
        Err(e) => Err(e.context("the primary folder is full and the secondary is unreadable")),
    }
}

/// Whether a notification URL is a Discord webhook.
///
/// It matters because Discord is the one common destination that will not take a plain text
/// body: it wants a JSON object and answers a bare body with 400. Detected from the URL
/// rather than configured, since there is only one right answer per address and asking the
/// owner to know it would be asking them to debug an HTTP status.
/// The host is what decides, not the text. A substring match would call
/// `https://example.com/discord.com/api/webhooks` a Discord webhook and send JSON to
/// somebody who wanted a plain body.
pub fn is_discord_webhook(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url.trim()) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    let discord = host == "discord.com"
        || host == "discordapp.com"
        || host.ends_with(".discord.com")
        || host.ends_with(".discordapp.com");

    discord && parsed.path().to_ascii_lowercase().starts_with("/api/webhooks")
}

/// What the interface and the log should say about the disks, and whether it is a warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub lines: Vec<String>,
    /// True when a folder in use is below the configured threshold.
    pub low: bool,
    /// One sentence naming what is short, for a notification.
    pub summary: String,
}

/// Looks at both folders and judges them against the thresholds.
///
/// Both are read here, unlike when choosing: this runs once a day and its whole purpose is
/// to say how much room there is, which cannot be answered without asking.
pub fn report(
    primary: &str,
    secondary: &str,
    warn_below_bytes: u64,
    warn_below_percent: u64,
) -> Report {
    let mut lines = Vec::new();
    let mut short: Vec<String> = Vec::new();

    for (label, dir) in [("elsődleges", primary), ("másodlagos", secondary)] {
        let dir = dir.trim();
        if dir.is_empty() {
            continue;
        }
        match space_for(Path::new(dir)) {
            Ok(s) => {
                let low = s.free_bytes < warn_below_bytes || s.free_percent() < warn_below_percent;
                lines.push(format!(
                    "{label} ({dir}): {} szabad a {}-ból ({}%){}",
                    crate::media::size_label(s.free_bytes),
                    crate::media::size_label(s.total_bytes),
                    s.free_percent(),
                    if low { ", kevés" } else { "" }
                ));
                if low {
                    // The message has to say which disk, because the whole point of it is
                    // knowing where to go and make room. The role is included too: on a
                    // machine with two folders, "the primary is nearly full" and "the spare
                    // is nearly full" call for different things.
                    short.push(format!(
                        "{label} {dir}, {} szabad ({}%)",
                        crate::media::size_label(s.free_bytes),
                        s.free_percent()
                    ));
                }
            }
            Err(_) => lines.push(format!("{label} ({dir}): nem olvasható")),
        }
    }

    Report {
        low: !short.is_empty(),
        summary: if short.is_empty() {
            "A lemezeken van elég hely.".to_string()
        } else {
            format!("Kevés a hely: {}", short.join(", "))
        },
        lines,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024 * 1024;

    /// The measurement the whole module rests on. A real folder on this machine has to
    /// report a plausible volume, or the choice between disks is guesswork.
    #[test]
    fn a_real_folder_reports_its_volume() {
        let s = space_for(&std::env::temp_dir()).expect("the temp volume is readable");
        assert!(s.total_bytes > 0, "a volume with no size is not a volume");
        assert!(s.free_bytes <= s.total_bytes, "free cannot exceed total");
        assert!(s.free_percent() <= 100);
    }

    /// A folder that does not exist yet is an ordinary first-run state, not an error.
    #[test]
    fn a_missing_folder_is_created_and_then_measured() {
        let root = std::env::temp_dir().join("stremhu-rs-disk-test");
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("deep");
        let s = space_for(&dir).expect("created and measured");
        assert!(s.total_bytes > 0);
        assert!(dir.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The primary wins while it has room, whatever the secondary looks like. This is the
    /// difference from picking the roomiest: what is where stays predictable.
    #[test]
    fn the_primary_is_used_while_it_has_room() {
        let temp = std::env::temp_dir();
        let primary = temp.join("stremhu-rs-primary");
        let chosen = choose(
            &primary.to_string_lossy(),
            "Z:/does-not-exist",
            1024, // a kilobyte fits anywhere
        )
        .expect("the primary is chosen");
        assert_eq!(chosen, primary);
        let _ = std::fs::remove_dir_all(&primary);
    }

    /// And the secondary is not even looked at in that case: naming an unreachable folder
    /// as the secondary must not slow down or break the ordinary path.
    #[test]
    fn an_unreachable_secondary_does_not_matter_while_the_primary_has_room() {
        let primary = std::env::temp_dir().join("stremhu-rs-primary-2");
        assert!(
            choose(&primary.to_string_lossy(), "\\\\no-such-host\\share", 4096).is_ok(),
            "the secondary should never have been consulted"
        );
        let _ = std::fs::remove_dir_all(&primary);
    }

    /// Asking for more than the machine has must fall through to the secondary.
    #[test]
    fn an_impossible_size_moves_to_the_secondary() {
        let temp = std::env::temp_dir();
        let primary = temp.join("stremhu-rs-primary-3");
        let secondary = temp.join("stremhu-rs-secondary-3");
        // Both folders are on the same volume here, so a size neither can hold is the only
        // way to exercise the fall-through without a second disk in the test environment.
        let huge = u64::MAX / 2;
        let err = choose(
            &primary.to_string_lossy(),
            &secondary.to_string_lossy(),
            huge,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("neither folder has room"),
            "the message has to name the real problem: {err}"
        );
        assert!(err.contains("free on"), "and carry the numbers: {err}");
        let _ = std::fs::remove_dir_all(&primary);
        let _ = std::fs::remove_dir_all(&secondary);
    }

    /// With no secondary configured, the failure has to say that rather than blaming the
    /// disk.
    #[test]
    fn a_full_primary_with_no_secondary_says_so() {
        let primary = std::env::temp_dir().join("stremhu-rs-primary-4");
        let err = choose(&primary.to_string_lossy(), "", u64::MAX / 2)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no second folder is configured"), "got: {err}");
        // And it names the folder, so which disk is meant is not left to be guessed.
        assert!(err.contains("stremhu-rs-primary-4"), "got: {err}");
        let _ = std::fs::remove_dir_all(&primary);
    }

    /// An unknown size is the state a torrent is in before its metadata is read, and the
    /// answer there is the primary.
    #[test]
    fn an_unknown_size_goes_to_the_primary() {
        let primary = std::env::temp_dir().join("stremhu-rs-primary-5");
        assert_eq!(
            choose(&primary.to_string_lossy(), "", 0).expect("chosen"),
            primary
        );
        let _ = std::fs::remove_dir_all(&primary);
    }

    #[test]
    fn no_primary_is_an_error() {
        assert!(choose("", "D:/second", 1024).is_err());
        assert!(choose("   ", "", 0).is_err());
    }

    /// The report reads both, because saying how much room there is cannot be done without
    /// asking both.
    #[test]
    fn the_report_covers_both_folders() {
        let temp = std::env::temp_dir();
        let a = temp.join("stremhu-rs-report-a");
        let b = temp.join("stremhu-rs-report-b");
        let r = report(&a.to_string_lossy(), &b.to_string_lossy(), 1, 0);

        assert_eq!(r.lines.len(), 2);
        assert!(r.lines[0].starts_with("elsődleges"));
        assert!(r.lines[1].starts_with("másodlagos"));
        assert!(!r.low, "a real temp volume has more than one byte free");
        assert_eq!(r.summary, "A lemezeken van elég hely.");

        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    /// The notification exists so somebody knows which disk to go and clear, so it has to
    /// name the folder, the drive it is on, and which of the two it is.
    #[test]
    fn the_warning_names_the_drive_and_its_role() {
        let a = std::env::temp_dir().join("stremhu-rs-report-c");
        let r = report(&a.to_string_lossy(), "", u64::MAX, 0);

        assert!(r.low);
        assert!(r.summary.starts_with("Kevés a hely:"), "got: {}", r.summary);
        assert!(r.summary.contains("elsődleges"), "the role: {}", r.summary);
        assert!(r.summary.contains("stremhu-rs-report-c"), "the folder: {}", r.summary);
        // The drive letter travels with the path, which is what somebody acts on.
        let drive = a.to_string_lossy().chars().next().unwrap();
        assert!(r.summary.contains(drive), "the drive: {}", r.summary);
        assert!(r.summary.contains("szabad"), "and how much is left");
        assert!(r.lines[0].ends_with(", kevés"));
        let _ = std::fs::remove_dir_all(&a);
    }

    /// With both short, both are named, so it is clear the spare will not save you either.
    #[test]
    fn both_folders_are_named_when_both_are_short() {
        let temp = std::env::temp_dir();
        let a = temp.join("stremhu-rs-report-f");
        let b = temp.join("stremhu-rs-report-g");
        let r = report(&a.to_string_lossy(), &b.to_string_lossy(), u64::MAX, 0);

        assert!(r.summary.contains("elsődleges"));
        assert!(r.summary.contains("másodlagos"));
        assert!(r.summary.contains("stremhu-rs-report-f"));
        assert!(r.summary.contains("stremhu-rs-report-g"));

        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    /// A percentage threshold catches a large disk that is nearly full even when the
    /// absolute figure still looks comfortable.
    #[test]
    fn the_percentage_threshold_is_applied_too() {
        let a = std::env::temp_dir().join("stremhu-rs-report-d");
        // A byte floor nothing trips, and a percentage nothing can satisfy.
        let r = report(&a.to_string_lossy(), "", 1, 101);
        assert!(r.low, "101% free is impossible, so this must warn");
        let _ = std::fs::remove_dir_all(&a);
    }

    /// Discord refuses a plain body, everything else wants one, so the address has to decide
    /// the shape. Getting this wrong shows up as a warning that never arrives.
    #[test]
    fn a_discord_webhook_is_recognised_and_nothing_else_is() {
        assert!(is_discord_webhook(
            "https://discord.com/api/webhooks/123456/abcdef"
        ));
        assert!(is_discord_webhook(
            "https://discordapp.com/api/webhooks/123456/abcdef"
        ));
        // ntfy and the like take the plain body.
        assert!(!is_discord_webhook("https://ntfy.sh/my-secret-topic"));
        assert!(!is_discord_webhook("http://192.168.0.5:8080/notify"));
        assert!(!is_discord_webhook(""));
        // A name that merely mentions Discord in its path is not a Discord webhook.
        assert!(!is_discord_webhook("https://example.com/discord.com/api/webhooks"));
        // Nor is the right host with the wrong path.
        assert!(!is_discord_webhook("https://discord.com/channels/1/2"));
        // A regional subdomain is.
        assert!(is_discord_webhook("https://ptb.discord.com/api/webhooks/1/x"));
        // And nonsense is not a URL at all.
        assert!(!is_discord_webhook("discord.com/api/webhooks/1/x"));
    }

    #[test]
    fn an_empty_secondary_is_left_out_of_the_report() {
        let a = std::env::temp_dir().join("stremhu-rs-report-e");
        let r = report(&a.to_string_lossy(), "   ", 1, 0);
        assert_eq!(r.lines.len(), 1);
        let _ = std::fs::remove_dir_all(&a);
    }
}
