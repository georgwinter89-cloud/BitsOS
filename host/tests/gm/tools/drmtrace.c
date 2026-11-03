// GM-1c: an LD_PRELOAD tracer of every DRM ioctl a program makes, for drmreplay.c and the scenario reports.
// DRMTRACE_FILE=<path> (a "%p" in it becomes the pid) turns it on; without it every call passes through untouched.
// DRMTRACE_MAX_CS=<n> ends the trace after the n-th AMDGPU_CS (record "E cs=<n>"): the start of a program, up to and
// with its first frames, without the megabytes of every later frame.
// One text record per line (format in drmtrace.md next to this file):
//   I <seq> fd=<fd> nr=<hex> req=<hex> size=<n> ret=<r> errno=<e> in=<hex> out=<hex>   the argument before and after the call
//   P <seq> <kind> ...                                                         what the argument points to
//   U <seq> fd=<fd> nr=<hex> req=<hex> size=<n> ret=<r> errno=<e>                a call whose argument was unreadable
//   C fd=<fd>                                                                  a traced fd was closed
// Payloads: INFO's answer, every CS chunk with each IB's dwords (read through the program's own CPU mapping of the
// buffer the IB's address lies in, known from GEM_MMAP + mmap + GEM_VA), BO lists, syncobj handle and point arrays,
// WAIT_FENCES' fences and GEM_OP's value. The tracer only reads what the call reads or wrote; it never changes an
// argument, so a traced program behaves as an untraced one (slower). Every read of the program's memory goes
// through process_vm_readv (rd()), never a plain dereference: a bad pointer or count that the kernel answers with
// EFAULT or EINVAL becomes an "unreadable" payload, not a SIGSEGV in the tracer. Only fds of DRM nodes (char major
// 226) are traced.
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/uio.h>
#include <unistd.h>
#include <drm.h>
#include <amdgpu_drm.h>

static int (*real_ioctl)(int, unsigned long, ...);
static void *(*real_mmap)(void *, size_t, int, int, int, off_t);
static int (*real_munmap)(void *, size_t);
static void *(*real_mremap)(void *, size_t, size_t, int, ...);
static int (*real_close)(int);
static int out_fd = -1;
static char *path_template;
static unsigned long seq, cs_seen, cs_max;
static int stopped;
static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;

// What the tracer knows of the program's buffers, per fd: mmap offsets, CPU mappings and GPU addresses.
// `cpu` shows the buffer's bytes from `skip` on, `len` of them (a program may unmap the front or the tail of a
// mapping). `copy`: what the program left in the buffer when it unmapped a part of it, at the buffer's own offsets
// (programs write an IB once and unmap it; a later CS still runs it), freed with the handle.
struct bo { int fd; uint32_t handle; uint64_t off; char *cpu; uint64_t skip; size_t len; char *copy; size_t copylen; };
struct va { int fd; uint32_t handle; uint64_t va, bo_off, size; };
static struct bo *bos;
static size_t nbos, capbos;
static struct va *vas;
static size_t nvas, capvas;
static int *fds;
static size_t nfds, capfds;

// No payload larger than this is copied (it is written as "unreadable"): a bad count in a program's argument, which
// the kernel refuses, must not make the tracer read or allocate gigabytes. Mesa's IBs and BO lists stay far below.
#define MAX_PAYLOAD ((size_t)16 << 20)

