# arm_none_eabi.cmake
#   CMake toolchain file for bare-metal ARM targets (LIGHT_SYSTEM=CMSIS).
#
#   The RP2 ports never need one of these: pico_sdk_init() installs pico-sdk's own toolchain
# file, which finds the compiler and sets the architecture flags. A bare-CMSIS target has no
# SDK to do that, so this file is selected before project() -- see the CMSIS branches in
# external/light_preinit.cmake and the root CMakeLists.txt. It must be set before project(),
# because that is when CMake probes the compiler; setting it afterwards is silently ignored and
# the build quietly uses the host compiler instead.
#
#   Architecture flags (-mcpu, -mfpu, -mfloat-abi) are deliberately NOT here. They belong to
# the chip, and a toolchain file is shared by every chip that uses this compiler -- the chip
# module puts them on its own INTERFACE target, so a second ARM chip with a different core does
# not have to fight a value baked in here.

set(CMAKE_SYSTEM_NAME Generic)
set(CMAKE_SYSTEM_PROCESSOR arm)

# without this CMake tries to build and LINK a test executable to validate the compiler, which
# cannot work before a linker script and startup object exist -- the compiler check fails and
# the configure dies with a message about a broken compiler, which it is not
set(CMAKE_TRY_COMPILE_TARGET_TYPE STATIC_LIBRARY)

# honours LIGHT_TOOLCHAIN_PATH the way pico-sdk honours PICO_TOOLCHAIN_PATH, and for the same
# reason: several cross toolchains may be installed and PATH order should not decide which one
# a build gets
if(LIGHT_TOOLCHAIN_PATH)
        set(ENV{LIGHT_TOOLCHAIN_PATH} "${LIGHT_TOOLCHAIN_PATH}")
endif()

#   every tool is located with find_program() rather than by appending a name to the compiler's
# directory. On Windows the executables carry a .exe suffix that find_program() supplies and
# string concatenation does not, and CMake rejects a CMAKE_CXX_COMPILER that is "not a full path
# to an existing compiler tool" -- naming a real file that merely lacks its suffix fails exactly
# the same way as naming nothing
foreach(_tool gcc g++ objcopy objdump size)
        string(TOUPPER "${_tool}" _tool_var)
        string(REPLACE "+" "X" _tool_var "${_tool_var}")
        find_program(LIGHT_ARM_${_tool_var} arm-none-eabi-${_tool}
                PATHS ENV LIGHT_TOOLCHAIN_PATH PATH_SUFFIXES bin)
endforeach()

if(NOT LIGHT_ARM_GCC)
        message(FATAL_ERROR
                "arm-none-eabi-gcc not found. Put it on PATH, or set LIGHT_TOOLCHAIN_PATH to the "
                "toolchain root (the directory containing bin/).")
endif()

set(CMAKE_C_COMPILER "${LIGHT_ARM_GCC}")
set(CMAKE_CXX_COMPILER "${LIGHT_ARM_GXX}")
set(CMAKE_ASM_COMPILER "${LIGHT_ARM_GCC}")
set(CMAKE_OBJCOPY "${LIGHT_ARM_OBJCOPY}" CACHE FILEPATH "")
set(CMAKE_OBJDUMP "${LIGHT_ARM_OBJDUMP}" CACHE FILEPATH "")
set(CMAKE_SIZE "${LIGHT_ARM_SIZE}" CACHE FILEPATH "")

# look for headers and libraries only in the target sysroot, but keep finding PROGRAMS on the
# host -- build tools that get run (python, a code generator) are host binaries
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)
