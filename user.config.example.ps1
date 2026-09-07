# Template for user.config.ps1 -- copy it, do not rename this file.
#
#     cp user.config.example.ps1 user.config.ps1
#
# user.config.ps1 is gitignored. This template is committed, and is the only file in these
# repositories that is allowed to talk about absolute paths at all.
#
# YOU PROBABLY DO NOT NEED ONE. Everything below has a default derived from where the projects
# are checked out: a dependency named `pico-sdk` is looked for at ../pico-sdk beside the project
# that needs it, and toolchains are looked for under LIGHT_TOOLS_PATH (default ~/tools) and then
# on PATH. If your checkout looks like the normal one -- all these repos side by side in a single
# directory -- a fresh clone builds with no configuration at all. Write this file only where your
# machine deviates.
#
# Precedence, highest first:
#   1. an environment variable already set in the shell (PICO_SDK_PATH, FONT_CRUSHER_PATH, ...)
#   2. this file
#   3. the derived default (../<name> for projects, LIGHT_TOOLS_PATH then PATH for tools)
#
# LIGHT_USER_CONFIG can point somewhere else entirely, which is how a CI runner or a second
# checkout supplies its own without editing anything here.

@{
        #   dependency checkouts, keyed by DIRECTORY NAME -- the same name used for the sibling
        # default, so an entry here simply says "that one is not beside me".
        Projects = @{
                # 'pico-sdk'            = 'D:/src/pico-sdk'
                # 'font-crusher'        = 'D:/src/font-crusher'
        }

        #   toolchain directories. Give the directory the tools live in, not the executable.
        # Anything omitted is looked for under LIGHT_TOOLS_PATH (default ~/tools) and then on
        # PATH, which is where a packaged toolchain on Linux already is.
        Tools = @{
                # 'w64devkit'      = 'C:/tools/w64devkit/bin'
                # 'arm-toolchain'  = 'C:/tools/arm-gnu-toolchain-14.2.rel1-mingw-w64-x86_64-arm-none-eabi/bin'
                # 'openocd'        = 'C:/tools/xpack-openocd-0.12.0-7/bin'
                # 'openocd-scripts'= 'C:/tools/xpack-openocd-0.12.0-7/openocd/scripts'
                #   RISC-V is deliberately NOT put on PATH -- it reaches the build through
                # PICO_TOOLCHAIN_PATH in the riscv preset, and having it on PATH breaks the ARM
                # trees. Naming it here is how the riscv preset finds it.
                # 'riscv-toolchain' = 'C:/tools/riscv-toolchain-15-x64-win'
        }
}
