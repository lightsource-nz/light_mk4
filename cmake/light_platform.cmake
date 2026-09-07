macro(light_platform_on_include)
        light_declare(LIGHT_ARCH_LIST)
        light_declare(LIGHT_CHIP_LIST)
        light_declare(LIGHT_BOARD_LIST)
        light_declare(LIGHT_ADD_HW_MODULE_HOOKS)
        light_declare(LIGHT_ADD_ARCH_HOOKS)
        light_declare(LIGHT_ADD_CHIP_HOOKS)
        light_declare(LIGHT_ADD_BOARD_HOOKS)
        light_declare(LIGHT_SELECT_ARCH_HOOKS)
        light_declare(LIGHT_SELECT_CHIP_HOOKS)
        light_declare(LIGHT_SELECT_BOARD_HOOKS)
        light_declare(LIGHT_ARCH)
        light_declare(LIGHT_ARCH_REQUESTED)
        light_declare(LIGHT_ARCH_LIB)
        light_declare(LIGHT_CHIP)
        light_declare(LIGHT_CHIP_LIB)
        light_declare(LIGHT_BOARD)
        light_declare(LIGHT_BOARD_LIB)
        light_declare(LIGHT_TARGET_PROPERTY_HOOKS_GLOBAL)

        light_declare(LIGHT_RUN_MODE)
        # mode defaults to production
        if(NOT DEFINED LIGHT_RUN_MODE)
                light_info("LIGHT_RUN_MODE not set, defaulting to PRODUCTION")
                light_set(LIGHT_RUN_MODE PRODUCTION)
        else()
                light_info("LIGHT_RUN_MODE is set to ${LIGHT_RUN_MODE}")
        endif()

        light_declare(LIGHT_SYSTEM)
        # system defaults to build-host native
        if(NOT DEFINED LIGHT_SYSTEM)
                light_info("LIGHT_SYSTEM not set, defaulting to HOST_OS")
                light_set(LIGHT_SYSTEM HOST_OS)
        else()
                light_info("LIGHT_SYSTEM is set to ${LIGHT_SYSTEM}")
        endif()

        light_declare(LIGHT_PLATFORM)
        if(LIGHT_SYSTEM STREQUAL HOST_OS)
                light_set(LIGHT_PLATFORM HOST)
        endif()

        # platform defaults to on-device
        if(NOT DEFINED LIGHT_PLATFORM)
                light_info("LIGHT_PLATFORM not set, defaulting to TARGET")
                light_set(LIGHT_PLATFORM TARGET)
        else()
                light_info("LIGHT_PLATFORM is set to ${LIGHT_PLATFORM}")
        endif()

        set(LIGHT_BOARD "${LIGHT_BOARD}" CACHE STRING "Name of the target board for the light framework")
        if(LIGHT_PLATFORM STREQUAL "HOST" AND NOT LIGHT_BOARD)
                light_set(LIGHT_BOARD host_os)
        endif()
        # NOT the same as "NOT DEFINED": the set(... CACHE STRING ...) above always defines
        # LIGHT_BOARD, even to an empty string, so DEFINED alone can never catch a missing board
        if(NOT LIGHT_BOARD)
                light_error("LIGHT_BOARD is not set")
                light_fatal("LIGHT_BOARD must be set to the name of the "
                "target hardware board for all non-host based build configurations "
                "(i.e. those where LIGHT_PLATFORM is set to TARGET or EMULATOR)")
        else()
                light_info("LIGHT_BOARD is set to ${LIGHT_BOARD}")
        endif()

        # most chips have exactly one CPU architecture and this is left empty, in which case
        # the chip's own declared arch (see light_add_chip()) is used. it exists for chips
        # that carry more than one -- RP2350 has both Cortex-M33 and Hazard3 RISC-V cores on
        # the die, and which one an image targets is a build-time choice, not a property of
        # the board or the chip.
        #
        # captured into LIGHT_ARCH_REQUESTED immediately, because LIGHT_ARCH is also an
        # OUTPUT of this system: light_select_arch() writes the resolved arch back into it.
        # reading the user's request from LIGHT_ARCH after that point would read whatever the
        # resolver last decided, so the request has to be saved before anything can overwrite
        # it
        set(LIGHT_ARCH "${LIGHT_ARCH}" CACHE STRING
                "CPU architecture to target, for chips that support more than one")
        light_set(LIGHT_ARCH_REQUESTED "${LIGHT_ARCH}")
        if(LIGHT_ARCH_REQUESTED)
                light_info("LIGHT_ARCH is set to ${LIGHT_ARCH_REQUESTED}")
        endif()
