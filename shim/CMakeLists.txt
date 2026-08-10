cmake_minimum_required(VERSION 3.20)
project(stremhu_shim CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

find_package(LibtorrentRasterbar CONFIG REQUIRED)

# Built as a DLL on purpose: all the transitive C++ linking (libtorrent, Boost,
# OpenSSL, iconv) is resolved here by MSVC, so the Rust side needs exactly one
# import library instead of a hand-maintained list of link flags.
add_library(stremhu_shim SHARED shim.cpp)

target_link_libraries(stremhu_shim PRIVATE LibtorrentRasterbar::torrent-rasterbar)

if(MSVC)
	target_compile_options(stremhu_shim PRIVATE /EHsc /W3)
	target_compile_definitions(stremhu_shim PRIVATE _WIN32_WINNT=0x0601 WIN32_LEAN_AND_MEAN)
endif()

# build.rs links against the import library and copies the DLL next to the exe.
install(TARGETS stremhu_shim
	RUNTIME DESTINATION bin
	ARCHIVE DESTINATION lib
	LIBRARY DESTINATION lib)
