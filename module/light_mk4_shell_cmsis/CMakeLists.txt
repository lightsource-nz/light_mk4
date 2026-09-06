#   the C shell for the bare-CMSIS STM32 targets: what light_mk4_shell is for the RP2 boards,
# minus an SDK. The chip's CMSIS device files and core headers come from the framework's vendored
# checkouts under lib/ -- the one thing the framework checkout is used for besides its presets,
# and it is not framework code.
#
#   light_mk4_shell_cmsis_configure(<target> CHIP <stm32h743|stm32f411>): sources, include
# paths, the chip's flags, its linker script and the packaging step, for an executable that links
# a Rust staticlib built for thumbv7em-none-eabihf -- hard float, matching -mfloat-abi=hard here
# on both chips (fpv5 double on the M7, fpv4 single on the M4; the calling convention is the same).
#
#   directory-scoped variables do not reach a function called from another directory, so
# everything the function needs is settled from the cache
set(LIGHT_MK4_CMSIS_CORE "${LIGHT_PATH}/lib/CMSIS_6/CMSIS/Core/Include" CACHE INTERNAL "CMSIS core headers")
set(LIGHT_MK4_CMSIS_H7 "${LIGHT_PATH}/lib/cmsis_device_h7" CACHE INTERNAL "CMSIS device files for the H7")
set(LIGHT_MK4_CMSIS_F4 "${LIGHT_PATH}/lib/cmsis_device_f4" CACHE INTERNAL "CMSIS device files for the F4")
if(NOT EXISTS ${LIGHT_MK4_CMSIS_CORE}/core_cm7.h)
        message(FATAL_ERROR "CMSIS sources are missing under ${LIGHT_PATH}/lib: run git -C ${LIGHT_PATH} submodule update --init --recursive")
endif()

#   the H743 linker script is the framework's template with its two variables settled here: .data/.bss/heap
# in AXI SRAM, the DMA-reachable choice, and the stack at the top of DTCM. The F411's is plain
set(LIGHT_H743_DATA_REGION AXI)
set(LIGHT_H743_STACK_ASSERT "ASSERT(_Min_Stack_Size <= LENGTH(DTCM), \"stack does not fit in DTCM\")")
set(LIGHT_MK4_H743_LD "${CMAKE_CURRENT_BINARY_DIR}/stm32h743xi.ld" CACHE INTERNAL "the configured H743 linker script")
configure_file("${LIGHT_PATH}/module/light_core_chip_stm32h743/stm32h743xi.ld.in" "${LIGHT_MK4_H743_LD}" @ONLY)
set(LIGHT_MK4_F411_LD "${LIGHT_PATH}/module/light_core_chip_stm32f411/stm32f411ce.ld" CACHE INTERNAL "the F411 linker script")

function(light_mk4_shell_cmsis_configure TARGET)
        cmake_parse_arguments(SHELL "" "CHIP" "" ${ARGN})
        set(_src ${CMAKE_CURRENT_FUNCTION_LIST_DIR}/src)
        if(SHELL_CHIP STREQUAL stm32h743)
                set(_dev "${LIGHT_MK4_CMSIS_H7}")
                set(_chip_sources ${_src}/clock.c ${_dev}/Source/Templates/system_stm32h7xx.c ${_dev}/Source/Templates/gcc/startup_stm32h743xx.s)
                set(_define STM32H743xx)
                #   Cortex-M7 with the double-precision FPU: fpv5-d16, not the single-precision
                # variant that would send every double through software
                set(_arch -mcpu=cortex-m7 -mthumb -mfpu=fpv5-d16 -mfloat-abi=hard)
                set(_ld "${LIGHT_MK4_H743_LD}")
        elseif(SHELL_CHIP STREQUAL stm32f411)
                set(_dev "${LIGHT_MK4_CMSIS_F4}")
                set(_chip_sources ${_src}/clock_f4.c ${_dev}/Source/Templates/system_stm32f4xx.c ${_dev}/Source/Templates/gcc/startup_stm32f411xe.s)
                set(_define STM32F411xE)
                set(_arch -mcpu=cortex-m4 -mthumb -mfpu=fpv4-sp-d16 -mfloat-abi=hard)
                set(_ld "${LIGHT_MK4_F411_LD}")
        else()
                message(FATAL_ERROR "light_mk4_shell_cmsis_configure: CHIP must be stm32h743 or stm32f411, not '${SHELL_CHIP}'")
        endif()
        target_sources(${TARGET} PRIVATE ${_src}/main.c ${_src}/console.c ${_chip_sources})
        target_include_directories(${TARGET} PRIVATE ${_src} ${LIGHT_MK4_CMSIS_CORE} ${_dev}/Include)
        target_compile_definitions(${TARGET} PRIVATE ${_define})
        target_compile_options(${TARGET} PRIVATE ${_arch})
        target_link_options(${TARGET} PRIVATE
                ${_arch}
                "-T${_ld}"
                --specs=nano.specs
                --specs=nosys.specs
                -Wl,--gc-sections
                -Wl,-Map=${TARGET}.map,--cref
        )
        set_property(TARGET ${TARGET} APPEND PROPERTY LINK_DEPENDS "${_ld}")
        add_custom_command(TARGET ${TARGET} POST_BUILD
                COMMAND ${CMAKE_OBJCOPY} -O binary $<TARGET_FILE:${TARGET}> $<TARGET_FILE_DIR:${TARGET}>/${TARGET}.bin
                COMMAND ${CMAKE_OBJCOPY} -O ihex $<TARGET_FILE:${TARGET}> $<TARGET_FILE_DIR:${TARGET}>/${TARGET}.hex
                COMMAND ${CMAKE_SIZE} $<TARGET_FILE:${TARGET}>
                COMMENT "generating .bin/.hex for ${TARGET}"
                VERBATIM)
endfunction()