endmacro()

macro(light_add_hw_module MODULE)
        list(APPEND LIGHT_HW_MODULE_LIST "${MODULE}")
        foreach(HOOK IN LISTS LIGHT_ADD_HW_MODULE_HOOKS)
                cmake_language(CALL ${HOOK} ${MODULE})
        endforeach()
endmacro()

macro(light_add_arch ARCH BITS MMU CALLBACK)
        light_debug("added arch ${ARCH}")
        light_declare(LIGHT_ARCH_BITS__${ARCH})
        light_declare(LIGHT_ARCH_MMU__${ARCH})
        light_declare(LIGHT_ARCH_CALLBACK__${ARCH})
        light_set(LIGHT_ARCH_BITS__${ARCH} ${BITS})
        light_set(LIGHT_ARCH_MMU__${ARCH} ${MMU})
        light_set(LIGHT_ARCH_CALLBACK__${ARCH} ${CALLBACK})
        light_append(LIGHT_ARCH_LIST ${ARCH})

        foreach(HOOK IN LISTS LIGHT_ADD_ARCH_HOOKS)
                cmake_language(CALL ${HOOK} ${ARCH})
        endforeach()
endmacro()
#   ARCH is the chip's DEFAULT architecture, and for most chips its only one. chips with a
# second (see light_add_chip_arch() below) still name their default here, so that a build
# which says nothing about arch keeps targeting whatever it always did
macro(light_add_chip CHIP ARCH CALLBACK)
        light_debug("added chip ${CHIP}")
        light_declare(LIGHT_CHIP_ARCH__${CHIP})
        light_declare(LIGHT_CHIP_ARCH_LIST__${CHIP})
        light_declare(LIGHT_CHIP_CALLBACK__${CHIP})
        light_set(LIGHT_CHIP_ARCH__${CHIP} ${ARCH})
        light_set(LIGHT_CHIP_ARCH_LIST__${CHIP} ${ARCH})
        light_set(LIGHT_CHIP_CALLBACK__${CHIP} ${CALLBACK})
        light_append(LIGHT_CHIP_LIST ${CHIP})

        foreach(HOOK IN LISTS LIGHT_ADD_CHIP_HOOKS)
                cmake_language(CALL ${HOOK} ${CHIP})
        endforeach()
endmacro()
#   declares an ADDITIONAL architecture that CHIP can be targeted at, selectable via
# LIGHT_ARCH. the list is declared and seeded by light_add_chip() rather than here, so that
# calling this twice for the same chip doesn't trip light_declare()'s multiple-declaration
# warning
macro(light_add_chip_arch CHIP ARCH)
        light_debug("chip ${CHIP} also supports arch ${ARCH}")
        light_append(LIGHT_CHIP_ARCH_LIST__${CHIP} ${ARCH})
endmacro()
macro(light_add_board BOARD CHIP CALLBACK)
        light_debug("added board ${BOARD}")
        light_declare(LIGHT_BOARD_CHIP__${BOARD})
        light_declare(LIGHT_BOARD_CALLBACK__${BOARD})
        light_set(LIGHT_BOARD_CHIP__${BOARD} ${CHIP})
        light_set(LIGHT_BOARD_CALLBACK__${BOARD} ${CALLBACK})
        light_append(LIGHT_BOARD_LIST ${BOARD})

        foreach(HOOK IN LISTS LIGHT_ADD_BOARD_HOOKS)
                cmake_language(CALL ${HOOK} ${BOARD})
        endforeach()
endmacro()
macro(light_select_arch ARCH)
        light_info("arch set to ${ARCH}")
        light_set(LIGHT_ARCH ${ARCH})
        light_set(LIGHT_ARCH_LIB arch_${ARCH})
        light_add_hw_module(${LIGHT_ARCH_LIB})
        
        foreach(HOOK IN LISTS LIGHT_SELECT_ARCH_HOOKS)
                cmake_language(CALL ${HOOK} ${ARCH})
        endforeach()
        cmake_language(CALL ${LIGHT_ARCH_CALLBACK__${ARCH}})
