/* AB-7b: the system-wide /proc and /sys files as glibc, musl, busybox ps/top/free, Chromium and Node read
 * them: meminfo, stat, uptime, loadavg, version, filesystems, cpuinfo, /proc/sys/kernel,
 * /proc/sys/vm/overcommit_memory, /sys/devices/system/cpu, /sys/class/net. Prints key names, formats,
 * modes and relations, never host values (CPU model or count, memory, names of cards, times); concrete
 * values only where Linux fixes them (lo's attributes, kernel_max, entropy). Run by uid 1000 on both sides. */
#include "abi.h"
#include <cpuid.h>
#include <ctype.h>
#include <dirent.h>
#include <fcntl.h>
#include <sys/stat.h>
#include <sys/utsname.h>
#include <sys/wait.h>

static char buf[1 << 18];

static long slurp(const char *path) {
    int fd = open(path, O_RDONLY);
    if (fd < 0)
        return -errno;
    long n = 0, r = 0;
    while (n < (long)sizeof buf - 1 && (r = read(fd, buf + n, sizeof buf - 1 - n)) > 0)
        n += r;
    if (r < 0 && n == 0) {
        /* A sysfs attribute that refuses its read (lo's speed: EINVAL). */
        int e = errno;
        close(fd);
        buf[0] = 0;
        return -e;
    }
    close(fd);
    buf[n] = 0;
    return n;
}

/* The file's text into a fresh string ("" when it cannot be read). */
static char *text(const char *path) {
    long n = slurp(path);
    return strdup(n < 0 ? "" : buf);
}

static const char *line_of(const char *s, const char *key) {
    size_t k = strlen(key);
    for (const char *p = s; p && *p; p = strchr(p, '\n') ? strchr(p, '\n') + 1 : 0)
        if (!strncmp(p, key, k))
            return p;
    return 0;
}

static unsigned long long num_after(const char *s, const char *key) {
    const char *l = line_of(s, key);
    return l ? strtoull(l + strlen(key), 0, 10) : 0;
}

static void mode_is(const char *path, long size) {
    struct stat st;
    if (stat(path, &st) < 0) {
        printf("mode %s = -%s\n", path, ename(errno));
        return;
    }
    printf("mode %s = %o size %s\n", path, st.st_mode & 07777, st.st_size == size ? "as Linux" : "differs");
}

static void open_write(const char *label, const char *path) {
    int fd = open(path, O_WRONLY);
    show(label, fd < 0 ? -1 : 0);
    if (fd >= 0)
        close(fd);
}

/* A CPU list ("0-3,8") as a bit set (at most 4096 CPUs are looked at). */
static int in_list(const char *s, int cpu) {
    while (*s && *s != '\n') {
        int a = strtol(s, (char **)&s, 10), b = a;
        if (*s == '-')
            b = strtol(s + 1, (char **)&s, 10);
        if (cpu >= a && cpu <= b)
            return 1;
        if (*s == ',')
            s++;
        else
            break;
    }
    return 0;
}

static int list_count(const char *s, int max) {
    int n = 0;
    for (int c = 0; c < max; c++)
        n += in_list(s, c);
    return n;
}

/* A `%*pb` mask ("ff,ffffffff") holds `cpu`. */
static int in_mask(const char *s, int cpu) {
    char hex[4096];
    int n = 0;
    for (const char *p = s; *p && *p != '\n' && n < (int)sizeof hex - 1; p++)
        if (*p != ',')
            hex[n++] = *p;
    int digit = n - 1 - cpu / 4;
    if (digit < 0)
        return 0;
    int v = isdigit((unsigned char)hex[digit]) ? hex[digit] - '0' : tolower((unsigned char)hex[digit]) - 'a' + 10;
    return (v >> (cpu % 4)) & 1;
}

