/* Behavioral checks through real libc and, on i386, both Linux IPC ABIs. */
#include <sys/sem.h>

union probe_semun { int val; struct semid_ds *buf; unsigned short *array; struct seminfo *info; };
struct semaphore_worker { int id, delta, flags, result, error; };

static void *semaphore_waiter(void *argument) {
    struct semaphore_worker *worker = argument;
    struct sembuf operation = {0, worker->delta, worker->flags};
    worker->result = semop(worker->id, &operation, 1);
    worker->error = worker->result < 0 ? errno : 0;
    return argument;
}

static int semaphore_undo_child(void *argument) {
    semaphore_waiter(argument);
    return 0;
}

static void *semaphore_metadata_reader(void *argument) {
    struct semaphore_worker *worker = argument;
    for (int round = 0; round < 1024; round++) {
        struct semid_ds info;
        union probe_semun data = {.buf = &info};
        if (semctl(worker->id, 0, IPC_STAT, data) != 0 || info.sem_nsems != 1) return NULL;
        sched_yield();
    }
    return argument;
}

static int semaphore_wait_count(int id, int command) {
    const struct timespec pause = {0, 1000000};
    for (int retry = 0; retry < 5000; retry++) {
        if (semctl(id, 0, command) == 1) return 1;
        nanosleep(&pause, NULL);
    }
    return 0;
}

