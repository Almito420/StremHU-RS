//! Telling the owner when something has gone wrong.
//!
//! Two sources, because problems arrive in two shapes.
//!
//! An error logged anywhere in the program is caught here rather than at each call site. Wiring
//! a notification into every failure path would mean remembering to do it in every failure path
//! added later, and the one that gets forgotten is the one that matters. This hooks the logging
//! itself, so anything that reports an error is reported onwards whether the author thought
//! about notifications or not.
//!
//! And a watchdog, because the worst failures do not log anything: a server that is spinning,
//! wedged, or eating memory says nothing at all. That one is measured rather than reported.
//!
//! Both go out through the same throttled channel as the disk warnings, so a condition that
//! persists is mentioned rather than repeated.

use std::sync::OnceLock;

use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

/// Where caught problems go. Set once the server is up; anything before that has nowhere to go
/// and is only in the log, which is where a startup failure is reported anyway.
static SINK: OnceLock<UnboundedSender<Problem>> = OnceLock::new();

/// One thing that went wrong, ready to send.
#[derive(Debug, Clone)]
pub struct Problem {
    /// Groups repeats: the module and the level, so a tracker that is down for an hour is one
    /// message rather than sixty.
    pub kind: String,
    pub text: String,
}

/// Reports a problem, if anyone is listening. Never blocks and never fails.
pub fn report(kind: &str, text: String) {
    if let Some(sink) = SINK.get() {
        let _ = sink.send(Problem {
            kind: kind.to_string(),
            text,
        });
    }
}

/// Opens the channel. Returns the receiving end for the server to drain.
pub fn channel() -> tokio::sync::mpsc::UnboundedReceiver<Problem> {
    let (tx, rx) = unbounded_channel();
    // Second call would mean two servers in one process; the first sink keeps the channel.
    let _ = SINK.set(tx);
    rx
}

/// Pulls the `message` field out of a tracing event.
#[derive(Default)]
struct Message(String);

impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // Fields other than the message are kept too: `error = %e` carries the reason, and a
        // notification without the reason is only half a notification.
        let text = format!("{value:?}");
        if field.name() == "message" {
            if self.0.is_empty() {
                self.0 = text;
            } else {
                self.0 = format!("{text} {}", self.0);
            }
        } else {
            if !self.0.is_empty() {
                self.0.push(' ');
            }
            self.0.push_str(&format!("{}={}", field.name(), text.trim_matches('"')));
        }
    }
}

/// The logging layer that turns an error into a notification.
pub struct ErrorLayer;

impl<S: tracing::Subscriber> Layer<S> for ErrorLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        if *meta.level() != tracing::Level::ERROR {
            return;
        }
        let mut message = Message::default();
        event.record(&mut message);
        report(meta.target(), message.0);
    }
}

/// What this process is using right now: processor time as a fraction of one core since the
/// last look, and the memory it privately holds, in bytes.
///
/// Straight out of the Windows API. Two calls, no dependency.
#[cfg(windows)]
pub fn usage(previous_cpu: u64, elapsed: std::time::Duration) -> (f64, u64, u64) {
    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct MemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool: usize,
        quota_paged_pool: usize,
        quota_peak_non_paged_pool: usize,
        quota_non_paged_pool: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut std::ffi::c_void;
        fn GetProcessTimes(
            process: *mut std::ffi::c_void,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
        fn K32GetProcessMemoryInfo(
            process: *mut std::ffi::c_void,
            counters: *mut MemoryCounters,
            size: u32,
        ) -> i32;
    }

    fn to_100ns(t: &FileTime) -> u64 {
        (u64::from(t.high) << 32) | u64::from(t.low)
    }

    let process = unsafe { GetCurrentProcess() };
    let mut creation = FileTime::default();
    let mut exit = FileTime::default();
    let mut kernel = FileTime::default();
    let mut user = FileTime::default();
    let cpu = if unsafe {
        GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user)
    } != 0
    {
        to_100ns(&kernel) + to_100ns(&user)
    } else {
        previous_cpu
    };

    let mut counters = MemoryCounters {
        cb: std::mem::size_of::<MemoryCounters>() as u32,
        ..Default::default()
    };
    // Private commit, not the working set.
    //
    // Measured on this machine while it was writing: working set 1808 MB, private 71 MB. The
    // difference is memory-mapped file pages, which is how libtorrent 2.0 writes to disk;
    // Windows counts them in the working set but they are reclaimable cache, not memory the
    // process holds. Watching the working set would have raised an alarm about a server that
    // was working perfectly, and it would have missed a real leak behind the same number.
    let rss = if unsafe {
        K32GetProcessMemoryInfo(process, &mut counters, counters.cb)
    } != 0
    {
        counters.pagefile_usage as u64
    } else {
        0
    };

    // Processor time is counted in hundreds of nanoseconds.
    let used = cpu.saturating_sub(previous_cpu) as f64 / 10_000_000.0;
    let share = if elapsed.as_secs_f64() > 0.0 {
        used / elapsed.as_secs_f64()
    } else {
        0.0
    };
    (share, rss, cpu)
}