// Copies n bytes of the program's memory at src; 0 when every byte was readable. process_vm_readv on the own pid
// answers EFAULT for an address that is not mapped, where a plain read would end the program with SIGSEGV: a
// pointer the kernel refuses with EFAULT stays a refused call in a traced program too.
static int rd(void *dst, uint64_t src, size_t n) {
    size_t done = 0;
    while (done < n) {
        struct iovec l = { (char *)dst + done, n - done }, r = { (void *)(uintptr_t)(src + done), n - done };
        ssize_t got = process_vm_readv(getpid(), &l, 1, &r, 1, 0);
        if (got < 0 && errno == EINTR) continue;
        if (got <= 0) return -1;
        done += (size_t)got;
    }
    return 0;
}
// A copy of n bytes at the program's src (malloc'd), or NULL when they are not all readable or too many.
static unsigned char *snap(uint64_t src, size_t n) {
    if (n > MAX_PAYLOAD || (!src && n)) return NULL;
    unsigned char *b = malloc(n ? n : 1);
    if (b && rd(b, src, n)) { free(b); b = NULL; }
    return b;
}
// n * each without wrapping, or a value above MAX_PAYLOAD.
static size_t times(uint64_t n, size_t each) { return each && n > MAX_PAYLOAD / each ? MAX_PAYLOAD + 1 : (size_t)n * each; }
// Whether [at, at + n) lies in [0, size), without wrapping.
static int fits(uint64_t at, uint64_t n, uint64_t size) { return at <= size && n <= size - at; }

static void *grow(void *p, size_t *cap, size_t n, size_t each) {
    if (n < *cap) return p;
    size_t c = *cap ? *cap * 2 : 64;
    void *q = realloc(p, c * each);
    if (!q) return NULL;
    *cap = c;
    return q;
}

// The line being written: a growing buffer, written with one write() so lines of threads never mix.
static char *line;
static size_t len, cap;
static void put(const char *fmt, ...) {
    for (;;) {
        va_list ap;
        va_start(ap, fmt);
        int n = vsnprintf(line ? line + len : NULL, line ? cap - len : 0, fmt, ap);
        va_end(ap);
        if (n < 0) return;
        if (line && len + (size_t)n < cap) { len += (size_t)n; return; }
        size_t c = (cap ? cap : 4096);
        while (c <= len + (size_t)n) c *= 2;
        char *q = realloc(line, c);
        if (!q) return;
        line = q;
        cap = c;
    }
}
static void hex(const void *p, size_t n) {
    static const char d[] = "0123456789abcdef";
    put("%s", "");
    if (!line) return;
    if (len + 2 * n + 1 > cap) {
        size_t c = cap;
        while (c <= len + 2 * n + 1) c *= 2;
        char *q = realloc(line, c);
        if (!q) return;
        line = q;
        cap = c;
    }
    const unsigned char *b = p;
    for (size_t i = 0; i < n; i++) { line[len++] = d[b[i] >> 4]; line[len++] = d[b[i] & 15]; }
    line[len] = 0;
}
static void flush(void) {
    put("\n");
    size_t done = 0;
    while (line && done < len) {
        ssize_t w = write(out_fd, line + done, len - done);
        if (w < 0 && errno == EINTR) continue;
        if (w <= 0) break;
        done += (size_t)w;
    }
    len = 0;
}

// The trace file's name: DRMTRACE_FILE with "%p" as the pid.
static int open_trace(void) {
    char path[4096];
    size_t o = 0;
    for (const char *s = path_template; *s && o + 24 < sizeof path; s++) {
        if (s[0] == '%' && s[1] == 'p') { o += (size_t)snprintf(path + o, sizeof path - o, "%d", (int)getpid()); s++; }
        else path[o++] = *s;
    }
    path[o] = 0;
    return open(path, O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC, 0644);
}

// fork() in one thread while another holds `mu` would leave the child with a lock nobody releases (its first DRM call
// would hang): the lock is taken around fork. A child with a "%p" trace name writes its own file; one without
// stops tracing, so two processes never write one file.
static void fork_prepare(void) { pthread_mutex_lock(&mu); }
static void fork_parent(void) { pthread_mutex_unlock(&mu); }
static void fork_child(void) {
    if (strstr(path_template, "%p")) {
        int f = open_trace();
        if (f >= 0) { if (real_close) real_close(out_fd); out_fd = f; }
        else stopped = 1;
    } else stopped = 1;
    pthread_mutex_unlock(&mu);
}

