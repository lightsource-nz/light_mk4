if (NOT TARGET _light_preinit_marker)
        add_library(_light_preinit_marker INTERFACE)
        get_filename_component(LIGHT_PATH "${LIGHT_PATH}" REALPATH BASE_DIR "${CMAKE_SOURCE_DIR}")

        set(LIGHT_INIT_CMAKE_FILE ${LIGHT_PATH}/light_init.cmake)
        if(NOT EXISTS ${LIGHT_INIT_CMAKE_FILE})
            message(FATAL_ERROR "Directory '${LIGHT_PATH}' does not appear to contain the Light Framework SDK")
        endif()

        include(${LIGHT_INIT_CMAKE_FILE})

        if(LIGHT_SYSTEM STREQUAL CMSIS)
                # the consuming project's project() call is what probes the compiler, and this
                # file runs before it -- which is the whole reason light_preinit.cmake exists
                include(cmsis)
                light_cmsis_select_toolchain()
        elseif(LIGHT_SYSTEM STREQUAL PICO_SDK)
                light_declare(PICO_SDK_PATH)
                if(LIGHT_PLATFORM STREQUAL TARGET)
                        # LIGHT_ARCH straight from the cache, not LIGHT_ARCH_REQUESTED: this
                        # runs before the platform database is loaded at all, so nothing has
                        # resolved (or overwritten) it yet. the other call site, in
                        # light_load_pico_sdk_support(), passes LIGHT_ARCH_REQUESTED, which
                        # is that same cache value captured -- so both feed the mapping the
                        # user's request rather than anything derived, and cannot disagree
                        light_resolve_pico_platform(PICO_PLATFORM "${LIGHT_BOARD}" "${LIGHT_ARCH}")
                        set(PICO_BOARD "${LIGHT_BOARD}")
                elseif(LIGHT_PLATFORM STREQUAL HOST)
                        set(PICO_PLATFORM host)
                endif()

                if(DEFINED ENV{PICO_SDK_PATH} AND (NOT PICO_SDK_PATH))
                        set(PICO_SDK_PATH $ENV{PICO_SDK_PATH})
                endif()

                cmake_path(IS_RELATIVE PICO_SDK_PATH _SDK_PATH_RELATIVE)
                if(${_SDK_PATH_RELATIVE})
                        light_expand_relative_path(PICO_SDK_PATH "${PICO_SDK_PATH}" ${CMAKE_BINARY_DIR})
                endif()

                light_set(PICO_SDK_PATH "${PICO_SDK_PATH}")
                set(PICO_SDK_PATH "${PICO_SDK_PATH}" CACHE PATH "Path to the Raspberry Pi Pico SDK" FORCE)

                include(${PICO_SDK_PATH}/external/pico_sdk_import.cmake)
        endif()
endif()