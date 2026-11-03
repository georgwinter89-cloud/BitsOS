// GM-1c: plays a drmtrace.c trace (drmtrace.md) against a render node again and compares every answer with the
// trace's. The handles of the trace (GEM objects, contexts, syncobjs, BO lists, CS sequence numbers, exported fds)
// are mapped to the ones this run gets; GPU addresses stay as traced (the GPU VM is per fd and starts empty); each
// IB's dwords are written into this run's own buffer at the traced address before its CS.
//   drmreplay [--node PATH] [--until-cs N] [--quiet] TRACE
// Output: a line per answer that differs or per call that cannot be replayed (an fd the program got from elsewhere,
// USERPTR memory), then "replay: <n> ioctls, <same> same, <differ> differ, <skipped> skipped, <cs> CS, <bad> damaged".
// A trace line that holds fewer bytes than its call reads (cut or edited by hand, or bad hex) is "damaged": the call
// is not made (the kernel would read past the replay's buffer) and counts like a difference.
// Exit 0: nothing differs; 1: something differs or is damaged; 2: the trace or the node could not be used.
// Never point it at a GPU that drives a display someone uses: a replayed CS runs real GPU work.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>
#include <drm.h>
#include <amdgpu_drm.h>

enum kind { GEM, CTX, SYNCOBJ, BOLIST, SEQ, FD };
struct map { int kind, fd; uint64_t aux, from, to; };
static struct map *maps;
static size_t nmaps, capmaps;
struct fdpair { int traced, own; };
static struct fdpair *fds;
static size_t nfds, capfds;
struct own_bo { int fd; uint32_t handle; uint64_t size; void *cpu; };
static struct own_bo *bos;
static size_t nbos, capbos;
struct own_va { int fd; uint32_t handle; uint64_t va, off, size; };
static struct own_va *vas;
static size_t nvas, capvas;

struct payload { char *kind; int index; char *line; };
struct rec { unsigned long seq; char *line; struct payload *p; size_t np, capp; };
static struct rec *recs;    // by seq (index = seq)
static size_t nrecs;
static char **order;        // the I, U, C and E lines in the file's order
static size_t norder, caporder;

static const char *node = "/dev/dri/renderD128";
static int quiet;
static unsigned long n_calls, n_same, n_differ, n_skipped, n_cs, n_bad;
static int bad_hex;  // bytes() met a field that is no even run of hex digits since the last reset

static void *grow(void *p, size_t *cap, size_t n, size_t each) {
    if (n < *cap) return p;
    size_t c = *cap ? *cap * 2 : 64;
    void *q = realloc(p, c * each);
    if (!q) { perror("drmreplay: realloc"); exit(2); }
    *cap = c;
    return q;
}

static void put_map(int kind, int fd, uint64_t aux, uint64_t from, uint64_t to) {
    for (size_t i = 0; i < nmaps; i++)
        if (maps[i].kind == kind && maps[i].fd == fd && maps[i].aux == aux && maps[i].from == from) { maps[i].to = to; return; }
    maps = grow(maps, &capmaps, nmaps, sizeof *maps);
    maps[nmaps++] = (struct map){ kind, fd, aux, from, to };
}
// 0 = unknown (also the "none" value of every handle kind), otherwise the mapped value.
static int get_map(int kind, int fd, uint64_t aux, uint64_t from, uint64_t *to) {
    if (!from) { *to = 0; return 1; }
    for (size_t i = 0; i < nmaps; i++)
        if (maps[i].kind == kind && maps[i].fd == fd && maps[i].aux == aux && maps[i].from == from) { *to = maps[i].to; return 1; }
    return 0;
}
static void drop_map(int kind, int fd, uint64_t from) {
    for (size_t i = 0; i < nmaps; i++)
        if (maps[i].kind == kind && maps[i].fd == fd && maps[i].from == from) { maps[i] = maps[--nmaps]; i--; }
}

static int own_fd(int traced) {
    for (size_t i = 0; i < nfds; i++) if (fds[i].traced == traced) return fds[i].own;
    int fd = open(node, O_RDWR | O_CLOEXEC);
    if (fd < 0) { fprintf(stderr, "drmreplay: %s: %s\n", node, strerror(errno)); exit(2); }
    fds = grow(fds, &capfds, nfds, sizeof *fds);
    fds[nfds++] = (struct fdpair){ traced, fd };
    return fd;
}
static void close_fd(int traced) {
    for (size_t i = 0; i < nfds; i++)
        if (fds[i].traced == traced) {
            close(fds[i].own);
            for (size_t j = 0; j < nbos; j++) if (bos[j].fd == traced) { if (bos[j].cpu) munmap(bos[j].cpu, bos[j].size); bos[j] = bos[--nbos]; j--; }
            for (size_t j = 0; j < nvas; j++) if (vas[j].fd == traced) { vas[j] = vas[--nvas]; j--; }
            for (size_t j = 0; j < nmaps; j++) if (maps[j].fd == traced) { maps[j] = maps[--nmaps]; j--; }
            fds[i] = fds[--nfds];
            return;
        }
}

