# drmtrace: DRM ioctl traces and their replay

Three tools, all in this directory, none needs a GPU of the host:

| Tool | What it does |
|---|---|
| `drmtrace.c` | LD_PRELOAD library: writes every DRM ioctl of a program (`_IOC_TYPE` `'d'`) with what it points to |
| `drmtrace.py` | reads a trace: `summary TRACE` (calls by name, failures, the first CS with its PM4 packets), `info TRACE REF` (INFO answers against a reference) |
| `drmreplay.c` | plays a trace against a render node again, maps the handles, compares the answers |
| `drmhostile.c` | bad and odd DRM calls for the tracer to survive (bad pointers and counts, a `'d'` ioctl on `/dev/null`, fork while a thread is in a DRM call, a partly unmapped buffer); `--host` keeps to calls that make nothing on the GPU |

Build: `gcc -O2 -shared -fPIC -I/usr/include/libdrm -o drmtrace.so drmtrace.c -ldl -lpthread` and
`gcc -O2 -I/usr/include/libdrm -o drmreplay drmreplay.c` (the uapi headers of libdrm; the numbers are the kernel's).
Tests: `host/tests/gm/30-drmtrace-tools.sh` (host, no QEMU) and the hostile and damaged-trace steps of
`host/tests/gm/20-radv-fake.sh`.
On V2 both run through the Mesa tree's loader like every glibc program there (`run /path/drmreplay TRACE`).

## Turning it on

- `DRMTRACE_FILE=/tmp/x/trace-%p.txt` (`%p` = pid): without it the library passes every call through untouched.
- `DRMTRACE_MAX_CS=n`: the trace ends after the n-th `AMDGPU_CS` (record `E`), so a program's start and its first
  frames are kept without every later frame.
- The library reads what a call reads or wrote; it never changes an argument. Every read of the program's memory
  goes through `process_vm_readv` on the own pid, so a pointer the kernel answers with EFAULT is an "unreadable"
  payload (or a `U` record), never a SIGSEGV in the tracer; a payload over 16 MiB (a bad count) is "unreadable" too.
- Only fds of DRM nodes (character devices of major 226) are traced; another driver's `'d'` ioctl passes unread.
- `fork()`: the tracer's lock is taken around it; a child writes its own file when the name has `%p`, else it
  stops tracing. A partial `munmap` keeps the part still mapped (front or tail cut); `mremap` of a whole mapping
  follows it.

## Records (one per line, text)

| Record | Meaning |
|---|---|
| `I <seq> fd=<fd> nr=<hex> req=<hex> size=<n> ret=<r> errno=<e> in=<hex> out=<hex>` | one ioctl: the argument before and after the call (`size` bytes each). `seq` counts calls in the order they started; the records come in the order they ended |
| `P <seq> info query=<hex> data=<hex>` | INFO's answer (`return_size` bytes), after a successful call |
| `P <seq> chunk <i> id=<n> len_dw=<n> data=<hex>` | CS chunk i as passed |
| `P <seq> ib <i> va=<hex> ip=<n> ring=<n> flags=<hex> bytes=<n> data=<hex>` or `... unmapped` | the dwords of the IB in chunk i, read through the program's own CPU mapping of the buffer at that GPU address (known from GEM_MMAP, mmap and GEM_VA); `unmapped` when the program has no CPU mapping of it |
| `P <seq> bolist <i or -> size=<n> n=<n> data=<hex>` | the BO list entries of a BO_HANDLES chunk or of BO_LIST |
| `P <seq> handles n=<n> data=<hex>` / `points ...` | syncobj handle and point arrays (WAIT, RESET, SIGNAL, TIMELINE_WAIT, QUERY, TIMELINE_SIGNAL) |
| `P <seq> fences n=<n> data=<hex>` | WAIT_FENCES' `drm_amdgpu_fence` array |
| `P <seq> value n=1 data=<hex>` | GEM_OP GET_GEM_CREATE_INFO's answer |
| `U <seq> fd=<fd> nr=<hex> req=<hex> size=<n> ret=<r> errno=<e>` | a call whose argument the tracer could not read (a bad pointer): no payloads; the replay skips it |
| `P <seq> <kind> ... unreadable` | a payload the call points to that is not mapped, or larger than 16 MiB (`chunks n=<n> unreadable` for a bad chunk array, `chunk <i> unreadable` for a bad chunk pointer) |
| `C fd=<fd>` | a traced fd was closed (the next open may get the same number) |
| `E cs=<n>` | the trace ends here (`DRMTRACE_MAX_CS`) |

Payloads read before the call are written before its `I` record, those of the answer after it; the reader joins
them by `seq`.

## Replay

`drmreplay [--node /dev/dri/renderD128] [--until-cs N] [--quiet] TRACE` opens the node once per traced fd and runs
the `I` records in the file's order (a call that waited for another one's result comes after it). GEM handles,
contexts, syncobjs, BO lists, exported fds and CS sequence numbers are mapped from the trace's values to this run's;
GPU addresses stay (each fd's GPU VM starts empty, so the program's addresses are free again). Before a CS the
traced IB dwords are written into this run's buffer at the IB's address. Absolute timeouts are moved: a wait that
ended well gets 5 s from now, one that timed out none. "Ended well" follows Linux: the syncobj waits answer ETIME
on a timeout, GEM_WAIT_IDLE and WAIT_CS answer 0 with `out.status` 1 for "still busy", WAIT_FENCES 0 with status
1 for "signalled" (linux-6.6/drivers/gpu/drm/amd/amdgpu/amdgpu_cs.c:1493, :1631); that status is compared too.

Same = the same return value and errno, and for INFO (except counters and clocks: TIMESTAMP, usage, evictions,
MEMORY, SENSOR, VRAM_LOST_COUNTER, GPUVM_FAULT, NUM_HANDLES) and GET_CAP the same answer.
Not replayed (counted as skipped, with the reason): fds the program got from elsewhere (a compositor's dma-buf or
sync file), USERPTR, SCHED, every ioctl without handle rules here (KMS calls among them), `U` records, calls with an
"unreadable" payload and calls that failed with EFAULT when traced (the replay's own buffers are all good).
Damaged (counted apart, exit 1, the call is not made): a line whose `in=`/`out=` is not the request's size, bad
hex, or a payload shorter than the call's count (chunk data under `len_dw`, an IB under `ib_bytes`, a BO list,
fences, handles or points under their count). The summary line ends `..., <n> CS, <bad> damaged`.

Limits: only the IBs' memory is carried over; vertex data, descriptors, shaders and other buffers a program filled
through its CPU mapping start as this run's GEM_CREATE leaves them (zero). That is enough for the fake GPU and for
the order and answers of the calls; a replay that should draw the same picture on hardware (GX-8) needs those
buffers too (a `DRMTRACE_BOS` dump of the CPU-mapped buffers of each CS's list is the next step).
