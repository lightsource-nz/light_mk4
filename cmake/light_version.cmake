# Deriving a project's version from its git tags, and checking the versions it depends on.
#
# WHY THIS EXISTS: until this, every version string in these projects was a hand-typed "0.1.0" in
# a header, and a binary in someone else's hands could not say which commit produced it. That is
# the first question worth asking when a field report arrives, and it had no answer.
#
# scripts/light-version.ps1 is the single source of truth for the derivation; this file only
# calls it. Keeping that in one place means the string a release is tagged with and the string
# compiled into the image cannot drift apart.
#
# GENERALISED, and deliberately so. This began as the framework's own version machinery, spelled
# in terms of LIGHT_* throughout, at which point font-crusher, light_display and light_ui each
# needed the same thing -- and the resolver next door records what happens when consuming
# projects grow their own copy of shared logic: three implementations, two of them wrong in ways
# that fail quietly. So the framework provides it and its consumers call it.

if(NOT TARGET _light_version_init_marker)
add_library(_light_version_init_marker INTERFACE)

#   asks light-version.ps1 what a checkout's version is. Answers "0.0.0-unknown" rather than
# failing: a build from a source archive has no .git, and must still build. Making the version
# machinery a hard dependency of compiling at all is a much worse trade than an honest "unknown"
function(_light_query_version repo_path out_var)
        find_program(_light_pwsh NAMES pwsh powershell)
        if(NOT _light_pwsh)
                set(${out_var} "0.0.0-unknown" PARENT_SCOPE)
                return()
        endif()
        execute_process(
                COMMAND ${_light_pwsh} -NoProfile -File "${LIGHT_PATH}/scripts/light-version.ps1"
                        -Path "${repo_path}"
                OUTPUT_VARIABLE _version
                ERROR_VARIABLE _version_err
                RESULT_VARIABLE _version_rc
                OUTPUT_STRIP_TRAILING_WHITESPACE)
        if(NOT _version_rc EQUAL 0 OR _version STREQUAL "")
                set(${out_var} "0.0.0-unknown" PARENT_SCOPE)
        else()
                set(${out_var} "${_version}" PARENT_SCOPE)
        endif()
endfunction()

#   splits "1.9.0-dev.5+main.gabc1234" into its core triple and whether it carries a pre-release.
# The build metadata after '+' is ignored, as semver requires -- it never affects ordering.
function(_light_version_split version out_core out_prerelease)
        string(REGEX REPLACE "\\+.*$" "" _v "${version}")
        string(REGEX MATCH "^[0-9]+\\.[0-9]+\\.[0-9]+" _core "${_v}")
        if(NOT _core)
                set(_core "0.0.0")
        endif()
        string(REGEX REPLACE "^[0-9]+\\.[0-9]+\\.[0-9]+" "" _rest "${_v}")
        string(REGEX REPLACE "^-" "" _pre "${_rest}")
        set(${out_core} "${_core}" PARENT_SCOPE)
        set(${out_prerelease} "${_pre}" PARENT_SCOPE)
endfunction()

#   REQUIRES a dependency to be at least `minimum`, which must be a plain release triple.
#
#   THE PRE-RELEASE RULE IS THE WHOLE POINT, and it is semver's, not an approximation of it.
# light-version.ps1 reports a working tree as the version it is working TOWARD: five commits past
# v1.7.2 is "1.8.0-dev.5". Semver orders that BEFORE 1.8.0, which is exactly right -- such a tree
# does not contain 1.8.0, because 1.8.0 has not been cut. CMake's own VERSION_GREATER_EQUAL would
# ignore the suffix and accept it, and the consumer would then fail to compile against a
# dependency the check had just declared adequate.
#
#   a version of "0.0.0-unknown" -- no git, no pwsh, a source archive -- WARNS rather than fails.
# The alternative is that a project cannot be built at all outside a git checkout, which is a
# worse outcome than an unverified dependency.
function(light_require_project_version PREFIX minimum)
        set(_path "${${PREFIX}_PATH}")
        if(NOT _path)
                message(FATAL_ERROR
                        "light_require_project_version(${PREFIX} ${minimum}): ${PREFIX}_PATH is not "
                        "set. Resolve the project with light_resolve_project() first.")
        endif()
        _light_query_version("${_path}" _found)

        if(_found STREQUAL "0.0.0-unknown")
                light_info("${PREFIX}: version unknown, cannot verify the requirement >= ${minimum}")
                return()
        endif()

        _light_version_split("${_found}" _found_core _found_pre)
        _light_version_split("${minimum}" _min_core _min_pre)

        set(_ok FALSE)
        if(_found_core VERSION_GREATER _min_core)
                set(_ok TRUE)
        elseif(_found_core VERSION_EQUAL _min_core AND NOT _found_pre)
                set(_ok TRUE)
        endif()

        if(NOT _ok)
                message(FATAL_ERROR
                        "${PREFIX} is version ${_found}, but this project requires >= ${minimum}.\n"
                        "  found at: ${_path}\n"
                        "A pre-release (-dev.N) sorts BEFORE the release it names, so a tree working "
                        "toward ${minimum} does not satisfy it -- tag that project, or check out a "
                        "revision at or past ${minimum}.")
        endif()
        light_info("${PREFIX}: ${_found} (satisfies >= ${minimum})")
