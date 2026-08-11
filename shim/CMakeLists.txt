cmake_minimum_required(VERSION 3.20)
project(stremhu_shim CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

find_package(LibtorrentRasterbar CONFIG REQUIRED)

# Shared on purpose: all the transitive C++ linking (libtorrent, Boost, OpenSSL,
# iconv) is resolved here by the C++ toolchain, so the Rust side needs exactly one
# library name instead of a hand-maintained list of link flags.
add_library(stremhu_shim SHARED shim.cpp)

target_link_libraries(stremhu_shim PRIVATE LibtorrentRasterbar::torrent-rasterbar)

if(MSVC)
	target_compile_options(stremhu_shim PRIVATE /EHsc /W3)
	target_compile_definitions(stremhu_shim PRIVATE _WIN32_WINNT=0x0601 WIN32_LEAN_AND_MEAN)
endif()

# build.rs links against what lands here and copies the runtime next to the binary:
# the DLL from `bin` on Windows, the shared object from `lib` on Linux.
install(TARGETS stremhu_shim
	RUNTIME DESTINATION bin
	ARCHIVE DESTINATION lib
	LIBRARY DESTINATION lib)
