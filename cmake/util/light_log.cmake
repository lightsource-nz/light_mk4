macro(light_log_on_include)
        # define local symbols
        set(LEVEL_TRACE 4)
        set(LEVEL_DEBUG 3)
        set(LEVEL_INFO 2)
        set(LEVEL_WARN 1)
        set(LEVEL_ERROR 0)

        # log level silently defaults to INFO
        if(NOT DEFINED LIGHT_OUTPUT_LEVEL)
                set(LIGHT_OUTPUT_LEVEL INFO)
        endif()

endmacro()
macro(light_log LEVEL MESSAGE)
        # compare numeric values of log levels to determine if this message is logged
        if(${LEVEL_${LEVEL}} LESS_EQUAL ${LEVEL_${LIGHT_OUTPUT_LEVEL}})
                message("${LEVEL}: ${MESSAGE}")
        endif()
endmacro()

macro(light_trace MESSAGE)
        light_log(TRACE "${MESSAGE}")
endmacro()
macro(light_debug MESSAGE)
        light_log(DEBUG "${MESSAGE}")
endmacro()
macro(light_info MESSAGE)
        light_log(INFO "${MESSAGE}")
endmacro()
macro(light_warn MESSAGE)
        light_log(WARN "${MESSAGE}")
endmacro()
macro(light_error MESSAGE)
        light_log(ERROR "${MESSAGE}")
endmacro()


#   stops the configure, which none of the levels above do -- light_error() only prints.
# Called by light_platform_on_include()'s missing-LIGHT_BOARD check since long before this
# existed, where it would have failed with "Unknown CMake command light_fatal" instead of the
# message it was written to give. A diagnostic path that itself fails is worse than no
# diagnostic, because the error it reports is about the wrong thing entirely.
#   takes ${ARGV} rather than a single MESSAGE parameter: that same call site passes three
# adjacent string literals, and a one-parameter macro would print the first and silently drop
# the rest. Joined with no separator, since the strings already carry their own trailing spaces.
macro(light_fatal)
        string(JOIN "" _light_fatal_message ${ARGV})
        message(FATAL_ERROR "FATAL: ${_light_fatal_message}")
endmacro()
light_log_on_include()
