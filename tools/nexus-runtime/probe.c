/* Linked to actual upstream libc, with no replacement libc routines. */
#define _GNU_SOURCE
#include <errno.h>
#include <dirent.h>
#include <fcntl.h>
#include <pthread.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/epoll.h>
#include <time.h>
#include <unistd.h>
#include <sys/syscall.h>
#include <stdatomic.h>
#include <stdint.h>
#include <sched.h>
#include <linux/futex.h>
#include <sys/sysmacros.h>
#ifdef __GLIBC__
#include <gnu/libc-version.h>
#endif
#ifdef PROBE_DYNAMIC
#include <dlfcn.h>
#endif
#if defined(__GLIBC__) && defined(__i386__)
#define PROBE_NAME "glibc32-probe"
#elif defined(__GLIBC__)
#define PROBE_NAME "glibc-probe"
#else
#define PROBE_NAME "musl-probe"
#endif

static int check(int ok, int stage) {
    if (!ok) {
        printf(PROBE_NAME ": FAIL stage=%d errno=%d\n", stage, errno);
        fflush(stdout);
    }
    return ok;
}

#ifdef PROBE_THREADS
static _Thread_local volatile int local_value;
static _Atomic int ready;
static _Atomic int finished;
static int *wait_word;
struct poll_roundtrip { int input, output; };
static void *tls_worker(void *argument) {
    int wanted = (int)(uintptr_t)argument;
    local_value = wanted;
    for (int round = 0; round < 1024; round++) {
        sched_yield();
        if (local_value != wanted) return NULL;
#ifdef __i386__
        uint16_t selector;
        __asm__ volatile ("mov %%gs, %0" : "=r"(selector));
        uint32_t descriptor[4] = {selector >> 3, 0, 0, 0};
        if (syscall(SYS_get_thread_area, descriptor) != 0 ||
            descriptor[1] != (uint32_t)(uintptr_t)__builtin_thread_pointer()) return NULL;
#endif
    }
    return argument;
}
static void *poll_worker(void *argument) {
    struct poll_roundtrip *channels = argument;
    for (int round = 0; round < 256; round++) {
        unsigned char byte;
        if (read(channels->input, &byte, 1) != 1 || byte != (unsigned char)round ||
            write(channels->output, &byte, 1) != 1) return NULL;
    }
    return argument;
}
static void *immediate_worker(void *argument) {
    local_value = 42;
    return argument;
}
static void *worker(void *argument) {
    local_value = 42;
    atomic_store(&ready, 1);
    const struct timespec limit = {5, 0};
    long waited = syscall(SYS_futex, wait_word, FUTEX_WAIT, 0, &limit, NULL, 0);
    if (waited != 0 || *wait_word != 0) return NULL;
    *(int *)argument = local_value;
    atomic_store(&finished, 1);
    return argument;
}
static int wake_worker(int *word) {
    for (int retry = 0; retry < 5000; retry++) {
        if (atomic_load(&ready)) {
            if (syscall(SYS_futex, word, FUTEX_WAKE, 0, NULL, NULL, 0) != 0) return 0;
            long count = syscall(SYS_futex, word, FUTEX_WAKE, 1, NULL, NULL, 0);
            if (count == 1) return 1;
            if (count < 0) return 0;
            // Linux permits a spurious successful wait. A wake racing the
            // kernel's queue insertion can return zero while that waiter
            // already completed; the join below still checks its result.
            if (atomic_load(&finished)) return 1;
        }
        const struct timespec pause = {0, 1000000};
        nanosleep(&pause, NULL);
    }
    return 0;
}
#include "semaphore-probe.h"
#include "eventfd-probe.h"
#endif

#include "proc-probe.h"
#include "cwd-probe.h"
#include "symlink-probe.h"
#include "random-probe.h"
#include "socket32-probe.h"