endmacro()
macro(light_select_chip CHIP)
        light_info("chip set to ${CHIP}")
        light_set(LIGHT_CHIP ${CHIP})
        light_set(LIGHT_CHIP_LIB chip_${CHIP})
        light_add_hw_module(${LIGHT_CHIP_LIB})

        foreach(HOOK IN LISTS LIGHT_SELECT_CHIP_HOOKS)
                cmake_language(CALL ${HOOK} ${CHIP})
        endforeach()
        cmake_language(CALL ${LIGHT_CHIP_CALLBACK__${CHIP}})

        set(_LIGHT_CHIP_SELECTED_ARCH ${LIGHT_CHIP_ARCH__${CHIP}})
        if(LIGHT_ARCH_REQUESTED)
                if("${LIGHT_ARCH_REQUESTED}" IN_LIST LIGHT_CHIP_ARCH_LIST__${CHIP})
                        set(_LIGHT_CHIP_SELECTED_ARCH ${LIGHT_ARCH_REQUESTED})
                else()
                        # message(FATAL_ERROR) rather than light_fatal(): light_fatal is
                        # called at the LIGHT_BOARD check in light_platform_on_include()
                        # above but is defined nowhere -- cmake/util/light_log.cmake defines
                        # only trace/debug/info/warn/error -- so routing this through it
                        # would fail with a CMake "Unknown CMake command" instead of the
                        # message. this is a user-error path and has to actually report
                        message(FATAL_ERROR
                                "LIGHT_ARCH is set to '${LIGHT_ARCH_REQUESTED}', which chip "
                                "${CHIP} does not support. supported: "
                                "${LIGHT_CHIP_ARCH_LIST__${CHIP}}")
                endif()
        endif()
        light_select_arch(${_LIGHT_CHIP_SELECTED_ARCH})
endmacro()
macro(light_select_board BOARD)
        light_info("board set to ${BOARD}")
        light_set(LIGHT_BOARD ${BOARD})
        light_set(LIGHT_BOARD_LIB board_${BOARD})
        light_add_hw_module(${LIGHT_BOARD_LIB})
        
        foreach(HOOK IN LISTS LIGHT_SELECT_BOARD_HOOKS)
                cmake_language(CALL ${HOOK} ${BOARD})
        endforeach()
        cmake_language(CALL ${LIGHT_BOARD_CALLBACK__${BOARD}})
        light_select_chip(${LIGHT_BOARD_CHIP__${BOARD}})
endmacro()

# all hooked 'add' actions should retrospectively pass historic actions to
#       new hooks as they are registered, so that hook functions can
#       respond to actions which took place before the hook was registered
macro(light_hook_add_hw_module HOOK)
        light_append(LIGHT_ADD_HW_MODULE_HOOKS ${HOOK})
        foreach(MODULE IN LISTS LIGHT_HW_MODULE_LIST)
                cmake_language(CALL ${HOOK} ${MODULE})
        endforeach()
endmacro()
macro(light_hook_add_arch HOOK)
        light_append(LIGHT_ADD_ARCH_HOOKS ${HOOK})
        foreach(ARCH IN LISTS LIGHT_ARCH_LIST)
                cmake_language(CALL ${ARCH} ${MODULE})
        endforeach()
endmacro()
macro(light_hook_add_chip HOOK)
        light_append(LIGHT_ADD_CHIP_HOOKS ${HOOK})
        foreach(CHIP IN LISTS LIGHT_CHIP_LIST)
                cmake_language(CALL ${HOOK} ${CHIP})
        endforeach()
endmacro()
macro(light_hook_add_board HOOK)
        light_append(LIGHT_ADD_BOARD_HOOKS ${HOOK})
        foreach(BOARD IN LISTS LIGHT_BOARD_LIST)
                cmake_language(CALL ${HOOK} ${BOARD})
        endforeach()
endmacro()
macro(light_hook_select_arch HOOK)
        light_append(LIGHT_SELECT_ARCH_HOOKS ${HOOK})
        # LIGHT_ARCH_LIB, not "DEFINED LIGHT_ARCH". what this guard needs to ask is "has
        # light_select_arch() already run", and DEFINED stopped being able to answer that once
        # LIGHT_ARCH became an input as well as an output: declaring it with
        # set(... CACHE STRING ...) defines it on EVERY build, empty when no arch was
        # requested -- the same distinction light_platform_on_include() already spells out for
        # LIGHT_BOARD above. an empty LIGHT_ARCH called the hook with no argument at all
        # ("Macro invoked with incorrect arguments"), and a non-empty one would have called it
        # before the arch was validated against the chip, with no implementations registered
        # yet -- which light_core_select_arch_hook() reports as "no light_core implementation
        # found". LIGHT_ARCH_LIB is written only by light_select_arch(), so it means exactly
        # what this needs to know
        if(LIGHT_ARCH_LIB)
                cmake_language(CALL ${HOOK} ${LIGHT_ARCH})
        endif()