// "key=value" of a record line; NULL if absent. The value runs to the next space.
static const char *field(const char *line, const char *key, size_t *len) {
    size_t k = strlen(key);
    for (const char *p = line; (p = strstr(p, key)); p++) {
        if ((p == line || p[-1] == ' ') && p[k] == '=') {
            const char *v = p + k + 1;
            *len = strcspn(v, " \n");
            return v;
        }
    }
    return NULL;
}
static uint64_t num(const char *line, const char *key, int *ok) {
    size_t l;
    const char *v = field(line, key, &l);
    if (!v) { if (ok) *ok = 0; return 0; }
    return strtoull(v, NULL, 0);
}
static int nibble(char c) {
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return c - 'a' + 10;
    if (c >= 'A' && c <= 'F') return c - 'A' + 10;
    return -1;
}
static unsigned char *bytes(const char *line, const char *key, size_t *n) {
    size_t l;
    const char *v = field(line, key, &l);
    *n = v ? l / 2 : 0;
    if (v && l % 2) bad_hex = 1;
    unsigned char *b = malloc(*n ? *n : 1);
    if (!b) { perror("drmreplay: malloc"); exit(2); }
    for (size_t i = 0; i < *n; i++) {
        int hi = nibble(v[2 * i]), lo = nibble(v[2 * i + 1]);
        if (hi < 0 || lo < 0) { bad_hex = 1; *n = i; break; }
        b[i] = (unsigned char)(hi << 4 | lo);
    }
    return b;
}
static struct payload *find(struct rec *r, const char *kind, int index) {
    for (size_t i = 0; i < r->np; i++)
        if (!strcmp(r->p[i].kind, kind) && (index < 0 || r->p[i].index == index)) return &r->p[i];
    return NULL;
}

static void load(const char *path) {
    FILE *f = fopen(path, "r");
    if (!f) { fprintf(stderr, "drmreplay: %s: %s\n", path, strerror(errno)); exit(2); }
    char *line = NULL;
    size_t cap = 0;
    while (getline(&line, &cap, f) > 0) {
        char *l = strdup(line);
        if (!l) exit(2);
        if (l[0] == 'I' || l[0] == 'P') {
            unsigned long seq = strtoul(l + 2, NULL, 10);
            if (seq >= nrecs) {
                size_t n = seq + 1024;
                recs = realloc(recs, n * sizeof *recs);
                if (!recs) exit(2);
                memset(recs + nrecs, 0, (n - nrecs) * sizeof *recs);
                nrecs = n;
            }
            struct rec *r = &recs[seq];
            r->seq = seq;
            if (l[0] == 'I') r->line = l;
            else {
                char kind[32] = "";
                int index = -1;
                sscanf(l, "P %*u %31s %d", kind, &index);
                r->p = grow(r->p, &r->capp, r->np, sizeof *r->p);
                r->p[r->np++] = (struct payload){ strdup(kind), index, l };
            }
        }
        if (l[0] == 'I' || l[0] == 'U' || l[0] == 'C' || l[0] == 'E') {
            order = grow(order, &caporder, norder, sizeof *order);
            order[norder++] = l;
        }
    }
    free(line);
    fclose(f);
}

static uint64_t now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint64_t)t.tv_sec * 1000000000ull + (uint64_t)t.tv_nsec;
}
// A traced absolute timeout lies in the past now: a wait that ended well gets 5 s from now, one that timed out none.
static uint64_t timeout(int done) { return done ? now_ns() + 5000000000ull : 0; }

static void skip(unsigned long seq, const char *what, const char *why) {
    n_skipped++;
    if (!quiet) printf("replay: seq %lu %s skipped: %s\n", seq, what, why);
}

// This run's CPU view of GPU address `va` of traced fd, mapping the buffer on first use.
static void *own_cpu(int tfd, uint64_t va, uint64_t n) {
    va &= 0xffffffffffffull;  // 48-bit GPU addresses: GEM_VA and CS chunks give the upper half sign-extended
    for (size_t i = 0; i < nvas; i++) {
        struct own_va *v = &vas[i];
        if (v->fd != tfd || va < v->va || va + n > v->va + v->size) continue;
        for (size_t j = 0; j < nbos; j++) {
            struct own_bo *b = &bos[j];
            if (b->fd != tfd || b->handle != v->handle) continue;
            if (!b->cpu) {
                union drm_amdgpu_gem_mmap m = { .in.handle = b->handle };
                if (ioctl(own_fd(tfd), DRM_IOCTL_AMDGPU_GEM_MMAP, &m)) return NULL;
                void *p = mmap(NULL, b->size, PROT_READ | PROT_WRITE, MAP_SHARED, own_fd(tfd), (off_t)m.out.addr_ptr);
                if (p == MAP_FAILED) return NULL;
                b->cpu = p;
            }
            uint64_t at = v->off + (va - v->va);
            return at + n <= b->size ? (char *)b->cpu + at : NULL;
        }
    }
    return NULL;
}

