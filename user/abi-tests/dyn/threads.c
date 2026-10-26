// AB-8: a dynamic glibc pthread program: clone3 threads with static TLS of the program and of libc (errno),
// a mutex, a condition variable and a barrier (futex), join.
#include "abi.h"
#include <pthread.h>

#define THREADS 8
#define ROUNDS 20000

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
static pthread_barrier_t barrier;
static long total;
static int started;
static __thread long mine;
static int tls_ok = 1, errno_ok = 1;

static void *worker(void *arg) {
    long id = (long)arg;
    pthread_mutex_lock(&lock);
    started++;
    pthread_cond_broadcast(&cond);
    pthread_mutex_unlock(&lock);
    pthread_barrier_wait(&barrier);
    errno = (int)id + 100;
    for (int i = 0; i < ROUNDS; i++) {
        mine++;
        pthread_mutex_lock(&lock);
        total++;
        pthread_mutex_unlock(&lock);
    }
    if (mine != ROUNDS)
        tls_ok = 0;
    if (errno != (int)id + 100)
        errno_ok = 0;
    return (void *)(id * 2);
}

int main(void) {
    pthread_t t[THREADS];
    pthread_barrier_init(&barrier, NULL, THREADS);
    int created = 0;
    for (long i = 0; i < THREADS; i++)
        created += pthread_create(&t[i], NULL, worker, (void *)i) == 0;
    printf("threads created: %d\n", created);
    pthread_mutex_lock(&lock);
    while (started < THREADS)
        pthread_cond_wait(&cond, &lock);
    pthread_mutex_unlock(&lock);
    long back = 0;
    for (int i = 0; i < THREADS; i++) {
        void *r;
        pthread_join(t[i], &r);
        back += (long)r;
    }
    printf("total: %ld\n", total);
    printf("returns: %ld\n", back);
    yes("tls per thread", tls_ok && mine == 0);
    yes("errno per thread", errno_ok);
    printf("dyn threads: done\n");
    return 0;
}
