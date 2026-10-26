/* BitsOS V2 kernel: higher half at -2 GiB (V2-PLAN §2.3). Registries (kreg) are read-only sections with
   start/stop symbols; adding a registry means adding it here (desk). */
ENTRY(_start)

SECTIONS
{
    . = 0xFFFFFFFF80000000;
    __kernel_start = .;

    .text : ALIGN(4096) {
        __text_start = .;
        *(.text._start)
        *(.text .text.*)
        __text_end = .;
    }

    .rodata : ALIGN(4096) {
        __rodata_start = .;
        *(.rodata .rodata.*)
    }

    kreg_initcall : ALIGN(8) { __start_kreg_initcall = .; KEEP(*(kreg_initcall)) __stop_kreg_initcall = .; }
    kreg_ktest    : ALIGN(8) { __start_kreg_ktest = .;    KEEP(*(kreg_ktest))    __stop_kreg_ktest = .; }
    kreg_syscall  : ALIGN(8) { __start_kreg_syscall = .;  KEEP(*(kreg_syscall))  __stop_kreg_syscall = .; }
    kreg_mcptool  : ALIGN(8) { __start_kreg_mcptool = .;  KEEP(*(kreg_mcptool))  __stop_kreg_mcptool = .; }
    kreg_devnode  : ALIGN(8) { __start_kreg_devnode = .;  KEEP(*(kreg_devnode))  __stop_kreg_devnode = .; }
    kreg_procfile : ALIGN(8) { __start_kreg_procfile = .; KEEP(*(kreg_procfile)) __stop_kreg_procfile = .; }
    kreg_procdir  : ALIGN(8) { __start_kreg_procdir = .;  KEEP(*(kreg_procdir))  __stop_kreg_procdir = .; }
    kreg_console_cmd : ALIGN(8) { __start_kreg_console_cmd = .; KEEP(*(kreg_console_cmd)) __stop_kreg_console_cmd = .; }
    kreg_pcidrv   : ALIGN(8) { __start_kreg_pcidrv = .;   KEEP(*(kreg_pcidrv))   __stop_kreg_pcidrv = .; }
    kreg_metric   : ALIGN(8) { __start_kreg_metric = .;   KEEP(*(kreg_metric))   __stop_kreg_metric = .; }
    /* The registries are read-only too. */
    __rodata_end = .;

    .data : ALIGN(4096) {
        __data_start = .;
        *(.data .data.*)
        *(.got .got.*)
    }

    .bss : ALIGN(4096) {
        *(.bss .bss.*)
        *(COMMON)
    }

    __kernel_end = .;

    /DISCARD/ : {
        *(.eh_frame .eh_frame_hdr .eh_frame.*)
        *(.note .note.*)
        *(.comment)
    }
}
