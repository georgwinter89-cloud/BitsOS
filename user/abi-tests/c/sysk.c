/* SYS-K: the system's measurement files read the way htop, btop and sensors read them, as a normal user. Every line
 * is a yes/no relation that holds on any Linux machine and on BitsOS whatever hardware it has (a file the machine
 * lacks counts as fine where Linux lacks it too), so the ABI diff compares like with like; the ktest so.sysk.abi
 * runs it as uid 1000 on BitsOS and wants every line "yes". With the argument "strict" (the Mini-PC check list) the
 * sources a Ryzen machine with its GPU must have are required too: cpufreq, k10temp, amdgpu, DMI, card0's memory. */
#include "abi.h"
#include <ctype.h>
#include <dirent.h>
#include <fcntl.h>
#include <sys/stat.h>
#include <sys/wait.h>

static char buf[1 << 16];

/* The whole file into buf; -errno on failure. */
static long slurp(const char *p) {
    int fd = open(p, O_RDONLY | O_CLOEXEC);
    if (fd < 0)
        return -errno;
    long n = 0, r;
    while (n < (long)sizeof buf - 1 && (r = read(fd, buf + n, sizeof buf - 1 - n)) > 0)
        n += r;
    int e = r < 0 ? errno : 0;
    close(fd);
    if (e)
        return -e;
    buf[n] = 0;
    return n;
}

/* A text of one unsigned decimal number and a newline (an optional '-' allowed when signed). */
static int number_line(const char *s, int sign) {
    if (sign && *s == '-')
        s++;
    if (!isdigit((unsigned char)*s))
        return 0;
    while (isdigit((unsigned char)*s))
        s++;
    return s[0] == '\n' && s[1] == 0;
}

static int words(const char *s) {
    int n = 0, in = 0;
    for (; *s && *s != '\n'; s++) {
        if (*s == ' ' || *s == '\t')
            in = 0;
        else if (!in) {
            in = 1;
            n++;
        }
    }
    return n;
}

/* "a missing file or one we may not read": what a normal user meets for root-only files and absent hardware. */
static int closed_or_missing(const char *p) {
    long r = slurp(p);
    return r == -EACCES || r == -ENOENT;
}

/* Every process of uid 0 (init on Linux and on BitsOS; none in a test boot) refuses its io file. */
static int root_io_closed(void) {
    DIR *d = opendir("/proc");
    int ok = d != NULL;
    char p[300];
    for (struct dirent *e; d && (e = readdir(d));) {
        if (!isdigit((unsigned char)e->d_name[0]))
            continue;
        snprintf(p, sizeof p, "/proc/%s/status", e->d_name);
        if (slurp(p) <= 0)
            continue;
        char *u = strstr(buf, "\nUid:\t");
        if (!u || strtoul(u + 6, 0, 10) != 0)
            continue;
        snprintf(p, sizeof p, "/proc/%s/io", e->d_name);
        long r = slurp(p);
        ok = ok && (r == -EACCES || r == -ENOENT || r == -ESRCH);
    }
    if (d)
        closedir(d);
    return ok;
}