// Rewrites an array of u32 handles of `kind` in place; 0 if one is unknown.
static int map_u32s(unsigned char *b, size_t count, size_t stride, int kind, int tfd) {
    for (size_t i = 0; i < count; i++) {
        uint32_t h;
        uint64_t to;
        memcpy(&h, b + i * stride, 4);
        if (!get_map(kind, tfd, 0, h, &to)) return 0;
        h = (uint32_t)to;
        memcpy(b + i * stride, &h, 4);
    }
    return 1;
}
static int map_field32(unsigned char *a, size_t size, size_t off, int kind, int tfd, uint64_t aux) {
    if (off + 4 > size) return 1;
    uint32_t h;
    uint64_t to;
    memcpy(&h, a + off, 4);
    if (!get_map(kind, tfd, aux, h, &to)) return 0;
    h = (uint32_t)to;
    memcpy(a + off, &h, 4);
    return 1;
}
static void set64(unsigned char *a, size_t size, size_t off, uint64_t v) { if (off + 8 <= size) memcpy(a + off, &v, 8); }
static uint64_t get64(const unsigned char *a, size_t size, size_t off) { uint64_t v = 0; if (off + 8 <= size) memcpy(&v, a + off, 8); return v; }
static uint32_t get32(const unsigned char *a, size_t size, size_t off) { uint32_t v = 0; if (off + 4 <= size) memcpy(&v, a + off, 4); return v; }

// The traced payload `kind` as this run's array (handles mapped when kind_of_handles >= 0), pointed to at `off`.
// `count` entries of `stride` bytes must be in the payload (the kernel reads that many).
static const char *point_at(struct rec *r, const char *kind, unsigned char *a, size_t size, size_t off, int handle_kind,
                            size_t stride, uint32_t count, int tfd, unsigned char **keep) {
    struct payload *p = find(r, kind, -1);
    if (!p) { set64(a, size, off, 0); return NULL; }
    if (strstr(p->line, " unreadable")) return "its array was unreadable when traced";
    size_t n;
    unsigned char *b = bytes(p->line, "data", &n);
    if (n < (size_t)count * stride) { free(b); return "damaged: an array is shorter than the call's count"; }
    if (handle_kind >= 0 && !map_u32s(b, n / stride, stride, handle_kind, tfd)) { free(b); return "unknown syncobj"; }
    set64(a, size, off, (uint64_t)(uintptr_t)b);
    *keep = b;
    return NULL;
}

static const char *name_of(unsigned nr) {
    switch (nr) {
    case 0x00: return "VERSION"; case 0x09: return "GEM_CLOSE"; case 0x0c: return "GET_CAP"; case 0x0d: return "SET_CLIENT_CAP";
    case 0x2d: return "PRIME_HANDLE_TO_FD"; case 0x2e: return "PRIME_FD_TO_HANDLE"; case 0xbf: return "SYNCOBJ_CREATE";
    case 0xc0: return "SYNCOBJ_DESTROY"; case 0xc1: return "SYNCOBJ_HANDLE_TO_FD"; case 0xc2: return "SYNCOBJ_FD_TO_HANDLE";
    case 0xc3: return "SYNCOBJ_WAIT"; case 0xc4: return "SYNCOBJ_RESET"; case 0xc5: return "SYNCOBJ_SIGNAL";
    case 0xca: return "SYNCOBJ_TIMELINE_WAIT"; case 0xcb: return "SYNCOBJ_QUERY"; case 0xcc: return "SYNCOBJ_TRANSFER";
    case 0xcd: return "SYNCOBJ_TIMELINE_SIGNAL"; case 0x40: return "AMDGPU_GEM_CREATE"; case 0x41: return "AMDGPU_GEM_MMAP";
    case 0x42: return "AMDGPU_CTX"; case 0x43: return "AMDGPU_BO_LIST"; case 0x44: return "AMDGPU_CS"; case 0x45: return "AMDGPU_INFO";
    case 0x46: return "AMDGPU_GEM_METADATA"; case 0x47: return "AMDGPU_GEM_WAIT_IDLE"; case 0x48: return "AMDGPU_GEM_VA";
    case 0x49: return "AMDGPU_WAIT_CS"; case 0x50: return "AMDGPU_GEM_OP"; case 0x51: return "AMDGPU_GEM_USERPTR";
    case 0x52: return "AMDGPU_WAIT_FENCES"; case 0x53: return "AMDGPU_VM"; case 0x54: return "AMDGPU_FENCE_TO_HANDLE";
    case 0x55: return "AMDGPU_SCHED";
    default: return "other";
    }
}

// INFO queries whose answers change with time or with what was allocated before (counters, clocks, usage).
static int volatile_query(uint32_t q) {
    return q == AMDGPU_INFO_TIMESTAMP || q == AMDGPU_INFO_NUM_BYTES_MOVED || q == AMDGPU_INFO_VRAM_USAGE ||
           q == AMDGPU_INFO_GTT_USAGE || q == AMDGPU_INFO_VIS_VRAM_USAGE || q == AMDGPU_INFO_NUM_EVICTIONS ||
           q == AMDGPU_INFO_MEMORY || q == AMDGPU_INFO_SENSOR || q == AMDGPU_INFO_NUM_VRAM_CPU_PAGE_FAULTS ||
           q == AMDGPU_INFO_VRAM_LOST_COUNTER || q == AMDGPU_INFO_GPUVM_FAULT || q == AMDGPU_INFO_NUM_HANDLES;
}