__attribute__((constructor)) static void init(void) {
    real_ioctl = dlsym(RTLD_NEXT, "ioctl");
    real_mmap = dlsym(RTLD_NEXT, "mmap");
    real_munmap = dlsym(RTLD_NEXT, "munmap");
    real_mremap = dlsym(RTLD_NEXT, "mremap");
    real_close = dlsym(RTLD_NEXT, "close");
    const char *f = getenv("DRMTRACE_FILE");
    if (!f || !*f) return;
    path_template = strdup(f);
    if (!path_template) return;
    out_fd = open_trace();
    if (out_fd < 0) return;
    pthread_atfork(fork_prepare, fork_parent, fork_child);
    const char *m = getenv("DRMTRACE_MAX_CS");
    if (m && *m) cs_max = strtoul(m, NULL, 10);
}

static int known_fd(int fd) {
    for (size_t i = 0; i < nfds; i++) if (fds[i] == fd) return 1;
    return 0;
}
// Whether fd is a DRM node (character device of major 226), remembered once seen. Other drivers use ioctl type 'd'
// too, and their argument may be a number rather than a pointer: those calls pass through unread.
static int drm_fd(int fd) {
    if (known_fd(fd)) return 1;
    struct stat st;
    if (fstat(fd, &st) || !S_ISCHR(st.st_mode) || major(st.st_rdev) != 226) return 0;
    int *q = grow(fds, &capfds, nfds, sizeof *fds);
    if (!q) return 0;
    fds = q;
    fds[nfds++] = fd;
    return 1;
}

// GPU addresses are 48 bits; the upper half comes sign-extended in some fields (GEM_VA, CS chunks), not in PM4.
#define VA(x) ((x) & 0xffffffffffffull)

// Where the program (or the tracer's copy of an unmapped part) holds the bytes at GPU address `addr` of fd, or 0.
// The address is the program's, so it is read through snap() only.
static uint64_t cpu_of(int fd, uint64_t addr, uint64_t bytes) {
    addr = VA(addr);
    for (size_t i = 0; i < nvas; i++) {
        struct va *v = &vas[i];
        if (v->fd != fd || addr < v->va || !fits(addr - v->va, bytes, v->size)) continue;
        uint64_t at = v->bo_off + (addr - v->va);
        for (size_t j = 0; j < nbos; j++) {
            struct bo *b = &bos[j];
            if (b->fd != fd || b->handle != v->handle) continue;
            if (b->cpu && at >= b->skip && fits(at - b->skip, bytes, b->len)) return (uint64_t)(uintptr_t)(b->cpu + (at - b->skip));
            if (b->copy && fits(at, bytes, b->copylen)) return (uint64_t)(uintptr_t)(b->copy + at);
        }
    }
    return 0;
}

// "data=<hex>" of n bytes at the program's src, or "unreadable"; returns the copy (to free) or NULL.
static unsigned char *data(uint64_t src, size_t n) {
    unsigned char *b = snap(src, n);
    if (b) { put("data="); hex(b, n); }
    else put("unreadable");
    return b;
}

static void array(unsigned long s, const char *kind, uint64_t ptr, uint64_t n, size_t each) {
    if (!ptr || !n) return;
    put("P %lu %s n=%llu ", s, kind, (unsigned long long)n);
    free(data(ptr, times(n, each)));
    flush();
}

// The IBs a gfx or compute IB calls or chains to (PM4 INDIRECT_BUFFER, opcode 0x3f: address in dwords 1-2, size
// in dwords in the low 20 bits of dword 3), each address once per CS, as "subib" payloads, and theirs in turn.
// `w` is the tracer's copy of the IB.
struct seen { uint64_t *va; size_t n, cap; };
static void sub_ibs(int fd, unsigned long s, unsigned i, const uint32_t *w, size_t n, struct seen *seen) {
    for (size_t k = 0; k < n;) {
        uint32_t h = w[k];
        if (h >> 30 != 3) { k++; continue; }
        size_t cnt = ((h >> 16) & 0x3fff) + 1;
        if (((h >> 8) & 0xff) == 0x3f && cnt >= 3 && k + 3 < n) {
            uint64_t va = (w[k + 1] & ~3u) | ((uint64_t)(w[k + 2] & 0xffff) << 32);
            size_t bytes = (size_t)(w[k + 3] & 0xfffff) * 4;
            int known = 0;
            for (size_t j = 0; j < seen->n; j++) known |= seen->va[j] == va;
            uint64_t *q = known ? seen->va : grow(seen->va, &seen->cap, seen->n, sizeof *seen->va);
            if (!known && q) {
                seen->va = q;
                seen->va[seen->n++] = va;
                uint64_t p = cpu_of(fd, va, bytes);
                put("P %lu subib %u va=0x%llx bytes=%zu ", s, i, (unsigned long long)va, bytes);
                unsigned char *copy = p ? data(p, bytes) : (put("unmapped"), NULL);
                flush();
                if (copy) sub_ibs(fd, s, i, (const uint32_t *)copy, bytes / 4, seen);
                free(copy);
            }
        }
        k += cnt + 1;
    }
}

