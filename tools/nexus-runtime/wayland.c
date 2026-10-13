/* The transport, marshalling, dispatch, object table and callbacks are the
 * unmodified upstream libwayland. xdg-shell bindings come from its scanner. */
#define _GNU_SOURCE
#include <wayland-client.h>
#include "xdg-shell-client-protocol.h"
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
#ifdef PROBE_MESA_LINKED
#define GL_GLEXT_PROTOTYPES
#include <GL/osmesa.h>
#endif

static struct wl_display *display;
static struct wl_compositor *compositor;
static struct wl_shm *shm;
static struct wl_seat *seat;
static struct xdg_wm_base *wm;
static struct wl_surface *surface;
static struct xdg_surface *xdg_surface;
static int width = 320, height = 200, frames, releases, key_received, closed, configured;

static void die(const char *step) {
    fprintf(stderr, "libwayland-probe: FAIL %s\n", step);
    exit(1);
}
static void buffer_release(void *data, struct wl_buffer *buffer) {
    (void)data;
    releases++;
    wl_buffer_destroy(buffer);
}
static const struct wl_buffer_listener buffer_listener = {buffer_release};
static void frame_done(void *data, struct wl_callback *callback, uint32_t time) {
    (void)data; (void)time;
    frames++;
    wl_callback_destroy(callback);
    printf("libwayland-probe: frame callback %d\n", frames);
    fflush(stdout);
}
static const struct wl_callback_listener frame_listener = {frame_done};

#ifdef PROBE_MESA_LINKED
static GLuint compile_shader(GLenum kind, const char *text) {
    GLuint result = glCreateShader(kind);
    glShaderSource(result, 1, &text, NULL);
    glCompileShader(result);
    GLint compiled = 0;
    glGetShaderiv(result, GL_COMPILE_STATUS, &compiled);
    if (!compiled) die("Mesa GLSL compilation");
    return result;
}
static void draw_triangle(float depth) {
    glBegin(GL_TRIANGLES);
    glVertex3f(-0.8f, -0.8f, depth);
    glVertex3f(0.8f, -0.8f, depth);
    glVertex3f(0.0f, 0.8f, depth);
    glEnd();
}
static void draw_mesa(uint32_t *pixels) {
    OSMesaContext context = OSMesaCreateContextExt(OSMESA_BGRA, 24, 0, 0, NULL);
    if (!context || !OSMesaMakeCurrent(context, pixels, GL_UNSIGNED_BYTE, width, height)) die("Mesa shared buffer");
    const char *renderer = (const char *)glGetString(GL_RENDERER);
    if (!renderer || !strstr(renderer, "softpipe")) die("Mesa renderer");
    // OSMesa owns its internal colour attachment; initialize the background
    // through GL rather than assuming it imports the caller's existing bytes.
    const unsigned char colours[3][3] = {{0x33, 0x99, 0x66}, {0x22, 0x44, 0xee}, {0xcc, 0x55, 0x33}};
    glEnable(GL_SCISSOR_TEST);
    for (int band = 0; band < 3; band++) {
        int start = height * band / 3, end = height * (band + 1) / 3;
        glScissor(0, start, width, end - start);
        glClearColor(colours[band][0] / 255.0f, colours[band][1] / 255.0f, colours[band][2] / 255.0f, 1);
        glClear(GL_COLOR_BUFFER_BIT);
    }
    glDisable(GL_SCISSOR_TEST);
    glViewport(width / 4, height / 3, width / 2, height / 3);
    glClear(GL_DEPTH_BUFFER_BIT);
    glEnable(GL_DEPTH_TEST);
    GLuint vertex = compile_shader(GL_VERTEX_SHADER, "#version 120\nvoid main(){gl_Position=gl_Vertex;}");
    GLuint fragment = compile_shader(GL_FRAGMENT_SHADER, "#version 120\nuniform vec4 colour; void main(){gl_FragColor=colour;}");
    GLuint program = glCreateProgram();
    glAttachShader(program, vertex);
    glAttachShader(program, fragment);
    glLinkProgram(program);
    GLint linked = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) die("Mesa GLSL linking");
    glUseProgram(program);
    GLint colour = glGetUniformLocation(program, "colour");
    if (colour < 0) die("Mesa GLSL uniform");
    glUniform4f(colour, 0.9f, 0.1f, 0.05f, 1);
    draw_triangle(-0.5f);
    glUniform4f(colour, 0.1f, 0.9f, 0.5f, 1);
    draw_triangle(0.5f);
    glFinish();
    uint32_t centre = pixels[height / 2 * width + width / 2];
    if (((centre >> 16) & 255) < 210 || ((centre >> 8) & 255) > 30 || (centre & 255) > 20 || glGetError() != GL_NO_ERROR) die("Mesa depth pixels");
    glUseProgram(0);
    glDeleteProgram(program);
    glDeleteShader(vertex);
    glDeleteShader(fragment);
    OSMesaDestroyContext(context);
    puts("libwayland-probe: Mesa softpipe GLSL depth frame"); fflush(stdout);
}
#endif