/// Processor time in hundreds of nanoseconds from `/proc/self/stat`.
///
/// The command name is in the second field and may itself contain spaces and brackets, so the
/// fields are counted from the last `)` rather than from the start. That detail is the whole
/// reason this is a function with a test: splitting on whitespace from the beginning works on
/// every process except the one whose name contains a space, and then it silently reads the
/// wrong numbers.
#[cfg(unix)]
pub fn parse_proc_stat_cpu(text: &str, ticks_per_second: u64) -> Option<u64> {
    let after = text.rsplit_once(')')?.1;
    let fields: Vec<&str> = after.split_whitespace().collect();
    // After the `)` the first field is state, so utime is the 12th and stime the 13th.
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    let ticks = utime.checked_add(stime)?;
    let per_second = ticks_per_second.max(1);
    // The same unit Windows reports processor time in, so the caller does not care which is which.
    Some(ticks.saturating_mul(10_000_000) / per_second)
}

/// Privately held memory in bytes from `/proc/self/status`.
///
/// `RssAnon` rather than `VmRSS`: the difference is file-backed pages, which for this program is
/// libtorrent's memory-mapped writing. Measured on Windows the same distinction was 1808 MB of
/// working set against 71 MB genuinely held, and watching the larger number would report a
/// perfectly healthy server as a problem.
#[cfg(unix)]
pub fn parse_proc_status_rss_anon(text: &str) -> Option<u64> {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("RssAnon:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb.saturating_mul(1024));
        }
    }
    None
}

#[cfg(unix)]
pub fn usage(previous_cpu: u64, elapsed: std::time::Duration) -> (f64, u64, u64) {
    let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    let cpu = std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|t| parse_proc_stat_cpu(&t, ticks_per_second))
        .unwrap_or(previous_cpu);
    let rss = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|t| parse_proc_status_rss_anon(&t))
        .unwrap_or(0);

    let used = cpu.saturating_sub(previous_cpu) as f64 / 10_000_000.0;
    let share = if elapsed.as_secs_f64() > 0.0 {
        used / elapsed.as_secs_f64()
    } else {
        0.0
    };
    (share, rss, cpu)
}

#[cfg(not(any(windows, unix)))]
pub fn usage(previous_cpu: u64, _elapsed: std::time::Duration) -> (f64, u64, u64) {
    (0.0, 0, previous_cpu)
}