// The payloads a call reads, from the tracer's copy `arg` of its argument.
static void before(int fd, unsigned nr, const void *arg, size_t size, unsigned long s) {
    if (nr == DRM_COMMAND_BASE + DRM_AMDGPU_CS && size >= sizeof(struct drm_amdgpu_cs_in)) {
        const struct drm_amdgpu_cs_in *in = arg;
        if (!in->chunks || !in->num_chunks) return;
        uint64_t *chunks = (uint64_t *)snap(in->chunks, times(in->num_chunks, 8));
        if (!chunks) { put("P %lu chunks n=%u unreadable", s, in->num_chunks); flush(); return; }
        for (uint32_t i = 0; i < in->num_chunks; i++) {
            struct drm_amdgpu_cs_chunk ch;
            if (rd(&ch, chunks[i], sizeof ch)) { put("P %lu chunk %u unreadable", s, i); flush(); continue; }
            size_t bytes = (size_t)ch.length_dw * 4;
            put("P %lu chunk %u id=%u len_dw=%u ", s, i, ch.chunk_id, ch.length_dw);
            unsigned char *d = data(ch.chunk_data, bytes);
            flush();
            if (d && ch.chunk_id == AMDGPU_CHUNK_ID_IB && bytes >= sizeof(struct drm_amdgpu_cs_chunk_ib)) {
                const struct drm_amdgpu_cs_chunk_ib *ib = (const void *)d;
                uint64_t p = cpu_of(fd, ib->va_start, ib->ib_bytes);
                put("P %lu ib %u va=0x%llx ip=%u ring=%u flags=0x%x bytes=%u ", s, i, (unsigned long long)ib->va_start,
                    ib->ip_type, ib->ring, ib->flags, ib->ib_bytes);
                unsigned char *copy = p ? data(p, ib->ib_bytes) : (put("unmapped"), NULL);
                flush();
                if (copy && (ib->ip_type == AMDGPU_HW_IP_GFX || ib->ip_type == AMDGPU_HW_IP_COMPUTE)) {
                    struct seen seen = { 0 };
                    sub_ibs(fd, s, i, (const uint32_t *)copy, ib->ib_bytes / 4, &seen);
                    free(seen.va);
                }
                free(copy);
            }
            if (d && ch.chunk_id == AMDGPU_CHUNK_ID_BO_HANDLES && bytes >= sizeof(struct drm_amdgpu_bo_list_in)) {
                const struct drm_amdgpu_bo_list_in *bl = (const void *)d;
                put("P %lu bolist %u size=%u n=%u ", s, i, bl->bo_info_size, bl->bo_number);
                free(data(bl->bo_info_ptr, times(bl->bo_number, bl->bo_info_size)));
                flush();
            }
            free(d);
        }
        free(chunks);
    } else if (nr == DRM_COMMAND_BASE + DRM_AMDGPU_BO_LIST && size >= sizeof(struct drm_amdgpu_bo_list_in)) {
        const struct drm_amdgpu_bo_list_in *bl = arg;
        if (bl->operation != AMDGPU_BO_LIST_OP_DESTROY) {
            put("P %lu bolist - size=%u n=%u ", s, bl->bo_info_size, bl->bo_number);
            free(data(bl->bo_info_ptr, times(bl->bo_number, bl->bo_info_size)));
            flush();
        }
    } else if (nr == DRM_COMMAND_BASE + DRM_AMDGPU_WAIT_FENCES && size >= sizeof(struct drm_amdgpu_wait_fences_in)) {
        const struct drm_amdgpu_wait_fences_in *w = arg;
        array(s, "fences", w->fences, w->fence_count, sizeof(struct drm_amdgpu_fence));
    } else if ((nr == 0xc3) && size >= 24) {
        const struct drm_syncobj_wait *w = arg;
        array(s, "handles", w->handles, w->count_handles, 4);
    } else if ((nr == 0xc4 || nr == 0xc5) && size >= 12) {
        const struct drm_syncobj_array *a = arg;
        array(s, "handles", a->handles, a->count_handles, 4);
    } else if (nr == 0xca && size >= 32) {
        const struct drm_syncobj_timeline_wait *w = arg;
        array(s, "handles", w->handles, w->count_handles, 4);
        array(s, "points", w->points, w->count_handles, 8);
    } else if ((nr == 0xcb || nr == 0xcd) && size >= 20) {
        const struct drm_syncobj_timeline_array *a = arg;
        array(s, "handles", a->handles, a->count_handles, 4);
        if (nr == 0xcd) array(s, "points", a->points, a->count_handles, 8);
    }
}

