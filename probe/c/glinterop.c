// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
/* glinterop: does CUDA accept a texture from an NVIDIA GL context that was
 * created WITHOUT any DRM node?
 *
 * THE QUESTION, and why it is the one that decides the display path.
 *
 * Sunshine's CUDA encoder path ends at cuGraphicsGLRegisterImage
 * (src/platform/linux/cuda.cpp:370). In the guest it returns 304,
 * CUDA_ERROR_OPERATING_SYSTEM -- "an OS call failed", per cuda.h. That is
 * NOT the code for a context on the wrong device (201,
 * CUDA_ERROR_INVALID_CONTEXT), so the failure is about a syscall, not about
 * policy. In the guest the GL context belonged to Mesa/virgl while CUDA
 * belonged to the forwarded NVIDIA driver -- two devices, and no measured
 * statement about what NVIDIA's own GL would do there.
 *
 * This probe supplies that statement. It builds the smallest possible
 * version of Sunshine's last step:
 *
 *   EGL surfaceless display -> GL context -> GL texture
 *   cuInit -> cuCtxCreate -> cuGraphicsGLRegisterImage on that texture
 *
 * EGL_PLATFORM_SURFACELESS_MESA, not GBM, and that choice IS the
 * measurement: measured on the host, NVIDIA's EGL brings up a full OpenGL
 * 4.6 core context over /dev/nvidiactl and /dev/nvidia0 alone, with
 * /dev/dri replaced by an empty tmpfs. GBM, Wayland and X11 all fail in
 * that situation; surfaceless does not. So this is the one GL context a
 * guest with no render node can have today.
 *
 * WHAT A RESULT MEANS
 *   PASS natively AND in the guest  -- CUDA-GL interop needs no DRM node.
 *                                      Sunshine's second blocker is a
 *                                      library-staging problem, not a
 *                                      driver problem.
 *   PASS natively, FAIL in the guest -- the interop needs something the
 *                                      forwarded RM path does not carry;
 *                                      the failing syscall is then the
 *                                      next thing to name (strace -f -y).
 *   FAIL natively                    -- the probe is wrong, not the rig.
 *
 * Every step prints its own verdict, because "it failed" is worth nothing
 * without which of the six steps failed.
 *
 * SOURCES
 *   EGL_PLATFORM_SURFACELESS_MESA 0x31DD -- EGL_MESA_platform_surfaceless
 *   cuGraphicsGLRegisterImage, CU_GRAPHICS_REGISTER_FLAGS_NONE -- cudaGL.h
 *   CUDA_ERROR_OPERATING_SYSTEM = 304 "an OS call failed" -- cuda.h
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <stdlib.h>

#include <cuda.h>
#include <cudaGL.h>

static int fails;

static void step(const char *what, int ok, const char *detail)
{
    printf("  %-34s %s%s%s\n", what, ok ? "ok" : "FAILED",
           detail && *detail ? "  -- " : "", detail ? detail : "");
    if (!ok)
        fails++;
}

static const char *cuerr(CUresult r)
{
    const char *s = NULL;
    cuGetErrorName(r, &s);
    return s ? s : "?";
}

int main(void)
{
    char buf[256];

    printf("== glinterop: CUDA against a GL texture, no DRM node ==\n");

    /* ---- 1. an EGL display without a window system ---------------------
     * eglGetPlatformDisplay comes from libEGL (GLVND) and is resolved
     * through eglGetProcAddress rather than linked: on a 1.4 libEGL the
     * symbol is absent, and a link error would look like a driver problem.
     */
    PFNEGLGETPLATFORMDISPLAYEXTPROC getPlatformDisplay =
        (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
    if (!getPlatformDisplay) {
        step("eglGetPlatformDisplayEXT", 0, "symbol missing");
        return 1;
    }
    EGLDisplay dpy = getPlatformDisplay(EGL_PLATFORM_SURFACELESS_MESA,
                                        EGL_DEFAULT_DISPLAY, NULL);
    step("EGL surfaceless display", dpy != EGL_NO_DISPLAY, "");
    if (dpy == EGL_NO_DISPLAY)
        return 1;

    EGLint major = 0, minor = 0;
    if (!eglInitialize(dpy, &major, &minor)) {
        snprintf(buf, sizeof buf, "eglInitialize: 0x%x", eglGetError());
        step("eglInitialize", 0, buf);
        return 1;
    }
    snprintf(buf, sizeof buf, "EGL %d.%d, vendor %s", major, minor,
             eglQueryString(dpy, EGL_VENDOR));
    step("eglInitialize", 1, buf);

    /* WARNING: the vendor string is the premise of the whole run. A Mesa
     * context here would measure Mesa, which is what the guest already
     * does and what already fails. Say it out loud rather than let a
     * later reader assume NVIDIA.
     */
    const char *vendor = eglQueryString(dpy, EGL_VENDOR);
    step("EGL vendor is NVIDIA", vendor && strstr(vendor, "NVIDIA") != NULL,
         vendor ? vendor : "(null)");

    /* ---- 2. a GL context -------------------------------------------- */
    static const EGLint cfgattr[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
        EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_NONE
    };
    EGLConfig cfg;
    EGLint ncfg = 0;
    if (!eglChooseConfig(dpy, cfgattr, &cfg, 1, &ncfg) || ncfg < 1) {
        snprintf(buf, sizeof buf, "0x%x", eglGetError());
        step("eglChooseConfig", 0, buf);
        return 1;
    }
    step("eglChooseConfig", 1, "");

    if (!eglBindAPI(EGL_OPENGL_API)) {
        step("eglBindAPI(OpenGL)", 0, "");
        return 1;
    }
    EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, NULL);
    if (ctx == EGL_NO_CONTEXT) {
        snprintf(buf, sizeof buf, "0x%x", eglGetError());
        step("eglCreateContext", 0, buf);
        return 1;
    }
    step("eglCreateContext", 1, "");

    if (!eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx)) {
        snprintf(buf, sizeof buf, "0x%x", eglGetError());
        step("eglMakeCurrent (surfaceless)", 0, buf);
        return 1;
    }
    const char *glr = (const char *)glGetString(GL_RENDERER);
    step("eglMakeCurrent (surfaceless)", 1, glr ? glr : "?");

    /* ---- 3. a texture ------------------------------------------------ */
    GLuint tex = 0;
    glGenTextures(1, &tex);
    glBindTexture(GL_TEXTURE_2D, tex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, 1920, 1080, 0,
                 GL_RGBA, GL_UNSIGNED_BYTE, NULL);
    GLenum gle = glGetError();
    snprintf(buf, sizeof buf, "1920x1080 RGBA8, gl error 0x%x", gle);
    step("GL texture", gle == GL_NO_ERROR && tex != 0, buf);

    /* ---- 4. a CUDA context ------------------------------------------- */
    CUresult r = cuInit(0);
    step("cuInit", r == CUDA_SUCCESS, cuerr(r));
    if (r != CUDA_SUCCESS)
        return 1;

    CUdevice dev;
    r = cuDeviceGet(&dev, 0);
    step("cuDeviceGet(0)", r == CUDA_SUCCESS, cuerr(r));
    if (r != CUDA_SUCCESS)
        return 1;

    char name[128] = "";
    cuDeviceGetName(name, sizeof name, dev);
    char pci[64] = "";
    cuDeviceGetPCIBusId(pci, sizeof pci, dev);
    printf("  %-34s %s @ %s\n", "CUDA device", name, pci);

    /* From CUDA 12.5 cuda.h maps cuCtxCreate onto cuCtxCreate_v4, which
     * takes a parameter block. Same guard as hostregprobe.c -- the guest
     * and the host may not carry the same toolkit.
     */
    CUcontext cuctx;
