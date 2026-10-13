#ifdef __i386__
#include <sys/socket.h>
#include <sys/un.h>
static long socket32_call(unsigned operation, uint32_t *arguments) {
    return syscall(102, operation, arguments);
}
static int socket32_probe(void) {
    int pair[2];
    uint32_t words[4] = {AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, (uint32_t)(uintptr_t)pair};
    if (!check(socket32_call(8, words) == 0 && pair[0] >= 3 && pair[1] >= 3 &&
        fcntl(pair[0], F_GETFD) == FD_CLOEXEC && fcntl(pair[1], F_GETFD) == FD_CLOEXEC, 350)) return 0;
    char sent[] = "socketcall32", received[32];
    uint32_t send_words[4] = {(uint32_t)pair[0], (uint32_t)(uintptr_t)sent, sizeof(sent), 0};
    uint32_t recv_words[4] = {(uint32_t)pair[1], (uint32_t)(uintptr_t)received, sizeof(received), 0};
    if (!check(socket32_call(9, send_words) == sizeof(sent) && socket32_call(10, recv_words) == sizeof(sent) &&
        !memcmp(received, sent, sizeof(sent)), 351)) return 0;
    recv_words[3] = MSG_DONTWAIT;
    errno = 0;
    if (!check(socket32_call(10, recv_words) == -1 && errno == EAGAIN, 352)) return 0;
    void *readonly = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    words[3] = (uint32_t)(uintptr_t)readonly;
    errno = 0;
    if (!check(readonly != MAP_FAILED && socket32_call(8, words) == -1 && errno == EFAULT, 353)) return 0;
    if (!check(socket32_call(9, send_words) == sizeof(sent), 354)) return 0;
    recv_words[1] = (uint32_t)(uintptr_t)readonly;
    errno = 0;
    if (!check(socket32_call(10, recv_words) == -1 && errno == EFAULT, 355)) return 0;
    recv_words[1] = (uint32_t)(uintptr_t)received;
    if (!check(socket32_call(10, recv_words) == sizeof(sent) && !memcmp(received, sent, sizeof(sent)), 356)) return 0;
    errno = 0;
    if (!check(socket32_call(8, (uint32_t *)1) == -1 && errno == EFAULT, 357)) return 0;
    munmap(readonly, 4096);
    close(pair[0]); close(pair[1]);
    // Exercise bind/listen/connect/accept and a four-byte socklen at the end
    // of a page, so treating that pointer as a native eight-byte word fails.
    uint32_t create[3] = {AF_UNIX, SOCK_STREAM, 0};
    int server = socket32_call(1, create), client = socket32_call(1, create);
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    snprintf(address.sun_path + 1, sizeof(address.sun_path) - 1, "nexus-socket32-%ld", (long)getpid());
    uint32_t bind_words[3] = {(uint32_t)server, (uint32_t)(uintptr_t)&address,
        (uint32_t)(2 + 1 + strlen(address.sun_path + 1))};
    uint32_t listen_words[2] = {(uint32_t)server, 1};
    if (!check(server >= 3 && client >= 3 && socket32_call(2, bind_words) == 0 && socket32_call(4, listen_words) == 0, 358)) return 0;
    bind_words[0] = client;
    if (!check(socket32_call(3, bind_words) == 0, 359)) return 0;
    unsigned char *page = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (!check(page != MAP_FAILED && mprotect(page + 4096, 4096, PROT_NONE) == 0, 360)) return 0;
    uint32_t *length = (uint32_t *)(page + 4092);
    *length = sizeof(address);
    uint32_t accept_words[3] = {(uint32_t)server, (uint32_t)(uintptr_t)&address, (uint32_t)(uintptr_t)length};
    int accepted = socket32_call(5, accept_words);
    if (!check(accepted >= 3 && *length == 2 && address.sun_family == AF_UNIX, 361)) return 0;
    munmap(page, 8192);
    close(accepted); close(client); close(server);
    puts(PROBE_NAME ": PASS i386 socketcall packed words stream IPC and checked peer buffers");
    return 1;
}
#endif
