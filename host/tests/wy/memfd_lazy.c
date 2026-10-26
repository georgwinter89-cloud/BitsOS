/* WY-1b probe (50-foot.sh): foot 1.28's Wayland buffer pools as the kernel sees them. foot makes each pool with
 * memfd_create, grows it with ftruncate to 512 MiB, maps it shared and touches only what it draws (shm.c). Linux
 * charges a memfd's pages when they are first used, not at ftruncate (linux-6.6/mm/memfd.c:363 VM_NORESERVE,
 * mm/shmem.c:1153 shmem_setattr), so four such pools fit into 2 GB. Prints one line per step and "result ok" or
 * "result <errno name> at <step>"; CachyOS is the oracle. V2 matches since FS-14 (plan/notes/fs-FS-14.md), so
 * bitscomp starts foot with its SHM scrolling. */
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#define POOL (512ul << 20)
#define POOLS 4

static const char *ename(int e)
{
    switch (e) {
    case ENOMEM: return "ENOMEM";
    case ENOSPC: return "ENOSPC";
    case EFBIG: return "EFBIG";
    case EINVAL: return "EINVAL";
    case ENOSYS: return "ENOSYS";
    default: return strerror(e);
    }
}

int main(void)
{
    for (int i = 0; i < POOLS; i++) {
        int fd = memfd_create("wy-pool", MFD_CLOEXEC | MFD_ALLOW_SEALING);
        if (fd < 0) {
            printf("result %s at memfd_create %d\n", ename(errno), i);
            return 1;
        }
        if (ftruncate(fd, POOL) != 0) {
            printf("result %s at ftruncate %d\n", ename(errno), i);
            return 1;
        }
        char *p = mmap(NULL, POOL, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if (p == MAP_FAILED) {
            printf("result %s at mmap %d\n", ename(errno), i);
            return 1;
        }
        p[0] = 1;
        p[POOL / 2] = 2;
        printf("pool %d: 512 MiB, two pages touched\n", i);
    }
    printf("result ok\n");
    return 0;
}
