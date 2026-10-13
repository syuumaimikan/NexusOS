/* The native observer must see exit_group's status, never a sibling's stop. */
#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <sys/syscall.h>
#include <unistd.h>
static _Atomic int ready;
static void *worker(void *unused) {
    (void)unused;
    atomic_fetch_add(&ready, 1);
    for (;;) syscall(SYS_sched_yield);
}
int main(void) {
    pthread_t threads[8];
    for (int i = 0; i < 8; i++) if (pthread_create(&threads[i], NULL, worker, NULL) != 0) return 200;
    for (int retry = 0; retry < 100000 && atomic_load(&ready) != 8; retry++) sched_yield();
    if (atomic_load(&ready) != 8) return 201;
    syscall(SYS_exit_group, 42);
    return 202;
}
