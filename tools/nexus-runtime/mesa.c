#define GL_GLEXT_PROTOTYPES
#define _GNU_SOURCE
#include <GL/osmesa.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void fail(const char *step) {
    fprintf(stderr, "mesa-probe: FAIL %s\n", step);
    exit(1);
}
static void triangle(float depth) {
    glBegin(GL_TRIANGLES);
    glVertex3f(-0.8f, -0.8f, depth);
    glVertex3f(0.8f, -0.8f, depth);
    glVertex3f(0.0f, 0.8f, depth);
    glEnd();
}
static GLuint shader(GLenum kind, const char *source) {
    GLuint object = glCreateShader(kind);
    glShaderSource(object, 1, &source, NULL);
    glCompileShader(object);
    GLint compiled = 0;
    glGetShaderiv(object, GL_COMPILE_STATUS, &compiled);
    if (!compiled) {
        char log[1024];
        glGetShaderInfoLog(object, sizeof(log), NULL, log);
        puts(log);
        fail("GLSL compilation");
    }
    return object;
}
int main(void) {
    puts("mesa-probe: entered real OSMesa program"); fflush(stdout);
    const int width = 128, height = 128;
    unsigned char *pixels = calloc(width * height, 4);
    if (!pixels) fail("buffer allocation");
    OSMesaContext context = OSMesaCreateContextExt(OSMESA_RGBA, 24, 0, 0, NULL);
    if (!context || !OSMesaMakeCurrent(context, pixels, GL_UNSIGNED_BYTE, width, height)) fail("create context");
    const char *renderer = (const char *)glGetString(GL_RENDERER);
    printf("mesa-probe: renderer=%s version=%s\n", renderer, glGetString(GL_VERSION)); fflush(stdout);
    if (!renderer || !strstr(renderer, "softpipe")) fail("expected real softpipe driver");
    glViewport(0, 0, width, height);
    glClearColor(0, 1, 0, 1);
    glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
    glEnable(GL_DEPTH_TEST);
    // Draw near red first, then far blue. The depth attachment must keep red.
    glColor3f(1, 0, 0);
    triangle(-0.5f);
    glColor3f(0, 0, 1);
    triangle(0.5f);
    glFinish();
    const unsigned char *centre = pixels + (height / 2 * width + width / 2) * 4;
    if (centre[0] < 200 || centre[1] > 20 || centre[2] > 20 || pixels[1] < 200) fail("triangle depth pixels");
    if (glGetError() != GL_NO_ERROR) fail("fixed-function GL error");
    puts("mesa-probe: PASS real OpenGL triangle and depth occlusion"); fflush(stdout);
    GLuint vertex = shader(GL_VERTEX_SHADER, "#version 120\nvoid main(){gl_Position=gl_Vertex;}");
    GLuint fragment = shader(GL_FRAGMENT_SHADER, "#version 120\nvoid main(){gl_FragColor=vec4(0.9,0.1,0.05,1.0);}");
    GLuint program = glCreateProgram();
    glAttachShader(program, vertex);
    glAttachShader(program, fragment);
    glLinkProgram(program);
    GLint linked = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked) fail("GLSL linking");
    glUseProgram(program);
    glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
    triangle(0);
    glFinish();
    if (centre[0] < 210 || centre[0] > 240 || centre[1] < 20 || centre[1] > 30 || centre[2] > 20 || glGetError() != GL_NO_ERROR) fail("GLSL rendered pixels");
    puts("mesa-probe: PASS real GLSL compile link and rendered pixels"); fflush(stdout);
    glUseProgram(0);
    glDeleteProgram(program);
    glDeleteShader(vertex);
    glDeleteShader(fragment);
    OSMesaDestroyContext(context);
    free(pixels);
    return 0;
}
