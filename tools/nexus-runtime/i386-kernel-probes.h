#if defined(__i386__)
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

    puts(PROBE_NAME ": PASS i386 sysinfo and robust-list ABI layouts");
    return 1;
}
#else
static int i386_kernel_abi_probe(void) { return 1; }
#endif