static void meminfo(void) {
    char *m = text("/proc/meminfo");
    static const char *keys[] = {"MemTotal", "MemFree", "MemAvailable", "Buffers", "Cached", "SwapCached", "Active",
        "Inactive", "Active(anon)", "Inactive(anon)", "Active(file)", "Inactive(file)", "Unevictable", "Mlocked",
        "SwapTotal", "SwapFree", "Zswap", "Zswapped", "Dirty", "Writeback", "AnonPages", "Mapped", "Shmem",
        "KReclaimable", "Slab", "SReclaimable", "SUnreclaim", "KernelStack", "PageTables", "SecPageTables",
        "NFS_Unstable", "Bounce", "WritebackTmp", "CommitLimit", "Committed_AS", "VmallocTotal", "VmallocUsed",
        "VmallocChunk", "Percpu", "HardwareCorrupted", "AnonHugePages", "ShmemHugePages", "ShmemPmdMapped",
        "FileHugePages", "FilePmdMapped", "CmaTotal", "CmaFree", "Unaccepted", "HugePages_Total", "HugePages_Free",
        "HugePages_Rsvd", "HugePages_Surp", "Hugepagesize", "Hugetlb", 0};
    const char *last = m;
    int order = 1, well = 1;
    for (int i = 0; keys[i]; i++) {
        char k[40];
        snprintf(k, sizeof k, "%s:", keys[i]);
        const char *l = line_of(m, k);
        printf("meminfo %s: %s\n", keys[i], l ? "yes" : "no");
        if (l && l < last)
            order = 0;
        if (l)
            last = l;
        /* show_val_kb: key padded to 16, the number right-aligned to 8. */
        if (l && strlen(k) < 16 && strcmp(keys[i], "HardwareCorrupted") && strncmp(keys[i], "HugePages_", 10)) {
            const char *nl = strchr(l, '\n');
            if (!nl || nl - l < 27 || strncmp(nl - 3, " kB", 3))
                well = 0;
            for (const char *p = l + strlen(k); p < l + 16; p++)
                if (*p != ' ')
                    well = 0;
        }
    }
    yes("meminfo 6.6 keys in Linux order", order);
    yes("meminfo kB lines padded as show_val_kb", well);
    unsigned long long tot = num_after(m, "MemTotal:"), fr = num_after(m, "MemFree:"), av = num_after(m, "MemAvailable:");
    yes("meminfo MemFree and MemAvailable within MemTotal", tot > 0 && fr <= tot && av <= tot);
    yes("meminfo SwapFree within SwapTotal", num_after(m, "SwapFree:") <= num_after(m, "SwapTotal:"));
    yes("meminfo Committed_AS and CommitLimit set", num_after(m, "Committed_AS:") > 0 && num_after(m, "CommitLimit:") > 0);
    yes("meminfo Hugepagesize 2048", num_after(m, "Hugepagesize:") == 2048);
    free(m);
}

static int online_cpus(void) {
    char *o = text("/sys/devices/system/cpu/online");
    int n = list_count(o, 4096);
    free(o);
    return n;
}

static void stat_file(void) {
    char *s = text("/proc/stat");
    int n = online_cpus(), lines = 0, fields_ok = 1, sum_ok = 1;
    unsigned long long sum[10] = {0};
    for (char *p = s; p && *p; p = strchr(p, '\n') ? strchr(p, '\n') + 1 : 0) {
        if (strncmp(p, "cpu", 3))
            continue;
        unsigned long long v[10];
        int f = sscanf(p + 3 + strcspn(p + 3, " "), "%llu %llu %llu %llu %llu %llu %llu %llu %llu %llu", &v[0], &v[1], &v[2], &v[3], &v[4], &v[5], &v[6], &v[7], &v[8], &v[9]);
        fields_ok &= f == 10;
        if (lines++ == 0)
            memcpy(sum, v, sizeof sum);
        else
            for (int i = 0; i < 10; i++)
                sum_ok &= v[i] <= sum[i];
    }
    yes("stat starts with \"cpu  \"", !strncmp(s, "cpu  ", 5));
    yes("stat cpu lines = online CPUs + 1", lines == n + 1);
    yes("stat cpu lines have 10 numbers", fields_ok);
    yes("stat sum line >= each cpu line", sum_ok);
    static const char *keys[] = {"intr ", "ctxt ", "btime ", "processes ", "procs_running ", "procs_blocked ", "softirq ", 0};
    for (int i = 0; keys[i]; i++)
        printf("stat line %s: %s\n", keys[i], line_of(s, keys[i]) ? "yes" : "no");
    unsigned long long bt = num_after(s, "btime ");
    yes("stat btime in the past", bt > 0 && bt <= (unsigned long long)time(0));
    yes("stat processes and procs_running >= 1", num_after(s, "processes ") >= 1 && num_after(s, "procs_running ") >= 1);
    const char *sq = line_of(s, "softirq ");
    int sf = 0;
    for (const char *p = sq; p && *p && *p != '\n'; p++)
        sf += *p == ' ';
    yes("stat softirq has 11 numbers", sf == 11);
    free(s);
}