/// Whether a run of measurements is bad enough to be worth saying out loud.
///
/// Deliberately about a run rather than a moment. A media server pegs a core while it hashes a
/// finished download and takes a gigabyte of mapped pages while it writes at seventeen megabytes
/// a second; both are the program working, not failing. What is worth a message is the same
/// reading over and over with nothing to show for it.
pub fn sustained_problem(
    samples: &[(f64, u64)],
    cpu_limit: f64,
    rss_limit_bytes: u64,
    needed: usize,
) -> Option<String> {
    if samples.len() < needed {
        return None;
    }
    let recent = &samples[samples.len() - needed..];
    if recent.iter().all(|(cpu, _)| *cpu >= cpu_limit) {
        let worst = recent.iter().map(|(cpu, _)| *cpu).fold(0.0, f64::max);
        return Some(format!(
            "A processzorhasználat {} mérésen át {:.0}% fölött volt, csúcson {:.0}%.",
            needed,
            cpu_limit * 100.0,
            worst * 100.0
        ));
    }
    if recent.iter().all(|(_, rss)| *rss >= rss_limit_bytes) {
        let worst = recent.iter().map(|(_, rss)| *rss).max().unwrap_or(0);
        return Some(format!(
            "A memóriahasználat {} mérésen át {} fölött volt, csúcson {}.",
            needed,
            crate::media::size_label(rss_limit_bytes),
            crate::media::size_label(worst)
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The command name sits in brackets and may contain spaces, which is why the fields are
    /// counted from the last bracket. Splitting from the start reads the wrong numbers for any
    /// process whose name has a space in it, and says nothing about having done so.
    #[test]
    #[cfg(unix)]
    fn proc_stat_is_read_from_the_last_bracket() {
        // Real shape, trimmed: pid, comm, state, then the numbers. utime=1234, stime=567.
        let line = "4242 (stremhu-rs) S 1 4242 4242 0 -1 4194304 900 0 0 0 1234 567 0 0 20 0 5 0";
        // At a hundred ticks a second, 1801 ticks is 18.01 seconds of processor time.
        let hundred_ns = parse_proc_stat_cpu(line, 100).expect("parses");
        assert_eq!(hundred_ns, 180_100_000);

        // A name with a space and a bracket in it must not throw the count off.
        let awkward = "7 (my (odd) name) S 1 7 7 0 -1 0 0 0 0 0 10 20 0 0 20 0 5 0";
        assert_eq!(parse_proc_stat_cpu(awkward, 100).expect("parses"), 3_000_000);

        assert!(parse_proc_stat_cpu("nonsense", 100).is_none());
    }

    #[test]
    #[cfg(unix)]
    fn proc_status_gives_the_privately_held_memory() {
        let status = "Name:	stremhu-rs
VmRSS:	 1849344 kB
RssAnon:	   72704 kB
                      RssFile:	 1776640 kB
";
        // The anonymous part, not the resident total: the difference is mapped file pages.
        assert_eq!(parse_proc_status_rss_anon(status), Some(72_704 * 1024));
        assert_eq!(parse_proc_status_rss_anon("Name:	x
"), None);
    }

    /// The wiring that actually matters: an error has to reach the sink even when logging is
    /// switched off, because that is how the server normally runs. A filter that swallowed the
    /// event would mean no notification on the one evening something breaks unattended.
    #[test]
    fn an_error_reaches_the_sink_and_a_warning_does_not() {
        use tracing_subscriber::layer::SubscriberExt;

        let mut problems = channel();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new("stremhu_rs=error"))
            .finish()
            .with(ErrorLayer);

        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(error = "a tracker nem elérhető", "nem sikerült beolvasni a listát");
            tracing::warn!("ez nem hiba, csak figyelmeztetés");
        });

        let first = problems.try_recv().expect("an error was reported");
        assert!(first.text.contains("nem sikerült beolvasni a listát"), "{}", first.text);
        assert!(
            first.text.contains("a tracker nem elérhető"),
            "the reason travels with it: {}",
            first.text
        );
        assert!(first.kind.starts_with("stremhu_rs"), "{}", first.kind);
        assert!(problems.try_recv().is_err(), "a warning is not a problem to push");
    }

    /// A busy moment is not a problem; the same reading over and over is.
    #[test]
    fn only_a_sustained_reading_counts() {
        const GIB: u64 = 1024 * 1024 * 1024;
        // One spike among quiet samples: nothing to say.
        let spiky = vec![(0.05, 30 * 1024 * 1024), (0.99, 40 * 1024 * 1024), (0.02, 30 * 1024 * 1024)];
        assert!(sustained_problem(&spiky, 0.6, 2 * GIB, 3).is_none());

        // Pegged throughout: worth saying.
        let pegged = vec![(0.8, 30 * 1024 * 1024); 3];
        let said = sustained_problem(&pegged, 0.6, 2 * GIB, 3).expect("reported");
        assert!(said.contains("processzor"), "{said}");
        assert!(said.contains("80%"), "the worst reading is in it: {said}");

        // Memory the same way.
        let fat = vec![(0.01, 3 * GIB); 3];
        let said = sustained_problem(&fat, 0.6, 2 * GIB, 3).expect("reported");
        assert!(said.contains("memória"), "{said}");

        // Not enough samples yet: nothing, rather than a guess from one reading.
        assert!(sustained_problem(&[(0.9, 4 * GIB)], 0.6, 2 * GIB, 3).is_none());
    }
}
