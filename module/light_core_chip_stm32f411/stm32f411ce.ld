/* stm32f411ce.ld
 *
 * Full linker script for the STM32F411CE, unlike the RP2 ports' objects.rp2040.ld /
 * objects.rp2350.ld -- those are INSERT fragments that add sections to a layout pico-sdk
 * already provides. ST's cmsis_device_f4 ships register headers, startup and system_*.c but no
 * linker script at all (those live in STM32CubeF4, which this port deliberately does not pull
 * in), so the whole memory layout is defined here.
 *
 * STM32F411CE: 512K flash at 0x08000000, 128K SRAM at 0x20000000. The C variant of the F411
 * has no CCM and no second SRAM bank, so this is the entire address map worth naming.
 */

ENTRY(Reset_Handler)

MEMORY
{
        FLASH (rx)  : ORIGIN = 0x08000000, LENGTH = 512K
        RAM   (rwx) : ORIGIN = 0x20000000, LENGTH = 128K
}

/* the stack grows down from the top of RAM. _estack is what startup_stm32f411xe.s places in
 * the first vector-table entry, so the core loads it into MSP before a single instruction of
 * Reset_Handler runs -- meaning a wrong value here fails before anything can report it */
_estack = ORIGIN(RAM) + LENGTH(RAM);

/* minimum room left between the end of .bss and the stack. purely a link-time assertion: it
 * cannot catch runtime stack growth, only a build whose statically-allocated data has already
 * eaten the budget */
_Min_Heap_Size = 0x400;
_Min_Stack_Size = 0x800;

SECTIONS
{
        /* must be first in flash: the core fetches the initial MSP and the reset vector from
         * the first two words at 0x08000000 */
        .isr_vector :
        {
                . = ALIGN(4);
                KEEP(*(.isr_vector))
                . = ALIGN(4);
        } > FLASH

        .text :
        {
                . = ALIGN(4);
                *(.text)
                *(.text*)
                *(.glue_7)
                *(.glue_7t)
                *(.eh_frame)
                KEEP(*(.init))
                KEEP(*(.fini))
                . = ALIGN(4);
                _etext = .;
        } > FLASH

        .rodata :
        {
                . = ALIGN(4);
                *(.rodata)
                *(.rodata*)
                . = ALIGN(4);
        } > FLASH

        /*   the framework's static-registration sections. KEEP is not optional: nothing holds a
         * reference to these objects -- being found by a linker-section walk IS how they are
         * referenced -- so --gc-sections would discard every one of them.
         *   an empty section here is not an error and links fine; it means the walk finds
         * nothing and the modules/streams silently never register, which is the failure mode
         * worth knowing about when a board boots but does nothing. */
        .light.stream :
        {
                . = ALIGN(4);
                __light_streams_start = .;
                KEEP(*(.light.static_stream))
                . = ALIGN(4);
                __light_streams_end = .;
        } > FLASH
        .light.module :
        {
                . = ALIGN(4);
                __light_modules_start = .;
                KEEP(*(.light.static_module))
                . = ALIGN(4);
                __light_modules_end = .;
        } > FLASH
        .light.static :
        {
                . = ALIGN(4);
                __light_statics_start = .;
                KEEP(*(.light.static))
                . = ALIGN(4);
                __light_statics_end = .;
        } > FLASH

        .ARM.extab : { *(.ARM.extab* .gnu.linkonce.armextab.*) } > FLASH
        .ARM : {
                __exidx_start = .;
                *(.ARM.exidx*)
                __exidx_end = .;
        } > FLASH

        .preinit_array :
        {
                PROVIDE_HIDDEN(__preinit_array_start = .);
                KEEP(*(.preinit_array*))
                PROVIDE_HIDDEN(__preinit_array_end = .);
        } > FLASH
        .init_array :
        {
                PROVIDE_HIDDEN(__init_array_start = .);
                KEEP(*(SORT(.init_array.*)))
                KEEP(*(.init_array*))
                PROVIDE_HIDDEN(__init_array_end = .);
        } > FLASH
        .fini_array :
        {
                PROVIDE_HIDDEN(__fini_array_start = .);
                KEEP(*(SORT(.fini_array.*)))
                KEEP(*(.fini_array*))
                PROVIDE_HIDDEN(__fini_array_end = .);
        } > FLASH

        /* initialised data: stored in flash, copied to RAM by Reset_Handler. _sidata is where
         * it lives in flash, _sdata/_edata where it belongs in RAM -- all three are read by
         * ST's startup file by exactly these names */
        _sidata = LOADADDR(.data);
        .data :
        {
                . = ALIGN(4);
                _sdata = .;
                *(.data)
                *(.data*)
                . = ALIGN(4);
                _edata = .;
        } > RAM AT> FLASH

        . = ALIGN(4);
        .bss :
        {
                _sbss = .;
                __bss_start__ = _sbss;
                *(.bss)
                *(.bss*)
                *(COMMON)
                . = ALIGN(4);
                _ebss = .;
                __bss_end__ = _ebss;
        } > RAM

        /* newlib's sbrk grows the heap from `end`. named both ways because which one the C
         * library looks for depends on which sbrk implementation gets linked */
        ._user_heap_stack :
        {
                . = ALIGN(8);
                PROVIDE(end = .);
                PROVIDE(_end = .);
                . = . + _Min_Heap_Size;
                . = . + _Min_Stack_Size;
                . = ALIGN(8);
        } > RAM

        /* deliberately no /DISCARD/ { libc.a(*) ... } block, which ST's own templates carry.
         * It only does something harmless there because the wildcard rules above have already
         * claimed everything from those archives that matters, leaving it to discard the
         * remnants -- i.e. it depends on rule ordering for its safety. Reordering anything
         * above it would silently start discarding real code */
        .ARM.attributes 0 : { *(.ARM.attributes) }
}
