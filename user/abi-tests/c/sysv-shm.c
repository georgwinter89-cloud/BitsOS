/* SX-IPC: shared memory against linux-7.3.1/ipc/shm.c: shmget's rules, shmat's flags and addresses, nattch through
 * fork, split, exec and RMID, /proc/self/maps. Modes: basic, attach, life, exec, limits, plain. */
#include "sysv.h"
#include <setjmp.h>

static long shmat_(int id, void *a, int fl) { return syscall(SYS_shmat, id, a, fl); }
static const char *seen(long r) { return r == -1 ? sysv_err(errno) : "address"; }
#define AT(label, id, a, fl) (printf("%s = %s\n", label, seen(r = shmat_(id, a, fl))), fflush(stdout), r)

static struct shm64 st;
static long stat_(int id) { return syscall(SYS_shmctl, id, IPC_STAT, &st); }
static void nattch(const char *label, int id) {
    if (stat_(id))
        printf("%s: stat %s\n", label, sysv_err(errno));
    else
        printf("%s: nattch %lu mode %o key %x\n", label, st.nattch, st.p.mode, st.p.key);
    fflush(stdout);
}

static void basic(void) {
    fresh_ns();
    no_caps();
    S("shmget no key", SYS_shmget, K, 4096, 0600);
    S("shmget size 0 create", SYS_shmget, K, 0, IPC_CREAT | 0600);
    S("shmget private size 0", SYS_shmget, IPC_PRIVATE, 0, 0600);
    S("shmget size past SHMMAX", SYS_shmget, K, -1L, IPC_CREAT | 0600);
    int id = S("shmget create", SYS_shmget, K, 10000, IPC_CREAT | 0640);
    S("shmget again", SYS_shmget, K, 10000, 0);
    S("shmget smaller", SYS_shmget, K, 1, 0);
    S("shmget size 0 finds", SYS_shmget, K, 0, 0);
    S("shmget larger", SYS_shmget, K, 10001, 0);
    S("shmget excl", SYS_shmget, K, 1, IPC_CREAT | IPC_EXCL | 0640);
    S("second id", SYS_shmget, IPC_PRIVATE, 1, 0600);

    memset(&st, 0x55, sizeof st);
    S("IPC_STAT", SYS_shmctl, id, IPC_STAT, &st);
    printf("stat key %x mode %o size %lu uid %u gid %u cuid %u cgid %u seq %u atime0 %d dtime0 %d ctime %d cpid me %d lpid %d nattch %lu\n", st.p.key, st.p.mode, st.segsz, st.p.uid, st.p.gid, st.p.cuid, st.p.cgid, st.p.seq, st.atime == 0, st.dtime == 0, st.ctime > 0, st.cpid == getpid(), st.lpid, st.nattch);
    S("IPC_STAT fault", SYS_shmctl, id, IPC_STAT, KERNEL_ADDR);
    S("IPC_STAT stale", SYS_shmctl, id + (1 << 15), IPC_STAT, &st);
    S("negative id", SYS_shmctl, -1, IPC_STAT, &st);
    S("negative cmd", SYS_shmctl, id, -1, &st);
    S("bad cmd", SYS_shmctl, id, 77, &st);
    unsigned long inf[9];
    long max = S("IPC_INFO", SYS_shmctl, 0, IPC_INFO, inf);
    printf("info max %lu min %lu mni %lu seg %lu all %lu (idx %ld)\n", inf[0], inf[1], inf[2], inf[3], inf[4], max);
    S("IPC_INFO fault", SYS_shmctl, 0, IPC_INFO, KERNEL_ADDR);
    struct { int used; unsigned long tot, rss, swp, att, suc; } si;
    S("SHM_INFO", SYS_shmctl, 0, SHM_INFO_, &si);
    printf("shm_info used %d tot %lu rss %lu swp %lu\n", si.used, si.tot, si.rss, si.swp);
    S("SHM_STAT index 0", SYS_shmctl, 0, SHM_STAT_, &st);
    S("SHM_STAT free index", SYS_shmctl, 5, SHM_STAT_, &st);
    S("SHM_STAT_ANY index 1", SYS_shmctl, 1, SHM_STAT_ANY_, &st);

    stat_(id);
    st.p.mode = 0444;
    S("IPC_SET", SYS_shmctl, id, IPC_SET, &st);
    S("IPC_SET fault", SYS_shmctl, id, IPC_SET, KERNEL_ADDR);
    long r;
    AT("attach rw on 0444", id, 0, 0);
    r = AT("attach rdonly on 0444", id, 0, SHM_RDONLY_);
    S("shmdt", SYS_shmdt, r);
    stat_(id);
    st.p.mode = 0;
    S("IPC_SET 0", SYS_shmctl, id, IPC_SET, &st);
    S("stat no rights", SYS_shmctl, id, IPC_STAT, &st);
    S("SHM_STAT no rights", SYS_shmctl, 0, SHM_STAT_, &st);
    S("SHM_STAT_ANY no rights", SYS_shmctl, 0, SHM_STAT_ANY_, &st);
    S("shmget wants rights", SYS_shmget, K, 0, 0600);
    AT("attach no rights", id, 0, 0);
    S("SHM_LOCK", SYS_shmctl, id, SHM_LOCK_, 0);
    S("SHM_STAT_ANY shows it", SYS_shmctl, 0, SHM_STAT_ANY_, &st);
    printf("locked bit %d\n", (st.p.mode & SHM_LOCKED_) != 0);
    S("SHM_LOCK twice", SYS_shmctl, id, SHM_LOCK_, 0);
    S("SHM_UNLOCK", SYS_shmctl, id, SHM_UNLOCK_, 0);
    S("SHM_UNLOCK twice", SYS_shmctl, id, SHM_UNLOCK_, 0);
    struct rlimit rl = { 0, 0 };
    setrlimit(RLIMIT_MEMLOCK, &rl);
    S("SHM_LOCK without a limit", SYS_shmctl, id, SHM_LOCK_, 0);
    rl.rlim_cur = rl.rlim_max = 0;
    S("SHM_UNLOCK without a limit", SYS_shmctl, id, SHM_UNLOCK_, 0);
    S("IPC_RMID", SYS_shmctl, id, IPC_RMID);
    S("IPC_RMID again", SYS_shmctl, id, IPC_RMID);
    S("key free", SYS_shmget, K, 1, 0);
}

