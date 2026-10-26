/* abi-pthread (SC-5): musl's pthreads on the kernel's threads and futexes (clone with the thread flags,
 * CLONE_CHILD_CLEARTID + FUTEX_WAKE for join, FUTEX_WAIT/WAKE/REQUEUE for mutexes and condition variables, the
 * robust list at thread exit). Every mode prints counts and results only, the same on any correct kernel.
 * Modes (argv[1]): mutex, condvar, barrier, many, robust, timed, rwlock, sem. On V2 they need AB-4's clone. */
#include "abi.h"
#include <pthread.h>
#include <semaphore.h>

#define CHECK(label, call) do { int e_ = (call); if (e_) { printf("%s: %s\n", label, ename(e_)); exit(1); } } while (0)

static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static long counter;

static void *mutex_worker(void *arg) {
    long n = (long)arg;
    for (long i = 0; i < n; i++) {
        pthread_mutex_lock(&mu);
        long c = counter;
        for (volatile int k = 0; k < 10; k++)
            ;
        counter = c + 1;
        pthread_mutex_unlock(&mu);
    }
    return 0;
}

static void mode_mutex(void) {
    enum { T = 8, N = 20000 };
    pthread_t t[T];
    for (int i = 0; i < T; i++)
        CHECK("create", pthread_create(&t[i], 0, mutex_worker, (void *)(long)N));
    for (int i = 0; i < T; i++)
        CHECK("join", pthread_join(t[i], 0));
    printf("mutex: %d threads x %d = %ld\n", T, N, counter);
}

/* A bounded queue with one mutex and two condition variables, 4 producers and 4 consumers, then a broadcast
 * that ends the consumers. */
enum { QN = 8, ITEMS = 5000 };
static int q[QN], qh, qt, qlen, done_producing;
static pthread_cond_t not_full = PTHREAD_COND_INITIALIZER, not_empty = PTHREAD_COND_INITIALIZER;
static long consumed_sum, consumed_n;

static void *producer(void *arg) {
    long base = (long)arg;
    for (int i = 0; i < ITEMS; i++) {
        pthread_mutex_lock(&mu);
        while (qlen == QN)
            pthread_cond_wait(&not_full, &mu);
        q[qt] = (int)(base + i);
        qt = (qt + 1) % QN;
        qlen++;
        pthread_cond_signal(&not_empty);
        pthread_mutex_unlock(&mu);
    }
    return 0;
}

static void *consumer(void *arg) {
    (void)arg;
    for (;;) {
        pthread_mutex_lock(&mu);
        while (qlen == 0 && !done_producing)
            pthread_cond_wait(&not_empty, &mu);
        if (qlen == 0) {
            pthread_mutex_unlock(&mu);
            return 0;
        }
        consumed_sum += q[qh];
        consumed_n++;
        qh = (qh + 1) % QN;
        qlen--;
        pthread_cond_signal(&not_full);
        pthread_mutex_unlock(&mu);
    }
}

static void mode_condvar(void) {
    pthread_t p[4], c[4];
    for (int i = 0; i < 4; i++)
        CHECK("create consumer", pthread_create(&c[i], 0, consumer, 0));
    for (int i = 0; i < 4; i++)
        CHECK("create producer", pthread_create(&p[i], 0, producer, (void *)(long)(i * 100000)));
    for (int i = 0; i < 4; i++)
        CHECK("join producer", pthread_join(p[i], 0));
    pthread_mutex_lock(&mu);
    done_producing = 1;
    pthread_cond_broadcast(&not_empty);
    pthread_mutex_unlock(&mu);
    for (int i = 0; i < 4; i++)
        CHECK("join consumer", pthread_join(c[i], 0));
    long want = 0;
    for (int i = 0; i < 4; i++)
        for (int k = 0; k < ITEMS; k++)
            want += i * 100000 + k;
    printf("condvar: consumed %ld items, sum right: %s\n", consumed_n, consumed_sum == want ? "yes" : "no");
}

static pthread_barrier_t bar;
static _Atomic long phase_sum[100];

