# Writes a project's version header, run in script mode (cmake -P) at build time by
# light_project_version() in light_version.cmake.
#
# WHY A SEPARATE FILE: this has to run on every build, not once at configure, or the compiled-in
# version silently describes whichever commit happened to be checked out when you last
# configured. Only a custom command can do that, and a custom command can only run a script.
#
# The write goes through copy_if_different so that an unchanged version does not touch the
# header's timestamp -- otherwise every build would relink the whole tree to bake in a string
# that had not changed.
#
# Inputs: LIGHT_PATH (where light-version.ps1 lives), LIGHT_VERSION_PROJECT_PATH (the checkout
# being described), LIGHT_VERSION_PREFIX (the macro name), LIGHT_VERSION_HEADER, and
# LIGHT_VERSION_FALLBACK for when git or pwsh is unavailable.

set(_version "${LIGHT_VERSION_FALLBACK}")
if(NOT LIGHT_VERSION_PROJECT_PATH)
        set(LIGHT_VERSION_PROJECT_PATH "${LIGHT_PATH}")
endif()

find_program(_pwsh NAMES pwsh powershell)
if(_pwsh AND EXISTS "${LIGHT_PATH}/scripts/light-version.ps1")
        execute_process(
                COMMAND ${_pwsh} -NoProfile -File "${LIGHT_PATH}/scripts/light-version.ps1"
                        -Path "${LIGHT_VERSION_PROJECT_PATH}"
                OUTPUT_VARIABLE _queried
                RESULT_VARIABLE _rc
                ERROR_QUIET
                OUTPUT_STRIP_TRAILING_WHITESPACE)
        if(_rc EQUAL 0 AND NOT _queried STREQUAL "")
                set(_version "${_queried}")
        endif()
endif()

set(_tmp "${LIGHT_VERSION_HEADER}.tmp")
get_filename_component(_dir "${LIGHT_VERSION_HEADER}" DIRECTORY)
get_filename_component(_name "${LIGHT_VERSION_HEADER}" NAME)
string(REGEX REPLACE "[^A-Za-z0-9]" "_" _guard "_${_name}")
string(TOUPPER "${_guard}" _guard)
file(MAKE_DIRECTORY "${_dir}")

file(WRITE "${_tmp}"
"// GENERATED -- do not edit, and do not commit.
// Written by cmake/light_version_generate.cmake on every build; the value comes from
// scripts/light-version.ps1, which derives it from git tags. See cmake/light_version.cmake.
#ifndef ${_guard}
#define ${_guard}

#define ${LIGHT_VERSION_PREFIX}_VERSION_STRING \"${_version}\"

#endif
")

# only replaces the real header when the contents differ, so an unchanged version costs no relink
execute_process(COMMAND ${CMAKE_COMMAND} -E copy_if_different "${_tmp}" "${LIGHT_VERSION_HEADER}")
file(REMOVE "${_tmp}")