// The payloads a successful call wrote, from the tracer's copy `arg` of its argument after the call.
static void after(unsigned nr, const void *arg, size_t size, unsigned long s, int ret) {
    if (ret != 0) return;
    if (nr == DRM_COMMAND_BASE + DRM_AMDGPU_INFO && size >= 16) {
        const struct drm_amdgpu_info *in = arg;
        if (in->return_pointer && in->return_size) {
            put("P %lu info query=0x%x ", s, in->query);
            free(data(in->return_pointer, in->return_size));
            flush();
        }
    } else if (nr == 0xcb && size >= 20) {
        const struct drm_syncobj_timeline_array *a = arg;
        array(s, "points", a->points, a->count_handles, 8);
    } else if (nr == DRM_COMMAND_BASE + DRM_AMDGPU_GEM_OP && size >= 16) {
        const struct drm_amdgpu_gem_op *op = arg;
        if (op->op == AMDGPU_GEM_OP_GET_GEM_CREATE_INFO && op->value) array(s, "value", op->value, 1, sizeof(struct drm_amdgpu_gem_create_in));
    }
}

// After a successful call (from the copy of its argument): what it says about the program's buffers. `mmap_handle`
// is GEM_MMAP's handle from before the call (the union's answer overwrites it).
static void learn(int fd, unsigned nr, const void *arg, size_t size, uint32_t mmap_handle) {
    if (nr == DRM_COMMAND_BASE + DRM_AMDGPU_GEM_MMAP && size >= 8) {
        const union drm_amdgpu_gem_mmap *m = arg;
        struct bo *q = grow(bos, &capbos, nbos, sizeof *bos);
        if (!q) return;
        bos = q;
        bos[nbos++] = (struct bo){ .fd = fd, .handle = mmap_handle, .off = m->out.addr_ptr };
    } else if (nr == DRM_COMMAND_BASE + DRM_AMDGPU_GEM_VA && size >= 40) {
        const struct drm_amdgpu_gem_va *v = arg;
        if (v->operation == AMDGPU_VA_OP_MAP || v->operation == AMDGPU_VA_OP_REPLACE) {
            struct va *q = grow(vas, &capvas, nvas, sizeof *vas);
            if (!q) return;
            vas = q;
            vas[nvas++] = (struct va){ .fd = fd, .handle = v->handle, .va = VA(v->va_address), .bo_off = v->offset_in_bo, .size = v->map_size };
        } else if (v->operation == AMDGPU_VA_OP_UNMAP) {
            for (size_t i = 0; i < nvas; i++)
                if (vas[i].fd == fd && vas[i].va == VA(v->va_address)) { vas[i] = vas[--nvas]; break; }
        }
    } else if (nr == 0x09 && size >= 4) {
        const struct drm_gem_close *g = arg;
        // A closed handle's number comes back for the next buffer: forget everything learnt about it.
        for (size_t i = 0; i < nbos; i++)
            if (bos[i].fd == fd && bos[i].handle == g->handle) { free(bos[i].copy); bos[i] = bos[--nbos]; i--; }
        for (size_t i = 0; i < nvas; i++)
            if (vas[i].fd == fd && vas[i].handle == g->handle) { vas[i] = vas[--nvas]; i--; }
    }
}

