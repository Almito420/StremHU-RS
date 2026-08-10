//! Safe Rust wrapper over the libtorrent C ABI shim.
//!
//! The shim returns null pointers or negative numbers instead of throwing, and
//! carries the message in `lts_last_error`, so every failure here becomes a normal
//! `Result` with that message attached.
//!
//! Handles are `Send` because libtorrent's `session` and `torrent_handle` are
//! internally synchronised; they are deliberately not `Sync`-free-for-all, so the
//! wrapper hands out `&self` methods and relies on libtorrent's own locking.

use std::ffi::{CStr, CString, c_char};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

#[repr(C)]
struct RawSession {
    _private: [u8; 0],
}

#[repr(C)]
struct RawTorrent {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    pub total_done: i64,
    pub total_wanted: i64,
    pub total_upload: i64,
    pub download_rate: i32,
    pub upload_rate: i32,
    pub num_peers: i32,
    pub num_seeds: i32,
    pub state: i32,
    pub is_seeding: i32,
    pub is_finished: i32,
}

#[link(name = "stremhu_shim")]
unsafe extern "C" {
    fn lts_last_error(buf: *mut c_char, buf_len: i32) -> i32;
    fn lts_version() -> *const c_char;

    fn lts_session_new(settings: *const SessionSettings) -> *mut RawSession;
    fn lts_session_free(s: *mut RawSession);
    fn lts_session_limits(
        s: *mut RawSession,
        connections: *mut i32,
        download_rate: *mut i32,
        upload_rate: *mut i32,
    ) -> i32;
    fn lts_pump_alerts(s: *mut RawSession, err_buf: *mut c_char, err_len: i32) -> i32;

    fn lts_add_torrent_resume(
        s: *mut RawSession,
        data: *const u8,
        len: usize,
        save_path: *const c_char,
        resume_data: *const u8,
        resume_len: usize,
        out_hash40: *mut c_char,
    ) -> *mut RawTorrent;
    fn lts_torrent_free(t: *mut RawTorrent);
    fn lts_request_resume_data(t: *mut RawTorrent) -> i32;
    fn lts_take_resume_data(
        s: *mut RawSession,
        hash40: *const c_char,
        buf: *mut u8,
        buf_len: i32,
    ) -> i32;

    fn lts_num_files(t: *mut RawTorrent) -> i32;
    fn lts_file_size(t: *mut RawTorrent, index: i32) -> i64;
    fn lts_file_offset(t: *mut RawTorrent, index: i32) -> i64;
    fn lts_file_path(
        t: *mut RawTorrent,
        index: i32,
        save_path: *const c_char,
        buf: *mut c_char,
        buf_len: i32,
    ) -> i32;

    fn lts_num_pieces(t: *mut RawTorrent) -> i32;
    fn lts_piece_length(t: *mut RawTorrent) -> i64;

    fn lts_select_only_file(t: *mut RawTorrent, index: i32) -> i32;
    fn lts_prioritize_all_pieces(t: *mut RawTorrent, priority: i32) -> i32;
    fn lts_set_file_priority(t: *mut RawTorrent, index: i32, priority: i32) -> i32;
    fn lts_force_recheck(t: *mut RawTorrent) -> i32;
    fn lts_torrent_info_parse(
        data: *const u8,
        len: usize,
        out_hash40: *mut c_char,
    ) -> *mut RawTorrent;
    fn lts_set_piece_deadline(t: *mut RawTorrent, piece: i32, deadline_ms: i32) -> i32;
    fn lts_reset_piece_deadline(t: *mut RawTorrent, piece: i32) -> i32;
    fn lts_set_max_connections(t: *mut RawTorrent, limit: i32) -> i32;

    fn lts_have_pieces(t: *mut RawTorrent, out: *mut u8, out_len: i32) -> i32;
    fn lts_stats(t: *mut RawTorrent, out: *mut Stats) -> i32;
    fn lts_resume(t: *mut RawTorrent) -> i32;
    fn lts_remove_torrent(s: *mut RawSession, t: *mut RawTorrent, delete_files: i32) -> i32;
}

