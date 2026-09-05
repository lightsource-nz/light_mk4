#   light_mk4_add_theme(<name> THEME <file.json> CRATE <rust crate> ENV <VAR>)
#
#   Compiles a JSON look-and-feel with crush into an LTH blob and hands its path to a Rust
# crate as an environment variable, for `include_bytes!(env!("<VAR>"))` -- the same
# assets-as-data arrangement as light_mk4_add_font, sharing its ordering trick: the crate's
# cargo-prebuild target depends on the compile, and cargo tracks the blob through
# include_bytes!, so editing the theme recompiles it and rebuilds the crate. Restyling an
# interface is a data change: a theme file and this one call, no UI source touched.
function(light_mk4_add_theme NAME)
        set(one THEME CRATE ENV)
        cmake_parse_arguments(T "" "${one}" "" ${ARGN})
        foreach(req THEME CRATE ENV)
                if(NOT DEFINED T_${req})
                        message(FATAL_ERROR "light_mk4_add_theme(${NAME}) needs ${req}")
                endif()
        endforeach()
        if(NOT TARGET crush)
                message(FATAL_ERROR "light_mk4_add_theme(${NAME}) needs the crush target: import the crush crate with corrosion_set_hostbuild first")
        endif()

        get_filename_component(theme_abs "${T_THEME}" ABSOLUTE)
        set(lth "${CMAKE_CURRENT_BINARY_DIR}/${NAME}.lth")
        add_custom_command(
                OUTPUT "${lth}"
                COMMAND $<TARGET_FILE:crush> theme compile "${theme_abs}" "${lth}"
                DEPENDS crush "${theme_abs}"
                COMMENT "crush: theme ${NAME} -> LTH"
                VERBATIM
        )
        add_custom_target(${NAME} DEPENDS "${lth}")
        corrosion_set_env_vars(${T_CRATE} "${T_ENV}=${lth}")
        add_dependencies(cargo-prebuild_${T_CRATE} ${NAME})
endfunction()