// A call whose argument the tracer could not read (a bad pointer, which the kernel answers EFAULT): no payloads,
// and drmreplay skips it.
static void unreadable_call(unsigned long s, int fd, unsigned nr, unsigned long req, size_t size, int ret, int e) {
    put("U %lu fd=%d nr=0x%x req=0x%lx size=%zu ret=%d errno=%d", s, fd, nr, req, size, ret, e);
    flush();
}

int ioctl(int fd, unsigned long req, ...) {
    va_list ap;
    va_start(ap, req);
    void *arg = va_arg(ap, void *);
    va_end(ap);
    if (!real_ioctl) real_ioctl = dlsym(RTLD_NEXT, "ioctl");
    if (out_fd < 0 || stopped || _IOC_TYPE(req) != DRM_IOCTL_BASE || !arg) return real_ioctl(fd, req, arg);
    unsigned nr = _IOC_NR(req);
    size_t size = _IOC_SIZE(req);
    pthread_mutex_lock(&mu);
    if (!drm_fd(fd)) {
        pthread_mutex_unlock(&mu);
        return real_ioctl(fd, req, arg);
    }
    unsigned long s = ++seq;
    unsigned char *in = snap((uint64_t)(uintptr_t)arg, size);
    uint32_t mh = 0;
    if (in && nr == DRM_COMMAND_BASE + DRM_AMDGPU_GEM_MMAP && size >= 4) memcpy(&mh, in, 4);
    if (in) before(fd, nr, in, size, s);
    pthread_mutex_unlock(&mu);
    // The call itself runs without the lock: a wait in one thread must never hold up another thread's calls.
    int ret = real_ioctl(fd, req, arg);
    int e = ret < 0 ? errno : 0;
    pthread_mutex_lock(&mu);
    unsigned char *out = in ? snap((uint64_t)(uintptr_t)arg, size) : NULL;
    if (!out) unreadable_call(s, fd, nr, req, size, ret, e);
    else {
        if (ret == 0) learn(fd, nr, out, size, mh);
        put("I %lu fd=%d nr=0x%x req=0x%lx size=%zu ret=%d errno=%d in=", s, fd, nr, req, size, ret, e);
        hex(in, size);
        put(" out=");
        hex(out, size);
        flush();
        after(nr, out, size, s, ret);
    }
    // out_fd stays open after the end: closing it could hand its number to the program while a call is in flight.
    if (nr == DRM_COMMAND_BASE + DRM_AMDGPU_CS && cs_max && ++cs_seen >= cs_max && !stopped) {
        stopped = 1;
        put("E cs=%lu", cs_seen);
        flush();
    }
    pthread_mutex_unlock(&mu);
    free(in);
    free(out);
    errno = e;
    return ret;
}

void *mmap(void *addr, size_t n, int prot, int flags, int fd, off_t off) {
    if (!real_mmap) real_mmap = dlsym(RTLD_NEXT, "mmap");
    void *p = real_mmap(addr, n, prot, flags, fd, off);
    if (out_fd < 0 || stopped || p == MAP_FAILED || fd < 0) return p;
    pthread_mutex_lock(&mu);
    if (known_fd(fd))
        for (size_t i = 0; i < nbos; i++)
            if (bos[i].fd == fd && bos[i].off == (uint64_t)off && !bos[i].cpu) {
                bos[i].cpu = p;
                bos[i].skip = 0;
                bos[i].len = n;
                break;
            }
    pthread_mutex_unlock(&mu);
    return p;
}
void *mmap64(void *addr, size_t n, int prot, int flags, int fd, off_t off) __attribute__((alias("mmap")));

