/* Retained per-thread alternate signal-stack configuration. */
static int signal_stack_probe(void) {
    stack_t original, requested, current, disabled;
    void *memory = mmap(NULL, 8192, PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (!check(memory != MAP_FAILED, 322)) return 0;
    if (!check(sigaltstack(NULL, &original) == 0, 323)) return 0;

    requested.ss_sp = memory;
    requested.ss_flags = 0;
    requested.ss_size = 8192;
    if (!check(sigaltstack(&requested, &current) == 0, 324)) return 0;
    if (!check(current.ss_sp == original.ss_sp && current.ss_flags == original.ss_flags &&
        current.ss_size == original.ss_size, 325)) return 0;
    if (!check(sigaltstack(NULL, &current) == 0 && current.ss_sp == memory &&
        current.ss_size == 8192 && current.ss_flags == 0, 326)) return 0;

    disabled.ss_sp = NULL;
    disabled.ss_flags = SS_DISABLE;
    disabled.ss_size = 0;
    if (!check(sigaltstack(&disabled, &current) == 0 && current.ss_sp == memory &&
        current.ss_size == 8192 && current.ss_flags == 0, 327)) return 0;
    if (!check(sigaltstack(NULL, &current) == 0 && current.ss_flags == SS_DISABLE, 328)) return 0;

    requested.ss_size = 1;
    errno = 0;
    if (!check(sigaltstack(&requested, NULL) == -1 && errno == ENOMEM, 329)) return 0;
    if (!check(munmap(memory, 8192) == 0, 332)) return 0;
    puts(PROBE_NAME ": PASS sigaltstack install query disable and minimum size");
    return 1;
}