// Whether a traced wait saw its fences done (else it timed out). The syncobj waits answer ETIME on a timeout; the
// amdgpu waits answer 0 and say it in out.status: GEM_WAIT_IDLE and WAIT_CS 1 = still busy
// (linux-6.6/drivers/gpu/drm/amd/amdgpu/amdgpu_cs.c:1493, amdgpu_gem.c wait_idle), WAIT_FENCES 1 = signalled
// (amdgpu_cs.c:1631, :1685).
static int wait_done(unsigned nr, int tret, const unsigned char *tout, size_t tsize) {
    if (tret != 0) return 0;
    uint32_t status = get32(tout, tsize, 0);
    if (nr == 0x47 || nr == 0x49) return status == 0;
    if (nr == 0x52) return status == 1;
    return 1;
}
// The part of a wait's answer that says how it ended, compared like a return value: out.status, and for WAIT_FENCES
// with wait_all 0 also first_signaled (with wait_all 1 Linux leaves that word as the caller passed it).
static size_t wait_answer(unsigned nr, const unsigned char *in, size_t size) {
    if (nr == 0x47 || nr == 0x49) return 4;
    if (nr == 0x52) return get32(in, size, 12) ? 4 : 8;
    return 0;
}

// Prepares this run's argument from the traced one; returns NULL (and says why) when the call cannot be replayed.
// `keep` collects the buffers the argument points to.
// Reasons that start with "damaged:" count as a damaged trace, not as a skip.
static const char *prepare(struct rec *r, unsigned nr, int tfd, int done, unsigned char *a, size_t size,
                           unsigned char **keep, size_t *nkeep) {
#define KEEP(p) (keep[(*nkeep)++] = (unsigned char *)(p))
    switch (nr) {
    case 0x00: {  // VERSION: three strings into buffers of the traced lengths
        for (size_t f = 0; f < 3; f++) {
            uint64_t l = get64(a, size, 16 + f * 16);
            unsigned char *b = calloc(1, l + 1);
            if (!b) return "out of memory";
            KEEP(b);
            set64(a, size, 24 + f * 16, l ? (uint64_t)(uintptr_t)b : 0);
        }
        return NULL;
    }
    case 0x0c: case 0x0d: case 0x53: return NULL;
    case 0x09: case 0x41: case 0x46: return map_field32(a, size, 0, GEM, tfd, 0) ? NULL : "unknown GEM handle";
    case 0x2d: return map_field32(a, size, 0, GEM, tfd, 0) ? NULL : "unknown GEM handle";
    case 0x2e: case 0xc2: {
        size_t off = 8;  // drm_prime_handle and drm_syncobj_handle both keep the fd at byte 8
        int32_t f;
        uint64_t to;
        memcpy(&f, a + off, 4);
        if (!get_map(FD, tfd, 0, (uint64_t)(uint32_t)f + 1, &to)) return "an fd the program got from elsewhere";
        f = (int32_t)to;
        memcpy(a + off, &f, 4);
        if (nr == 0xc2 && (get32(a, size, 4) & DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE))
            return map_field32(a, size, 0, SYNCOBJ, tfd, 0) ? NULL : "unknown syncobj";
        return NULL;
    }
    case 0xbf: return NULL;
    case 0xc0: case 0xc1: return map_field32(a, size, 0, SYNCOBJ, tfd, 0) ? NULL : "unknown syncobj";
    case 0xcc:
        return map_field32(a, size, 0, SYNCOBJ, tfd, 0) && map_field32(a, size, 4, SYNCOBJ, tfd, 0) ? NULL : "unknown syncobj";
    case 0xc3: case 0xc4: case 0xc5: {
        unsigned char *b = NULL;
        const char *why = point_at(r, "handles", a, size, 0, SYNCOBJ, 4, get32(a, size, nr == 0xc3 ? 16 : 8), tfd, &b);
        if (b) KEEP(b);
        if (why) return why;
        if (nr == 0xc3) set64(a, size, 8, timeout(done));
        return NULL;
    }
    case 0xca: case 0xcb: case 0xcd: {
        unsigned char *b = NULL, *p = NULL;
        uint32_t count = get32(a, size, nr == 0xca ? 24 : 16);
        const char *why = point_at(r, "handles", a, size, 0, SYNCOBJ, 4, count, tfd, &b);
        if (b) KEEP(b);
        if (why) return why;
        if (nr == 0xcb) {
            uint32_t n = count;
            p = calloc(n ? n : 1, 8);
            if (!p) return "out of memory";
            set64(a, size, 8, (uint64_t)(uintptr_t)p);
        } else if ((why = point_at(r, "points", a, size, 8, -1, 8, count, tfd, &p))) {
            if (p) KEEP(p);
            return why;
        }
        if (p) KEEP(p);
        if (nr == 0xca) set64(a, size, 16, timeout(done));
        return NULL;
    }
    case 0x40: return NULL;
    case 0x42: {
        uint32_t op = get32(a, size, 0);
        if (op == AMDGPU_CTX_OP_ALLOC_CTX) return NULL;
        return map_field32(a, size, 8, CTX, tfd, 0) ? NULL : "unknown context";
    }
    case 0x43: {
        uint32_t op = get32(a, size, 0);
        if (op != AMDGPU_BO_LIST_OP_CREATE && !map_field32(a, size, 4, BOLIST, tfd, 0)) return "unknown BO list";
        if (op == AMDGPU_BO_LIST_OP_DESTROY) return NULL;
        struct payload *p = find(r, "bolist", -1);
        size_t n;
        if (p && strstr(p->line, " unreadable")) return "its BO list was unreadable when traced";
        unsigned char *b = p ? bytes(p->line, "data", &n) : NULL;
        uint32_t each = get32(a, size, 12);
        if (b) KEEP(b);
        if (b && n < (size_t)get32(a, size, 8) * each) return "damaged: the BO list is shorter than its count";
        if (!b || !each || !map_u32s(b, n / each, each, GEM, tfd)) return "unknown GEM handle in the BO list";
        set64(a, size, 16, (uint64_t)(uintptr_t)b);
        return NULL;
    }
    case 0x44: {
        uint32_t trace_ctx = get32(a, size, 0);
        if (!map_field32(a, size, 0, CTX, tfd, 0)) return "unknown context";
        if (!map_field32(a, size, 4, BOLIST, tfd, 0)) return "unknown BO list";
        uint32_t nch = get32(a, size, 8);
        struct payload *all = find(r, "chunks", -1);
        if (all && strstr(all->line, " unreadable")) return "its chunk array was unreadable when traced";
        uint64_t *ptrs = calloc(nch ? nch : 1, 8);
        if (!ptrs) return "out of memory";
        KEEP(ptrs);
        for (uint32_t i = 0; i < nch; i++) {
            struct payload *p = find(r, "chunk", (int)i);
            if (!p) return "damaged: a chunk is missing from the trace";
            if (strstr(p->line, " unreadable")) return "a chunk was unreadable when traced";
            struct drm_amdgpu_cs_chunk *ch = calloc(1, sizeof *ch);
            if (!ch) return "out of memory";
            KEEP(ch);
            size_t n;
            unsigned char *d = bytes(p->line, "data", &n);
            KEEP(d);
            ch->chunk_id = (uint32_t)num(p->line, "id", NULL);
            ch->length_dw = (uint32_t)num(p->line, "len_dw", NULL);
            if (n < (size_t)ch->length_dw * 4) return "damaged: chunk data shorter than len_dw";
            ch->chunk_data = (uint64_t)(uintptr_t)d;
            ptrs[i] = (uint64_t)(uintptr_t)ch;
            switch (ch->chunk_id) {
            case AMDGPU_CHUNK_ID_IB: {
                struct payload *ib = find(r, "ib", (int)i);
                struct drm_amdgpu_cs_chunk_ib *in = (void *)d;
                if (n < sizeof *in) return "a short IB chunk";
                if (ib && strstr(ib->line, " unreadable")) return "the IB was unreadable when traced";
                size_t ibn;
                unsigned char *ibd = ib ? bytes(ib->line, "data", &ibn) : NULL;
                if (ibd) KEEP(ibd);
                if (ibd && ibn && ibn < in->ib_bytes) return "damaged: the IB's dwords are shorter than ib_bytes";
                if (!ibd || !ibn) { if (!quiet) printf("replay: seq %lu: IB %u was not CPU-mapped when traced: it runs as this run's memory holds it\n", r->seq, i); break; }
                void *dst = own_cpu(tfd, in->va_start, ibn);
                if (!dst) return "the IB's address is not in a buffer this run can map";
                memcpy(dst, ibd, ibn);
                break;
            }
            case AMDGPU_CHUNK_ID_FENCE:
                if (!map_field32(d, n, 0, GEM, tfd, 0)) return "unknown GEM handle in the fence chunk";
                break;
            case AMDGPU_CHUNK_ID_DEPENDENCIES: case AMDGPU_CHUNK_ID_SCHEDULED_DEPENDENCIES:
                for (size_t o = 0; o + sizeof(struct drm_amdgpu_cs_chunk_dep) <= n; o += sizeof(struct drm_amdgpu_cs_chunk_dep)) {
                    uint32_t dctx = get32(d, n, o + 12);
                    uint64_t seq = get64(d, n, o + 16), to;
                    if (!map_field32(d, n, o + 12, CTX, tfd, 0)) return "unknown context in a dependency";
                    if (!get_map(SEQ, tfd, dctx, seq, &to)) return "a dependency on a CS this run did not make";
                    set64(d, n, o + 16, to);
                }
                break;
            case AMDGPU_CHUNK_ID_SYNCOBJ_IN: case AMDGPU_CHUNK_ID_SYNCOBJ_OUT:
                if (!map_u32s(d, n / 4, 4, SYNCOBJ, tfd)) return "unknown syncobj in a chunk";
                break;
            case AMDGPU_CHUNK_ID_SYNCOBJ_TIMELINE_WAIT: case AMDGPU_CHUNK_ID_SYNCOBJ_TIMELINE_SIGNAL:
                if (!map_u32s(d, n / 16, 16, SYNCOBJ, tfd)) return "unknown syncobj in a chunk";
                break;
            case AMDGPU_CHUNK_ID_BO_HANDLES: {
                struct payload *bl = find(r, "bolist", (int)i);
                size_t bn;
                if (bl && strstr(bl->line, " unreadable")) return "the BO list was unreadable when traced";
                unsigned char *b = bl ? bytes(bl->line, "data", &bn) : NULL;
                uint32_t each = get32(d, n, 12);
                if (b) KEEP(b);
                if (b && bn < (size_t)get32(d, n, 8) * each) return "damaged: the BO list is shorter than its count";
                if (!b || !each || !map_u32s(b, bn / each, each, GEM, tfd)) return "unknown GEM handle in the BO list";
                set64(d, n, 16, (uint64_t)(uintptr_t)b);
                break;
            }
            default: break;
            }
        }
        // The IBs the chunks' IBs call or chain to (drmtrace's "subib" payloads) go to their addresses too.
        for (size_t k = 0; k < r->np; k++) {
            if (strcmp(r->p[k].kind, "subib")) continue;
            size_t sn;
            unsigned char *sd = bytes(r->p[k].line, "data", &sn);
            KEEP(sd);
            if (!sn) continue;
            if (sn < num(r->p[k].line, "bytes", NULL)) return "damaged: a called IB is shorter than its size";
            void *dst = own_cpu(tfd, num(r->p[k].line, "va", NULL), sn);
            if (!dst) return "a called IB's address is not in a buffer this run can map";
            memcpy(dst, sd, sn);
        }
        set64(a, size, 16, (uint64_t)(uintptr_t)ptrs);
        (void)trace_ctx;
        return NULL;
    }
    case 0x45: {
        uint32_t n = get32(a, size, 8);
        unsigned char *b = calloc(1, n ? n : 1);
        if (!b) return "out of memory";
        KEEP(b);
        set64(a, size, 0, (uint64_t)(uintptr_t)b);
        return NULL;
    }
    case 0x47:
        set64(a, size, 8, timeout(done));
        return map_field32(a, size, 0, GEM, tfd, 0) ? NULL : "unknown GEM handle";
    case 0x48:
        if (!map_field32(a, size, 0, GEM, tfd, 0)) return "unknown GEM handle";
        if (size >= 56 && get32(a, size, 52)) return "GEM_VA with input fences (not traced)";
        if (size >= 52 && !map_field32(a, size, 48, SYNCOBJ, tfd, 0)) return "unknown syncobj";
        return NULL;
    case 0x49: {
        uint32_t tctx = get32(a, size, 28);
        uint64_t seq = get64(a, size, 0), to;
        if (!map_field32(a, size, 28, CTX, tfd, 0)) return "unknown context";
        if (seq != ~0ull && !get_map(SEQ, tfd, tctx, seq, &to)) return "a CS this run did not make";
        if (seq != ~0ull) set64(a, size, 0, to);
        set64(a, size, 8, timeout(done));
        return NULL;
    }
    case 0x50: {
        if (!map_field32(a, size, 0, GEM, tfd, 0)) return "unknown GEM handle";
        if (get32(a, size, 4) == AMDGPU_GEM_OP_GET_GEM_CREATE_INFO) {
            unsigned char *b = calloc(1, sizeof(struct drm_amdgpu_gem_create_in));
            if (!b) return "out of memory";
            KEEP(b);
            set64(a, size, 8, (uint64_t)(uintptr_t)b);
        }
        return NULL;
    }
    case 0x52: {
        struct payload *p = find(r, "fences", -1);
        size_t n;
        if (p && strstr(p->line, " unreadable")) return "its fences were unreadable when traced";
        unsigned char *b = p ? bytes(p->line, "data", &n) : NULL;
        if (!b) return "damaged: no fences in the trace";
        KEEP(b);
        if (n < (size_t)get32(a, size, 8) * sizeof(struct drm_amdgpu_fence)) return "damaged: fewer fences than fence_count";
        for (size_t o = 0; o + sizeof(struct drm_amdgpu_fence) <= n; o += sizeof(struct drm_amdgpu_fence)) {
            uint32_t fctx = get32(b, n, o);
            uint64_t seq = get64(b, n, o + 16), to;
            if (!map_field32(b, n, o, CTX, tfd, 0)) return "unknown context in a fence";
            if (!get_map(SEQ, tfd, fctx, seq, &to)) return "a fence of a CS this run did not make";
            set64(b, n, o + 16, to);
        }
        set64(a, size, 0, (uint64_t)(uintptr_t)b);
        set64(a, size, 16, timeout(done));
        return NULL;
    }
    case 0x54: {
        uint32_t fctx = get32(a, size, 0);
        uint64_t seq = get64(a, size, 16), to;
        if (!map_field32(a, size, 0, CTX, tfd, 0)) return "unknown context";
        if (!get_map(SEQ, tfd, fctx, seq, &to)) return "a CS this run did not make";
        set64(a, size, 16, to);
        return NULL;
    }
    case 0x51: return "USERPTR needs the program's own memory";
    default: return "not replayed (no handle rules for it)";
    }
#undef KEEP
}

