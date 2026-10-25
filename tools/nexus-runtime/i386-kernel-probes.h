#if defined(__i386__)
struct i386_fifo_probe_state {
    const char *path;
    const char *payload;
};

static void *i386_fifo_writer(void *opaque) {
    struct i386_fifo_probe_state *state = opaque;
    int fd = open(state->path, O_WRONLY);
    if (fd < 0) return (void *)(uintptr_t)1;
    size_t length = strlen(state->payload) + 1;
    ssize_t sent = write(fd, state->payload, length);
    int closed = close(fd);
    return sent == (ssize_t)length && closed == 0 ? NULL : (void *)(uintptr_t)1;
}

static int i386_kernel_abi_probe(void) {
    struct sysinfo information;
    if (!check(syscall(SYS_sysinfo, &information) == 0 &&
        information.mem_unit == 4096 && information.totalram != 0, 333)) return 0;

    uint32_t robust_head[3] = {0};
    uint32_t *reported_head = NULL;
    uint32_t reported_size = 0;
    robust_head[0] = (uint32_t)(uintptr_t)&robust_head[0];
    if (!check(syscall(SYS_set_robust_list, robust_head, sizeof(robust_head)) == 0, 334)) return 0;
    if (!check(syscall(SYS_get_robust_list, 0, &reported_head, &reported_size) == 0 &&
        reported_head == robust_head && reported_size == sizeof(robust_head), 335)) return 0;
    if (!check(syscall(SYS_kill, getpid(), 0) == 0, 336)) return 0;
    errno = 0;
    if (!check(syscall(SYS_kill, 0x7ffffffe, 0) == -1 && errno == ESRCH, 337)) return 0;

    uint32_t limits[2] = {0};
    if (!check(syscall(SYS_ugetrlimit, RLIMIT_NOFILE, limits) == 0 &&
        limits[0] == 1024 && limits[1] == 1024, 338)) return 0;
    limits[0] = limits[1] = 0;
    if (!check(syscall(SYS_ugetrlimit, RLIMIT_CORE, limits) == 0 &&
        limits[0] == 0x7fffffff && limits[1] == 0x7fffffff, 339)) return 0;
    if (!check(syscall(SYS_ugetrlimit, RLIMIT_STACK, limits) == 0 &&
        limits[0] == 8 * 1024 * 1024 && limits[1] == 8 * 1024 * 1024, 342)) return 0;

    puts(PROBE_NAME ": PASS i386 sysinfo, robust-list and rlimit ABI");

    const char *fifo_path = "/tmp/nexus-i386-fifo-probe";
    const char *fifo_payload = "named FIFO preserves stream bytes";
    (void)unlink(fifo_path);
    if (!check(syscall(SYS_mknodat, AT_FDCWD, fifo_path, 0010000 | 0600, 0) == 0, 343)) return 0;
    errno = 0;
    int nonblocking_writer = open(fifo_path, O_WRONLY | O_NONBLOCK);
    if (!check(nonblocking_writer == -1 && errno == ENXIO, 344)) return 0;
    struct i386_fifo_probe_state state = {fifo_path, fifo_payload};
    pthread_t writer;
    if (!check(pthread_create(&writer, NULL, i386_fifo_writer, &state) == 0, 345)) return 0;
    int reader = open(fifo_path, O_RDONLY);
    if (!check(reader >= 0, 346)) return 0;
    char received[sizeof("named FIFO preserves stream bytes")] = {0};
    if (!check(read(reader, received, sizeof(received)) == (ssize_t)sizeof(received) &&
        memcmp(received, fifo_payload, sizeof(received)) == 0, 347)) return 0;
    void *writer_result = NULL;
    if (!check(pthread_join(writer, &writer_result) == 0 && writer_result == NULL, 348)) return 0;
    if (!check(unlink(fifo_path) == 0 && close(reader) == 0, 349)) return 0;
    puts(PROBE_NAME ": PASS named FIFO creation blocking rendezvous nonblocking error stream and unlink");
    return 1;
}
#else
static int i386_kernel_abi_probe(void) { return 1; }
#endif