static void *barrier_worker(void *arg) {
    long id = (long)arg;
    int serial = 0;
    for (int r = 0; r < 100; r++) {
        phase_sum[r] += id;
        int e = pthread_barrier_wait(&bar);
        if (e == PTHREAD_BARRIER_SERIAL_THREAD)
            serial++;
        else if (e)
            return (void *)-1L;
    }
    return (void *)(long)serial;
}

static void mode_barrier(void) {
    enum { T = 16 };
    pthread_t t[T];
    CHECK("barrier_init", pthread_barrier_init(&bar, 0, T));
    for (long i = 0; i < T; i++)
        CHECK("create", pthread_create(&t[i], 0, barrier_worker, (void *)i));
    long serials = 0;
    for (int i = 0; i < T; i++) {
        void *r;
        CHECK("join", pthread_join(t[i], &r));
        serials += (long)r;
    }
    int ok = 1;
    for (int r = 0; r < 100; r++)
        ok &= phase_sum[r] == T * (T - 1) / 2;
    printf("barrier: 16 threads x 100 rounds, one serial thread per round: %s, every round complete: %s\n", serials == 100 ? "yes" : "no", ok ? "yes" : "no");
    pthread_barrier_destroy(&bar);
}

static _Atomic int slots[256];

static void *many_worker(void *arg) {
    long i = (long)arg;
    slots[i] = 1;
    return (void *)(i * 3);
}

static void mode_many(void) {
    /* 256 threads alive at once (each with a small stack), then joined; twice, so thread ids and stacks are
     * reused. */
    for (int round = 0; round < 2; round++) {
        pthread_t t[256];
        pthread_attr_t a;
        pthread_attr_init(&a);
        pthread_attr_setstacksize(&a, 64 * 1024);
        for (long i = 0; i < 256; i++)
            CHECK("create", pthread_create(&t[i], &a, many_worker, (void *)i));
        long sum = 0, set = 0;
        for (int i = 0; i < 256; i++) {
            void *r;
            CHECK("join", pthread_join(t[i], &r));
            sum += (long)r;
        }
        for (int i = 0; i < 256; i++) {
            set += slots[i];
            slots[i] = 0;
        }
        printf("many: round %d: 256 joined, results right: %s, all ran: %s\n", round, sum == 3 * 255 * 256 / 2 ? "yes" : "no", set == 256 ? "yes" : "no");
    }
}

static pthread_mutex_t rmu;

static void *robust_owner(void *arg) {
    (void)arg;
    pthread_mutex_lock(&rmu);
    return 0; /* ends while holding it: the kernel's robust-list walk marks it OWNER_DIED */
}

static void mode_robust(void) {
    pthread_mutexattr_t a;
    pthread_mutexattr_init(&a);
    pthread_mutexattr_setrobust(&a, PTHREAD_MUTEX_ROBUST);
    CHECK("mutex_init", pthread_mutex_init(&rmu, &a));
    pthread_t t;
    CHECK("create", pthread_create(&t, 0, robust_owner, 0));
    CHECK("join", pthread_join(t, 0));
    int e = pthread_mutex_lock(&rmu);
    printf("robust: lock after the owner died: %s\n", e == EOWNERDEAD ? "EOWNERDEAD" : ename(e));
    printf("robust: consistent: %s\n", ename(pthread_mutex_consistent(&rmu)));
    printf("robust: unlock: %d\n", pthread_mutex_unlock(&rmu));
    printf("robust: lock again: %d\n", pthread_mutex_lock(&rmu));
    pthread_mutex_unlock(&rmu);
}

static void *timed_waker(void *arg) {
    (void)arg;
    struct timespec d = {0, 20000000};
    nanosleep(&d, 0);
    pthread_mutex_lock(&mu);
    counter = 1;
    pthread_cond_signal(&cv);
    pthread_mutex_unlock(&mu);
    return 0;
}

