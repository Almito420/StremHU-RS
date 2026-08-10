// C ABI over libtorrent 2.0.11.
//
// Only what the streaming server needs is exposed, and the reason this layer
// exists at all is `set_piece_deadline`. Measuring the alternatives on a real
// nCore swarm showed that sequential download and piece priorities are hints:
// the contiguous front stopped at 8 pieces out of 13254 while 30% of the torrent
// had already arrived. A deadline is a hard ordering instruction, so the engine
// fills forward from wherever the player is reading.
//
// Every entry point is noexcept from the caller's point of view: C++ exceptions
// are caught at the boundary and turned into a null pointer or a negative return,
// with the message retrievable via lts_last_error().

#include <libtorrent/session.hpp>
#include <libtorrent/settings_pack.hpp>
#include <libtorrent/add_torrent_params.hpp>
#include <libtorrent/torrent_info.hpp>
#include <libtorrent/torrent_handle.hpp>
#include <libtorrent/torrent_status.hpp>
#include <libtorrent/download_priority.hpp>
#include <libtorrent/alert_types.hpp>
#include <libtorrent/hex.hpp>
#include <libtorrent/read_resume_data.hpp>
#include <libtorrent/write_resume_data.hpp>
#include <libtorrent/error_code.hpp>

#include <map>

#include <cstdint>
#include <cstring>
#include <memory>
#include <mutex>
#include <string>
#include <vector>

#if defined(_WIN32)
#define LTS_API __declspec(dllexport)
#else
#define LTS_API __attribute__((visibility("default")))
#endif

namespace {

std::mutex g_error_mutex;
std::string g_last_error;

void set_error(std::string msg)
{
	std::lock_guard<std::mutex> lock(g_error_mutex);
	g_last_error = std::move(msg);
}

struct Session
{
	std::unique_ptr<lt::session> ses;

	// Resume data that has arrived and not yet been collected, by info hash.
	//
	// libtorrent produces this asynchronously: asking for it posts an alert, and the
	// data appears in the alert queue some time later. Since the queue is already being
	// drained on a timer, the buffers are parked here for the caller to pick up rather
	// than delivered through a callback across the language boundary.
	std::mutex resume_mutex;
	std::map<std::string, std::vector<char>> resume_ready;
};

struct Torrent
{
	lt::torrent_handle handle;
	// Kept so file metadata stays available even before the torrent is checked.
	std::shared_ptr<lt::torrent_info> info;
};

// Written out by hand rather than calling lt::aux::to_hex: the `aux` namespace is
// internal and not exported from libtorrent's DLL, so linking against it fails.
std::string to_hex_string(lt::sha1_hash const& h)
{
	static char const digits[] = "0123456789abcdef";
	std::string out;
	out.reserve(static_cast<size_t>(h.size()) * 2);
	for (int i = 0; i < static_cast<int>(h.size()); ++i)
	{
		auto const b = static_cast<unsigned char>(h.data()[i]);
		out.push_back(digits[b >> 4]);
		out.push_back(digits[b & 0x0f]);
	}
	return out;
}

int copy_out(std::string const& src, char* buf, int buf_len)
{
	if (buf == nullptr || buf_len <= 0) return -1;
	int const needed = static_cast<int>(src.size()) + 1;
	if (needed > buf_len) return -needed;
	std::memcpy(buf, src.c_str(), static_cast<size_t>(needed));
	return static_cast<int>(src.size());
}

} // namespace