static void draw(void) {
    const size_t bytes = (size_t)width * height * 4;
    int fd = memfd_create("libwayland-buffer", MFD_CLOEXEC);
    if (fd < 0 || ftruncate(fd, bytes)) die("memfd");
    uint32_t *pixels = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (pixels == MAP_FAILED) die("shared mapping");
    for (int y = 0; y < height; y++) {
        uint32_t colour = y < height / 3 ? 0xff339966 : y < height * 2 / 3 ? 0xff2244ee : 0xffcc5533;
        for (int x = 0; x < width; x++) pixels[y * width + x] = colour;
    }
#ifdef PROBE_MESA_LINKED
    draw_mesa(pixels);
#endif
    struct wl_shm_pool *pool = wl_shm_create_pool(shm, fd, bytes);
    struct wl_buffer *buffer = wl_shm_pool_create_buffer(pool, 0, width, height, width * 4, WL_SHM_FORMAT_XRGB8888);
    wl_buffer_add_listener(buffer, &buffer_listener, NULL);
    wl_shm_pool_destroy(pool);
    close(fd);
    // The memfd object survives through its mapping and the descriptor sent by
    // libwayland's outgoing queue until that queue is flushed.
    wl_surface_attach(surface, buffer, 0, 0);
    wl_surface_damage(surface, 0, 0, width, height);
    struct wl_callback *callback = wl_surface_frame(surface);
    wl_callback_add_listener(callback, &frame_listener, NULL);
    wl_surface_commit(surface);
    printf("libwayland-probe: committed %dx%d\n", width, height);
    fflush(stdout);
    munmap(pixels, bytes);
}
static void configure(void *data, struct xdg_surface *object, uint32_t serial) {
    (void)data;
    xdg_surface_ack_configure(object, serial);
    configured++;
    draw();
}
static const struct xdg_surface_listener surface_listener = {configure};
static void toplevel_configure(void *data, struct xdg_toplevel *object, int32_t w, int32_t h, struct wl_array *states) {
    (void)data; (void)object; (void)states;
    if (w > 0 && h > 0) { width = w; height = h; }
}
static void toplevel_close(void *data, struct xdg_toplevel *object) {
    (void)data; (void)object;
    closed = 1;
}
static const struct xdg_toplevel_listener toplevel_listener = {
    .configure = toplevel_configure, .close = toplevel_close
};
static void ping(void *data, struct xdg_wm_base *object, uint32_t serial) {
    (void)data;
    xdg_wm_base_pong(object, serial);
}
static const struct xdg_wm_base_listener wm_listener = {ping};
static void keymap(void *data, struct wl_keyboard *object, uint32_t format, int fd, uint32_t size) {
    (void)data; (void)object; (void)format; (void)size;
    close(fd);
}
static void enter(void *data, struct wl_keyboard *object, uint32_t serial, struct wl_surface *focus, struct wl_array *keys) {
    (void)data; (void)object; (void)serial; (void)focus; (void)keys;
    puts("libwayland-probe: keyboard focus"); fflush(stdout);
}
static void leave(void *data, struct wl_keyboard *object, uint32_t serial, struct wl_surface *focus) {
    (void)data; (void)object; (void)serial; (void)focus;
}
static void key(void *data, struct wl_keyboard *object, uint32_t serial, uint32_t time, uint32_t code, uint32_t state) {
    (void)data; (void)object; (void)serial; (void)time;
    if (code == 30 && state == WL_KEYBOARD_KEY_STATE_PRESSED) key_received = 1;
}
static void modifiers(void *data, struct wl_keyboard *object, uint32_t serial, uint32_t depressed, uint32_t latched, uint32_t locked, uint32_t group) {
    (void)data; (void)object; (void)serial; (void)depressed; (void)latched; (void)locked; (void)group;
}
static void repeat_info(void *data, struct wl_keyboard *object, int32_t rate, int32_t delay) {
    (void)data; (void)object; (void)rate; (void)delay;
}
static const struct wl_keyboard_listener keyboard_listener = {
    .keymap = keymap, .enter = enter, .leave = leave, .key = key, .modifiers = modifiers, .repeat_info = repeat_info
};
static void capabilities(void *data, struct wl_seat *object, uint32_t caps) {
    (void)data;
    if (caps & WL_SEAT_CAPABILITY_KEYBOARD) {
        struct wl_keyboard *keyboard = wl_seat_get_keyboard(object);
        wl_keyboard_add_listener(keyboard, &keyboard_listener, NULL);
    }
}
static void seat_name(void *data, struct wl_seat *object, const char *name) {
    (void)data; (void)object; (void)name;
}
static const struct wl_seat_listener seat_listener = {.capabilities = capabilities, .name = seat_name};
static void global(void *data, struct wl_registry *registry, uint32_t name, const char *interface, uint32_t version) {
    (void)data; (void)version;
    if (!strcmp(interface, "wl_compositor")) compositor = wl_registry_bind(registry, name, &wl_compositor_interface, 1);
    else if (!strcmp(interface, "wl_shm")) shm = wl_registry_bind(registry, name, &wl_shm_interface, 1);
    else if (!strcmp(interface, "wl_seat")) {
        seat = wl_registry_bind(registry, name, &wl_seat_interface, 5);
        wl_seat_add_listener(seat, &seat_listener, NULL);
    } else if (!strcmp(interface, "xdg_wm_base")) {
        wm = wl_registry_bind(registry, name, &xdg_wm_base_interface, 1);
        xdg_wm_base_add_listener(wm, &wm_listener, NULL);
    }
}
static void global_remove(void *data, struct wl_registry *registry, uint32_t name) {
    (void)data; (void)registry; (void)name;
}
static const struct wl_registry_listener registry_listener = {global, global_remove};
int main(void) {
    display = wl_display_connect("/tmp/wayland-0");
    if (!display) die("connect");
    puts("libwayland-probe: connected with upstream library"); fflush(stdout);
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &registry_listener, NULL);
    if (wl_display_roundtrip(display) < 0 || !compositor || !shm || !wm || !seat) die("registry roundtrip");
    surface = wl_compositor_create_surface(compositor);
    xdg_surface = xdg_wm_base_get_xdg_surface(wm, surface);
    xdg_surface_add_listener(xdg_surface, &surface_listener, NULL);
    struct xdg_toplevel *toplevel = xdg_surface_get_toplevel(xdg_surface);
    xdg_toplevel_add_listener(toplevel, &toplevel_listener, NULL);
    xdg_toplevel_set_title(toplevel, "Upstream libwayland on NexusOS");
    xdg_toplevel_set_app_id(toplevel, "nexus.libwayland.probe");
    wl_surface_commit(surface);
    while (!closed) if (wl_display_dispatch(display) < 0) die("event dispatch");
    if (configured < 2 || width != 240 || height != 150 || frames < 2 || releases < 2 || !key_received) die("frame resize release input acceptance");
    puts("libwayland-probe: PASS frames resize input buffer-release and close"); fflush(stdout);
    wl_display_disconnect(display);
    return 0;
}