static sigjmp_buf jb;
static void on_segv(int s) { siglongjmp(jb, s); }

static int faults(volatile char *p, int write) {
    signal(SIGSEGV, on_segv);
    signal(SIGBUS, on_segv);
    int s = sigsetjmp(jb, 1);
    if (s)
        return s;
    if (write)
        *p = 1;
    else
        (void)*p;
    return 0;
}

static void maps_line(unsigned key) {
    char want[32], buf[65536];
    snprintf(want, sizeof want, "/SYSV%08x (deleted)", key);
    rd("/proc/self/maps", buf, sizeof buf);
    char *p = strstr(buf, want);
    if (!p) {
        printf("no maps line for %s\n", want);
        return;
    }
    char *b = p;
    while (b > buf && b[-1] != '\n') b--;
    char perms[8] = "";
    unsigned long s, e, off;
    sscanf(b, "%lx-%lx %7s %lx", &s, &e, perms, &off);
    printf("maps %s len %lu perms %s off %lu\n", want, e - s, perms, off);
}

static void attach(void) {
    fresh_ns();
    int id = S("segment 3 pages", SYS_shmget, K + 1, 3 * PAGE - 100, IPC_CREAT | 0600);
    long r;
    AT("bad id", 77, 0, 0);
    AT("negative id", -1, 0, 0);
    AT("REMAP without address", id, 0, SHM_REMAP_);
    AT("misaligned", id, (void *)0x10000001, 0);
    /* Find a free place, then use it. */
    long p = shmat_(id, 0, 0);
    yes("page aligned", (p & (PAGE - 1)) == 0);
    maps_line(K + 1);
    char *m = (char *)p;
    yes("zero filled", m[0] == 0 && m[3 * PAGE - 101] == 0 && m[3 * PAGE - 1] == 0);
    m[0] = 'x';
    AT("over a mapping", id, m, 0);
    AT("rounded over a mapping", id, m + 5, SHM_RND_);
    S("detach off page", SYS_shmdt, m + 1);
    S("detach mapped elsewhere", SYS_shmdt, m + 100 * PAGE);
    S("detach", SYS_shmdt, m);
    S("detach again", SYS_shmdt, m);
    printf("gone: %d\n", faults(m, 0));
    r = shmat_(id, m + 5, SHM_RND_);
    yes("SHM_RND rounds down", r == (long)m);
    yes("contents kept", m[0] == 'x');
    long q = shmat_(id, m, SHM_REMAP_);
    yes("SHM_REMAP takes the place", q == (long)m);
    nattch("after remap", id);
    S("detach", SYS_shmdt, m);
    r = AT("low address", id, (void *)0x1000, 0);
    r = AT("past user space", id, (void *)0x7ffffffff000ul, 0);
    AT("wrapping", id, (void *)0xfffffffffffff000ul, 0);
    AT("wrapping with REMAP", id, (void *)0xfffffffffffff000ul, SHM_REMAP_);
    p = shmat_(id, 0, SHM_RDONLY_);
    m = (char *)p;
    maps_line(K + 1);
    printf("write to rdonly: %d\n", faults(m, 1));
    yes("reads", m[0] == 'x');
    S("mprotect rw on rdonly", SYS_mprotect, m, PAGE, PROT_READ | PROT_WRITE);
    S("detach", SYS_shmdt, m);
    p = shmat_(id, 0, SHM_EXEC_);
    m = (char *)p;
    maps_line(K + 1);
    S("detach", SYS_shmdt, m);
    /* mprotect to exec without SHM_EXEC. */
    p = shmat_(id, 0, 0);
    m = (char *)p;
    S("mprotect exec", SYS_mprotect, m, PAGE, PROT_READ | PROT_EXEC);
    S("mprotect rw", SYS_mprotect, m, 3 * PAGE, PROT_READ | PROT_WRITE);
    /* A split attachment detaches whole; nattch counts VMAs as Linux. */
    S("mprotect middle", SYS_mprotect, m + PAGE, PAGE, PROT_READ);
    nattch("split by mprotect", id);
    S("mprotect back", SYS_mprotect, m + PAGE, PAGE, PROT_READ | PROT_WRITE);
    nattch("mprotect back does not merge", id);
    S("detach inside a VMA", SYS_shmdt, m + 2 * PAGE + 0);
    S("detach whole", SYS_shmdt, m);
    nattch("detached", id);
    p = shmat_(id, 0, 0);
    m = (char *)p;
    S("detach inside the only VMA", SYS_shmdt, m + PAGE);
    nattch("still attached", id);
    /* mremap: a shrink keeps one VMA, a move too. */
    long moved = syscall(SYS_mremap, m, 3 * PAGE, PAGE, 0);
    yes("shrunk in place", moved == (long)m);
    nattch("after the shrink", id);
    char *spot = mmap(0, 16 * PAGE, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    moved = syscall(SYS_mremap, m, PAGE, PAGE, 3 /* MAYMOVE|FIXED */, spot + PAGE);
    yes("moved", moved == (long)(spot + PAGE));
    nattch("after the move", id);
    S("detach at the old place", SYS_shmdt, m);
    S("detach at the new place", SYS_shmdt, spot + PAGE);
    nattch("detached", id);
    p = shmat_(id, 0, 0);
    m = (char *)p;
    S("munmap middle", SYS_munmap, m + PAGE, PAGE);
    nattch("hole", id);
    S("detach from the start finds both", SYS_shmdt, m);
    nattch("detached", id);
    p = shmat_(id, 0, 0);
    m = (char *)p;
    S("munmap first page", SYS_munmap, m, PAGE);
    S("detach at the hole: pgoff 1 at distance 1", SYS_shmdt, m);
    nattch("rest detached", id);
    p = shmat_(id, 0, 0);
    m = (char *)p;
    S("plain munmap", SYS_munmap, m, 3 * PAGE);
    nattch("munmap detaches", id);
    S("detach at unrelated map", SYS_shmdt, mmap(0, PAGE, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    S("remove", SYS_shmctl, id, IPC_RMID);
}

static int lid;

static void child_attach_more(void) {
    nattch("fork child inherited", lid);
    char *m = (char *)shmat_(lid, 0, 0);
    nattch("child attached again", lid);
    yes("child sees parent's byte", m[1] == 'p');
    m[2] = 'c';
}

static void child_makes(void) {
    S("child's segment", SYS_shmget, IPC_PRIVATE, PAGE, 0600);
}

static void life(void) {
    fresh_ns();
    lid = S("segment", SYS_shmget, K + 2, PAGE, IPC_CREAT | 0600);
    char *m = (char *)shmat_(lid, 0, 0);
    nattch("attached", lid);
    stat_(lid);
    printf("lpid me %d atime %d dtime0 %d\n", st.lpid == getpid(), st.atime > 0, st.dtime == 0);
    m[1] = 'p';
    in_child("child", child_attach_more);
    nattch("child gone", lid);
    yes("parent sees child's byte", m[2] == 'c');
    stat_(lid);
    printf("dtime %d lpid not me %d\n", st.dtime > 0, st.lpid != getpid());

    S("RMID while attached", SYS_shmctl, lid, IPC_RMID);
    nattch("marked", lid);
    printf("dest %d\n", (st.p.mode & SHM_DEST_) != 0);
    maps_line(K + 2);
    S("key free", SYS_shmget, K + 2, 0, 0);
    S("key again is a new one", SYS_shmget, K + 2, PAGE, IPC_CREAT | 0600);
    char *m2 = (char *)shmat_(lid, 0, 0);
    yes("attach after RMID works", m2 != (char *)-1 && m2[1] == 'p');
    nattch("second attach", lid);
    S("RMID twice", SYS_shmctl, lid, IPC_RMID);
    struct { int used; unsigned long tot, rss, swp, att, suc; } si;
    S("SHM_INFO", SYS_shmctl, 0, SHM_INFO_, &si);
    printf("used %d tot %lu rss %lu\n", si.used, si.tot, si.rss);
    char buf[2048];
    rd("/proc/sysvipc/shm", buf, sizeof buf);
    char *line = strchr(buf, '\n') + 1;
    int key, sid, nat; unsigned mode; unsigned long size; int cp, lp;
    while (sscanf(line, "%d %d %o %lu %d %d %d", &key, &sid, &mode, &size, &cp, &lp, &nat) == 7) {
        printf("proc line key %d id %d mode %o size %lu nattch %d\n", key, sid, mode, size, nat);
        line = strchr(line, '\n');
        if (!line) break;
        line++;
    }
    S("detach", SYS_shmdt, m2);
    S("detach last", SYS_shmdt, m);
    S("stat gone", SYS_shmctl, lid, IPC_STAT, &st);
    S("SHM_INFO", SYS_shmctl, 0, SHM_INFO_, &si);
    printf("used %d tot %lu\n", si.used, si.tot);
    /* shm_rmid_forced: an orphan goes at the write, a child's segment with its end, a detach destroys. */
    in_child("child leaves a segment", child_makes);
    S("SHM_INFO", SYS_shmctl, 0, SHM_INFO_, &si);
    printf("used %d\n", si.used);
    wr("forced on", "/proc/sys/kernel/shm_rmid_forced", "1\n");
    S("SHM_INFO", SYS_shmctl, 0, SHM_INFO_, &si);
    printf("orphan gone, used %d\n", si.used);
    in_child("child with forced", child_makes);
    S("SHM_INFO", SYS_shmctl, 0, SHM_INFO_, &si);
    printf("child's gone with it, used %d\n", si.used);
    int fid = (int)syscall(SYS_shmget, IPC_PRIVATE, PAGE, 0600);
    char *x = (char *)shmat_(fid, 0, 0);
    S("detach with forced", SYS_shmdt, x);
    S("destroyed", SYS_shmctl, fid, IPC_STAT, &st);
}

static void child_exec_exit(char **argv, int id) {
    char num[16];
    snprintf(num, sizeof num, "%d", id);
    char *a[] = { argv[0], "exec", num, 0 };
    fflush(stdout);
    execv("/proc/self/exe", a);
    _exit(77);
}

static void do_exec(char **argv) {
    if (argv[2]) {
        int id = atoi(argv[2]);
        nattch("after exec", id);
        char buf[65536];
        rd("/proc/self/maps", buf, sizeof buf);
        yes("no segment left in maps", !strstr(buf, "/SYSV"));
        return;
    }
    fresh_ns();
    int id = S("segment", SYS_shmget, K + 3, PAGE, IPC_CREAT | 0600);
    shmat_(id, 0, 0);
    shmat_(id, 0, 0);
    nattch("two attaches", id);
    pid_t p = fork();
    if (p == 0)
        child_exec_exit(argv, id);
    int s;
    waitpid(p, &s, 0);
    printf("exec child exit %d\n", WEXITSTATUS(s));
    nattch("child's gone", id);
}

static void limits(void) {
    fresh_ns();
    no_caps();
    struct rlimit rl = { 4 << 20, 4 << 20 };
    S("MEMLOCK 4 MiB", SYS_setrlimit, RLIMIT_MEMLOCK, &rl);
    S("huge bad size log", SYS_shmget, IPC_PRIVATE, PAGE, SHM_HUGETLB | (20 << 26) | 0600);
    S("huge without a pool", SYS_shmget, IPC_PRIVATE, PAGE, SHM_HUGETLB | 0600);
    S("huge 2M without a pool", SYS_shmget, IPC_PRIVATE, PAGE, SHM_HUGETLB | (21 << 26) | 0600);
    rl.rlim_cur = rl.rlim_max = 4096;
    setrlimit(RLIMIT_MEMLOCK, &rl);
    S("huge over MEMLOCK", SYS_shmget, IPC_PRIVATE, PAGE, SHM_HUGETLB | 0600);
    S("huge 2M over MEMLOCK", SYS_shmget, IPC_PRIVATE, PAGE, SHM_HUGETLB | (21 << 26) | 0600);
    int s = (int)syscall(SYS_shmget, IPC_PRIVATE, PAGE, 0600);
    S("SHM_LOCK within the limit", SYS_shmctl, s, SHM_LOCK_, 0);
    int b = (int)syscall(SYS_shmget, IPC_PRIVATE, 2 * PAGE, 0600);
    S("SHM_LOCK over the limit", SYS_shmctl, b, SHM_LOCK_, 0);
    S("SHM_UNLOCK first", SYS_shmctl, s, SHM_UNLOCK_, 0);
    S("big without room", SYS_shmget, IPC_PRIVATE, 1ul << 50, 0600);
    int nr = S("big NORESERVE", SYS_shmget, IPC_PRIVATE, 1ul << 40, SHM_NORESERVE | 0600);
    struct { int used; unsigned long tot, rss, swp, att, suc; } si;
    S("SHM_INFO", SYS_shmctl, 0, SHM_INFO_, &si);
    printf("tot %lu rss %lu\n", si.tot, si.rss);
    S("remove", SYS_shmctl, nr, IPC_RMID);
    /* The limits of the namespace, read by shmget. */
    wr("no-caps root writes shmmax", "/proc/sys/kernel/shmmax", "8192\n");
    S("over shmmax", SYS_shmget, IPC_PRIVATE, 8193, 0600);
    S("at shmmax", SYS_shmget, IPC_PRIVATE, 8192, 0600);
    wr("shmall", "/proc/sys/kernel/shmall", "8\n");
    S("over shmall", SYS_shmget, IPC_PRIVATE, 8192, 0600);
    wr("shmall back", "/proc/sys/kernel/shmall", "18446744073692774399\n");
    wr("shmmni", "/proc/sys/kernel/shmmni", "4\n");
    S("over shmmni", SYS_shmget, IPC_PRIVATE, 4096, 0600);
    S("over shmmni", SYS_shmget, IPC_PRIVATE, 4096, 0600);
}

static void plain(void) {
    int id = (int)syscall(SYS_shmget, IPC_PRIVATE, 100000, 0600);
    yes("shmget works", id >= 0);
    char *m = (char *)shmat_(id, 0, 0);
    yes("attached", m != (char *)-1);
    yes("zero", m[99999] == 0);
    memset(m, 7, 100000);
    S("RMID", SYS_shmctl, id, IPC_RMID);
    yes("still there", m[50000] == 7);
    S("detach", SYS_shmdt, m);
}

int main(int argc, char **argv) {
    const char *m = argc > 1 ? argv[1] : "plain";
    setvbuf(stdout, 0, _IOLBF, 0);
    if (!strcmp(m, "basic")) basic();
    else if (!strcmp(m, "attach")) attach();
    else if (!strcmp(m, "life")) life();
    else if (!strcmp(m, "exec")) do_exec(argv);
    else if (!strcmp(m, "limits")) limits();
    else plain();
    return 0;
}
