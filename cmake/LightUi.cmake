#   light_mk4_add_ui(<name> UI <design.json> CRATE <rust crate> ENV <VAR>)
#
#   Compiles a JSON UI design with crush into an LUI blob and hands its path to a Rust crate as an
# environment variable, for `include_bytes!(env!("<VAR>"))` -- the same assets-as-data arrangement
# as light_mk4_add_font and light_mk4_add_theme, sharing their ordering trick: the crate's
# cargo-prebuild target depends on the compile, and cargo tracks the blob through include_bytes!, so
# editing the design recompiles it and rebuilds the crate. A UI is data: a design file and this one
# call, no firmware source touched. light-ui reads the blob with its `lui` module.

function(light_mk4_add_ui NAME)
        set(one UI CRATE ENV)
        cmake_parse_arguments(U "" "${one}" "" ${ARGN})
        foreach(req UI CRATE ENV)
                if(NOT DEFINED U_${req})
                        message(FATAL_ERROR "light_mk4_add_ui(${NAME}) needs ${req}")
                endif()
        endforeach()
        if(NOT TARGET crush)
                message(FATAL_ERROR "light_mk4_add_ui(${NAME}) needs the crush target: import the crush crate with corrosion_set_hostbuild first")
        endif()

        get_filename_component(design_abs "${U_UI}" ABSOLUTE)
        set(lui "${CMAKE_CURRENT_BINARY_DIR}/${NAME}.lui")
        add_custom_command(
                OUTPUT "${lui}"
                COMMAND $<TARGET_FILE:crush> ui compile "${design_abs}" "${lui}"
                DEPENDS crush "${design_abs}"
                COMMENT "crush: ui ${NAME} -> LUI"
                VERBATIM
        )
        add_custom_target(${NAME} DEPENDS "${lui}")
        corrosion_set_env_vars(${U_CRATE} "${U_ENV}=${lui}")
        add_dependencies(cargo-prebuild_${U_CRATE} ${NAME})
endfunction()
