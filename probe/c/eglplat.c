// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//
// One EGL platform, end to end: get a display for the platform named on the
// command line, make a context current on a pbuffer, draw a known colour and
// READ IT BACK.
//
//   eglplat gbm|wayland|xcb|xlib
//
// WHY ONE PROBE PER PLATFORM: NVIDIA ships a separate external-platform
// library per windowing system (libnvidia-egl-gbm, -wayland, -wayland2,
// -xcb, -xlib), each registered by its own JSON in
// egl_external_platform.d. They are five different libraries on five
// different paths through libEGL_nvidia, so "EGL works" measured on one of
// them says nothing about the other four. A single eglinfo run would collapse
// them into one row of the matrix and hide exactly the difference the matrix
// is for.
//
// WHY THE PIXEL IS READ BACK: eglInitialize succeeding proves that a vendor
// library was found, nothing more. GL_RENDERER can name NVIDIA while the
// platform module underneath silently falls back. A pixel that comes back at
// the value that was cleared is the one statement that covers the whole path
// -- and the trap it guards against is on record in this project already: a
// client's own counter reported 58 FPS while the screen showed nothing.
//
// Deterministic: fixed 64x64 pbuffer, one glClear with a fixed colour, one
// glReadPixels of the centre pixel, exact comparison. No timing, no files.
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <stdio.h>
#include <string.h>

#ifdef LEA_HAVE_GBM
#include <fcntl.h>
#include <gbm.h>
#include <unistd.h>
#endif
#ifdef LEA_HAVE_WAYLAND
#include <wayland-client.h>
#endif
#ifdef LEA_HAVE_X11
#include <X11/Xlib.h>
#endif
#ifdef LEA_HAVE_XCB
#include <xcb/xcb.h>
#endif

// The colour cleared and expected back. Not 0 and not 0xff in any channel:
// a stuck plane, a zeroed buffer and a saturated one are then all visibly
// different from a correct read.
#define R 0x33
#define G 0x77
#define B 0xbb

static void usage(void)
{
    fprintf(stderr, "usage: eglplat gbm|wayland|xcb|xlib\n");
}

