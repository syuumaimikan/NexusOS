/* Linux advisory-lock checks against separate opens and shared descriptions. */
static int flock_probe(void) {
    const char *path = "flock-probe.data";
    int first = open(path, O_CREAT | O_TRUNC | O_RDWR, 0600);
    if (!check(first >= 3 && flock(first, LOCK_EX | LOCK_NB) == 0, 310)) return 0;
    int duplicate = dup(first);
    if (!check(duplicate >= 3 && flock(duplicate, LOCK_EX | LOCK_NB) == 0, 311)) return 0;

    int second = open(path, O_RDWR);
    errno = 0;
    if (!check(second >= 3 && flock(second, LOCK_SH | LOCK_NB) == -1 && errno == EWOULDBLOCK, 312)) return 0;
    if (!check(flock(duplicate, LOCK_UN) == 0 && flock(second, LOCK_SH | LOCK_NB) == 0, 313)) return 0;
    errno = 0;
    if (!check(flock(first, LOCK_EX | LOCK_NB) == -1 && errno == EWOULDBLOCK, 314)) return 0;
    if (!check(flock(second, LOCK_UN) == 0 && flock(first, LOCK_EX | LOCK_NB) == 0, 315)) return 0;

    if (!check(close(first) == 0, 316)) return 0;
    errno = 0;
    if (!check(flock(second, LOCK_SH | LOCK_NB) == -1 && errno == EWOULDBLOCK, 317)) return 0;
    if (!check(close(duplicate) == 0 && flock(second, LOCK_SH | LOCK_NB) == 0, 318)) return 0;
    errno = 0;
    if (!check(flock(second, LOCK_SH | LOCK_EX) == -1 && errno == EINVAL, 319)) return 0;
    if (!check(flock(second, LOCK_UN) == 0 && close(second) == 0 && unlink(path) == 0, 320)) return 0;

    int command_line = open("/proc/self/cmdline", O_RDONLY);
    if (!check(command_line >= 3 && flock(command_line, LOCK_SH | LOCK_NB) == 0 &&
        flock(command_line, LOCK_UN) == 0 && close(command_line) == 0, 321)) return 0;
    puts(PROBE_NAME ": PASS flock shared/exclusive conflicts dup lifetime and proc descriptors");
    return 1;
}