static int semaphore_checks(void) {
    int id = semget(IPC_PRIVATE, 2, 0600);
    if (!check(id >= 0 && semctl(id, 0, GETVAL) == 0 && semctl(id, 1, GETVAL) == 0, 80)) return 80;
    unsigned short values[2] = {2, 0}, result[2] = {99, 99};
    union probe_semun argument = {.array = values};
    if (!check(semctl(id, 0, SETALL, argument) == 0, 81)) return 81;
    struct semid_ds info;
    memset(&info, 0xa5, sizeof(info));
    argument.buf = &info;
    if (!check(semctl(id, 0, IPC_STAT, argument) == 0 && info.sem_nsems == 2 &&
        info.sem_perm.uid == 0 && info.sem_perm.gid == 0 && (info.sem_perm.mode & 0777) == 0600 &&
        info.sem_ctime > 0 && info.sem_otime == 0, 82)) return 82;
    void *readonly = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (!check(readonly != MAP_FAILED, 124)) return 124;
    // i386 glibc converts metadata in userspace after a successful call using
    // its own temporary buffer. Test kernel output validation at the syscall
    // boundary so that conversion does not legitimately fault in userspace.
    errno = 0;
    if (!check(syscall(SYS_semctl, id, 0, IPC_STAT | 0x100, readonly) == -1 && errno == EFAULT &&
        munmap(readonly, 4096) == 0, 125)) return 125;
    info.sem_perm.mode = 0640;
    info.sem_perm.uid = 123;
    info.sem_perm.gid = 456;
    if (!check(semctl(id, 0, IPC_SET, argument) == 0, 83)) return 83;
    memset(&info, 0, sizeof(info));
    if (!check(semctl(id, 0, IPC_STAT, argument) == 0 && (info.sem_perm.mode & 0777) == 0640 &&
        info.sem_perm.uid == 123 && info.sem_perm.gid == 456 && info.sem_perm.cuid == 0, 84)) return 84;
    // The first decrease must roll back when the second element would block.
    struct sembuf operations[2] = {{0, -1, SEM_UNDO}, {1, -1, IPC_NOWAIT}};
    errno = 0;
    if (!check(semop(id, operations, 2) == -1 && errno == EAGAIN && semctl(id, 0, GETVAL) == 2, 85)) return 85;
    operations[1] = (struct sembuf){0, 1, SEM_UNDO};
    if (!check(semop(id, operations, 2) == 0 && semctl(id, 0, GETVAL) == 2 &&
        semctl(id, 0, GETPID) == getpid(), 86)) return 86;
    argument.buf = &info;
    if (!check(semctl(id, 0, IPC_STAT, argument) == 0 && info.sem_otime > 0, 87)) return 87;
    argument.array = result;
    if (!check(semctl(id, 0, GETALL, argument) == 0 && result[0] == 2 && result[1] == 0, 88)) return 88;
    values[0] = 3; values[1] = 32768; argument.array = values;
    errno = 0;
    if (!check(semctl(id, 0, SETALL, argument) == -1 && errno == ERANGE && semctl(id, 0, GETVAL) == 2, 89)) return 89;
    argument.val = -1; errno = 0;
    if (!check(semctl(id, 0, SETVAL, argument) == -1 && errno == ERANGE, 90)) return 90;
    struct sembuf invalid_index = {2, 1, IPC_NOWAIT}; errno = 0;
    if (!check(semop(id, &invalid_index, 1) == -1 && errno == EFBIG, 91)) return 91;
    struct sembuf acquire_empty = {1, -1, 0};
    const struct timespec zero_timeout = {0, 0}, bad_timeout = {-1, 0}, short_timeout = {0, 2000000};
    errno = 0;
    if (!check(semtimedop(id, &acquire_empty, 1, &zero_timeout) == -1 && errno == EAGAIN, 92)) return 92;
    errno = 0;
    if (!check(semtimedop(id, &acquire_empty, 1, &bad_timeout) == -1 && errno == EINVAL, 93)) return 93;
    struct timespec before_wait, after_wait;
    clock_gettime(CLOCK_MONOTONIC, &before_wait);
    errno = 0;
    if (!check(semtimedop(id, &acquire_empty, 1, &short_timeout) == -1 && errno == EAGAIN &&
        semctl(id, 1, GETNCNT) == 0, 94)) return 94;
    clock_gettime(CLOCK_MONOTONIC, &after_wait);
    long long elapsed = (after_wait.tv_sec - before_wait.tv_sec) * 1000000000LL + after_wait.tv_nsec - before_wait.tv_nsec;
    if (!check(elapsed >= 2000000LL, 95)) return 95;
#ifdef __i386__
    // Old ipc dereferences a union word; direct semctl takes that word itself.
    uint32_t union_word = 5;
    if (!check(syscall(SYS_ipc, 3, id, 0, SETVAL | 0x100, &union_word, 0) == 0 &&
        syscall(SYS_semctl, id, 0, GETVAL, 0) == 5, 96)) return 96;
    unsigned char old_info[44];
    memset(old_info, 0xa5, sizeof(old_info));
    union_word = (uint32_t)(uintptr_t)old_info;
    if (!check(syscall(SYS_ipc, 3, id, 0, IPC_STAT, &union_word, 0) == 0, 97)) return 97;
    uint16_t old_count, old_uid, old_gid, old_mode;
    memcpy(&old_count, old_info + 40, 2); memcpy(&old_uid, old_info + 4, 2);
    memcpy(&old_gid, old_info + 6, 2); memcpy(&old_mode, old_info + 12, 2);
    if (!check(old_count == 2 && old_uid == 123 && old_gid == 456 && old_mode == 0640, 98)) return 98;
    int32_t legacy_zero[2] = {0, 0};
    int64_t time64_zero[2] = {0, 0}; errno = 0;
    if (!check(syscall(SYS_ipc, 4, id, 1, 0, &acquire_empty, legacy_zero) == -1 && errno == EAGAIN, 99)) return 99;
    errno = 0;
    if (!check(syscall(SYS_semtimedop_time64, id, &acquire_empty, 1, time64_zero) == -1 && errno == EAGAIN, 100)) return 100;
    puts(PROBE_NAME ": PASS old i386 ipc and direct semaphore/time64 ABI");
#endif
    // An intermediate undo overflow must fail even if the final adjustment
    // would be unchanged. SETVAL then clears accumulated undo adjustments.
    argument.val = 32767;
    if (!check(semctl(id, 0, SETVAL, argument) == 0, 101)) return 101;
    struct sembuf undo_limit = {0, -32767, SEM_UNDO};
    struct sembuf deposit = {0, 1, 0};
    if (!check(semop(id, &undo_limit, 1) == 0 && semop(id, &deposit, 1) == 0, 102)) return 102;
    operations[0] = (struct sembuf){0, -1, SEM_UNDO}; operations[1] = (struct sembuf){0, 1, SEM_UNDO}; errno = 0;
    if (!check(semop(id, operations, 2) == -1 && errno == ERANGE && semctl(id, 0, GETVAL) == 1, 103)) return 103;
    operations[1] = (struct sembuf){1, -1, IPC_NOWAIT}; errno = 0;
    if (!check(semop(id, operations, 2) == -1 && errno == ERANGE && semctl(id, 0, GETVAL) == 1, 129)) return 129;
    operations[1] = (struct sembuf){0, 1, SEM_UNDO};
    argument.val = 0;
    if (!check(semctl(id, 0, SETVAL, argument) == 0 && semop(id, &deposit, 1) == 0 && semop(id, operations, 2) == 0, 104)) return 104;
    if (!check(semctl(id, 0, IPC_RMID) == 0, 105)) return 105;
    int named = semget((key_t)0x4e58534d, 1, IPC_CREAT | IPC_EXCL | 0600);
    if (!check(named >= 0 && named != id && semget((key_t)0x4e58534d, 0, 0) == named, 106)) return 106;
    errno = 0;
    if (!check(semget((key_t)0x4e58534d, 1, IPC_CREAT | IPC_EXCL | 0600) == -1 && errno == EEXIST, 107)) return 107;
    errno = 0;
    if (!check(semget((key_t)0x4e58534d, 2, 0) == -1 && errno == EINVAL, 108)) return 108;
    errno = 0;
    if (!check(semctl(id, 0, GETVAL) == -1 && errno == EINVAL && semctl(named, 0, IPC_RMID) == 0, 109)) return 109;
    puts(PROBE_NAME ": PASS semaphore metadata atomic rollback limits deadlines and stale IDs");

    id = semget(IPC_PRIVATE, 1, 0600);
    pthread_t thread;
    void *returned;
    struct semaphore_worker worker = {id, -1, 0, -99, -99};
    if (!check(id >= 0 && pthread_create(&thread, NULL, semaphore_waiter, &worker) == 0 &&
        semaphore_wait_count(id, GETNCNT), 110)) return 110;
    deposit = (struct sembuf){0, 1, 0};
    if (!check(semop(id, &deposit, 1) == 0 && pthread_join(thread, &returned) == 0 &&
        returned == &worker && worker.result == 0 && semctl(id, 0, GETNCNT) == 0, 111)) return 111;
    argument.val = 1;
    if (!check(semctl(id, 0, SETVAL, argument) == 0, 112)) return 112;
    worker.delta = 0;
    if (!check(pthread_create(&thread, NULL, semaphore_waiter, &worker) == 0 &&
        semaphore_wait_count(id, GETZCNT), 113)) return 113;
    argument.val = 0;
    if (!check(semctl(id, 0, SETVAL, argument) == 0 && pthread_join(thread, &returned) == 0 &&
        returned == &worker && worker.result == 0 && semctl(id, 0, GETZCNT) == 0, 114)) return 114;
    worker.delta = -1;
    if (!check(pthread_create(&thread, NULL, semaphore_waiter, &worker) == 0 &&
        semaphore_wait_count(id, GETNCNT), 115)) return 115;
    if (!check(semctl(id, 0, IPC_RMID) == 0 && pthread_join(thread, &returned) == 0 &&
        returned == &worker && worker.result == -1 && worker.error == EIDRM, 116)) return 116;
    puts(PROBE_NAME ": PASS semaphore negative/zero wait wake counters and removal");

    id = semget(IPC_PRIVATE, 1, 0600);
    argument.val = 1;
    if (!check(id >= 0 && semctl(id, 0, SETVAL, argument) == 0, 117)) return 117;
    worker = (struct semaphore_worker){id, -1, SEM_UNDO, -99, -99};
    if (!check(pthread_create(&thread, NULL, semaphore_waiter, &worker) == 0 &&
        pthread_join(thread, &returned) == 0 && worker.result == 0 && semctl(id, 0, GETVAL) == 0, 118)) return 118;
    // libc pthreads share CLONE_SYSVSEM: their exit must not apply the list.
    // A clone without that flag has independent ownership, undone on its exit.
    if (!check(semctl(id, 0, SETVAL, argument) == 0, 119)) return 119;
#ifdef __GLIBC__
    // musl deliberately disallows CLONE_THREAD in its public clone wrapper.
    // Exercise the independent list through glibc's genuine clone wrapper.
    void *stack = mmap(NULL, 1024 * 1024, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (!check(stack != MAP_FAILED, 120)) return 120;
    _Atomic int child_tid = -1;
    int flags = CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD |
        CLONE_PARENT_SETTID | CLONE_CHILD_CLEARTID;
    if (!check(clone(semaphore_undo_child, (char *)stack + 1024 * 1024, flags, &worker,
        &child_tid, NULL, &child_tid) >= 0, 121)) return 121;
    const struct timespec join_limit = {5, 0};
    while (atomic_load(&child_tid) != 0) {
        int observed = atomic_load(&child_tid);
        if (observed == 0) break;
        long joined = syscall(SYS_futex, &child_tid, FUTEX_WAIT, observed, &join_limit, NULL, 0);
        if (!check(joined == 0 || (joined == -1 && errno == EAGAIN), 122)) return 122;
    }
    if (!check(worker.result == 0 && semctl(id, 0, GETVAL) == 1 && semctl(id, 0, GETPID) == getpid() &&
        munmap(stack, 1024 * 1024) == 0 && semctl(id, 0, IPC_RMID) == 0, 123)) return 123;
    puts(PROBE_NAME ": PASS shared and independent SEM_UNDO thread exit ownership");
#else
    if (!check(semctl(id, 0, IPC_RMID) == 0, 123)) return 123;
    puts(PROBE_NAME ": PASS shared SEM_UNDO thread exit ownership");
#endif
    // Race metadata copying (registry lock) against clone's shared-undo
    // registration (scheduler lock). This catches inverted lock ordering.
    id = semget(IPC_PRIVATE, 1, 0600); worker.id = id;
    pthread_t reader;
    if (!check(id >= 0 && pthread_create(&reader, NULL, semaphore_metadata_reader, &worker) == 0, 126)) return 126;
    for (int round = 0; round < 80; round++) {
        if (!check(pthread_create(&thread, NULL, immediate_worker, &worker) == 0 &&
            pthread_join(thread, &returned) == 0 && returned == &worker, 127)) return 127;
    }
    if (!check(pthread_join(reader, &returned) == 0 && returned == &worker && semctl(id, 0, IPC_RMID) == 0, 128)) return 128;
    puts(PROBE_NAME ": PASS semaphore metadata racing 80 clones without lock inversion");
    return 0;
}
