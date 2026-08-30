#   the C shell for the bare-CMSIS STM32 targets: what light_mk4_shell is for the RP2 boards,
# minus an SDK. The chip's CMSIS device files and core headers come from the framework's vendored
# checkouts under lib/ -- the one thing mk3 is used for besides its presets, and it is not
# framework code.
#
#   light_mk4_shell_cmsis_configure(<target>): sources, include paths, the H743's flags, the
# configured linker script and the packaging step, for an executable that links a Rust
# staticlib built for thumbv7em-none-eabihf -- hard float, matching -mfloat-abi=hard here.
#   directory-scoped variables do not reach a function called from another directory, so
# everything the function needs is settled inside it, from the cache
set(LIGHT_MK4_CMSIS_CORE "${LIGHT_PATH}/lib/CMSIS_6/CMSIS/Core/Include" CACHE INTERNAL "CMSIS core headers")
set(LIGHT_MK4_CMSIS_H7 "${LIGHT_PATH}/lib/cmsis_device_h7" CACHE INTERNAL "CMSIS device files for the H7")
if(NOT EXISTS ${LIGHT_MK4_CMSIS_CORE}/core_cm7.h OR NOT EXISTS ${LIGHT_MK4_CMSIS_H7}/Include/stm32h743xx.h)
        message(FATAL_ERROR "CMSIS sources are missing under ${LIGHT_PATH}/lib: run git -C ${LIGHT_PATH} submodule update --init --recursive")
endif()

#   the linker script is mk3's H743 template with its two variables settled here: .data/.bss/heap
# in AXI SRAM, the DMA-reachable choice, and the stack at the top of DTCM
set(LIGHT_H743_DATA_REGION AXI)
set(LIGHT_H743_STACK_ASSERT "ASSERT(_Min_Stack_Size <= LENGTH(DTCM), \"stack does not fit in DTCM\")")
set(LIGHT_MK4_H743_LD "${CMAKE_CURRENT_BINARY_DIR}/stm32h743xi.ld" CACHE INTERNAL "the configured H743 linker script")
configure_file("${LIGHT_PATH}/module/light_core_chip_stm32h743/stm32h743xi.ld.in" "${LIGHT_MK4_H743_LD}" @ONLY)

function(light_mk4_shell_cmsis_configure TARGET)
        set(_cmsis_core "${LIGHT_MK4_CMSIS_CORE}")
        set(_cmsis_h7 "${LIGHT_MK4_CMSIS_H7}")
        set(_light_h743_ld "${LIGHT_MK4_H743_LD}")
        target_sources(${TARGET} PRIVATE
                ${CMAKE_CURRENT_FUNCTION_LIST_DIR}/src/main.c
                ${CMAKE_CURRENT_FUNCTION_LIST_DIR}/src/clock.c
                ${CMAKE_CURRENT_FUNCTION_LIST_DIR}/src/console.c
                ${_cmsis_h7}/Source/Templates/system_stm32h7xx.c
                ${_cmsis_h7}/Source/Templates/gcc/startup_stm32h743xx.s
        )
        target_include_directories(${TARGET} PRIVATE
                ${CMAKE_CURRENT_FUNCTION_LIST_DIR}/src
                ${_cmsis_core}
                ${_cmsis_h7}/Include
        )
        target_compile_definitions(${TARGET} PRIVATE STM32H743xx)
        #   Cortex-M7 with the double-precision FPU: fpv5-d16, not the single-precision variant
        # that would send every double through software
        target_compile_options(${TARGET} PRIVATE -mcpu=cortex-m7 -mthumb -mfpu=fpv5-d16 -mfloat-abi=hard)
        target_link_options(${TARGET} PRIVATE
                -mcpu=cortex-m7 -mthumb -mfpu=fpv5-d16 -mfloat-abi=hard
                "-T${_light_h743_ld}"
                --specs=nano.specs
                --specs=nosys.specs
                -Wl,--gc-sections
                -Wl,-Map=${TARGET}.map,--cref
        )
        set_property(TARGET ${TARGET} APPEND PROPERTY LINK_DEPENDS "${_light_h743_ld}")
        add_custom_command(TARGET ${TARGET} POST_BUILD
                COMMAND ${CMAKE_OBJCOPY} -O binary $<TARGET_FILE:${TARGET}> $<TARGET_FILE_DIR:${TARGET}>/${TARGET}.bin
                COMMAND ${CMAKE_OBJCOPY} -O ihex $<TARGET_FILE:${TARGET}> $<TARGET_FILE_DIR:${TARGET}>/${TARGET}.hex
                COMMAND ${CMAKE_SIZE} $<TARGET_FILE:${TARGET}>
                COMMENT "generating .bin/.hex for ${TARGET}"
                VERBATIM)
endfunction()