/* "%lu.%02lu" */
static int two_decimals(const char *s, const char **end) {
    const char *p = s;
    if (!isdigit((unsigned char)*p))
        return 0;
    while (isdigit((unsigned char)*p))
        p++;
    if (p[0] != '.' || !isdigit((unsigned char)p[1]) || !isdigit((unsigned char)p[2]))
        return 0;
    *end = p + 3;
    return 1;
}

static void uptime_loadavg_version(void) {
    char *u = text("/proc/uptime");
    const char *e = u, *e2 = u;
    int ok = two_decimals(u, &e) && *e == ' ' && two_decimals(e + 1, &e2) && !strcmp(e2, "\n");
    yes("uptime is two %lu.%02lu numbers", ok);
    yes("uptime idle <= uptime x online CPUs", strtod(e + 1, 0) <= strtod(u, 0) * online_cpus() + 1);
    free(u);
    pid_t c = fork();
    if (c == 0)
        _exit(0);
    waitpid(c, 0, 0);
    char *l = text("/proc/loadavg");
    const char *p = l;
    ok = 1;
    for (int i = 0; i < 3 && ok; i++)
        ok = two_decimals(p, &p) && *p++ == ' ';
    unsigned run = 0, all = 0, last = 0;
    ok = ok && sscanf(p, "%u/%u %u", &run, &all, &last) == 3 && l[strlen(l) - 1] == '\n';
    yes("loadavg is three loads, running/total, last pid", ok);
    yes("loadavg 1 <= running <= total", run >= 1 && run <= all);
    yes("loadavg last pid >= the child's pid", last >= (unsigned)c);
    free(l);
    struct utsname un;
    uname(&un);
    char *v = text("/proc/version"), want[600];
    snprintf(want, sizeof want, "Linux version %s (", un.release);
    yes("version starts with \"Linux version <release> (\"", !strncmp(v, want, strlen(want)));
    snprintf(want, sizeof want, " %s\n", un.version);
    yes("version ends with the uname version", strlen(v) > strlen(want) && !strcmp(v + strlen(v) - strlen(want), want));
    free(v);
    char *f = text("/proc/filesystems");
    static const char *fs[] = {"nodev\tsysfs\n", "nodev\ttmpfs\n", "nodev\tproc\n", "nodev\tdevtmpfs\n", "nodev\tsockfs\n", "nodev\tpipefs\n", "\tvfat\n", 0};
    for (int i = 0; fs[i]; i++)
        printf("filesystems %.*s: %s\n", (int)strlen(fs[i]) - 1, fs[i], strstr(f, fs[i]) ? "yes" : "no");
    int shape = 1;
    for (char *q = f; *q; q = strchr(q, '\n') + 1)
        shape &= !strncmp(q, "nodev\t", 6) || *q == '\t';
    yes("filesystems lines start with nodev<TAB> or <TAB>", shape);
    free(f);
}