#if CUDA_VERSION >= 12050
    CUctxCreateParams cparams;
    memset(&cparams, 0, sizeof cparams);
    r = cuCtxCreate(&cuctx, &cparams, 0, dev);
#else
    r = cuCtxCreate(&cuctx, 0, dev);
#endif
    step("cuCtxCreate", r == CUDA_SUCCESS, cuerr(r));
    if (r != CUDA_SUCCESS)
        return 1;

    /* ---- 5. THE question --------------------------------------------- */
    CUgraphicsResource res = NULL;
    r = cuGraphicsGLRegisterImage(&res, tex, GL_TEXTURE_2D,
                                  CU_GRAPHICS_REGISTER_FLAGS_NONE);
    snprintf(buf, sizeof buf, "%s (%d)", cuerr(r), (int)r);
    step("cuGraphicsGLRegisterImage", r == CUDA_SUCCESS, buf);

    /* ---- 5b. DRAW something, and read it back ------------------------
     * A context that exists and a texture that allocates are not yet a GPU
     * that renders. Bind the texture to a framebuffer, clear it to a known
     * colour, read it back, and sum the bytes. The sum is printed so a
     * guest run and a native run can be compared as NUMBERS rather than as
     * two "ok"s -- which is the difference between "it did not crash" and
     * "it produced the same pixels".
     */
    GLuint fbo = 0;
    glGenFramebuffers(1, &fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                           GL_TEXTURE_2D, tex, 0);
    GLenum fbs = glCheckFramebufferStatus(GL_FRAMEBUFFER);
    snprintf(buf, sizeof buf, "status 0x%x", fbs);
    step("GL framebuffer complete", fbs == GL_FRAMEBUFFER_COMPLETE, buf);

    if (fbs == GL_FRAMEBUFFER_COMPLETE) {
        /* 0.25 / 0.50 / 0.75 / 1.0 -> 64 128 191 255 per pixel after the
         * driver's own rounding. Deliberately not 0 or 1: a readback of
         * all-zero or all-one bytes is what a broken path returns too. */
        glViewport(0, 0, 256, 256);
        glClearColor(0.25f, 0.5f, 0.75f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glFinish();

        unsigned char *px = malloc(256 * 256 * 4);
        unsigned long long sum = 0;
        if (px) {
            glReadPixels(0, 0, 256, 256, GL_RGBA, GL_UNSIGNED_BYTE, px);
            for (size_t i = 0; i < 256u * 256u * 4u; i++)
                sum += px[i];
            snprintf(buf, sizeof buf,
                     "256x256 cleared, first pixel %u/%u/%u/%u, byte sum %llu",
                     px[0], px[1], px[2], px[3], sum);
            free(px);
        } else {
            snprintf(buf, sizeof buf, "out of memory");
        }
        /* 65536 pixels * (64 + 128 + 191 + 255) = 41,156,608. The exact
         * value is the driver's rounding, so it is REPORTED and compared
         * against the native run, not asserted here. */
        step("GL render + readback", glGetError() == GL_NO_ERROR && sum > 0, buf);
    }
    glBindFramebuffer(GL_FRAMEBUFFER, 0);
    glDeleteFramebuffers(1, &fbo);

    /* ---- 6. and can the mapping actually be used? --------------------
     * Registering is a promise; mapping is the promise kept. Sunshine does
     * both every frame, so a probe that stops at step 5 would report a
     * capability the encoder still could not use.
     */
    if (r == CUDA_SUCCESS) {
        CUresult m = cuGraphicsMapResources(1, &res, 0);
        step("cuGraphicsMapResources", m == CUDA_SUCCESS, cuerr(m));
        if (m == CUDA_SUCCESS) {
            CUarray arr = NULL;
            CUresult g = cuGraphicsSubResourceGetMappedArray(&arr, res, 0, 0);
            step("cuGraphicsSubResourceGetMappedArray",
                 g == CUDA_SUCCESS && arr != NULL, cuerr(g));
            cuGraphicsUnmapResources(1, &res, 0);
        }
        cuGraphicsUnregisterResource(res);
    }

    cuCtxDestroy(cuctx);
    eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    eglDestroyContext(dpy, ctx);
    eglTerminate(dpy);

    printf("\n%s\n", fails ? "RESULT: FAILED" : "RESULT: PASS");
    return fails ? 1 : 0;
}