static void proc_files(void) {
    long n = slurp("/proc/diskstats");
    int ok = n >= 0;
    for (char *l = buf; ok && n > 0 && *l; l = strchr(l, '\n') + 1) {
        ok = words(l) == 20 || words(l) == 18 || words(l) == 14;
        if (!strchr(l, '\n'))
            break;
    }
    yes("diskstats lines have Linux's columns", ok);

    n = slurp("/proc/self/io");
    const char *keys[] = {"rchar: ", "wchar: ", "syscr: ", "syscw: ", "read_bytes: ", "write_bytes: ", "cancelled_write_bytes: "};
    char *at = buf;
    ok = n > 0;
    for (unsigned i = 0; ok && i < 7; i++) {
        ok = !strncmp(at, keys[i], strlen(keys[i]));
        at = strchr(at, '\n');
        ok = ok && at;
        if (at)
            at++;
    }
    yes("own io has the seven keys in order", ok && *at == 0);
    unsigned long long r0 = 0, c0 = 0, r1 = 0, c1 = 0;
    sscanf(buf, "rchar: %llu\nwchar: %*u\nsyscr: %llu", &r0, &c0);
    int fd = open("/proc/self/stat", O_RDONLY);
    char small[4096];
    long got = fd >= 0 ? read(fd, small, sizeof small) : -1;
    if (fd >= 0)
        close(fd);
    slurp("/proc/self/io");
    sscanf(buf, "rchar: %llu\nwchar: %*u\nsyscr: %llu", &r1, &c1);
    yes("own rchar grows by what read returned", got > 0 && r1 >= r0 + (unsigned long long)got);
    yes("own syscr grows", c1 > c0);
    struct stat st;
    /* Linux wait_task_zombie: a collected child's I/O goes to the parent (a shell's io holds its commands'). */
    pid_t kid = fork();
    if (kid == 0) {
        int z = open("/dev/zero", O_RDONLY);
        long left = 1 << 20;
        while (z >= 0 && left > 0) {
            long r = read(z, buf, sizeof buf < (unsigned long)left ? sizeof buf : (unsigned long)left);
            if (r <= 0)
                break;
            left -= r;
        }
        _exit(left == 0 ? 0 : 1);
    }
    int status = -1;
    int reaped = kid > 0 && waitpid(kid, &status, 0) == kid && WIFEXITED(status) && WEXITSTATUS(status) == 0;
    unsigned long long r2 = 0;
    slurp("/proc/self/io");
    sscanf(buf, "rchar: %llu", &r2);
    yes("a collected child's reads count in its parent's rchar", reaped && r2 >= r1 + (1ull << 20));
    yes("own io is 0400", !stat("/proc/self/io", &st) && (st.st_mode & 0777) == 0400);
    yes("a root process's io is closed to a normal user", getuid() == 0 || root_io_closed());

    n = slurp("/proc/net/wireless");
    yes("wireless is missing or starts with the two header lines", n == -ENOENT || (n > 0 && !strncmp(buf, "Inter-| sta-|", 13) && strstr(buf, "\n face | tus |")));

    n = slurp("/proc/bitsos/metrics");
    ok = n == -ENOENT || n > 0;
    for (char *l = buf; n > 0 && ok && *l;) {
        ok = words(l) == 3;
        char *e = strchr(l, '\n');
        if (!e)
            break;
        l = e + 1;
    }
    yes("bitsos metrics are missing or name value unit lines", ok);
    int programs = 0;
    for (char *l = buf; n > 0 && *l;) {
        programs += !strncmp(l, "proc.", 5) && isdigit((unsigned char)l[5]);
        char *e = strchr(l, '\n');
        if (!e)
            break;
        l = e + 1;
    }
    yes("bitsos metrics name no program", programs == 0);
}

static void block_files(void) {
    DIR *d = opendir("/sys/block");
    int ok = d != NULL, seen = 0;
    char p[600];
    for (struct dirent *e; d && (e = readdir(d));) {
        if (e->d_name[0] == '.')
            continue;
        seen++;
        snprintf(p, sizeof p, "/sys/block/%s/stat", e->d_name);
        ok = ok && slurp(p) > 0 && (words(buf) == 17 || words(buf) == 15 || words(buf) == 11);
        snprintf(p, sizeof p, "/sys/block/%s/size", e->d_name);
        ok = ok && slurp(p) > 0 && number_line(buf, 0);
        snprintf(p, sizeof p, "/sys/block/%s/queue/rotational", e->d_name);
        ok = ok && slurp(p) > 0 && (!strcmp(buf, "0\n") || !strcmp(buf, "1\n"));
    }
    if (d)
        closedir(d);
    /* The test boot has its stick, the Mini-PC its stick and NVMe, CachyOS its disks. */
    yes("every block device has stat, size and rotational", ok && seen > 0);
}

static void hwmon_files(void) {
    DIR *d = opendir("/sys/class/hwmon");
    int ok = 1;
    char p[600];
    for (struct dirent *e; d && (e = readdir(d));) {
        if (strncmp(e->d_name, "hwmon", 5))
            continue;
        snprintf(p, sizeof p, "/sys/class/hwmon/%s/name", e->d_name);
        ok = ok && slurp(p) > 1 && buf[strlen(buf) - 1] == '\n';
        snprintf(p, sizeof p, "/sys/class/hwmon/%s", e->d_name);
        DIR *c = opendir(p);
        for (struct dirent *a; c && (a = readdir(c));) {
            size_t l = strlen(a->d_name);
            if (l > 6 && !strcmp(a->d_name + l - 6, "_input") && (!strncmp(a->d_name, "temp", 4) || !strncmp(a->d_name, "power", 5) || !strncmp(a->d_name, "freq", 4) || !strncmp(a->d_name, "in", 2))) {
                snprintf(p, sizeof p, "/sys/class/hwmon/%s/%s", e->d_name, a->d_name);
                long r = slurp(p);
                /* A sensor may be unreadable at a given moment (ENODATA, EOPNOTSUPP, EIO), never wrong in shape. */
                ok = ok && (r < 0 || number_line(buf, 1));
            }
        }
        if (c)
            closedir(c);
    }
    if (d)
        closedir(d);
    yes("hwmon names and inputs have the sysfs shapes", ok);
}

