/* Layout of the vDSO image (AB-14): one read+exec PT_LOAD at address 0 holding the ELF header, the dynamic
   symbol tables (both hash styles: musl reads DT_HASH, glibc either), the note and the code. The kernel maps the
   file as it is (file offsets equal addresses), with the vvar page right before it. build.rs checks the result
   (one PT_LOAD, nothing writable, no relocations). */
SECTIONS
{
	. = SIZEOF_HEADERS;
	.hash		: { *(.hash) }			:text
	.gnu.hash	: { *(.gnu.hash) }
	.dynsym		: { *(.dynsym) }
	.dynstr		: { *(.dynstr) }
	.gnu.version	: { *(.gnu.version) }
	.gnu.version_d	: { *(.gnu.version_d) }
	.gnu.version_r	: { *(.gnu.version_r) }
	.dynamic	: { *(.dynamic) }		:text	:dynamic
	.rodata		: { *(.rodata .rodata.*) }	:text
	.note		: { *(.note.*) }		:text	:note
	.eh_frame_hdr	: { *(.eh_frame_hdr) }		:text	:eh_frame_hdr
	.eh_frame	: { KEEP (*(.eh_frame)) }	:text
	.text		: { *(.text .text.*) }		:text
	/DISCARD/	: { *(.comment) *(.note.GNU-stack) }
}

PHDRS
{
	text		PT_LOAD FLAGS(5) FILEHDR PHDRS;	/* PF_R | PF_X */
	dynamic		PT_DYNAMIC FLAGS(4);
	note		PT_NOTE FLAGS(4);
	eh_frame_hdr	PT_GNU_EH_FRAME;
}