int main(int argc, char **argv, char **environment) {
    // Emit complete, short result lines rather than splitting a large stdio
    // buffer at the kernel console's legitimate short-write boundary.
    if (setvbuf(stdout, NULL, _IOLBF, 0) != 0) return 75;
#ifndef __i386__
    if (argc > 1 && !strcmp(argv[1], "--proc-exec")) return proc_exec_verify(argc, argv, environment);
#endif
    if (!proc_probe(argc, argv, environment)) return 190;
    if (!cwd_probe()) return 191;
    if (!symlink_probe()) return 192;
    if (!random_probe()) return 193;
#ifdef __i386__
    if (!socket32_probe()) return 194;
#endif
#ifdef __GLIBC__
    printf(PROBE_NAME ": entered glibc %s main\n", gnu_get_libc_version());
#else
    puts("musl-probe: entered real libc main");
#endif
#ifdef PROBE_DYNAMIC
    const char *library_path = getenv("NEXUS_PROBE_LIBRARY");
#ifdef PROBE_GLIBC_LIBRARY
#ifdef __i386__
    const char *default_library = "/usr/lib/i386-linux-gnu/libnexus-glibc-probe.so";
#else
    const char *default_library = "/usr/lib/libnexus-glibc-probe.so";
#endif
#else
    const char *default_library = "/usr/lib/libnexus-probe.so";
#endif
    void *library = dlopen(library_path ? library_path : default_library, RTLD_NOW);
    if (!check(library != NULL, 25)) { puts(dlerror()); return 25; }
    int (*value)(void) = (int (*)(void))dlsym(library, "nexus_library_value");
    if (!check(value != NULL && value() == 47, 26)) {
        printf(PROBE_NAME ": library value=%d symbol=%p\n", value ? value() : -1, (void *)value);
        const char *reason = dlerror();
        if (reason) puts(reason);
        return 26;
    }
    if (!check(dlclose(library) == 0, 27)) return 27;
    puts(PROBE_NAME ": PASS real dynamic loader dlopen symbol and library TLS");
#endif
    unsigned char *memory = malloc(192 * 1024);
    if (!check(memory != NULL, 10)) return 10;
    memset(memory, 0x5a, 192 * 1024);
    if (!check(memory[191 * 1024] == 0x5a, 11)) return 11;
    free(memory);
    unsigned char *guard = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (!check(guard != MAP_FAILED, 30)) return 30;
    guard[0] = 0x71;
    if (!check(mprotect(guard, 4096, PROT_NONE) == 0, 31)) return 31;
    errno = 0;
    if (!check(write(1, guard, 1) == -1 && errno == EFAULT, 32)) return 32;
    if (!check(mprotect(guard, 4096, PROT_READ | PROT_WRITE) == 0 && guard[0] == 0x71, 33)) return 33;
    if (!check(munmap(guard, 4096) == 0, 34)) return 34;
    int fd = open("musl-probe.txt", O_CREAT | O_TRUNC | O_RDWR, 0600);
    if (!check(fd >= 3, 12)) return 12;
    const char contents[] = "a real musl program wrote these bytes";
    if (!check(write(fd, contents, sizeof(contents)) == sizeof(contents), 13)) return 13;
    char back[sizeof(contents)] = {0};
    if (!check(pread(fd, back, sizeof(back), 0) == sizeof(back), 14)) return 14;
    if (!check(memcmp(back, contents, sizeof(back)) == 0, 15)) return 15;
    if (!check(lseek(fd, 0, SEEK_CUR) == sizeof(contents), 16)) return 16;
    struct stat info;
    if (!check(fstat(fd, &info) == 0 && info.st_size == sizeof(contents), 17)) return 17;
    int same = open("musl-probe.txt", O_RDONLY);
    struct stat same_info;
    if (!check(same >= 0 && fstat(same, &same_info) == 0 && same_info.st_ino != 0 &&
        same_info.st_dev == info.st_dev && same_info.st_ino == info.st_ino, 28)) return 28;
    struct statx extended, same_extended, other_extended;
    if (!check(syscall(SYS_statx, fd, "", AT_EMPTY_PATH, STATX_BASIC_STATS, &extended) == 0 &&
        (extended.stx_mask & (STATX_INO | STATX_SIZE)) == (STATX_INO | STATX_SIZE) &&
        extended.stx_ino == info.st_ino && extended.stx_size == sizeof(contents) &&
        extended.stx_dev_major == major(info.st_dev) && extended.stx_dev_minor == minor(info.st_dev), 52)) return 52;
    // Nonempty paths remain paths even with AT_EMPTY_PATH. The loader uses
    // this identity to distinguish loaded objects and avoid loading one twice.
    if (!check(syscall(SYS_statx, AT_FDCWD, "musl-probe.txt", AT_EMPTY_PATH, STATX_INO, &same_extended) == 0 &&
        same_extended.stx_ino == extended.stx_ino, 53)) return 53;
    errno = 0;
    if (!check(syscall(SYS_statx, fd, "", 0, STATX_INO, &same_extended) == -1 && errno == ENOENT, 54)) return 54;
    errno = 0;
    if (!check(syscall(SYS_statx, fd, (void *)1, AT_EMPTY_PATH, STATX_INO, &same_extended) == -1 && errno == EFAULT, 55)) return 55;
    close(same);
    int other = open("musl-other.txt", O_CREAT | O_RDWR, 0600);
    struct stat other_info;
    if (!check(other >= 0 && fstat(other, &other_info) == 0 && other_info.st_ino != info.st_ino, 29)) return 29;
    if (!check(syscall(SYS_statx, other, "", AT_EMPTY_PATH, STATX_INO, &other_extended) == 0 &&
        other_extended.stx_ino != extended.stx_ino, 56)) return 56;
    close(other);
    int epoll_pipe[2];
    int epoll_fd = epoll_create1(EPOLL_CLOEXEC);
    if (!check(epoll_fd >= 0 && pipe(epoll_pipe) == 0, 71)) return 71;
    struct epoll_event interest = {.events = EPOLLIN, .data.u64 = UINT64_C(0x8765432101234567)};
    if (!check(epoll_ctl(epoll_fd, EPOLL_CTL_ADD, epoll_pipe[0], &interest) == 0, 72)) return 72;
    struct epoll_event event;
    if (!check(epoll_wait(epoll_fd, &event, 1, 0) == 0 && write(epoll_pipe[1], "e", 1) == 1 &&
        epoll_wait(epoll_fd, &event, 1, 100) == 1 && (event.events & EPOLLIN) && event.data.u64 == interest.data.u64, 73)) return 73;
    char event_byte;
    if (!check(read(epoll_pipe[0], &event_byte, 1) == 1 && event_byte == 'e' &&
        epoll_ctl(epoll_fd, EPOLL_CTL_DEL, epoll_pipe[0], NULL) == 0, 74)) return 74;
    close(epoll_pipe[0]); close(epoll_pipe[1]); close(epoll_fd);
    puts(PROBE_NAME ": PASS epoll pipe readiness and packed 64-bit event data");
    if (!check(close(fd) == 0, 18)) return 18;
    struct timespec before, after;
    if (!check(clock_gettime(CLOCK_MONOTONIC, &before) == 0, 19)) return 19;
    const struct timespec pause = {0, 2000000};
    if (!check(nanosleep(&pause, NULL) == 0, 20)) return 20;
    if (!check(clock_gettime(CLOCK_MONOTONIC, &after) == 0 &&
        (after.tv_sec > before.tv_sec || (after.tv_sec == before.tv_sec && after.tv_nsec >= before.tv_nsec)), 21)) return 21;
#ifdef PROBE_THREADS
    local_value = 7;
#ifdef __i386__
    uint16_t tls_selector;
    __asm__ volatile ("mov %%gs, %0" : "=r"(tls_selector));
    uint32_t current_tls[4] = {tls_selector >> 3, 0, 0, 0};
    if (!check(syscall(SYS_get_thread_area, current_tls) == 0 && current_tls[1] ==
        (uint32_t)(uintptr_t)__builtin_thread_pointer(), 60)) return 60;
    // Allocate both remaining GDT slots, check exhaustion and clear them.
    uint32_t extra[4] = {UINT32_MAX, (uint32_t)(uintptr_t)&current_tls, 0xfffff, 0x51};
    if (!check(syscall(SYS_set_thread_area, extra) == 0 && extra[0] != current_tls[0], 61)) return 61;
    uint32_t readback[4] = {extra[0], 0, 0, 0};
    if (!check(syscall(SYS_get_thread_area, readback) == 0 && memcmp(extra, readback, sizeof(extra)) == 0, 62)) return 62;
    uint32_t second_extra[4] = {UINT32_MAX, extra[1], extra[2], extra[3]};
    if (!check(syscall(SYS_set_thread_area, second_extra) == 0 && second_extra[0] != extra[0], 63)) return 63;
    uint32_t exhausted[4] = {UINT32_MAX, extra[1], extra[2], extra[3]};
    errno = 0;
    if (!check(syscall(SYS_set_thread_area, exhausted) == -1 && errno == ESRCH, 64)) return 64;
    extra[1] = extra[2] = 0; extra[3] = 0x28;
    second_extra[1] = second_extra[2] = 0; second_extra[3] = 0x28;
    if (!check(syscall(SYS_set_thread_area, extra) == 0 && syscall(SYS_set_thread_area, second_extra) == 0, 65)) return 65;
    readback[0] = extra[0];
    if (!check(syscall(SYS_get_thread_area, readback) == 0 && memcmp(extra, readback, sizeof(extra)) == 0, 66)) return 66;
    uint32_t forbidden[4] = {1, 0, 0xfffff, 0x51};
    errno = 0;
    if (!check(syscall(SYS_set_thread_area, forbidden) == -1 && errno == EINVAL && local_value == 7, 67)) return 67;
    puts(PROBE_NAME ": PASS get/set_thread_area slot allocation exhaustion and descriptor validation");
#endif
    int result = 0;
    int local_word = 0;
    wait_word = &local_word;
    pthread_t thread;
    int error = pthread_create(&thread, NULL, worker, &result);
    if (!check(error == 0, 22)) { printf("pthread_create error=%d\n", error); return 22; }
    if (!check(wake_worker(wait_word), 35)) return 35;
    void *returned = NULL;
    if (!check(pthread_join(thread, &returned) == 0, 23)) return 23;
    if (!check(returned == &result && result == 42 && local_value == 7, 24)) return 24;
    puts(PROBE_NAME ": PASS pthread create join and independent TLS");
    int shared_fd = memfd_create("futex-alias", MFD_CLOEXEC);
    if (!check(shared_fd >= 0 && ftruncate(shared_fd, 4096) == 0, 36)) return 36;
    int *alias = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, shared_fd, 0);
    wait_word = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, shared_fd, 0);
    if (!check(alias != MAP_FAILED && wait_word != MAP_FAILED && alias != wait_word, 37)) return 37;
    *alias = 0;
    atomic_store(&ready, 0);
    atomic_store(&finished, 0);
    result = 0;
    if (!check(pthread_create(&thread, NULL, worker, &result) == 0, 38)) return 38;
    if (!check(wake_worker(alias), 39)) return 39;
    returned = NULL;
    if (!check(pthread_join(thread, &returned) == 0 && returned == &result && result == 42, 40)) return 40;
    // Protection changes must preserve shared-frame ownership and the alias.
    if (!check(mprotect(alias, 4096, PROT_NONE) == 0 && mprotect(alias, 4096, PROT_READ) == 0, 41)) return 41;
    if (!check(munmap(alias, 4096) == 0, 42)) return 42;
    *wait_word = 9;
    if (!check(*wait_word == 9 && munmap(wait_word, 4096) == 0 && close(shared_fd) == 0, 43)) return 43;
    int timed_word = 0;
    const struct timespec bad_time = {-1, 0};
    errno = 0;
    if (!check(syscall(SYS_futex, &timed_word, FUTEX_WAIT, 0, &bad_time, NULL, 0) == -1 && errno == EINVAL, 44)) return 44;
    clock_gettime(CLOCK_MONOTONIC, &before);
    errno = 0;
    if (!check(syscall(SYS_futex, &timed_word, FUTEX_WAIT_BITSET, 0, &before, NULL, FUTEX_BITSET_MATCH_ANY) == -1 && errno == ETIMEDOUT, 45)) return 45;
    puts(PROBE_NAME ": PASS futex unchanged wake zero-count shared aliases and deadlines");
    for (int iteration = 0; iteration < 80; iteration++) {
        if (!check(pthread_create(&thread, NULL, immediate_worker, &result) == 0, 46)) return 46;
        if (!check(pthread_join(thread, &returned) == 0 && returned == &result, 47)) return 47;
    }
    puts(PROBE_NAME ": PASS 80 immediate thread exits and repeated joins");
    pthread_t concurrent[4];
    for (int i = 0; i < 4; i++) {
        if (!check(pthread_create(&concurrent[i], NULL, tls_worker, (void *)(uintptr_t)(i + 100)) == 0, 68)) return 68;
    }
    for (int i = 0; i < 4; i++) {
        if (!check(pthread_join(concurrent[i], &returned) == 0 && returned == (void *)(uintptr_t)(i + 100), 69)) return 69;
    }
    if (!check(local_value == 7, 70)) return 70;
    puts(PROBE_NAME ": PASS four concurrent TLS owners across 4096 scheduler yields");
    // Responses race the poller's readiness check and watch registration.
    // Untimed polling must not lose the final write and sleep indefinitely.
    int gate[2], response[2];
    if (!check(pipe(gate) == 0 && pipe(response) == 0, 48)) return 48;
    struct poll_roundtrip channels = {gate[0], response[1]};
    if (!check(pthread_create(&thread, NULL, poll_worker, &channels) == 0, 49)) return 49;
    for (int round = 0; round < 256; round++) {
        unsigned char byte = (unsigned char)round, received;
        struct pollfd watched = {response[0], POLLIN, 0};
        if (!check(write(gate[1], &byte, 1) == 1 && poll(&watched, 1, -1) == 1 &&
            (watched.revents & POLLIN) && read(response[0], &received, 1) == 1 && received == byte, 50)) return 50;
    }
    if (!check(pthread_join(thread, &returned) == 0 && returned == &channels, 51)) return 51;
    close(gate[0]); close(gate[1]); close(response[0]); close(response[1]);
    puts(PROBE_NAME ": PASS 256 concurrent pipe/poll round trips without lost notifications");
    int semaphore_result = semaphore_checks();
    if (semaphore_result != 0) return semaphore_result;
    int eventfd_result = eventfd_checks();
    if (eventfd_result != 0) return eventfd_result;
#endif
    puts(PROBE_NAME ": PASS allocation files positional-read clock sleep");
#if !defined(__i386__) && !defined(PROBE_THREADS) && !defined(PROBE_DYNAMIC)
    return proc_exec_start(argv[0]);
#endif
    return 0;
}
