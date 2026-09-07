#   Locating a dependent project on disk, without any repository committing a path that is only
# true on one machine.
#
# WHY THIS EXISTS: every consuming project grew its own copy of this logic. screen-test had a
# hand-rolled foreach over seven module names; crossfire had nothing and relied on submodules
# being present; rend declares FONT_CRUSHER_PATH relative to ITSELF, which lands on
# <project>/module/font-crusher and silently misses a checkout sitting beside the project --
# at which point Findcrush.cmake fetches a copy from GitHub and local edits stop taking effect.
# Three implementations, two of them wrong in ways that fail quietly.
#
# THE RULE, in order:
#       1. an explicit ${PREFIX}_PATH already set (-D on the command line, or the cache)
#       2. $ENV{${PREFIX}_PATH}, which is what the script layer and a user config file set
#       3. an in-project checkout at module/<name>, so submodule layouts keep working
#       4. a sibling checkout at ../<name>  <-- the default, and the intended normal case
#
#   so the expected layout is simply that dependent projects sit next to each other, and nothing
# needs configuring at all. A path only ever appears in a user's own environment or their
# gitignored user config -- never in a committed file.
#
# WHY CACHE: this MUST create a cache entry. Creating one REMOVES any normal variable of the
# same name, so if a submodule added later declares the same variable with set(... CACHE ...)
# -- which rend does for both LIGHT_PATH and FONT_CRUSHER_PATH -- an uncached value set here is
# discarded the moment that submodule is added, and the submodule's own guess wins. That failure
# took a while to find precisely because it only bites when the variable is set in the
# ENVIRONMENT, so every tree configured before the script layer existed kept working.
#
# USAGE:
#       light_resolve_project(LIGHT_DISPLAY light-display)
#               -> sets LIGHT_DISPLAY_PATH, or fails with a message naming what to set
#
#       light_resolve_project(FONT_CRUSHER font-crusher OPTIONAL)
#               -> sets FONT_CRUSHER_PATH if found; leaves it unset rather than failing

if(NOT TARGET _light_resolve_init_marker)
add_library(_light_resolve_init_marker INTERFACE)

function(light_resolve_project PREFIX DIRNAME)
        cmake_parse_arguments(_LR "OPTIONAL" "MARKER" "" ${ARGN})
        #   the file that proves a directory is really the project and not an empty stub left
        # by an uninitialised submodule -- which is the most common way for this to go wrong
        if(NOT _LR_MARKER)
                set(_LR_MARKER "CMakeLists.txt")
        endif()

        set(_var "${PREFIX}_PATH")
        set(_found "")
        set(_how "")

        if(${_var})
                set(_found "${${_var}}")
                set(_how "explicitly set")
        elseif(DEFINED ENV{${_var}})
                set(_found "$ENV{${_var}}")
                set(_how "from the environment")
        else()
                #   CMAKE_SOURCE_DIR, not CMAKE_CURRENT_LIST_DIR: "sibling" means beside the
                # project being built, not beside whichever file happens to call this. Getting
                # that wrong is exactly rend's bug -- resolving relative to itself puts the
                # guess under module/, where no sibling checkout will ever be
                set(_in_project "${CMAKE_SOURCE_DIR}/module/${DIRNAME}")
                set(_sibling "${CMAKE_SOURCE_DIR}/../${DIRNAME}")
                if(EXISTS "${_in_project}/${_LR_MARKER}")
                        set(_found "${_in_project}")
                        set(_how "in-project checkout")
                else()
                        set(_found "${_sibling}")
                        set(_how "sibling checkout")
                endif()
        endif()

        get_filename_component(_found "${_found}" REALPATH BASE_DIR "${CMAKE_SOURCE_DIR}")

        if(NOT EXISTS "${_found}/${_LR_MARKER}")
                if(_LR_OPTIONAL)
                        return()
                endif()
                message(FATAL_ERROR
                        "could not find '${DIRNAME}': looked for ${_LR_MARKER} in '${_found}' (${_how}).\n"
                        "Check it out beside this project as ../${DIRNAME}, or set ${_var} -- "
                        "on the command line with -D${_var}=<path>, or in the environment.")
        endif()

        set(${_var} "${_found}" CACHE PATH "path to the ${DIRNAME} project" FORCE)
        light_info("${DIRNAME}: ${_found} (${_how})")
endfunction()

endif()