static int uuid_ok(const char *s) {
    if (strlen(s) != 37 || s[36] != '\n' || s[14] != '4' || !strchr("89ab", s[19]))
        return 0;
    for (int i = 0; i < 36; i++)
        if ((i == 8 || i == 13 || i == 18 || i == 23) ? s[i] != '-' : !isxdigit((unsigned char)s[i]) || isupper((unsigned char)s[i]))
            return 0;
    return 1;
}

static void sysctl(void) {
    struct utsname un;
    uname(&un);
    char *h = text("/proc/sys/kernel/hostname"), want[80];
    snprintf(want, sizeof want, "%s\n", un.nodename);
    yes("hostname = uname nodename", !strcmp(h, want));
    snprintf(want, sizeof want, "%s\n", un.domainname);
    char *d = text("/proc/sys/kernel/domainname");
    yes("domainname = uname domainname", !strcmp(d, want));
    snprintf(want, sizeof want, "%s\n", un.release);
    char *r = text("/proc/sys/kernel/osrelease");
    yes("osrelease = uname release", !strcmp(r, want));
    char *o = text("/proc/sys/kernel/ostype");
    printf("ostype = %s", o);
    char *ver = text("/proc/sys/kernel/version"), w2[80];
    snprintf(w2, sizeof w2, "%s\n", un.version);
    yes("version = uname version", !strcmp(ver, w2));
    char *pm = text("/proc/sys/kernel/pid_max");
    yes("pid_max >= 32768", strtoul(pm, 0, 10) >= 32768);
    char *u1 = text("/proc/sys/kernel/random/uuid"), *u2 = text("/proc/sys/kernel/random/uuid");
    yes("uuid is a v4 uuid", uuid_ok(u1));
    yes("uuid differs per read", strcmp(u1, u2) != 0);
    char *b1 = text("/proc/sys/kernel/random/boot_id"), *b2 = text("/proc/sys/kernel/random/boot_id");
    yes("boot_id is a v4 uuid and stays", uuid_ok(b1) && !strcmp(b1, b2));
    char *ea = text("/proc/sys/kernel/random/entropy_avail"), *ps = text("/proc/sys/kernel/random/poolsize");
    printf("entropy_avail = %spoolsize = %s", ea, ps);
    char *oc = text("/proc/sys/vm/overcommit_memory");
    yes("overcommit_memory is 0, 1 or 2", strlen(oc) == 2 && oc[0] >= '0' && oc[0] <= '2' && oc[1] == '\n');
    static const char *files[] = {"kernel/hostname", "kernel/domainname", "kernel/osrelease", "kernel/ostype", "kernel/version",
        "kernel/pid_max", "kernel/random/uuid", "kernel/random/boot_id", "kernel/random/entropy_avail", "kernel/random/poolsize",
        "vm/overcommit_memory", 0};
    for (int i = 0; files[i]; i++) {
        char p[80];
        snprintf(p, sizeof p, "/proc/sys/%s", files[i]);
        mode_is(p, 0);
    }
    open_write("open hostname for writing", "/proc/sys/kernel/hostname");
    open_write("open overcommit_memory for writing", "/proc/sys/vm/overcommit_memory");
    SYS("sethostname as a user", SYS_sethostname, "x", 1);
    SYS("sethostname 65 bytes as a user", SYS_sethostname, "x", 65);
    SYS("setdomainname as a user", SYS_setdomainname, "x", 1);
    /* Further knobs libcap, systemd, JVM/V8, Wine and procps read: modes, Linux's fixed values, shapes. */
    static const char *more[] = {"kernel/cap_last_cap", "kernel/ngroups_max", "kernel/overflowuid", "kernel/overflowgid",
        "kernel/randomize_va_space", "kernel/tainted", "kernel/threads-max", "vm/max_map_count", "vm/mmap_min_addr",
        "vm/overcommit_kbytes", "vm/overcommit_ratio", "vm/swappiness", 0};
    for (int i = 0; more[i]; i++) {
        char p[80];
        snprintf(p, sizeof p, "/proc/sys/%s", more[i]);
        mode_is(p, 0);
        long r = slurp(p);
        char *end = 0;
        strtoull(buf, &end, 10);
        printf("%s is one number: %s\n", more[i], r > 1 && end && *end == '\n' && end[1] == 0 ? "yes" : "no");
    }
    char *cl = text("/proc/sys/kernel/cap_last_cap"), *ng = text("/proc/sys/kernel/ngroups_max"), *ou = text("/proc/sys/kernel/overflowuid");
    printf("cap_last_cap = %sngroups_max = %soverflowuid = %s", cl, ng, ou);
    char *rv = text("/proc/sys/kernel/randomize_va_space");
    yes("randomize_va_space is 0, 1 or 2", strlen(rv) == 2 && rv[0] >= '0' && rv[0] <= '2');
    yes("max_map_count >= 65530", strtoull(text("/proc/sys/vm/max_map_count"), 0, 10) >= 65530);
    open_write("open cap_last_cap for writing", "/proc/sys/kernel/cap_last_cap");
    /* procps keeps a file open and reads it again from offset 0: each read is made anew (uuid changes). */
    int fd = open("/proc/sys/kernel/random/uuid", O_RDONLY);
    char r1[64] = {0}, r2[64] = {0};
    long n1 = read(fd, r1, sizeof r1 - 1);
    long ls = lseek(fd, 0, SEEK_SET);
    long n2 = read(fd, r2, sizeof r2 - 1);
    close(fd);
    yes("uuid reread after lseek 0 is new", n1 == 37 && ls == 0 && n2 == 37 && strcmp(r1, r2) != 0);
    free(cl), free(ng), free(ou), free(rv);
    free(h), free(d), free(r), free(o), free(ver), free(pm), free(u1), free(u2), free(b1), free(b2), free(ea), free(ps), free(oc);
}

