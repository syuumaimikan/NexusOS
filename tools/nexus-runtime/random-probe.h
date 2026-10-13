static int random_probe(void) {
    unsigned char first[64], second[64];
    struct stat named, opened;
    int fd = open("/dev/urandom", O_RDONLY);
    if (!check(fd >= 3 && stat("/dev/urandom", &named) == 0 && fstat(fd, &opened) == 0 &&
        S_ISCHR(named.st_mode) && S_ISCHR(opened.st_mode) && named.st_ino == opened.st_ino &&
        major(opened.st_rdev) == 1 && minor(opened.st_rdev) == 9, 330)) return 0;
    if (!check(read(fd, first, sizeof(first)) == sizeof(first) &&
        read(fd, second, sizeof(second)) == sizeof(second) && memcmp(first, second, sizeof(first)), 331)) return 0;
    int copy = dup(fd);
    if (!check(copy >= 3 && (fcntl(fd, F_GETFL) & O_ACCMODE) == O_RDONLY &&
        fcntl(copy, F_SETFL, O_NONBLOCK) == 0 && (fcntl(fd, F_GETFL) & O_NONBLOCK), 332)) return 0;
    struct pollfd ready = {fd, POLLIN, 0};
    if (!check(poll(&ready, 1, 0) == 1 && (ready.revents & POLLIN), 333)) return 0;
    errno = 0;
    if (!check(write(fd, first, sizeof(first)) == -1 && errno == EBADF, 334)) return 0;
    void *readonly = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    errno = 0;
    if (!check(readonly != MAP_FAILED && read(fd, readonly, 64) == -1 && errno == EFAULT &&
        syscall(SYS_read, fd, (void *)1, 0) == 0, 335)) return 0;
    munmap(readonly, 4096);
    if (!check(close(fd) == 0 && read(copy, second, 17) == 17 && close(copy) == 0, 336)) return 0;
    char link[128];
    snprintf(link, sizeof(link), "/tmp/random-link-%ld", (long)getpid());
    if (!check(symlink("/dev/urandom", link) == 0, 337)) return 0;
    fd = open(link, O_RDONLY | O_NOFOLLOW);
    if (!check(fd == -1 && errno == ELOOP, 338)) return 0;
    fd = open(link, O_RDONLY);
    if (!check(fd >= 3 && read(fd, second, 31) == 31 && close(fd) == 0 && unlink(link) == 0, 339)) return 0;
    errno = 0;
    if (!check(readlink("/dev/urandom", link, sizeof(link)) == -1 && errno == EINVAL, 340)) return 0;
    puts(PROBE_NAME ": PASS native urandom character metadata reads bounds dup flags and readiness");
    return 1;
}
