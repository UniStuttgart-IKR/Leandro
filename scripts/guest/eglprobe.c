// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
/*
 * eglprobe -- can EGL initialise on a GBM device made from this DRM node?
 *
 *   gcc -O2 -Wall -Wextra -o eglprobe eglprobe.c -lgbm -lEGL -ldrm
 *   ./eglprobe /dev/dri/card1
 *
 * X says "eglInitialize() failed" and falls back to ShadowFB, which means
 * software rendering on a path whose whole point is the GPU. That message
 * arrives after libglvnd has already tried several vendors, so the log says
 * which one failed LAST, not which one should have worked. This asks each
 * vendor on its own -- run it with __EGL_VENDOR_LIBRARY_FILENAMES set to a
 * single ICD -- and prints the GBM backend name, so "wrong backend" and
 * "right backend, vendor declined" stop looking alike.
 */
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <gbm.h>
#include <EGL/egl.h>
#include <EGL/eglext.h>

int main(int argc, char **argv)
{
	const char *node = argc > 1 ? argv[1] : "/dev/dri/card1";
	struct gbm_device *gbm;
	EGLDisplay dpy = EGL_NO_DISPLAY;
	EGLint major = 0, minor = 0;
	int fd;

	/* What NVIDIA's EGL thinks it owns. egl-gbm matches the gbm device's
	 * fd against this list by DRM node path, so a list that is empty, or
	 * that names a different node, is the whole answer. */
	{
		PFNEGLQUERYDEVICESEXTPROC queryDevices = (PFNEGLQUERYDEVICESEXTPROC)
			eglGetProcAddress("eglQueryDevicesEXT");
		PFNEGLQUERYDEVICESTRINGEXTPROC queryDeviceString =
			(PFNEGLQUERYDEVICESTRINGEXTPROC)
			eglGetProcAddress("eglQueryDeviceStringEXT");
		EGLDeviceEXT devs[16];
		EGLint n = 0, i;

		if (!queryDevices || !queryDeviceString) {
			printf("eglQueryDevicesEXT: not available\n");
		} else if (!queryDevices(16, devs, &n)) {
			printf("eglQueryDevicesEXT: FAILED, eglGetError %#x\n",
			       eglGetError());
		} else {
			printf("eglQueryDevicesEXT: %d device(s)\n", n);
			for (i = 0; i < n; i++) {
				const char *dev = queryDeviceString(devs[i],
						EGL_DRM_DEVICE_FILE_EXT);
				const char *rnd = queryDeviceString(devs[i],
						EGL_DRM_RENDER_NODE_FILE_EXT);
				const char *ext = queryDeviceString(devs[i],
						EGL_EXTENSIONS);

				printf("  [%d] drm \"%s\"  render \"%s\"\n", i,
				       dev ? dev : "(none)", rnd ? rnd : "(none)");
				printf("      ext: %s\n", ext ? ext : "(none)");
			}
		}
	}

	fd = open(node, O_RDWR | O_CLOEXEC);
	if (fd < 0) {
		perror(node);
		return 2;
	}
	gbm = gbm_create_device(fd);
	if (!gbm) {
		fprintf(stderr, "gbm_create_device(%s) failed\n", node);
		return 1;
	}
	printf("%s: gbm backend \"%s\"\n", node, gbm_device_get_backend_name(gbm));

	/* The platform path first: it is what a modern glamor uses, and it is
	 * the only one that tells libglvnd which platform the pointer belongs
	 * to. The legacy eglGetDisplay has to guess. */
	{
		PFNEGLGETPLATFORMDISPLAYEXTPROC getPlatformDisplay =
			(PFNEGLGETPLATFORMDISPLAYEXTPROC)
			eglGetProcAddress("eglGetPlatformDisplayEXT");

		if (getPlatformDisplay) {
			dpy = getPlatformDisplay(EGL_PLATFORM_GBM_KHR, gbm, NULL);
			printf("  eglGetPlatformDisplayEXT(GBM): %s\n",
			       dpy == EGL_NO_DISPLAY ? "EGL_NO_DISPLAY" : "got a display");
		} else {
			printf("  eglGetPlatformDisplayEXT: not available\n");
		}
	}
	if (dpy == EGL_NO_DISPLAY) {
		dpy = eglGetDisplay((EGLNativeDisplayType)gbm);
		printf("  eglGetDisplay(legacy):         %s\n",
		       dpy == EGL_NO_DISPLAY ? "EGL_NO_DISPLAY" : "got a display");
	}
	if (dpy == EGL_NO_DISPLAY)
		return 1;

	if (!eglInitialize(dpy, &major, &minor)) {
		printf("  eglInitialize:                 FAILED, eglGetError %#x\n",
		       eglGetError());
		return 1;
	}
	printf("  eglInitialize:                 OK, EGL %d.%d\n", major, minor);
	printf("  EGL_VENDOR:  %s\n", eglQueryString(dpy, EGL_VENDOR));
	printf("  EGL_VERSION: %s\n", eglQueryString(dpy, EGL_VERSION));
	return 0;
}