static const char *topo_names[] = {"physical_package_id", "die_id", "cluster_id", "core_id", "core_cpus", "core_cpus_list",
    "thread_siblings", "thread_siblings_list", "core_siblings", "core_siblings_list", "cluster_cpus", "cluster_cpus_list",
    "die_cpus", "die_cpus_list", "package_cpus", "package_cpus_list", 0};

static char *topo(int cpu, const char *name) {
    char p[128];
    snprintf(p, sizeof p, "/sys/devices/system/cpu/cpu%d/topology/%s", cpu, name);
    return text(p);
}

static void cpus(void) {
    const char *d = "/sys/devices/system/cpu";
    char *on = text("/sys/devices/system/cpu/online"), *po = text("/sys/devices/system/cpu/possible");
    char *pr = text("/sys/devices/system/cpu/present"), *off = text("/sys/devices/system/cpu/offline");
    char *km = text("/sys/devices/system/cpu/kernel_max"), *iso = text("/sys/devices/system/cpu/isolated");
    int n = list_count(on, 4096), np = list_count(po, 4096);
    yes("online count = sysconf(_SC_NPROCESSORS_ONLN)", n == sysconf(_SC_NPROCESSORS_ONLN));
    int sub = 1;
    for (int c = 0; c < 4096; c++)
        sub &= !in_list(on, c) || in_list(po, c);
    yes("online within possible", sub);
    yes("present = possible", !strcmp(pr, po));
    yes("offline = possible - online", list_count(off, 4096) == np - n);
    printf("kernel_max = %sisolated empty: %s\n", km, strcmp(iso, "\n") ? "no" : "yes");
    yes("kernel_max >= the highest possible CPU", strtol(km, 0, 10) >= np - 1);
    static const char *top[] = {"online", "possible", "present", "offline", "kernel_max", "isolated", 0};
    for (int i = 0; top[i]; i++) {
        char p[80];
        snprintf(p, sizeof p, "%s/%s", d, top[i]);
        mode_is(p, 4096);
    }
    int dirs = 0;
    DIR *dir = opendir(d);
    for (struct dirent *e; dir && (e = readdir(dir));)
        dirs += !strncmp(e->d_name, "cpu", 3) && isdigit((unsigned char)e->d_name[3]);
    if (dir)
        closedir(dir);
    yes("cpuN directories = possible CPUs", dirs == np);
    long r0 = slurp("/sys/devices/system/cpu/cpu0/online");
    printf("cpu0/online = %s\n", r0 < 0 ? ename(-r0) : "present");
    int names = 1, rel = 1, masks = 1, cpuinfo_rel = 1;
    char *ci = text("/proc/cpuinfo");
    int blocks = 0, apic_unique = 1;
    long apics[4096];
    for (char *b = ci; b && *b; b = strstr(b, "\n\n") ? strstr(b, "\n\n") + 2 : 0) {
        int cpu = (int)num_after(b, "processor\t: ");
        char *end = strstr(b, "\n\n");
        if (end)
            *end = 0;
        long a = (long)num_after(b, "apicid\t\t: ");
        for (int i = 0; i < blocks && i < 4096; i++)
            apic_unique &= apics[i] != a;
        if (blocks < 4096)
            apics[blocks] = a;
        blocks++;
        char *pkg = topo(cpu, "physical_package_id"), *core = topo(cpu, "core_id"), *pl = topo(cpu, "package_cpus_list");
        cpuinfo_rel &= strtoull(pkg, 0, 10) == num_after(b, "physical id\t: ") && strtoull(core, 0, 10) == num_after(b, "core id\t\t: ");
        cpuinfo_rel &= list_count(pl, 4096) == (int)num_after(b, "siblings\t: ");
        /* cpu cores: distinct thread-sibling sets in the package. */
        int cores = 0;
        for (int o = 0; o < 4096; o++) {
            if (!in_list(pl, o))
                continue;
            char *ts = topo(o, "thread_siblings_list");
            int first = 1;
            for (int x = 0; x < o; x++)
                first &= !in_list(ts, x);
            cores += first;
            free(ts);
        }
        cpuinfo_rel &= cores == (int)num_after(b, "cpu cores\t: ");
        free(pkg), free(core), free(pl);
        if (end)
            *end = '\n';
    }
    yes("cpuinfo blocks = online CPUs", blocks == n);
    yes("cpuinfo apicid unique", apic_unique);
    yes("cpuinfo physical id, core id, siblings, cpu cores match topology", cpuinfo_rel);
    for (int c = 0; c < 4096; c++) {
        if (!in_list(on, c))
            continue;
        for (int i = 0; topo_names[i]; i++) {
            char *t = topo(c, topo_names[i]);
            names &= t[0] != 0;
            free(t);
        }
        char *cc = topo(c, "core_cpus_list"), *ts = topo(c, "thread_siblings_list"), *pk = topo(c, "package_cpus_list");
        char *cs = topo(c, "core_siblings_list"), *cl = topo(c, "cluster_cpus_list"), *di = topo(c, "die_cpus_list");
        char *pm = topo(c, "package_cpus");
        rel &= !strcmp(cc, ts) && !strcmp(pk, cs) && in_list(cc, c) && in_list(cl, c) && in_list(di, c) && in_list(pk, c);
        for (int o = 0; o < 4096; o++)
            masks &= in_mask(pm, o) == in_list(pk, o);
        if (c > 0) {
            char p[80];
            snprintf(p, sizeof p, "%s/cpu%d/online", d, c);
            char *ol = text(p);
            rel &= !strcmp(ol, "1\n");
            free(ol);
        }
        free(cc), free(ts), free(pk), free(cs), free(cl), free(di), free(pm);
    }
    yes("every online CPU has the 16 topology files", names);
    yes("topology lists hold the CPU, thread = core_cpus, package = core_siblings", rel);
    yes("package_cpus mask = package_cpus_list", masks);
    char *b0 = strdup(ci), *e0 = strstr(b0, "\n\n");
    if (e0)
        e0[1] = 0;
    printf("cpuinfo keys:");
    for (char *p = b0; p && *p; p = strchr(p, '\n') ? strchr(p, '\n') + 1 : 0) {
        int k = strcspn(p, "\t:");
        if (!strncmp(p, "microcode", k) || !strncmp(p, "vmx flags", k) || !strncmp(p, "cache size", k) || !strncmp(p, "TLB size", k))
            continue;
        printf(" %.*s;", k, p);
    }
    printf("\n");
    unsigned a, b, c7, dd;
    int avx2 = __get_cpuid_count(7, 0, &a, &b, &c7, &dd) && (b >> 5 & 1);
    const char *fl = line_of(ci, "flags\t\t: ");
    char *fe = fl ? strndup(fl, strcspn(fl, "\n")) : strdup("");
    size_t fl_len = strlen(fe);
    int has_avx2 = strstr(fe, " avx2 ") != 0 || (fl_len >= 5 && !strcmp(fe + fl_len - 5, " avx2"));
    yes("cpuinfo flags list avx2 exactly when CPUID has it", has_avx2 == avx2);
    yes("cpuinfo flags start with fpu", strncmp(fe, "flags\t\t: fpu ", 13) == 0);
    mode_is("/sys/devices/system/cpu/cpu0/topology/core_cpus_list", 28672);
    mode_is("/sys/devices/system/cpu/cpu0/topology/core_cpus", 4096);
    mode_is("/sys/devices/system/cpu/cpu0/topology", 0);
    char *sa = text("/sys/devices/system/cpu/smt/active"), *sc = text("/sys/devices/system/cpu/smt/control");
    yes("smt/active is 0 or 1", !strcmp(sa, "0\n") || !strcmp(sa, "1\n"));
    yes("smt/control is a state", !strcmp(sc, "on\n") || !strcmp(sc, "off\n") || !strcmp(sc, "forceoff\n") || !strcmp(sc, "notsupported\n") || !strcmp(sc, "notimplemented\n"));
    yes("smt active means control on", strcmp(sa, "1\n") || !strcmp(sc, "on\n"));
    mode_is("/sys/devices/system/cpu/smt/control", 4096);
    free(sa), free(sc);
    /* cacheinfo of cpu0: every index has the ten attributes x86 shows, levels ascending, cpu0 in its list. */
    static const char *cattr[] = {"id", "type", "level", "shared_cpu_map", "shared_cpu_list", "coherency_line_size",
        "ways_of_associativity", "number_of_sets", "size", "physical_line_partition", 0};
    int idx = 0, cache_ok = 1, last_level = 0;
    for (;; idx++) {
        char p[128];
        struct stat st;
        snprintf(p, sizeof p, "/sys/devices/system/cpu/cpu0/cache/index%d", idx);
        if (stat(p, &st) < 0)
            break;
        for (int i = 0; cattr[i]; i++) {
            snprintf(p, sizeof p, "/sys/devices/system/cpu/cpu0/cache/index%d/%s", idx, cattr[i]);
            cache_ok &= stat(p, &st) == 0 && (st.st_mode & 0777) == 0444 && slurp(p) > 1;
        }
        snprintf(p, sizeof p, "/sys/devices/system/cpu/cpu0/cache/index%d/level", idx);
        slurp(p);
        int lv = atoi(buf);
        cache_ok &= lv >= last_level && lv >= 1;
        last_level = lv;
        snprintf(p, sizeof p, "/sys/devices/system/cpu/cpu0/cache/index%d/size", idx);
        slurp(p);
        cache_ok &= buf[strspn(buf, "0123456789")] == 'K';
        snprintf(p, sizeof p, "/sys/devices/system/cpu/cpu0/cache/index%d/shared_cpu_list", idx);
        slurp(p);
        cache_ok &= in_list(buf, 0);
    }
    yes("cpu0 has cache leaves with Linux's attributes", idx > 0 && cache_ok);
    free(fe), free(b0), free(ci), free(on), free(po), free(pr), free(off), free(km), free(iso);
}