// Learns this run's handles from a call that went as traced.
static void learn(unsigned nr, int tfd, const unsigned char *in, const unsigned char *tout, size_t tsize,
                  const unsigned char *a, size_t size) {
    switch (nr) {
    case 0x40: {
        uint32_t own = get32(a, size, 0);
        put_map(GEM, tfd, 0, get32(tout, tsize, 0), own);
        bos = grow(bos, &capbos, nbos, sizeof *bos);
        bos[nbos++] = (struct own_bo){ tfd, own, get64(in, tsize, 0), NULL };
        break;
    }
    case 0x2e: put_map(GEM, tfd, 0, get32(tout, tsize, 0), get32(a, size, 0)); break;
    case 0x54:
        if (get32(in, tsize, 24) == AMDGPU_FENCE_TO_HANDLE_GET_SYNCOBJ) put_map(SYNCOBJ, tfd, 0, get32(tout, tsize, 0), get32(a, size, 0));
        else put_map(FD, tfd, 0, (uint64_t)get32(tout, tsize, 0) + 1, get32(a, size, 0));
        break;
    case 0x2d: case 0xc1:
        put_map(FD, tfd, 0, (uint64_t)get32(tout, tsize, 8) + 1, get32(a, size, 8));
        break;
    case 0xbf: put_map(SYNCOBJ, tfd, 0, get32(tout, tsize, 0), get32(a, size, 0)); break;
    case 0xc2:
        if (!(get32(in, tsize, 4) & DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE))
            put_map(SYNCOBJ, tfd, 0, get32(tout, tsize, 0), get32(a, size, 0));
        break;
    case 0xc0: drop_map(SYNCOBJ, tfd, get32(in, tsize, 0)); break;
    case 0x09: {
        uint32_t th = get32(in, tsize, 0);
        uint64_t own;
        if (get_map(GEM, tfd, 0, th, &own)) {
            for (size_t i = 0; i < nbos; i++)
                if (bos[i].fd == tfd && bos[i].handle == own) { if (bos[i].cpu) munmap(bos[i].cpu, bos[i].size); bos[i] = bos[--nbos]; i--; }
            for (size_t i = 0; i < nvas; i++) if (vas[i].fd == tfd && vas[i].handle == own) { vas[i] = vas[--nvas]; i--; }
        }
        drop_map(GEM, tfd, th);
        break;
    }
    case 0x42:
        if (get32(in, tsize, 0) == AMDGPU_CTX_OP_ALLOC_CTX) put_map(CTX, tfd, 0, get32(tout, tsize, 0), get32(a, size, 0));
        else if (get32(in, tsize, 0) == AMDGPU_CTX_OP_FREE_CTX) drop_map(CTX, tfd, get32(in, tsize, 8));
        break;
    case 0x43:
        if (get32(in, tsize, 0) == AMDGPU_BO_LIST_OP_CREATE) put_map(BOLIST, tfd, 0, get32(tout, tsize, 0), get32(a, size, 0));
        else if (get32(in, tsize, 0) == AMDGPU_BO_LIST_OP_DESTROY) drop_map(BOLIST, tfd, get32(in, tsize, 4));
        break;
    case 0x44: put_map(SEQ, tfd, get32(in, tsize, 0), get64(tout, tsize, 0), get64(a, size, 0)); break;
    case 0x48: {
        uint32_t op = get32(in, tsize, 8);
        uint64_t va = get64(in, tsize, 16);
        if (op == AMDGPU_VA_OP_MAP || op == AMDGPU_VA_OP_REPLACE) {
            vas = grow(vas, &capvas, nvas, sizeof *vas);
            vas[nvas++] = (struct own_va){ tfd, get32(a, size, 0), va & 0xffffffffffffull, get64(in, tsize, 24), get64(in, tsize, 32) };
        } else if (op == AMDGPU_VA_OP_UNMAP) {
            for (size_t i = 0; i < nvas; i++) if (vas[i].fd == tfd && vas[i].va == (va & 0xffffffffffffull)) { vas[i] = vas[--nvas]; break; }
        }
        break;
    }
    default: break;
    }
}