static void mode_timed(void) {
    struct timespec t;
    clock_gettime(CLOCK_REALTIME, &t);
    t.tv_nsec += 10000000;
    if (t.tv_nsec >= 1000000000) {
        t.tv_sec++;
        t.tv_nsec -= 1000000000;
    }
    pthread_mutex_lock(&mu);
    int e = pthread_cond_timedwait(&cv, &mu, &t);
    pthread_mutex_unlock(&mu);
    printf("timed: no signal: %s\n", ename(e));
    counter = 0;
    pthread_t w;
    CHECK("create", pthread_create(&w, 0, timed_waker, 0));
    clock_gettime(CLOCK_REALTIME, &t);
    t.tv_sec += 10;
    pthread_mutex_lock(&mu);
    e = 0;
    while (!counter && !e)
        e = pthread_cond_timedwait(&cv, &mu, &t);
    pthread_mutex_unlock(&mu);
    CHECK("join", pthread_join(w, 0));
    printf("timed: signalled: %d, flag %ld\n", e, counter);
    struct timespec past = {1, 0};
    printf("timed: mutex_timedlock free: %d\n", pthread_mutex_timedlock(&mu, &past));
    printf("timed: mutex_timedlock held: %s\n", ename(pthread_mutex_timedlock(&mu, &past)));
    pthread_mutex_unlock(&mu);
}

static pthread_rwlock_t rw = PTHREAD_RWLOCK_INITIALIZER;
static long shared_value;

static void *rw_worker(void *arg) {
    long id = (long)arg;
    long bad = 0;
    for (int i = 0; i < 5000; i++) {
        if ((i + id) % 8 == 0) {
            pthread_rwlock_wrlock(&rw);
            shared_value++;
            shared_value++;
            pthread_rwlock_unlock(&rw);
        } else {
            pthread_rwlock_rdlock(&rw);
            bad += shared_value & 1;
            pthread_rwlock_unlock(&rw);
        }
    }
    return (void *)bad;
}

static void mode_rwlock(void) {
    pthread_t t[8];
    for (long i = 0; i < 8; i++)
        CHECK("create", pthread_create(&t[i], 0, rw_worker, (void *)i));
    long bad = 0;
    for (int i = 0; i < 8; i++) {
        void *r;
        CHECK("join", pthread_join(t[i], &r));
        bad += (long)r;
    }
    printf("rwlock: value %ld, readers saw a half write: %ld\n", shared_value, bad);
}

static sem_t s_items, s_done;

static void *sem_worker(void *arg) {
    (void)arg;
    for (int i = 0; i < 1000; i++) {
        sem_wait(&s_items);
        sem_post(&s_done);
    }
    return 0;
}

static void mode_sem(void) {
    sem_init(&s_items, 0, 0);
    sem_init(&s_done, 0, 0);
    pthread_t t[4];
    for (int i = 0; i < 4; i++)
        CHECK("create", pthread_create(&t[i], 0, sem_worker, 0));
    for (int i = 0; i < 4000; i++)
        sem_post(&s_items);
    for (int i = 0; i < 4000; i++)
        sem_wait(&s_done);
    for (int i = 0; i < 4; i++)
        CHECK("join", pthread_join(t[i], 0));
    int v = -1;
    sem_getvalue(&s_items, &v);
    printf("sem: 4000 handed through 4 threads, left %d\n", v);
}

int main(int argc, char **argv) {
    const char *m = argc > 1 ? argv[1] : "";
    if (!strcmp(m, "mutex"))
        mode_mutex();
    else if (!strcmp(m, "condvar"))
        mode_condvar();
    else if (!strcmp(m, "barrier"))
        mode_barrier();
    else if (!strcmp(m, "many"))
        mode_many();
    else if (!strcmp(m, "robust"))
        mode_robust();
    else if (!strcmp(m, "timed"))
        mode_timed();
    else if (!strcmp(m, "rwlock"))
        mode_rwlock();
    else if (!strcmp(m, "sem"))
        mode_sem();
    else {
        puts("usage: abi-pthread mutex|condvar|barrier|many|robust|timed|rwlock|sem");
        return 2;
    }
    return 0;
}
