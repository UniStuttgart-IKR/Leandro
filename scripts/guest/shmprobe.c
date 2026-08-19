// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
/*
 * shmprobe -- read one pixel of the root window BOTH ways and compare.
 *
 *   gcc -O2 -Wall -Wextra -o shmprobe shmprobe.c -lX11 -lXext
 *   DISPLAY=:1 ./shmprobe [x] [y]
 *
 * Why this exists: `ffmpeg -f x11grab` reads black on the virtual display
 * while `xwd` reads the right pixels, and ffmpeg's xcbgrab differs from xwd
 * in more than one way at once -- XCB instead of Xlib, MIT-SHM instead of a
 * plain request, its own visual handling. This narrows it to the single
 * difference that matters: same connection, same drawable, same rectangle,
 * XGetImage against XShmGetImage.
 *
 * It also prints depth and visual, because a grab that reads the right
 * memory through the wrong visual is black in exactly the same way.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ipc.h>
#include <sys/shm.h>
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <X11/extensions/XShm.h>

static void report(const char *how, XImage *img, int x, int y)
{
	unsigned long p;

	if (img == NULL) {
		printf("  %-16s FAILED\n", how);
		return;
	}
	p = XGetPixel(img, x, y);
	printf("  %-16s pixel %#08lx  (R %3lu G %3lu B %3lu)  depth %d bpp %d\n",
	       how, p, (p >> 16) & 0xff, (p >> 8) & 0xff, p & 0xff,
	       img->depth, img->bits_per_pixel);
}

int main(int argc, char **argv)
{
	int px = argc > 1 ? atoi(argv[1]) : 400;
	int py = argc > 2 ? atoi(argv[2]) : 300;
	/* A window big enough to hold the sample point, small enough that the
	 * shared segment is cheap. The offset into it is what gets read. */
	const int w = px + 16, h = py + 16;
	Display *d;
	Window root;
	XWindowAttributes wa;
	XImage *plain, *shm = NULL;
	XShmSegmentInfo si;
	int major, minor, ignore;
	Bool pixmaps;

	d = XOpenDisplay(NULL);
	if (!d) {
		fprintf(stderr, "cannot open display\n");
		return 2;
	}
	root = DefaultRootWindow(d);
	XGetWindowAttributes(d, root, &wa);
	printf("root %dx%d, depth %d, visual class %d, root_visual id %#lx\n",
	       wa.width, wa.height, wa.depth, wa.visual->class,
	       wa.visual->visualid);

	plain = XGetImage(d, root, 0, 0, w, h, AllPlanes, ZPixmap);
	report("XGetImage", plain, px, py);

	if (!XQueryExtension(d, "MIT-SHM", &ignore, &ignore, &ignore)) {
		printf("  MIT-SHM          not present on this server\n");
		goto done;
	}
	XShmQueryVersion(d, &major, &minor, &pixmaps);
	printf("MIT-SHM %d.%d, shared pixmaps %s\n", major, minor,
	       pixmaps ? "yes" : "no");

	memset(&si, 0, sizeof(si));
	shm = XShmCreateImage(d, wa.visual, wa.depth, ZPixmap, NULL, &si, w, h);
	if (!shm) {
		printf("  XShmCreateImage  FAILED\n");
		goto done;
	}
	si.shmid = shmget(IPC_PRIVATE, (size_t)shm->bytes_per_line * shm->height,
			  IPC_CREAT | 0600);
	if (si.shmid < 0) {
		perror("  shmget");
		goto done;
	}
	si.shmaddr = shm->data = shmat(si.shmid, NULL, 0);
	si.readOnly = False;
	if (!XShmAttach(d, &si)) {
		printf("  XShmAttach       FAILED\n");
		goto done;
	}
	XSync(d, False);

	/* Poison the segment first. Black that was never written and black
	 * that was read back are the same colour, and only one of them is a
	 * bug in the server. */
	memset(shm->data, 0x5a, (size_t)shm->bytes_per_line * shm->height);

	if (!XShmGetImage(d, root, shm, 0, 0, AllPlanes)) {
		printf("  XShmGetImage     returned False\n");
	} else {
		XSync(d, False);
		report("XShmGetImage", shm, px, py);
		if (((unsigned char *)shm->data)[0] == 0x5a &&
		    ((unsigned char *)shm->data)[1] == 0x5a)
			printf("  ** the segment still holds the poison: the server "
			       "wrote NOTHING into it **\n");
	}

	XShmDetach(d, &si);
	shmdt(si.shmaddr);
	shmctl(si.shmid, IPC_RMID, NULL);
done:
	if (plain)
		XDestroyImage(plain);
	XCloseDisplay(d);
	return 0;
}