endmacro()
macro(light_hook_select_chip HOOK)
        light_append(LIGHT_SELECT_CHIP_HOOKS ${HOOK})
        if(DEFINED LIGHT_CHIP)
                cmake_language(CALL ${HOOK} ${LIGHT_CHIP})
        endif()
endmacro()
macro(light_hook_select_board HOOK)
        light_append(LIGHT_SELECT_BOARD_HOOKS ${HOOK})
        if(DEFINED LIGHT_BOARD)
                cmake_language(CALL ${HOOK} ${LIGHT_BOARD})
        endif()
endmacro()

#   property value hooks may be global, bound to all properties on a target, or bound to a
# specific property on a specific target.
#
#   NOTE the declare-then-append order below. light_append() warns "attempted to write to
# undeclared shared variable" and does nothing when the variable it is given has not been
# declared, so appending first meant a hook registered through either of the per-target
# entry points was silently dropped.
macro(light_hook_set_target_property_fine TARGET PROPERTY HOOK)
        #   PROPERTY is a parameter now. It used to reference ${PROPERTY} without taking it,
        # so the name expanded with whatever PROPERTY happened to hold in the caller's scope --
        # empty, normally. Hooks therefore registered under a key ending in "__" that the
        # lookup in light_target_set_property() could never produce, and never fired
        light_declare(LIGHT_TARGET_PROPERTY_HOOKS__${TARGET}__${PROPERTY})
        light_append(LIGHT_TARGET_PROPERTY_HOOKS__${TARGET}__${PROPERTY} ${HOOK})
endmacro()
macro(light_hook_set_target_property TARGET HOOK)
        light_declare(LIGHT_TARGET_PROPERTY_HOOKS__${TARGET})
        light_append(LIGHT_TARGET_PROPERTY_HOOKS__${TARGET} ${HOOK})
endmacro()
macro(light_hook_set_target_property_global HOOK)
        # declared by light_platform_on_include(), so no light_declare() here
        light_append(LIGHT_TARGET_PROPERTY_HOOKS_GLOBAL ${HOOK})
endmacro()

macro(light_target_set_property TARGET PROPERTY VALUE)
        #   this used to call light_register_common_scope_var() and
        # light_promote_common_scope_vars(), neither of which is defined anywhere in the
        # project -- so the first use of this macro died with "Unknown CMake command" rather
        # than setting anything. What they were reaching for already exists: the shared-var
        # mechanism in cmake/util/light_var.cmake, which is what carries a value out of the
        # directory scope that set it.
        #   declared only once, because setting the same property twice is legitimate and
        # light_declare() warns on redeclaration
        set(_ltsp_var LIGHT_TARGET_PROPERTY__${TARGET}__${PROPERTY})
        if(NOT ${_ltsp_var} IN_LIST LIGHT_SHARED_VARS)
                light_declare(${_ltsp_var})
        endif()
        light_set(${_ltsp_var} ${VALUE})
        light_promote_shared_vars()

        # LIGHT_TARGET_PROPERTY_HOOKS_GLOBAL, with one underscore before GLOBAL. This read
        # LIGHT_TARGET_PROPERTY_HOOKS__GLOBAL, which nothing ever writes -- the registration
        # macro above appends to the single-underscore name. That mismatch is why a global
        # hook never fired even once the missing functions above are accounted for, and it is
        # the reason every RP2 application calls pico_add_extra_outputs() directly instead of
        # going through this
        foreach(HOOK IN LISTS LIGHT_TARGET_PROPERTY_HOOKS_GLOBAL)
                cmake_language(CALL ${HOOK} ${TARGET} ${PROPERTY} ${VALUE})
        endforeach()
        foreach(HOOK IN LISTS LIGHT_TARGET_PROPERTY_HOOKS__${TARGET})
                cmake_language(CALL ${HOOK} ${TARGET} ${PROPERTY} ${VALUE})
        endforeach()
        foreach(HOOK IN LISTS LIGHT_TARGET_PROPERTY_HOOKS__${TARGET}__${PROPERTY})
                cmake_language(CALL ${HOOK} ${TARGET} ${PROPERTY} ${VALUE})
        endforeach()
endmacro()

light_platform_on_include()