static void net(void) {
    char l[256];
    long n = readlink("/sys/class/net/lo", l, sizeof l - 1);
    l[n < 0 ? 0 : n] = 0;
    printf("lo link = %s\n", l);
    static const char *lo[] = {"address", "addr_len", "broadcast", "flags", "type", "ifindex", "iflink", "mtu", "operstate",
        "tx_queue_len", "carrier", "carrier_changes", "carrier_up_count", "carrier_down_count", "dev_id", "dev_port",
        "addr_assign_type", "name_assign_type", "link_mode", "netdev_group", "dormant", "testing", "proto_down", "uevent",
        "speed", "duplex", 0};
    for (int i = 0; lo[i]; i++) {
        char p[80];
        snprintf(p, sizeof p, "/sys/class/net/lo/%s", lo[i]);
        long r = slurp(p);
        if (r < 0)
            printf("lo %s = -%s\n", lo[i], ename(-r));
        else
            printf("lo %s = %s", lo[i], buf);
    }
    static const char *stats[] = {"rx_packets", "tx_packets", "rx_bytes", "tx_bytes", "rx_errors", "tx_errors", "rx_dropped",
        "tx_dropped", "multicast", "collisions", "rx_length_errors", "rx_over_errors", "rx_crc_errors", "rx_frame_errors",
        "rx_fifo_errors", "rx_missed_errors", "tx_aborted_errors", "tx_carrier_errors", "tx_fifo_errors",
        "tx_heartbeat_errors", "tx_window_errors", "rx_compressed", "tx_compressed", "rx_nohandler", 0};
    int all_links = 1, cards_ok = 1, count = 0, lo_seen = 0;
    DIR *dir = opendir("/sys/class/net");
    for (struct dirent *e; dir && (e = readdir(dir));) {
        if (e->d_name[0] == '.')
            continue;
        count++;
        lo_seen |= !strcmp(e->d_name, "lo");
        char p[300];
        struct stat st;
        snprintf(p, sizeof p, "/sys/class/net/%s", e->d_name);
        all_links &= lstat(p, &st) == 0 && S_ISLNK(st.st_mode);
        for (int i = 0; stats[i]; i++) {
            snprintf(p, sizeof p, "/sys/class/net/%s/statistics/%s", e->d_name, stats[i]);
            cards_ok &= slurp(p) > 0 && isdigit((unsigned char)buf[0]);
        }
        snprintf(p, sizeof p, "/sys/class/net/%s/address", e->d_name);
        cards_ok &= slurp(p) == 18 && buf[2] == ':' && buf[17] == '\n';
        snprintf(p, sizeof p, "/sys/class/net/%s/operstate", e->d_name);
        slurp(p);
        cards_ok &= !strcmp(buf, "up\n") || !strcmp(buf, "down\n") || !strcmp(buf, "unknown\n") || !strcmp(buf, "dormant\n") || !strcmp(buf, "lowerlayerdown\n");
        snprintf(p, sizeof p, "/sys/class/net/%s/mtu", e->d_name);
        cards_ok &= slurp(p) > 0 && strtol(buf, 0, 10) > 0;
    }
    if (dir)
        closedir(dir);
    yes("class/net lists lo", lo_seen);
    yes("class/net entries are links", count > 0 && all_links);
    yes("every interface has address, operstate, mtu and the 24 counters", cards_ok);
    mode_is("/sys/class/net/lo/mtu", 4096);
    mode_is("/sys/class/net/lo/address", 4096);
    mode_is("/sys/class/net/lo/statistics/rx_bytes", 4096);
    mode_is("/sys/class/net/lo/statistics", 0);
    open_write("open lo/mtu for writing", "/sys/class/net/lo/mtu");
    long rn = slurp("/sys/class/net/nosuchif0/mtu");
    printf("class/net/nosuchif0/mtu = %s\n", rn < 0 ? ename(-rn) : "present");
}

int main(void) {
    meminfo();
    stat_file();
    uptime_loadavg_version();
    sysctl();
    cpus();
    net();
    return 0;
}