static void other_files(void) {
    long n = slurp("/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq");
    int ok = n == -ENOENT || (n > 0 && number_line(buf, 0));
    if (ok && n > 0) {
        unsigned long cur = strtoul(buf, 0, 10);
        ok = slurp("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq") > 0 && number_line(buf, 0) && strtoul(buf, 0, 10) > 0 && cur > 0;
    }
    yes("cpufreq is missing or kHz numbers", ok);
    yes("package energy is missing or root's", closed_or_missing("/sys/class/powercap/intel-rapl:0/energy_uj"));
    n = slurp("/sys/class/powercap/intel-rapl:0/max_energy_range_uj");
    yes("energy range is missing or a number", n == -ENOENT || (n > 0 && number_line(buf, 0)));
    n = slurp("/sys/class/dmi/id/sys_vendor");
    yes("dmi vendor is missing or a line", n == -ENOENT || (n > 0 && buf[n - 1] == '\n'));
    yes("dmi serials are missing or root's", closed_or_missing("/sys/class/dmi/id/product_serial") && closed_or_missing("/sys/class/dmi/id/product_uuid") && closed_or_missing("/sys/class/dmi/id/board_serial"));
    n = slurp("/sys/class/drm/card0/device/mem_info_vram_total");
    yes("card0 vram total is missing or a number", n == -ENOENT || (n > 0 && number_line(buf, 0)));

    DIR *d = opendir("/sys/bus/usb/devices");
    ok = 1;
    char p[600];
    for (struct dirent *e; d && (e = readdir(d));) {
        snprintf(p, sizeof p, "/sys/bus/usb/devices/%s/idVendor", e->d_name);
        n = slurp(p);
        if (n == -ENOENT)
            continue;
        ok = ok && n == 5 && isxdigit((unsigned char)buf[0]) && isxdigit((unsigned char)buf[3]) && buf[4] == '\n';
        snprintf(p, sizeof p, "/sys/bus/usb/devices/%s/speed", e->d_name);
        ok = ok && slurp(p) > 0 && (!strcmp(buf, "1.5\n") || !strcmp(buf, "12\n") || !strcmp(buf, "480\n") || !strcmp(buf, "5000\n") || !strcmp(buf, "10000\n") || !strcmp(buf, "20000\n"));
    }
    if (d)
        closedir(d);
    yes("usb devices have four-digit ids and Linux's speeds", ok);
}

/* A hwmon chip of that name exists. */
static int chip(const char *name) {
    DIR *d = opendir("/sys/class/hwmon");
    int found = 0;
    char p[600];
    for (struct dirent *e; d && !found && (e = readdir(d));) {
        snprintf(p, sizeof p, "/sys/class/hwmon/%s/name", e->d_name);
        found = !strncmp(e->d_name, "hwmon", 5) && slurp(p) > 0 && !strncmp(buf, name, strlen(name)) && buf[strlen(name)] == '\n';
    }
    if (d)
        closedir(d);
    return found;
}

static int number_file(const char *p) {
    return slurp(p) > 0 && number_line(buf, 0) && strtoull(buf, 0, 10) > 0;
}

/* Only with "strict": not every Linux machine has these, so the ABI diff never runs it. */
static void present_files(void) {
    yes("strict: cpufreq of cpu0 is there", number_file("/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq") && number_file("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq"));
    yes("strict: hwmon k10temp is there", chip("k10temp"));
    yes("strict: hwmon amdgpu is there", chip("amdgpu"));
    yes("strict: dmi vendor is there", slurp("/sys/class/dmi/id/sys_vendor") > 1);
    yes("strict: card0 vram total is there", number_file("/sys/class/drm/card0/device/mem_info_vram_total"));
}

int main(int argc, char **argv) {
    proc_files();
    block_files();
    hwmon_files();
    other_files();
    if (argc > 1 && !strcmp(argv[1], "strict"))
        present_files();
    return 0;
}