static void replay(struct rec *r) {
    const char *l = r->line;
    int tfd = (int)num(l, "fd", NULL), tret = (int)(int64_t)num(l, "ret", NULL), terr = (int)num(l, "errno", NULL);
    unsigned nr = (unsigned)num(l, "nr", NULL);
    int ok = 1;
    unsigned long req = (unsigned long)num(l, "req", &ok);
    size_t size, tsize;
    bad_hex = 0;
    unsigned char *a = bytes(l, "in", &size), *tout = bytes(l, "out", &tsize);
    unsigned char *in = malloc(size ? size : 1);
    if (!in) exit(2);
    memcpy(in, a, size);
    n_calls++;
    // The buffers the argument points to: at most 3 for most calls; a CS keeps its pointer array, per chunk its
    // struct, data and IB or BO list, and every called IB.
    size_t want = 8;
    if (nr == 0x44) {
        want += 3 * (size_t)get32(a, size, 8);
        for (size_t k = 0; k < r->np; k++) want += !strcmp(r->p[k].kind, "subib");
    }
    unsigned char **keep = calloc(want, sizeof *keep);
    if (!keep) exit(2);
    size_t nkeep = 0;
    const char *why = ok ? NULL : "the trace has no request number (an older drmtrace)";
    // The kernel reads _IOC_SIZE(req) bytes of the argument: a line with fewer must not reach it.
    if (!why && (bad_hex || size != _IOC_SIZE(req) || tsize != size)) why = "damaged: in= or out= is not the request's size";
    // A pointer of the program that the kernel refused (EFAULT) cannot be made again: the replay's buffers are good.
    if (!why && terr == EFAULT) why = "the program passed a bad pointer (EFAULT when traced)";
    if (!why) why = prepare(r, nr, tfd, wait_done(nr, tret, tout, tsize), a, size, keep, &nkeep);
    if (!why && bad_hex) why = "damaged: a payload is not hex";
    if (why && !strncmp(why, "damaged:", 8)) {
        n_bad++;
        printf("replay: seq %lu %s not made: %s\n", r->seq, name_of(nr), why);
    } else if (why) {
        skip(r->seq, name_of(nr), why);
    } else {
        int ret = ioctl(own_fd(tfd), req, a);
        int e = ret < 0 ? errno : 0;
        if (nr == 0x44 && ret == 0) n_cs++;
        int same = ret == tret && e == terr;
        size_t wa = wait_answer(nr, in, size);
        if (same && ret == 0 && wa && memcmp(a, tout, wa)) {
            same = 0;
            if (!quiet) printf("replay: seq %lu %s: traced status %u, now %u\n", r->seq, name_of(nr), get32(tout, tsize, 0), get32(a, size, 0));
        }
        if (same && ret == 0) learn(nr, tfd, in, tout, tsize, a, size);
        if (same && ret == 0 && nr == 0x45) {
            uint32_t q = get32(in, size, 12);
            struct payload *p = find(r, "info", -1);
            size_t n;
            unsigned char *want = p ? bytes(p->line, "data", &n) : NULL;
            const unsigned char *got = (const unsigned char *)(uintptr_t)get64(a, size, 0);
            if (want && !volatile_query(q) && (n != get32(in, size, 8) || memcmp(want, got, n))) {
                same = 0;
                if (!quiet) printf("replay: seq %lu AMDGPU_INFO query 0x%x: the answer differs from the trace's\n", r->seq, q);
            }
            free(want);
        }
        if (same && ret == 0 && nr == 0x0c && memcmp(a, tout, size < tsize ? size : tsize)) {
            same = 0;
            if (!quiet) printf("replay: seq %lu GET_CAP 0x%x: the value differs\n", r->seq, get32(in, size, 0));
        }
        if (same) n_same++;
        else {
            n_differ++;
            if (!quiet && !(ret == tret && e == terr) ) printf("replay: seq %lu %s: traced ret=%d errno=%d, now ret=%d errno=%d\n", r->seq, name_of(nr), tret, terr, ret, e);
        }
    }
    for (size_t i = 0; i < nkeep; i++) free(keep[i]);
    free(keep);
    free(a);
    free(tout);
    free(in);
}

