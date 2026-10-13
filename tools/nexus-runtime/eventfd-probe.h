#include <sys/eventfd.h>

struct eventfd_worker { int input, output; _Atomic int ready; int result; };

static void *eventfd_echo(void *argument) {
    struct eventfd_worker *worker = argument;
    for (int round = 1; round <= 128; round++) {
        eventfd_t value;
        if (eventfd_read(worker->input, &value) != 0 || value != (eventfd_t)round ||
            eventfd_write(worker->output, value) != 0) return NULL;
    }
    return argument;
}

static void *eventfd_full_writer(void *argument) {
    struct eventfd_worker *worker = argument;
    atomic_store(&worker->ready, 1);
    worker->result = eventfd_write(worker->input, 1);
    return argument;
}

static int eventfd_checks(void) {
    errno = 0;
    if (!check(eventfd(0, 0x10) == -1 && errno == EINVAL, 130)) return 130;
    int legacy = syscall(SYS_eventfd, 7);
    eventfd_t value;
    if (!check(legacy >= 0 && eventfd_read(legacy, &value) == 0 && value == 7 && close(legacy) == 0, 131)) return 131;
    int fd = eventfd(1, EFD_NONBLOCK | EFD_CLOEXEC);
    if (!check(fd >= 0 && fcntl(fd, F_GETFD) == FD_CLOEXEC &&
        (fcntl(fd, F_GETFL) & (O_ACCMODE | O_NONBLOCK)) == (O_RDWR | O_NONBLOCK), 132)) return 132;
    struct pollfd watched = {fd, POLLIN | POLLOUT, 0};
    if (!check(poll(&watched, 1, 0) == 1 && (watched.revents & (POLLIN | POLLOUT)) == (POLLIN | POLLOUT), 133)) return 133;
    int alias = dup(fd), exec_alias = fcntl(fd, F_DUPFD_CLOEXEC, 3);
    if (!check(alias >= 0 && exec_alias >= 0 && fcntl(alias, F_GETFD) == 0 &&
        fcntl(exec_alias, F_GETFD) == FD_CLOEXEC && eventfd_write(alias, 2) == 0, 134)) return 134;
    uint64_t output[2] = {0, 0xaabbccddeeff0011ULL};
    if (!check(read(exec_alias, output, sizeof(output)) == 8 && output[0] == 3 && output[1] == 0xaabbccddeeff0011ULL, 135)) return 135;
    errno = 0;
    if (!check(eventfd_read(fd, &value) == -1 && errno == EAGAIN, 136)) return 136;
    value = UINT64_MAX; errno = 0;
    if (!check(write(fd, &value, 8) == -1 && errno == EINVAL, 137)) return 137;
    errno = 0;
    if (!check(write(fd, output, sizeof(output)) == -1 && errno == EINVAL, 138)) return 138;
    errno = 0;
    if (!check(read(fd, &value, 7) == -1 && errno == EINVAL, 139)) return 139;
    if (!check(eventfd_write(fd, 2) == 0, 140)) return 140;
    errno = 0;
    if (!check(read(fd, (void *)1, 8) == -1 && errno == EFAULT, 141)) return 141;
    void *readonly = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (!check(readonly != MAP_FAILED, 142)) return 142;
    errno = 0;
    if (!check(read(fd, readonly, 8) == -1 && errno == EFAULT && eventfd_read(fd, &value) == 0 &&
        value == 2 && munmap(readonly, 4096) == 0, 143)) return 143;
    if (!check(fcntl(alias, F_SETFL, 0) == 0 && (fcntl(fd, F_GETFL) & O_NONBLOCK) == 0 &&
        fcntl(fd, F_SETFL, O_NONBLOCK) == 0 && (fcntl(exec_alias, F_GETFL) & O_NONBLOCK), 144)) return 144;
    if (!check(eventfd_write(fd, UINT64_MAX - 1) == 0 && poll(&watched, 1, 0) == 1 &&
        (watched.revents & POLLIN) && !(watched.revents & POLLOUT), 145)) return 145;
    errno = 0;
    if (!check(eventfd_write(fd, 1) == -1 && errno == EAGAIN && eventfd_write(fd, 0) == 0, 146)) return 146;
    if (!check(close(fd) == 0 && close(exec_alias) == 0 && eventfd_read(alias, &value) == 0 &&
        value == UINT64_MAX - 1 && close(alias) == 0, 147)) return 147;
    int semaphore = eventfd(3, EFD_SEMAPHORE | EFD_NONBLOCK);
    if (!check(semaphore >= 0, 148)) return 148;
    for (int i = 0; i < 3; i++) {
        if (!check(eventfd_read(semaphore, &value) == 0 && value == 1, 149)) return 149;
    }
    errno = 0;
    if (!check(eventfd_read(semaphore, &value) == -1 && errno == EAGAIN && close(semaphore) == 0, 150)) return 150;
    puts(PROBE_NAME ": PASS eventfd counter semaphore limits buffer validation and shared flags");

    pthread_t thread;
    void *returned;
    struct eventfd_worker worker = {eventfd(0, 0), eventfd(0, 0), 0, -99};
    int epoll = epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event interest = {.events = EPOLLIN, .data.u64 = 0x8877665544332211ULL};
    struct epoll_event actual;
    if (!check(worker.input >= 0 && worker.output >= 0 && epoll >= 0 &&
        epoll_ctl(epoll, EPOLL_CTL_ADD, worker.output, &interest) == 0 &&
        epoll_wait(epoll, &actual, 1, 0) == 0 && pthread_create(&thread, NULL, eventfd_echo, &worker) == 0, 151)) return 151;
    for (int round = 1; round <= 128; round++) {
        if (!check(eventfd_write(worker.input, round) == 0 && epoll_wait(epoll, &actual, 1, 5000) == 1 &&
            actual.data.u64 == interest.data.u64 && (actual.events & EPOLLIN) &&
            eventfd_read(worker.output, &value) == 0 && value == (eventfd_t)round, 152)) return 152;
    }
    if (!check(pthread_join(thread, &returned) == 0 && returned == &worker &&
        close(worker.input) == 0 && close(worker.output) == 0 && close(epoll) == 0, 153)) return 153;
    worker.input = eventfd(0, 0);
    if (!check(worker.input >= 0 && eventfd_write(worker.input, UINT64_MAX - 1) == 0 &&
        pthread_create(&thread, NULL, eventfd_full_writer, &worker) == 0, 154)) return 154;
    while (!atomic_load(&worker.ready)) sched_yield();
    const struct timespec pause = {0, 10000000};
    nanosleep(&pause, NULL);
    if (!check(eventfd_read(worker.input, &value) == 0 && value == UINT64_MAX - 1 &&
        pthread_join(thread, &returned) == 0 && returned == &worker && worker.result == 0 &&
        eventfd_read(worker.input, &value) == 0 && value == 1 && close(worker.input) == 0, 155)) return 155;
    puts(PROBE_NAME ": PASS 128 concurrent eventfd/epoll round trips and blocking overflow wake");
    return 0;
}