// Keeps what the program's mapping of b holds now in b's copy, at the buffer's own offsets (an earlier copy of parts
// no longer mapped stays).
static void keep_copy(struct bo *b) {
    size_t end = b->skip + b->len;
    if (end > b->copylen) {
        char *q = realloc(b->copy, end);
        if (!q) return;
        memset(q + b->copylen, 0, end - b->copylen);
        b->copy = q;
        b->copylen = end;
    }
    if (rd(b->copy + b->skip, (uint64_t)(uintptr_t)b->cpu, b->len))
        memset(b->copy + b->skip, 0, b->len);
}

// The part [a, z) of the program's address space goes away (munmap, or the old place of a moved mremap).
static void unmapped(char *a, char *z) {
    for (size_t i = 0; i < nbos; i++) {
        struct bo *b = &bos[i];
        if (!b->cpu) continue;
        char *lo = b->cpu, *hi = b->cpu + b->len;
        if (z <= lo || a >= hi) continue;
        keep_copy(b);
        if (a <= lo && z >= hi) { b->cpu = NULL; b->len = 0; }
        else if (a <= lo) { size_t cut = (size_t)(z - lo); b->cpu += cut; b->skip += cut; b->len -= cut; }
        else b->len = (size_t)(a - lo);  // the tail, or a hole: the front stays mapped (a part after a hole only in the copy)
    }
}

int munmap(void *addr, size_t n) {
    if (!real_munmap) real_munmap = dlsym(RTLD_NEXT, "munmap");
    if (out_fd >= 0 && !stopped) {
        pthread_mutex_lock(&mu);
        long page = sysconf(_SC_PAGESIZE);
        size_t whole = page > 0 ? (n + (size_t)page - 1) / (size_t)page * (size_t)page : n;
        if (whole >= n) unmapped(addr, (char *)addr + whole);
        pthread_mutex_unlock(&mu);
    }
    return real_munmap(addr, n);
}

void *mremap(void *old, size_t old_size, size_t new_size, int flags, ...) {
    void *want = NULL;
    if (flags & MREMAP_FIXED) {
        va_list ap;
        va_start(ap, flags);
        want = va_arg(ap, void *);
        va_end(ap);
    }
    if (!real_mremap) real_mremap = dlsym(RTLD_NEXT, "mremap");
    if (out_fd < 0 || stopped) return real_mremap(old, old_size, new_size, flags, want);
    pthread_mutex_lock(&mu);
    // The whole mapping moving or growing follows it; anything else counts as an unmap of the old range.
    struct bo *whole = NULL;
    for (size_t i = 0; i < nbos; i++)
        if (bos[i].cpu == old && bos[i].len == old_size) whole = &bos[i];
    if (!whole) unmapped(old, (char *)old + old_size);
    else keep_copy(whole);
    void *p = real_mremap(old, old_size, new_size, flags, want);
    int e = errno;
    if (whole && p != MAP_FAILED) {
        whole->cpu = p;
        whole->len = new_size < old_size ? new_size : old_size;
    } else if (whole) { whole->cpu = NULL; whole->len = 0; }
    pthread_mutex_unlock(&mu);
    errno = e;
    return p;
}

int close(int fd) {
    if (!real_close) real_close = dlsym(RTLD_NEXT, "close");
    if (out_fd >= 0 && !stopped && fd != out_fd) {
        pthread_mutex_lock(&mu);
        if (known_fd(fd)) {
            for (size_t i = 0; i < nfds; i++) if (fds[i] == fd) { fds[i] = fds[--nfds]; break; }
            for (size_t i = 0; i < nbos; i++) if (bos[i].fd == fd) { free(bos[i].copy); bos[i] = bos[--nbos]; i--; }
            for (size_t i = 0; i < nvas; i++) if (vas[i].fd == fd) { vas[i] = vas[--nvas]; i--; }
            put("C fd=%d", fd);
            flush();
        }
        pthread_mutex_unlock(&mu);
    }
    return real_close(fd);
}