int main(int argc, char **argv) {
    unsigned long until_cs = 0;
    const char *trace = NULL;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--node") && i + 1 < argc) node = argv[++i];
        else if (!strcmp(argv[i], "--until-cs") && i + 1 < argc) until_cs = strtoul(argv[++i], NULL, 10);
        else if (!strcmp(argv[i], "--quiet")) quiet = 1;
        else if (argv[i][0] != '-' && !trace) trace = argv[i];
        else { fprintf(stderr, "usage: drmreplay [--node PATH] [--until-cs N] [--quiet] TRACE\n"); return 2; }
    }
    if (!trace) { fprintf(stderr, "usage: drmreplay [--node PATH] [--until-cs N] [--quiet] TRACE\n"); return 2; }
    load(trace);
    for (size_t i = 0; i < norder; i++) {
        const char *l = order[i];
        if (l[0] == 'C') { close_fd((int)num(l, "fd", NULL)); continue; }
        if (l[0] == 'U') {
            n_calls++;
            skip(strtoul(l + 2, NULL, 10), name_of((unsigned)num(l, "nr", NULL)), "its argument was unreadable when traced");
            continue;
        }
        if (l[0] == 'E') break;
        replay(&recs[strtoul(l + 2, NULL, 10)]);
        if (until_cs && n_cs >= until_cs) break;
    }
    printf("replay: %lu ioctls, %lu same, %lu differ, %lu skipped, %lu CS, %lu damaged\n", n_calls, n_same, n_differ, n_skipped,
           n_cs, n_bad);
    return n_differ || n_bad ? 1 : 0;
}
