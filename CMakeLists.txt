cmake_minimum_required(VERSION 3.25)

#   THIS IS THE C SHELL'S BUILD. CMake remains the outer driver so the shared light-*.ps1 layer,
# the presets and CI keep working; cargo is invoked from inside it by Corrosion to produce the
# Rust staticlib the firmware links. The Rust crates themselves are a plain cargo workspace and
# are tested with cargo directly on the host (see the HOST branch at the bottom).
#
#   mk3 is used for exactly two things here, neither of them framework code: its preinit
# resolves PICO_SDK_PATH / PICO_PLATFORM / PICO_BOARD from the LIGHT_* preset variables the same
# way every sibling project does, and its presets file supplies the board configurations. The
# spike takes no mk3 C code.
if(DEFINED ENV{LIGHT_PATH} AND (NOT LIGHT_PATH))
        set(LIGHT_PATH "$ENV{LIGHT_PATH}" CACHE PATH "path to the light framework project")
endif()
if(DEFINED ENV{PICO_SDK_PATH} AND (NOT PICO_SDK_PATH))
        set(PICO_SDK_PATH "$ENV{PICO_SDK_PATH}" CACHE PATH "path to the pico-sdk checkout")
endif()
if(NOT LIGHT_PATH)
        set(LIGHT_PATH "${CMAKE_CURRENT_LIST_DIR}/../light_framework_mk3" CACHE PATH "path to the light framework project")
endif()
get_filename_component(LIGHT_PATH "${LIGHT_PATH}" REALPATH BASE_DIR "${CMAKE_CURRENT_LIST_DIR}")
if(NOT EXISTS "${LIGHT_PATH}/light_init.cmake")
        message(FATAL_ERROR
                "could not find the light framework at '${LIGHT_PATH}'.\n"
                "Check it out beside this project as ../light_framework_mk3, or set LIGHT_PATH.")
endif()
include(${LIGHT_PATH}/external/light_preinit.cmake)

project(light_mk4 C CXX ASM)

#   every STREQUAL literal below is QUOTED. An unquoted word on the right of STREQUAL is
# dereferenced if a variable of that name exists, and pico_sdk_init() defines one called
# PICO_SDK -- so `LIGHT_SYSTEM STREQUAL PICO_SDK` was true before the SDK initialised and
# false after, and the pico trees silently configured with no executable in them
if(LIGHT_PLATFORM STREQUAL "TARGET")
        if(LIGHT_SYSTEM STREQUAL "PICO_SDK")
                pico_sdk_init()
        endif()

        #   rustup installs to ~/.cargo/bin and does not touch PATH (light-tools.ps1 adds it for
        # the script layer); this lets a plain `cmake --preset` from a shell without it still
        # find cargo. HOME is not set on Windows, hence both
        list(APPEND CMAKE_PROGRAM_PATH "$ENV{USERPROFILE}/.cargo/bin" "$ENV{HOME}/.cargo/bin")

        #   Corrosion: cargo-from-CMake. The cross target comes from the preset as
        # Rust_CARGO_TARGET -- thumbv8m.main-none-eabi, the SOFT-float ABI, because pico-sdk
        # compiles Cortex-M33 with -mfloat-abi=softfp; linking eabihf objects against it fails
        # with a VFP register argument mismatch
        include(FetchContent)
        FetchContent_Declare(Corrosion
                GIT_REPOSITORY https://github.com/corrosion-rs/corrosion.git
                GIT_TAG v0.6.1
                GIT_SHALLOW TRUE
        )
        FetchContent_MakeAvailable(Corrosion)

        #   one staticlib per board application; only the one the executable links is built
        corrosion_import_crate(
                MANIFEST_PATH ${CMAKE_CURRENT_LIST_DIR}/Cargo.toml
                CRATES light_app_touch169 light_app_pico2 light_app_crossfire light_app_h7 light_app_f411
                NO_STD
        )
        #   crush is a host tool the firmware build runs, so it is built for the host inside the
        # cross build -- Corrosion's hostbuild is what mk3 did with an ExternalProject for the C
        # crush, without the second CMake tree
        corrosion_import_crate(
                MANIFEST_PATH ${CMAKE_CURRENT_LIST_DIR}/Cargo.toml
                CRATES crush
        )
        corrosion_set_hostbuild(crush)
        include(${CMAKE_CURRENT_LIST_DIR}/cmake/LightFont.cmake)

        if(LIGHT_SYSTEM STREQUAL "PICO_SDK")
                add_subdirectory(module/light_mk4_shell)
                #   module/<target>/ because that is where light-flash.ps1 expects <target>.uf2
                # -- the layout every mk3 project shares, and worth matching rather than forking
                # the script
                if(PICO_BOARD STREQUAL "waveshare_rp2350_touch_lcd_1.69")
                        add_subdirectory(module/light_mk4_touch169)
                elseif(PICO_BOARD STREQUAL "pico2")
                        #   two applications share the Pico 2: the po13 demo, and crossfire
                        # with the native port in its host role. One per tree -- they
                        # configure the shell differently -- chosen by the preset
                        set(LIGHT_MK4_PICO2_APP "pico2" CACHE STRING "which application the Pico 2 tree builds: pico2 or crossfire")
                        if(LIGHT_MK4_PICO2_APP STREQUAL "crossfire")
                                add_subdirectory(module/light_mk4_crossfire)
                        else()
                                add_subdirectory(module/light_mk4_pico2)
                        endif()
                endif()
        elseif(LIGHT_SYSTEM STREQUAL "CMSIS")
                #   bare CMSIS: no SDK under the shell at all. The board selects the chip's
                # shell and its application; only the H743 so far
                add_subdirectory(module/light_mk4_shell_cmsis)
                if(LIGHT_BOARD STREQUAL "mini_stm32h7")
                        add_subdirectory(module/light_mk4_h7)
                elseif(LIGHT_BOARD STREQUAL "blackpill")
                        add_subdirectory(module/light_mk4_f411)
                else()
                        message(FATAL_ERROR "no mk4 application for the CMSIS board '${LIGHT_BOARD}' (mini_stm32h7 and blackpill so far)")
                endif()
        endif()
endif()

#   the host tree exists so scripts/test.ps1 -- and CI's project-test workflow -- run the Rust
# tests through the same door as every other project: one ctest that is `cargo test` over the
# portable crates. The staticlib crates are excluded: they have no tests and their panic
# handlers are target-only anyway. So are the STM32 port crates, for a subtler reason: cargo
# unifies features across everything in one invocation, and cortex-m's single-core critical
# section selects critical-section's restore-state-u32 while light-rp2350 selects
# restore-state-bool -- fine in a firmware build, which sees one port crate, a compile error in
# a workspace-wide test run that sees both
if(LIGHT_PLATFORM STREQUAL "HOST")
        include(CTest)
        find_program(LIGHT_CARGO cargo
                PATHS "$ENV{USERPROFILE}/.cargo/bin" "$ENV{HOME}/.cargo/bin")
        if(NOT LIGHT_CARGO)
                message(FATAL_ERROR "cargo not found -- install rustup (see README.md)")
        endif()
        add_test(NAME cargo_test
                COMMAND ${LIGHT_CARGO} test --workspace --exclude light_app_touch169 --exclude light_app_pico2 --exclude light_app_crossfire --exclude light_app_h7 --exclude light_app_f411 --exclude light-stm32h7 --exclude light-stm32f4
                WORKING_DIRECTORY ${CMAKE_CURRENT_LIST_DIR}
        )
endif()