endfunction()

#   DERIVES THIS PROJECT'S OWN VERSION and compiles it in, as <PREFIX>_VERSION_STRING in a
# generated header. Sets <PREFIX>_VERSION_STRING for CMake too, and <PREFIX>_VERSION_INCLUDE_DIR
# for the include path.
#
#   REGENERATED ON EVERY BUILD, not only at configure time. A version baked at configure time
# goes stale the moment you commit, so the binary would claim a commit it was not built from --
# which defeats the entire purpose of having it. The generator writes through copy_if_different,
# so a relink only happens when the string actually changes.
function(light_project_version)
        cmake_parse_arguments(_LV "" "PREFIX;PATH;HEADER" "" ${ARGN})
        if(NOT _LV_PREFIX)
                message(FATAL_ERROR "light_project_version: PREFIX is required")
        endif()
        if(NOT _LV_PATH)
                set(_LV_PATH "${CMAKE_SOURCE_DIR}")
        endif()
        if(NOT _LV_HEADER)
                string(TOLOWER "${_LV_PREFIX}" _lower)
                set(_LV_HEADER "${_lower}_version.h")
        endif()

        _light_query_version("${_LV_PATH}" _version)
        light_info("${_LV_PREFIX} version: ${_version}")

        set(_dir "${CMAKE_BINARY_DIR}/light_generated")
        set(_header "${_dir}/${_LV_HEADER}")
        set(_script "${LIGHT_PATH}/cmake/light_version_generate.cmake")
        string(TOLOWER "${_LV_PREFIX}" _target_name)

        add_custom_target(${_target_name}_version_header ALL
                BYPRODUCTS "${_header}"
                COMMAND ${CMAKE_COMMAND}
                        -DLIGHT_PATH=${LIGHT_PATH}
                        -DLIGHT_VERSION_PREFIX=${_LV_PREFIX}
                        -DLIGHT_VERSION_PROJECT_PATH=${_LV_PATH}
                        -DLIGHT_VERSION_HEADER=${_header}
                        -DLIGHT_VERSION_FALLBACK=${_version}
                        -P "${_script}"
                COMMENT "checking ${_LV_PREFIX} version"
                VERBATIM)

        # generated once now as well, so the first compile has a header to include even if the
        # custom target has not run yet
        execute_process(COMMAND ${CMAKE_COMMAND}
                -DLIGHT_PATH=${LIGHT_PATH}
                -DLIGHT_VERSION_PREFIX=${_LV_PREFIX}
                -DLIGHT_VERSION_PROJECT_PATH=${_LV_PATH}
                -DLIGHT_VERSION_HEADER=${_header}
                -DLIGHT_VERSION_FALLBACK=${_version}
                -P "${_script}")

        set(${_LV_PREFIX}_VERSION_STRING "${_version}" PARENT_SCOPE)
        set(${_LV_PREFIX}_VERSION_INCLUDE_DIR "${_dir}" CACHE INTERNAL "")
endfunction()

endif()

#   the framework's own version, which is what this file used to do and nothing else. Kept at
# file scope rather than moved into light_core's CMakeLists so that including this file
# continues to mean "LIGHT_VERSION_STRING is now available", as it always has.
light_project_version(PREFIX LIGHT PATH "${LIGHT_PATH}" HEADER light_version.h)
set(LIGHT_VERSION_INCLUDE_DIR "${LIGHT_VERSION_INCLUDE_DIR}" CACHE INTERNAL "")