extern "C" {

LTS_API int lts_last_error(char* buf, int buf_len)
{
	std::lock_guard<std::mutex> lock(g_error_mutex);
	return copy_out(g_last_error, buf, buf_len);
}

LTS_API char const* lts_version()
{
	return LIBTORRENT_VERSION;
}

// What the caller decides about the session.
//
// Passed as a struct rather than a growing argument list, and every field is one the
// configuration file owns. The previous version took only the port and baked the rest in,
// which meant four settings in the configuration were read, written, displayed, and then
// silently ignored.
struct SessionSettings
{
	int32_t listen_port;
	int32_t max_active_torrents;
	int32_t connections_limit;
	int32_t download_rate_limit;
	int32_t upload_rate_limit;
	int32_t enable_port_mapping;
};

// listen_port of 0 lets libtorrent choose. DHT and local discovery stay off whatever the
// caller says: the target trackers are private, where both are useless and can get an
// account banned, so that is not a decision to expose.
LTS_API Session* lts_session_new(SessionSettings const* cfg)
{
	if (cfg == nullptr)
	{
		set_error("session_new: null settings");
		return nullptr;
	}
	try
	{
		lt::settings_pack sp;

		std::string const port = std::to_string(cfg->listen_port);
		sp.set_str(lt::settings_pack::listen_interfaces, "0.0.0.0:" + port + ",[::]:" + port);

		sp.set_bool(lt::settings_pack::enable_dht, false);
		sp.set_bool(lt::settings_pack::enable_lsd, false);
		bool const map_ports = cfg->enable_port_mapping != 0;
		sp.set_bool(lt::settings_pack::enable_upnp, map_ports);
		sp.set_bool(lt::settings_pack::enable_natpmp, map_ports);

		// Zero means unlimited in libtorrent as well, so the configuration's own
		// convention passes straight through.
		sp.set_int(lt::settings_pack::download_rate_limit, cfg->download_rate_limit);
		sp.set_int(lt::settings_pack::upload_rate_limit, cfg->upload_rate_limit);
		sp.set_int(lt::settings_pack::connections_limit, cfg->connections_limit);

		// Streaming shape: never let libtorrent decide the order for us, and keep
		// requests clustered so pieces complete near each other rather than
		// scattered across the file.
		sp.set_bool(lt::settings_pack::auto_sequential, false);
		sp.set_bool(lt::settings_pack::piece_extent_affinity, true);
		sp.set_bool(lt::settings_pack::strict_end_game_mode, false);

		// How many torrents may be active. libtorrent defaults to three downloads and five
		// seeds and pauses everything past that, which means torrents that quietly stop
		// seeding and stop paying off their obligation. One setting covers all three
		// counters, because a limit that applied to seeding but not downloading, or the
		// reverse, would only be a way to get it half wrong.
		sp.set_int(lt::settings_pack::active_downloads, cfg->max_active_torrents);
		sp.set_int(lt::settings_pack::active_seeds, cfg->max_active_torrents);
		sp.set_int(lt::settings_pack::active_limit, cfg->max_active_torrents);

		// Do not throttle TCP for the sake of uTP fairness. The default,
		// peer_proportional, holds TCP peers back whenever a uTP transfer is running, and
		// on this tracker's swarms most peers are TCP. Comparing the two implementations
		// on the same swarm, this was one of the settings the faster one had.
		sp.set_int(lt::settings_pack::mixed_mode_algorithm,
			lt::settings_pack::bandwidth_mixed_algo_t::prefer_tcp);

		// A stalled piece has to be given up on quickly, otherwise one slow peer
		// holds up the read head. These are the values the implementation being replaced
		// runs with, and they are not exposed because they are a property of streaming
		// rather than a preference: there is no useful setting of them for a viewer to
		// make, and a wrong one would show up as playback that stalls.
		sp.set_int(lt::settings_pack::piece_timeout, 10);
		sp.set_int(lt::settings_pack::request_timeout, 30);
		sp.set_int(lt::settings_pack::peer_timeout, 60);
		sp.set_int(lt::settings_pack::min_reconnect_time, 5);
		sp.set_int(lt::settings_pack::connection_speed, 100);
		sp.set_int(lt::settings_pack::unchoke_slots_limit, 16);
		sp.set_int(lt::settings_pack::unchoke_interval, 10);
		sp.set_int(lt::settings_pack::optimistic_unchoke_interval, 20);

		sp.set_int(lt::settings_pack::alert_mask,
			lt::alert_category::error
			| lt::alert_category::status
			| lt::alert_category::storage
			| lt::alert_category::piece_progress);

		auto s = std::make_unique<Session>();
		s->ses = std::make_unique<lt::session>(sp);
		return s.release();
	}
	catch (std::exception const& e)
	{
		set_error(std::string("session_new: ") + e.what());
		return nullptr;
	}
}

LTS_API void lts_session_free(Session* s)
{
	delete s;
}

// Reads back what the session is actually running with.
//
// Here so a setting can be checked rather than assumed. Four settings were previously
// written to the configuration file and never applied, and nothing in the program could
// have noticed, because nothing ever asked the engine what it was using.
LTS_API int lts_session_limits(
	Session* s, int32_t* connections, int32_t* download_rate, int32_t* upload_rate)
{
	if (s == nullptr) return -1;
	try
	{
		lt::settings_pack const sp = s->ses->get_settings();
		if (connections != nullptr)
			*connections = sp.get_int(lt::settings_pack::connections_limit);
		if (download_rate != nullptr)
			*download_rate = sp.get_int(lt::settings_pack::download_rate_limit);
		if (upload_rate != nullptr)
			*upload_rate = sp.get_int(lt::settings_pack::upload_rate_limit);
		return 0;
	}
	catch (std::exception const& e)
	{
		set_error(std::string("session_limits: ") + e.what());
		return -1;
	}
}

// Drains the alert queue. libtorrent buffers alerts until they are popped, so
// this has to be called periodically even when the results are ignored.
LTS_API int lts_pump_alerts(Session* s, char* err_buf, int err_len)
{
	if (s == nullptr) return -1;
	try
	{
		std::vector<lt::alert*> alerts;
		s->ses->pop_alerts(&alerts);
		std::string errors;
		int count = 0;
		for (lt::alert* a : alerts)
		{
			++count;

			// Resume data arriving. Serialised here, where the alert is still alive:
			// the buffer inside it is not valid after the queue is drained again.
			if (auto const* rd = lt::alert_cast<lt::save_resume_data_alert>(a))
			{
				std::vector<char> buf = lt::write_resume_data_buf(rd->params);
				std::string const hex = to_hex_string(rd->handle.info_hashes().get_best());
				std::lock_guard<std::mutex> lock(s->resume_mutex);
				s->resume_ready[hex] = std::move(buf);
				continue;
			}
			// A torrent with nothing worth saving reports failure. That is ordinary and
			// not an error worth showing, so it is swallowed rather than surfaced.
			if (lt::alert_cast<lt::save_resume_data_failed_alert>(a) != nullptr)
			{
				continue;
			}

			if (a->category() & lt::alert_category::error)
			{
				if (!errors.empty()) errors += " | ";
				errors += a->message();
			}
		}
		if (!errors.empty() && err_buf != nullptr) copy_out(errors, err_buf, err_len);
		else if (err_buf != nullptr && err_len > 0) err_buf[0] = '\0';
		return count;
	}
	catch (std::exception const& e)
	{
		set_error(std::string("pump_alerts: ") + e.what());
		return -1;
	}
}

// out_hash40 receives the lowercase hex info hash and needs 41 bytes.
//
// resume_data is optional. When present it carries what libtorrent already knew about
// this torrent's completed pieces, which lets it skip re-reading and re-hashing every
// file. Without it, re-adding a finished 17 GB download costs a full pass over the disk
// before it can seed again.
LTS_API Torrent* lts_add_torrent_resume(
	Session* s,
	uint8_t const* data,
	size_t len,
	char const* save_path,
	uint8_t const* resume_data,
	size_t resume_len,
	char* out_hash40)
{
	if (s == nullptr || data == nullptr || save_path == nullptr)
	{
		set_error("add_torrent: null argument");
		return nullptr;
	}
	try
	{
		auto ti = std::make_shared<lt::torrent_info>(
			reinterpret_cast<char const*>(data), static_cast<int>(len));

		lt::add_torrent_params atp;
		if (resume_data != nullptr && resume_len > 0)
		{
			lt::error_code ec;
			atp = lt::read_resume_data(
				{reinterpret_cast<char const*>(resume_data), static_cast<long>(resume_len)}, ec);
			if (ec)
			{
				// Stale or corrupt resume data is not a reason to refuse the torrent;
				// dropping it costs a re-check and nothing else.
				set_error(std::string("resume data ignored: ") + ec.message());
				atp = lt::add_torrent_params();
			}
		}
		atp.ti = ti;
		// Resume data carries the folder the data is already in, and that is the folder it
		// has to stay in: with several disks in use, overwriting it would point libtorrent
		// at a volume where the files are not, and every piece would be fetched again.
		// The caller's path is only a fallback for a torrent being added for the first time.
		if (atp.save_path.empty()) atp.save_path = save_path;
		// Nothing is downloaded until a file is selected, so a season pack cannot
		// start pulling episodes nobody asked for. Resume data carries its own
		// priorities, and those are the ones that were in force before, so they are
		// left alone when it is present.
		if (atp.file_priorities.empty())
		{
			atp.file_priorities.assign(
				static_cast<size_t>(ti->files().num_files()), lt::dont_download);
		}

		auto t = std::make_unique<Torrent>();
		t->handle = s->ses->add_torrent(atp);
		t->info = ti;

		if (out_hash40 != nullptr)
		{
			std::string const hex = to_hex_string(ti->info_hashes().get_best());
			copy_out(hex, out_hash40, 41);
		}
		return t.release();
	}
	catch (std::exception const& e)
	{
		set_error(std::string("add_torrent: ") + e.what());
		return nullptr;
	}
}


// Asks libtorrent to produce resume data for this torrent.
//
// Returns immediately: the data arrives through the alert queue, so the caller pumps
// alerts and then collects it with lts_take_resume_data.
LTS_API int lts_request_resume_data(Torrent* t)
{
	if (t == nullptr) return -1;
	try
	{
		// save_info_dict keeps the torrent file itself inside the resume data, and
		// only_if_modified avoids rewriting an unchanged one every time.
		t->handle.save_resume_data(
			lt::torrent_handle::save_info_dict | lt::torrent_handle::only_if_modified);
		return 0;
	}
	catch (std::exception const& e)
	{
		set_error(std::string("request_resume_data: ") + e.what());
		return -1;
	}
}

// Collects resume data that has arrived for one torrent, removing it from the queue.
//
// Call with a null buffer to learn the size. Returns 0 when nothing is waiting, the
// number of bytes written on success, or the negative required size when the buffer is
// too small.
LTS_API int lts_take_resume_data(
	Session* s, char const* hash40, uint8_t* buf, int buf_len)
{
	if (s == nullptr || hash40 == nullptr)
	{
		set_error("take_resume_data: null argument");
		return -1;
	}
	try
	{
		std::lock_guard<std::mutex> lock(s->resume_mutex);
		auto it = s->resume_ready.find(std::string(hash40));
		if (it == s->resume_ready.end()) return 0;

		int const size = static_cast<int>(it->second.size());
		if (buf == nullptr) return -size;
		if (buf_len < size) return -size;

		std::memcpy(buf, it->second.data(), static_cast<size_t>(size));
		s->resume_ready.erase(it);
		return size;
	}
	catch (std::exception const& e)
	{
		set_error(std::string("take_resume_data: ") + e.what());
		return -1;
	}
}

// Reads a .torrent without adding it to a session.
//
// There is a decision that has to be made before a torrent is added and cannot be made
// afterwards: which disk to write to, which depends on how much will actually be written. For
// a season pack that is one episode out of ten, not the pack, and the only way to know an
// episode's size is to read the file list. Adding the torrent to find that out would mean
// libtorrent has already been told a save path, which is the thing being decided.
//
// The returned object carries the metadata and no handle, so only the file and piece
// accessors are valid on it. lts_torrent_free releases it as usual.
LTS_API Torrent* lts_torrent_info_parse(
	uint8_t const* data, size_t len, char* out_hash40)
{
	if (data == nullptr || len == 0)
	{
		set_error("torrent_info_parse: empty buffer");
		return nullptr;
	}
	try
	{
		auto info = std::make_shared<lt::torrent_info>(
			reinterpret_cast<char const*>(data), static_cast<int>(len));
		auto t = std::make_unique<Torrent>();
		t->info = info;
		if (out_hash40 != nullptr)
		{
			std::string const hex = to_hex_string(info->info_hash());
			std::memcpy(out_hash40, hex.c_str(), hex.size() + 1);
		}
		return t.release();
	}
	catch (std::exception const& e)
	{
		set_error(std::string("torrent_info_parse: ") + e.what());
		return nullptr;
	}
}

LTS_API void lts_torrent_free(Torrent* t)
{
	delete t;
}

LTS_API int lts_num_files(Torrent* t)
{
	if (t == nullptr || !t->info) return -1;
	return t->info->files().num_files();
}

LTS_API int64_t lts_file_size(Torrent* t, int index)
{
	if (t == nullptr || !t->info) return -1;
	auto const& fs = t->info->files();
	if (index < 0 || index >= fs.num_files()) return -1;
	return fs.file_size(lt::file_index_t{index});
}

// Byte offset of the file within the whole torrent. Piece indices are global, so
// the caller needs this to turn a file offset into a piece.
LTS_API int64_t lts_file_offset(Torrent* t, int index)
{
	if (t == nullptr || !t->info) return -1;
	auto const& fs = t->info->files();
	if (index < 0 || index >= fs.num_files()) return -1;
	return fs.file_offset(lt::file_index_t{index});
}

LTS_API int lts_file_path(Torrent* t, int index, char const* save_path, char* buf, int buf_len)
{
	if (t == nullptr || !t->info) return -1;
	auto const& fs = t->info->files();
	if (index < 0 || index >= fs.num_files()) return -1;
	try
	{
		std::string const p = fs.file_path(
			lt::file_index_t{index}, save_path == nullptr ? std::string() : std::string(save_path));
		return copy_out(p, buf, buf_len);
	}
	catch (std::exception const& e)
	{
		set_error(std::string("file_path: ") + e.what());
		return -1;
	}
}

LTS_API int lts_num_pieces(Torrent* t)
{
	if (t == nullptr || !t->info) return -1;
	return t->info->num_pieces();
}

LTS_API int64_t lts_piece_length(Torrent* t)
{
	if (t == nullptr || !t->info) return -1;
	return t->info->piece_length();
}

// Everything except `index` goes to priority 0, so only the wanted episode is
// fetched. Boundary pieces shared with a neighbouring file still arrive, because
// libtorrent needs them for the selected file.
LTS_API int lts_select_only_file(Torrent* t, int index)
{
	if (t == nullptr || !t->info) return -1;
	try
	{
		int const n = t->info->files().num_files();
		if (index < 0 || index >= n) return -1;
		std::vector<lt::download_priority_t> prios(
			static_cast<size_t>(n), lt::dont_download);
		prios[static_cast<size_t>(index)] = lt::top_priority;
		t->handle.prioritize_files(prios);
		return 0;
	}
	catch (std::exception const& e)
	{
		set_error(std::string("select_only_file: ") + e.what());
		return -1;
	}
}

// Turns one file on or off on its own, leaving the others as they are.
//
// Needed because select_only_file is all-or-nothing. A film's torrent usually carries a
// sample and an nfo beside the film, and leaving those at zero forever means the torrent
// never becomes a complete seed: the tracker shows 98.94% and no amount of seeding time
// changes that. Once the film itself is on disk, the leftovers can be switched on one by one
// without disturbing anything else.
LTS_API int lts_set_file_priority(Torrent* t, int index, int priority)
{
	if (t == nullptr || !t->info) return -1;
	if (priority < 0 || priority > 7) return -1;
	try
	{
		int const n = t->info->files().num_files();
		if (index < 0 || index >= n) return -1;
		t->handle.file_priority(
			lt::file_index_t{index},
			lt::download_priority_t{static_cast<std::uint8_t>(priority)});
		return 0;
	}
	catch (std::exception const& e)
	{
		set_error(std::string("set_file_priority: ") + e.what());
		return -1;
	}
}

// Re-reads the torrent's files from disk and rebuilds what it has.
//
// Needed when one file of a torrent is deleted while the others stay. Setting the file's
// priority to zero stops it being wanted, but libtorrent still believes it holds those
// pieces and would offer them to peers; the first read of the deleted file then fails and
// takes the whole torrent down with it, along with the episodes that were still seeding.
// A recheck is what makes it forget them properly.
LTS_API int lts_force_recheck(Torrent* t)
{
	if (t == nullptr) return -1;
	try
	{
		t->handle.force_recheck();
		return 0;
	}
	catch (std::exception const& e)
	{
		set_error(std::string("force_recheck: ") + e.what());
		return -1;
	}
}

// Sets every piece of the torrent to one priority.
//
// Used to drop everything to zero while a stream is active, so the only pieces
// fetched are the ones the deadline window asks for. Without this the selected
// file downloads in full even if the viewer watches ten minutes of it.
LTS_API int lts_prioritize_all_pieces(Torrent* t, int priority)
{
	if (t == nullptr || !t->info) return -1;
	if (priority < 0 || priority > 7) return -1;
	try
	{
		std::vector<lt::download_priority_t> prios(
			static_cast<size_t>(t->info->num_pieces()),
			lt::download_priority_t{static_cast<std::uint8_t>(priority)});
		t->handle.prioritize_pieces(prios);
		return 0;
	}
	catch (std::exception const& e)
	{
		set_error(std::string("prioritize_all_pieces: ") + e.what());
		return -1;
	}
}

// The whole point of this shim. `deadline_ms` is relative to now; lower means
// more urgent. alert_when_available is not requested here because the server
// reads from the filesystem rather than through read_piece.
LTS_API int lts_set_piece_deadline(Torrent* t, int piece, int deadline_ms)
{
	if (t == nullptr) return -1;
	try
	{
		t->handle.set_piece_deadline(lt::piece_index_t{piece}, deadline_ms);
		return 0;
	}
	catch (std::exception const& e)
	{
		set_error(std::string("set_piece_deadline: ") + e.what());
		return -1;
	}
}

LTS_API int lts_reset_piece_deadline(Torrent* t, int piece)
{
	if (t == nullptr) return -1;
	try
	{
		t->handle.reset_piece_deadline(lt::piece_index_t{piece});
		return 0;
	}
	catch (std::exception const& e)
	{
		set_error(std::string("reset_piece_deadline: ") + e.what());
		return -1;
	}
}


LTS_API int lts_set_max_connections(Torrent* t, int limit)
{
	if (t == nullptr) return -1;
	try
	{
		t->handle.set_max_connections(limit);
		return 0;
	}
	catch (std::exception const& e)
	{
		set_error(std::string("set_max_connections: ") + e.what());
		return -1;
	}
}

// Fills out[0..num_pieces) with 1 for a completed piece and 0 otherwise.
// Returns the number of entries written.
LTS_API int lts_have_pieces(Torrent* t, uint8_t* out, int out_len)
{
	if (t == nullptr || out == nullptr) return -1;
	try
	{
		lt::torrent_status const st = t->handle.status(lt::torrent_handle::query_pieces);
		int const n = st.pieces.size();
		int const count = n < out_len ? n : out_len;
		for (int i = 0; i < count; ++i)
		{
			out[i] = st.pieces.get_bit(lt::piece_index_t{i}) ? uint8_t{1} : uint8_t{0};
		}
		return count;
	}
	catch (std::exception const& e)
	{
		set_error(std::string("have_pieces: ") + e.what());
		return -1;
	}
}

struct LtsStats
{
	int64_t total_done;
	int64_t total_wanted;
	int64_t total_upload;
	int32_t download_rate;
	int32_t upload_rate;
	int32_t num_peers;
	int32_t num_seeds;
	int32_t state;
	int32_t is_seeding;
	int32_t is_finished;
};

LTS_API int lts_stats(Torrent* t, LtsStats* out)
{
	if (t == nullptr || out == nullptr) return -1;
	try
	{
		lt::torrent_status const st = t->handle.status();
		out->total_done = st.total_done;
		out->total_wanted = st.total_wanted;
		out->total_upload = st.total_upload;
		out->download_rate = st.download_rate;
		out->upload_rate = st.upload_rate;
		out->num_peers = st.num_peers;
		out->num_seeds = st.num_seeds;
		out->state = static_cast<int32_t>(st.state);
		out->is_seeding = st.is_seeding ? 1 : 0;
		out->is_finished = st.is_finished ? 1 : 0;
		return 0;
	}
	catch (std::exception const& e)
	{
		set_error(std::string("stats: ") + e.what());
		return -1;
	}
}

LTS_API int lts_resume(Torrent* t)
{
	if (t == nullptr) return -1;
	try
	{
		t->handle.resume();
		return 0;
	}
	catch (std::exception const& e)
	{
		set_error(std::string("resume: ") + e.what());
		return -1;
	}
}


// Drops a torrent from the session, optionally erasing what it downloaded.
//
// The wrapper is deliberately left alive for lts_torrent_free to release, so ownership
// stays with the caller and matches how every other function here behaves. A
// torrent_handle is only a weak reference into the session, so holding and destroying
// one after removal is safe; calls made through it simply stop working.
//
// Removal is asynchronous inside libtorrent. It detaches the torrent at once and then
// deletes the files from its own disk thread, so nothing here waits for the data to
// actually be gone.
LTS_API int lts_remove_torrent(Session* s, Torrent* t, int delete_files)
{
	if (s == nullptr || t == nullptr)
	{
		set_error("remove_torrent: null argument");
		return -1;
	}
	try
	{
		lt::remove_flags_t flags = {};
		if (delete_files != 0) flags = lt::session_handle::delete_files;
		s->ses->remove_torrent(t->handle, flags);
		return 0;
	}
	catch (std::exception const& e)
	{
		set_error(std::string("remove_torrent: ") + e.what());
		return -1;
	}
}

} // extern "C"