fn last_error() -> String {
    let mut buf = vec![0i8 as c_char; 1024];
    let n = unsafe { lts_last_error(buf.as_mut_ptr(), buf.len() as i32) };
    if n <= 0 {
        return "no error message".to_string();
    }
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

pub fn libtorrent_version() -> String {
    unsafe { CStr::from_ptr(lts_version()) }
        .to_string_lossy()
        .into_owned()
}

/// Reads a `.torrent` without opening it.
///
/// For the one decision that has to happen before the torrent is added: how much will be
/// written, and therefore which disk it goes to. Only the file and piece accessors are valid
/// on what comes back; there is no torrent behind it to resume, prioritise or stream.
pub fn parse_torrent(bytes: &[u8]) -> Result<Torrent> {
    let mut hash = vec![0i8 as c_char; 41];
    let raw = unsafe { lts_torrent_info_parse(bytes.as_ptr(), bytes.len(), hash.as_mut_ptr()) };
    if raw.is_null() {
        bail!("this .torrent could not be read: {}", last_error());
    }
    let info_hash = unsafe { CStr::from_ptr(hash.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    Ok(Torrent {
        raw,
        info_hash,
        // No save path: the paths that come back are relative to the torrent, which is all
        // that is needed to weigh up the files.
        save_path: String::new(),
    })
}

pub struct Session {
    raw: *mut RawSession,
}

// libtorrent's session is internally synchronised.
unsafe impl Send for Session {}
unsafe impl Sync for Session {}

/// What the engine is told at startup. Mirrors the shim's struct field for field.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SessionSettings {
    pub listen_port: i32,
    pub max_active_torrents: i32,
    pub connections_limit: i32,
    pub download_rate_limit: i32,
    pub upload_rate_limit: i32,
    pub enable_port_mapping: i32,
}

impl SessionSettings {
    /// Builds the engine's settings from the configuration.
    ///
    /// The configuration section is taken apart field by field, with no `..` and nothing
    /// ignored silently. That is the point of writing it this way: four settings used to be
    /// read from the file, shown in the interface, and then never passed to the engine at
    /// all, because the engine took a single port and baked everything else in. With an
    /// exhaustive pattern, adding a setting to that section stops the build until somebody
    /// decides what it does, instead of it quietly doing nothing.
    pub fn from_config(t: &crate::config::Torrent) -> Self {
        let crate::config::Torrent {
            listen_port,
            global_connections_limit,
            download_limit_bytes,
            upload_limit_bytes,
            enable_upnp_and_natpmp,
            max_active_torrents,
            // Not the engine's business. Where files go is decided per download by the disk
            // chooser, and how many peers one torrent may use is applied to that torrent
            // rather than to the session.
            save_path: _,
            save_path_secondary: _,
            connections_while_streaming: _,
            connections_while_idle: _,
            // A download policy, not a session setting: applied per torrent once its
            // wanted file is on disk.
            complete_extras_below_bytes: _,
        } = t;

        Self {
            listen_port: i32::from(*listen_port),
            max_active_torrents: *max_active_torrents,
            connections_limit: (*global_connections_limit).min(i32::MAX as u32) as i32,
            download_rate_limit: *download_limit_bytes,
            upload_rate_limit: *upload_limit_bytes,
            enable_port_mapping: i32::from(*enable_upnp_and_natpmp),
        }
    }
}

impl Session {
    pub fn new(settings: SessionSettings) -> Result<Self> {
        let raw = unsafe { lts_session_new(&settings) };
        if raw.is_null() {
            bail!("libtorrent session could not start: {}", last_error());
        }
        Ok(Self { raw })
    }

    /// What the engine is actually running with: peer limit, then the two rate caps in
    /// bytes per second, where zero means unlimited.
    ///
    /// Read back rather than remembered, so a setting can be checked instead of assumed.
    pub fn limits(&self) -> Option<(i32, i32, i32)> {
        let mut connections = 0i32;
        let mut download = 0i32;
        let mut upload = 0i32;
        let rc = unsafe {
            lts_session_limits(self.raw, &mut connections, &mut download, &mut upload)
        };
        (rc == 0).then_some((connections, download, upload))
    }

    /// Must be called periodically: libtorrent buffers alerts until they are
    /// popped. Returns any error messages it found.
    pub fn pump_alerts(&self) -> Option<String> {
        let mut buf = vec![0i8 as c_char; 2048];
        let n = unsafe { lts_pump_alerts(self.raw, buf.as_mut_ptr(), buf.len() as i32) };
        if n <= 0 {
            return None;
        }
        let msg = unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        (!msg.is_empty()).then_some(msg)
    }

    /// Adds a torrent with every file disabled, so nothing downloads until a file
    /// is selected.
    pub fn add_torrent(&self, torrent_bytes: &[u8], save_path: &str) -> Result<Torrent> {
        self.add_torrent_with_resume(torrent_bytes, save_path, None)
    }

    /// Adds a torrent, reusing what libtorrent knew about it before.
    ///
    /// Resume data is what lets a finished download start seeding immediately instead of
    /// re-reading and re-hashing every file first. Measured on a 17 GB release, that
    /// check is a full pass over the disk.
    pub fn add_torrent_with_resume(
        &self,
        torrent_bytes: &[u8],
        save_path: &str,
        resume: Option<&[u8]>,
    ) -> Result<Torrent> {
        let save = CString::new(save_path).context("save path contains a NUL byte")?;
        let mut hash = vec![0i8 as c_char; 41];
        let (resume_ptr, resume_len) = match resume {
            Some(bytes) if !bytes.is_empty() => (bytes.as_ptr(), bytes.len()),
            _ => (std::ptr::null(), 0),
        };

        let raw = unsafe {
            lts_add_torrent_resume(
                self.raw,
                torrent_bytes.as_ptr(),
                torrent_bytes.len(),
                save.as_ptr(),
                resume_ptr,
                resume_len,
                hash.as_mut_ptr(),
            )
        };
        if raw.is_null() {
            bail!("could not add the torrent: {}", last_error());
        }

        let info_hash = unsafe { CStr::from_ptr(hash.as_ptr()) }
            .to_string_lossy()
            .into_owned();

        Ok(Torrent {
            raw,
            info_hash,
            save_path: save_path.to_string(),
        })
    }

    /// Collects resume data that has arrived for a torrent, if any.
    ///
    /// Returns None when nothing is waiting, which is the normal answer: the data is
    /// produced asynchronously, so it appears one or more alert pumps after being asked
    /// for.
    pub fn take_resume_data(&self, info_hash: &str) -> Option<Vec<u8>> {
        let hash = CString::new(info_hash).ok()?;

        // Called with no buffer first, which reports the size as a negative number.
        let size = unsafe { lts_take_resume_data(self.raw, hash.as_ptr(), std::ptr::null_mut(), 0) };
        if size == 0 {
            return None;
        }
        if size > 0 {
            // Cannot happen with a null buffer, but treating it as nothing is safer than
            // reading from a pointer that was never written.
            return None;
        }
        let needed = size.unsigned_abs() as usize;

        let mut buf = vec![0u8; needed];
        let written =
            unsafe { lts_take_resume_data(self.raw, hash.as_ptr(), buf.as_mut_ptr(), needed as i32) };
        if written <= 0 {
            return None;
        }
        buf.truncate(written as usize);
        Some(buf)
    }

    /// Detaches a torrent from the session, erasing its data when asked.
    ///
    /// Taken by reference: the wrapper still owns its handle and releases it on drop,
    /// which is safe because a handle is only a weak reference into the session. What
    /// this does not do is wait — libtorrent deletes the files from its own disk
    /// thread, so the data disappears shortly after this returns.
    pub fn remove_torrent(&self, torrent: &Torrent, delete_files: bool) -> Result<()> {
        let rc =
            unsafe { lts_remove_torrent(self.raw, torrent.raw, i32::from(delete_files)) };
        if rc != 0 {
            bail!("could not remove the torrent: {}", last_error());
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe { lts_session_free(self.raw) };
    }
}

pub struct Torrent {
    raw: *mut RawTorrent,
    pub info_hash: String,
    save_path: String,
}

unsafe impl Send for Torrent {}
unsafe impl Sync for Torrent {}

#[derive(Debug, Clone)]
pub struct FileInfo {
    pub index: usize,
    pub size: u64,
    /// Byte offset within the whole torrent; piece indices are global.
    pub offset: u64,
    pub path: PathBuf,
}

impl Torrent {
    /// The folder this torrent writes into.
    pub fn save_path(&self) -> &str {
        &self.save_path
    }

    pub fn num_files(&self) -> Result<usize> {
        let n = unsafe { lts_num_files(self.raw) };
        if n < 0 {
            bail!("num_files failed: {}", last_error());
        }
        Ok(n as usize)
    }

    pub fn files(&self) -> Result<Vec<FileInfo>> {
        let n = self.num_files()?;
        let save = CString::new(self.save_path.as_str()).context("save path has a NUL byte")?;
        let mut out = Vec::with_capacity(n);

        for index in 0..n {
            let i = index as i32;
            let size = unsafe { lts_file_size(self.raw, i) };
            let offset = unsafe { lts_file_offset(self.raw, i) };
            if size < 0 || offset < 0 {
                bail!("file metadata for index {index} failed: {}", last_error());
            }

            let mut buf = vec![0i8 as c_char; 4096];
            let written =
                unsafe { lts_file_path(self.raw, i, save.as_ptr(), buf.as_mut_ptr(), 4096) };
            if written < 0 {
                bail!("file path for index {index} did not fit or failed");
            }
            let path = unsafe { CStr::from_ptr(buf.as_ptr()) }
                .to_string_lossy()
                .into_owned();

            out.push(FileInfo {
                index,
                size: size as u64,
                offset: offset as u64,
                path: PathBuf::from(path),
            });
        }
        Ok(out)
    }

    pub fn num_pieces(&self) -> Result<usize> {
        let n = unsafe { lts_num_pieces(self.raw) };
        if n < 0 {
            bail!("num_pieces failed: {}", last_error());
        }
        Ok(n as usize)
    }

    pub fn piece_length(&self) -> Result<u64> {
        let n = unsafe { lts_piece_length(self.raw) };
        if n <= 0 {
            bail!("piece_length failed: {}", last_error());
        }
        Ok(n as u64)
    }

    /// Downloads only this file; every other file stays at priority zero.
    pub fn select_only_file(&self, index: usize) -> Result<()> {
        if unsafe { lts_select_only_file(self.raw, index as i32) } < 0 {
            bail!("select_only_file({index}) failed: {}", last_error());
        }
        Ok(())
    }

    /// Sets every piece to one priority. Zero, combined with deadlines, means only
    /// what the viewer actually reaches gets downloaded; the default priority means
    /// the whole selected file will be fetched.
    pub fn prioritize_all_pieces(&self, priority: u8) -> Result<()> {
        if unsafe { lts_prioritize_all_pieces(self.raw, i32::from(priority)) } < 0 {
            bail!("prioritize_all_pieces({priority}) failed: {}", last_error());
        }
        Ok(())
    }

    /// The reason this whole layer exists. `deadline_ms` is relative to now and
    /// lower means more urgent.
    /// Re-reads the files from disk and rebuilds what the torrent has.
    ///
    /// Used after deleting one file of a torrent whose other files stay: without it libtorrent
    /// keeps offering pieces whose data is gone, and the failed read stops the whole torrent.
    pub fn force_recheck(&self) -> Result<()> {
        let rc = unsafe { lts_force_recheck(self.raw) };
        if rc != 0 {
            bail!("force_recheck: {}", last_error());
        }
        Ok(())
    }

    /// Switches one file of the torrent on or off, leaving the rest alone.
    ///
    /// Zero means do not download it; seven is the top. Used to pick up a film's sample and
    /// nfo once the film itself is on disk, so the torrent can become a complete seed.
    pub fn set_file_priority(&self, index: usize, priority: i32) -> Result<()> {
        let rc = unsafe { lts_set_file_priority(self.raw, index as i32, priority) };
        if rc != 0 {
            bail!("set_file_priority({index}, {priority}): {}", last_error());
        }
        Ok(())
    }

    pub fn set_piece_deadline(&self, piece: u32, deadline_ms: u32) -> Result<()> {
        let ms = i32::try_from(deadline_ms).unwrap_or(i32::MAX);
        if unsafe { lts_set_piece_deadline(self.raw, piece as i32, ms) } < 0 {
            bail!("set_piece_deadline({piece}) failed: {}", last_error());
        }
        Ok(())
    }

    pub fn reset_piece_deadline(&self, piece: u32) -> Result<()> {
        if unsafe { lts_reset_piece_deadline(self.raw, piece as i32) } < 0 {
            bail!("reset_piece_deadline({piece}) failed: {}", last_error());
        }
        Ok(())
    }

    pub fn set_max_connections(&self, limit: u32) -> Result<()> {
        if unsafe { lts_set_max_connections(self.raw, limit as i32) } < 0 {
            bail!("set_max_connections failed: {}", last_error());
        }
        Ok(())
    }

    /// One byte per piece: 1 when complete.
    pub fn have_pieces(&self) -> Result<Vec<u8>> {
        let n = self.num_pieces()?;
        let mut out = vec![0u8; n];
        let written = unsafe { lts_have_pieces(self.raw, out.as_mut_ptr(), n as i32) };
        if written < 0 {
            bail!("have_pieces failed: {}", last_error());
        }
        out.truncate(written as usize);
        Ok(out)
    }

    pub fn stats(&self) -> Result<Stats> {
        let mut s = Stats::default();
        if unsafe { lts_stats(self.raw, &mut s) } < 0 {
            bail!("stats failed: {}", last_error());
        }
        Ok(s)
    }

    /// Asks libtorrent to produce resume data. It arrives through the alert queue, so
    /// the caller collects it with [`Session::take_resume_data`] on a later pump.
    pub fn request_resume_data(&self) -> Result<()> {
        let rc = unsafe { lts_request_resume_data(self.raw) };
        if rc != 0 {
            bail!("request_resume_data failed: {}", last_error());
        }
        Ok(())
    }

    pub fn resume(&self) -> Result<()> {
        if unsafe { lts_resume(self.raw) } < 0 {
            bail!("resume failed: {}", last_error());
        }
        Ok(())
    }

}

impl Drop for Torrent {
    fn drop(&mut self) {
        unsafe { lts_torrent_free(self.raw) };
    }
}

/// Length of the contiguous run of completed pieces starting at `from`. This is the
/// number that decides whether playback can start: scattered completed pieces are
/// useless to a player.
pub fn contiguous_from(have: &[u8], from: u32) -> u32 {
    let from = from as usize;
    if from >= have.len() {
        return 0;
    }
    have[from..].iter().take_while(|b| **b == 1).count() as u32
}

#[cfg(test)]
mod session_settings_tests {
    use super::*;
    use crate::config::Torrent;

    /// The failure this is built to prevent: a setting present in the file, shown in the
    /// interface, and never handed to the engine.
    #[test]
    fn every_session_setting_reaches_the_engine() {
        let cfg = Torrent {
            listen_port: 6890,
            global_connections_limit: 200,
            download_limit_bytes: 5_000_000,
            upload_limit_bytes: 1_000_000,
            enable_upnp_and_natpmp: true,
            ..Torrent::default()
        };
        let s = SessionSettings::from_config(&cfg);

        assert_eq!(s.listen_port, 6890);
        assert_eq!(s.connections_limit, 200, "this one was being ignored");
        assert_eq!(s.download_rate_limit, 5_000_000);
        assert_eq!(s.upload_rate_limit, 1_000_000);
        assert_eq!(s.enable_port_mapping, 1);
    }

    /// Zero means unlimited on both sides, so the file's convention passes straight through
    /// rather than being translated into something else.
    #[test]
    fn zero_rate_limits_pass_through_as_unlimited() {
        let s = SessionSettings::from_config(&Torrent::default());
        assert_eq!(s.download_rate_limit, 0);
        assert_eq!(s.upload_rate_limit, 0);
        assert_eq!(s.enable_port_mapping, 0, "port mapping stays off by default");
        assert_eq!(s.connections_limit, 200);
    }

    /// A connection limit larger than the engine's own type must not wrap around into a
    /// small number, which would throttle everything instead of removing the limit.
    #[test]
    fn an_absurd_connection_limit_saturates_rather_than_wrapping() {
        let cfg = Torrent {
            global_connections_limit: u32::MAX,
            ..Torrent::default()
        };
        assert_eq!(
            SessionSettings::from_config(&cfg).connections_limit,
            i32::MAX
        );
    }
}

#[cfg(test)]
mod tests {
    use super::contiguous_from;

    #[test]
    fn contiguous_run_stops_at_the_first_gap() {
        let have = [1u8, 1, 1, 0, 1, 1];
        assert_eq!(contiguous_from(&have, 0), 3);
        assert_eq!(contiguous_from(&have, 3), 0);
        assert_eq!(contiguous_from(&have, 4), 2);
    }

    #[test]
    fn past_the_end_is_zero_not_a_panic() {
        assert_eq!(contiguous_from(&[1, 1], 5), 0);
        assert_eq!(contiguous_from(&[], 0), 0);
    }

    /// The qBittorrent measurement that sent us down this path: 3994 pieces
    /// complete but only 8 contiguous from the start, so nothing could play.
    #[test]
    fn scattered_completion_is_worthless_for_streaming() {
        let mut have = vec![0u8; 100];
        for i in 0..8 {
            have[i] = 1;
        }
        for i in 16..100 {
            have[i] = 1;
        }
        assert_eq!(have.iter().filter(|b| **b == 1).count(), 92);
        assert_eq!(contiguous_from(&have, 0), 8, "only the front matters");
    }
}
