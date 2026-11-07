#ifdef __i386__
#include <netinet/in.h>
#include <sys/ioctl.h>
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
    struct iovec send_vec[2] = {{sent, 6}, {sent + 6, sizeof(sent) - 6}};
    struct msghdr send_hdr = {.msg_iov = send_vec, .msg_iovlen = 2};
    uint32_t sendmsg_words[3] = {(uint32_t)pair[0], (uint32_t)(uintptr_t)&send_hdr, 0};
    int queued = 0;
    memset(received, 0, sizeof(received));
    struct iovec recv_vec = {received, sizeof(received)};
    struct msghdr recv_hdr = {.msg_iov = &recv_vec, .msg_iovlen = 1};
    uint32_t recvmsg_words[3] = {(uint32_t)pair[1], (uint32_t)(uintptr_t)&recv_hdr, 0};
    if (!check(socket32_call(16, sendmsg_words) == sizeof(sent) &&
        ioctl(pair[1], FIONREAD, &queued) == 0 && queued == (int)sizeof(sent) &&
        socket32_call(17, recvmsg_words) == sizeof(sent) &&
        !memcmp(received, sent, sizeof(sent)), 362)) return 0;
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

    // Exercise AF_INET UDP/TCP socket options, ioctl(FIONBIO), getsockname,
    // getpeername, and RFC 3484 port-0 / AF_UNSPEC destination probing.
    uint32_t udp_create[3] = {AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0};
    int udp = socket32_call(1, udp_create);
    int sock_type = 0, sock_err = -1, nonblock = 1;
    socklen_t opt_len = sizeof(int);
    uint32_t getsockopt_words[5] = {(uint32_t)udp, SOL_SOCKET, SO_TYPE,
        (uint32_t)(uintptr_t)&sock_type, (uint32_t)(uintptr_t)&opt_len};
    if (!check(udp >= 3 && socket32_call(15, getsockopt_words) == 0 &&
        sock_type == SOCK_DGRAM && opt_len == sizeof(int), 363)) return 0;
    getsockopt_words[2] = SO_ERROR;
    getsockopt_words[3] = (uint32_t)(uintptr_t)&sock_err;
    if (!check(socket32_call(15, getsockopt_words) == 0 && sock_err == 0, 364)) return 0;
    if (!check(ioctl(udp, FIONBIO, &nonblock) == 0 && (fcntl(udp, F_GETFL) & O_NONBLOCK) != 0, 365)) return 0;
    nonblock = 0;
    if (!check(ioctl(udp, FIONBIO, &nonblock) == 0 && (fcntl(udp, F_GETFL) & O_NONBLOCK) == 0, 366)) return 0;
    struct sockaddr_in probe_dst = {.sin_family = AF_INET, .sin_port = 0};
    probe_dst.sin_addr.s_addr = htonl(0x0a000203U);
    uint32_t udp_connect[3] = {(uint32_t)udp, (uint32_t)(uintptr_t)&probe_dst, sizeof(probe_dst)};
    struct sockaddr_in local_in = {0}, peer_in = {0};
    socklen_t addr_len = sizeof(local_in);
    uint32_t sockname_words[3] = {(uint32_t)udp, (uint32_t)(uintptr_t)&local_in, (uint32_t)(uintptr_t)&addr_len};
    uint32_t peername_words[3] = {(uint32_t)udp, (uint32_t)(uintptr_t)&peer_in, (uint32_t)(uintptr_t)&addr_len};
    if (!check(socket32_call(3, udp_connect) == 0 &&
        socket32_call(6, sockname_words) == 0 && local_in.sin_family == AF_INET && addr_len == sizeof(local_in) &&
        socket32_call(7, peername_words) == 0 && peer_in.sin_family == AF_INET &&
        peer_in.sin_addr.s_addr == probe_dst.sin_addr.s_addr, 367)) return 0;
    struct sockaddr unspec = {.sa_family = AF_UNSPEC};
    udp_connect[1] = (uint32_t)(uintptr_t)&unspec;
    udp_connect[2] = sizeof(unspec);
    errno = 0;
    if (!check(socket32_call(3, udp_connect) == 0 &&
        socket32_call(7, peername_words) == -1 && errno == ENOTCONN, 368)) return 0;
    close(udp);
    uint32_t tcp_create[3] = {AF_INET, SOCK_STREAM | SOCK_CLOEXEC, IPPROTO_TCP};
    int tcp = socket32_call(1, tcp_create);
    struct sockaddr_in any_in = {.sin_family = AF_INET};
    uint32_t tcp_bind[3] = {(uint32_t)tcp, (uint32_t)(uintptr_t)&any_in, sizeof(any_in)};
    if (!check(tcp >= 3 && socket32_call(2, tcp_bind) == 0 && close(tcp) == 0, 369)) return 0;
    puts(PROBE_NAME ": PASS i386 socketcall packed words stream IPC and checked peer buffers");
    return 1;
}
#endif