int main(int argc, char **argv)
{
    if (argc != 2) {
        usage();
        return 2;
    }
    const char *want = argv[1];

    PFNEGLGETPLATFORMDISPLAYEXTPROC getPlatformDisplay =
        (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
    if (!getPlatformDisplay) {
        fprintf(stderr, "eglplat: no eglGetPlatformDisplayEXT -- EGL_EXT_platform_base missing\n");
        return 1;
    }

    EGLenum platform = 0;
    void *native = EGL_DEFAULT_DISPLAY;
#ifdef LEA_HAVE_GBM
    int drmfd = -1;
    struct gbm_device *gbm = NULL;
#endif

    if (!strcmp(want, "gbm")) {
#ifdef LEA_HAVE_GBM
        // A render node, not a card node: this asks about the EGL platform,
        // not about modesetting, and a render node needs no DRM master.
        drmfd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
        if (drmfd < 0) {
            fprintf(stderr, "eglplat: cannot open /dev/dri/renderD128\n");
            return 1;
        }
        gbm = gbm_create_device(drmfd);
        if (!gbm) {
            fprintf(stderr, "eglplat: gbm_create_device failed\n");
            return 1;
        }
        platform = EGL_PLATFORM_GBM_KHR;
        native = gbm;
#else
        fprintf(stderr, "eglplat: built without gbm\n");
        return 2;
#endif
    } else if (!strcmp(want, "wayland")) {
#ifdef LEA_HAVE_WAYLAND
        struct wl_display *wl = wl_display_connect(NULL);
        if (!wl) {
            fprintf(stderr, "eglplat: no Wayland display (WAYLAND_DISPLAY)\n");
            return 1;
        }
        platform = EGL_PLATFORM_WAYLAND_KHR;
        native = wl;
#else
        fprintf(stderr, "eglplat: built without wayland\n");
        return 2;
#endif
    } else if (!strcmp(want, "xlib")) {
#ifdef LEA_HAVE_X11
        Display *dpy = XOpenDisplay(NULL);
        if (!dpy) {
            fprintf(stderr, "eglplat: no X display (DISPLAY)\n");
            return 1;
        }
        platform = EGL_PLATFORM_X11_KHR;
        native = dpy;
#else
        fprintf(stderr, "eglplat: built without X11\n");
        return 2;
#endif
    } else if (!strcmp(want, "xcb")) {
#ifdef LEA_HAVE_XCB
        xcb_connection_t *c = xcb_connect(NULL, NULL);
        if (!c || xcb_connection_has_error(c)) {
            fprintf(stderr, "eglplat: no xcb connection (DISPLAY)\n");
            return 1;
        }
        platform = EGL_PLATFORM_XCB_EXT;
        native = c;
#else
        fprintf(stderr, "eglplat: built without xcb\n");
        return 2;
#endif
    } else {
        usage();
        return 2;
    }

    EGLDisplay dpy = getPlatformDisplay(platform, native, NULL);
    if (dpy == EGL_NO_DISPLAY) {
        fprintf(stderr, "eglplat: eglGetPlatformDisplayEXT(%s) -> EGL_NO_DISPLAY\n", want);
        return 1;
    }
    EGLint major = 0, minor = 0;
    if (!eglInitialize(dpy, &major, &minor)) {
        fprintf(stderr, "eglplat: eglInitialize(%s) failed (0x%x)\n", want, eglGetError());
        return 1;
    }
    const char *vendor = eglQueryString(dpy, EGL_VENDOR);
    printf("EGLPLATFORM=%s\n", want);
    printf("EGLVENDOR=%s\n", vendor ? vendor : "?");
    printf("EGLVERSION=%d.%d\n", major, minor);

    if (!vendor || !strstr(vendor, "NVIDIA")) {
        // Not a crash and not our bug -- but it is the finding, because a
        // measurement taken here would be a measurement of Mesa.
        fprintf(stderr, "eglplat: platform %s resolved to vendor '%s', not NVIDIA\n",
                want, vendor ? vendor : "?");
        return 1;
    }

    const EGLint cfgattr[] = {
        EGL_SURFACE_TYPE,    EGL_PBUFFER_BIT,
        EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
        EGL_RED_SIZE,        8,
        EGL_GREEN_SIZE,      8,
        EGL_BLUE_SIZE,       8,
        EGL_ALPHA_SIZE,      8,
        EGL_NONE
    };
    EGLConfig cfg;
    EGLint ncfg = 0;
    if (!eglChooseConfig(dpy, cfgattr, &cfg, 1, &ncfg) || ncfg < 1) {
        fprintf(stderr, "eglplat: no pbuffer config on platform %s\n", want);
        return 1;
    }
    if (!eglBindAPI(EGL_OPENGL_ES_API)) {
        fprintf(stderr, "eglplat: eglBindAPI failed\n");
        return 1;
    }
    const EGLint ctxattr[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctxattr);
    if (ctx == EGL_NO_CONTEXT) {
        fprintf(stderr, "eglplat: eglCreateContext failed (0x%x)\n", eglGetError());
        return 1;
    }
    const EGLint sfattr[] = { EGL_WIDTH, 64, EGL_HEIGHT, 64, EGL_NONE };
    EGLSurface surf = eglCreatePbufferSurface(dpy, cfg, sfattr);
    if (surf == EGL_NO_SURFACE) {
        fprintf(stderr, "eglplat: eglCreatePbufferSurface failed (0x%x)\n", eglGetError());
        return 1;
    }
    if (!eglMakeCurrent(dpy, surf, surf, ctx)) {
        fprintf(stderr, "eglplat: eglMakeCurrent failed (0x%x)\n", eglGetError());
        return 1;
    }

    printf("GLRENDERER=%s\n", (const char *)glGetString(GL_RENDERER));

    glClearColor(R / 255.0f, G / 255.0f, B / 255.0f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glFinish();

    unsigned char px[4] = { 0, 0, 0, 0 };
    glReadPixels(32, 32, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
    printf("PIXEL=%02x%02x%02x%02x\n", px[0], px[1], px[2], px[3]);

    // One step of tolerance per channel: the pbuffer may be a different
    // precision than 8888 and the clear then round-trips one bit off. Two
    // steps would let a black buffer through, which is the whole point.
    int ok = (px[0] >= R - 1 && px[0] <= R + 1) && (px[1] >= G - 1 && px[1] <= G + 1) &&
             (px[2] >= B - 1 && px[2] <= B + 1);

    eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    eglDestroySurface(dpy, surf);
    eglDestroyContext(dpy, ctx);
    eglTerminate(dpy);
#ifdef LEA_HAVE_GBM
    if (gbm)
        gbm_device_destroy(gbm);
    if (drmfd >= 0)
        close(drmfd);
#endif

    if (!ok) {
        fprintf(stderr, "eglplat: read back %02x%02x%02x, expected %02x%02x%02x\n",
                px[0], px[1], px[2], R, G, B);
        return 1;
    }
    printf("eglplat %s ok\n", want);
    return 0;
}
