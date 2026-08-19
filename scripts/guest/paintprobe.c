// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
/*
 * paintprobe -- put a KNOWN colour on the screen with a client that draws.
 *
 *   gcc -O2 -Wall -Wextra -o paintprobe paintprobe.c -lX11
 *   DISPLAY=:1 ./paintprobe 20a060                 # hold one colour
 *   DISPLAY=:1 ./paintprobe --seq c05020,3060c0,20a060 --hold 500
 *
 * Why this exists, and why `xsetroot -solid` is not enough.
 *
 * Measured 2026-08-15 on the NVIDIA X driver: xsetroot sets the root
 * window's background PIXMAP, and with no window manager nothing then asks
 * the server to repaint. All three readers -- XGetImage, XShmGetImage and
 * ffmpeg's x11grab -- come back black, on a server that is working
 * perfectly well. Setting the colour twice does not help; the background is
 * simply never painted. On the `modesetting` driver the same call DOES show
 * up, which is why the capture stage got away with it for as long as the
 * gate ran there.
 *
 * That is a property of the root background, not of the capture chain: a
 * client that draws its own pixels comes through every reader on the same
 * server (measured 2026-08-14, and vkcube presenting on 2026-08-15).
 *
 * So this draws. An override-redirect window, filled by us, repainted on
 * every Expose -- no window manager, no background pixmap, no assumption
 * about who else might redraw.
 *
 * It does NOT exit on its own in single-colour mode: the readers need
 * something to read. Kill it (`pkill -x paintprobe` -- with -x, because -f
 * matches the caller's own shell, twice measured).
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <X11/Xlib.h>

#define MAX_SEQ 16

static unsigned long parse_hex(const char *s)
{
	return strtoul(s, NULL, 16) & 0xffffffUL;
}

/* Fill the whole window and make sure the request has actually left. */
static void paint(Display *d, Window w, GC gc, unsigned long rgb, int wd, int ht)
{
	XSetForeground(d, gc, rgb);
	XFillRectangle(d, w, gc, 0, 0, wd, ht);
	/* Sync, not Flush: the caller's next step is a grab from ANOTHER
	 * process, and a queued request that has not been executed yet reads
	 * back as the previous frame. */
	XSync(d, False);
}

int main(int argc, char **argv)
{
	const char *geom = NULL;
	unsigned long seq[MAX_SEQ];
	int nseq = 0, hold_ms = 500, i;
	int x = 100, y = 100, wd = 500, ht = 500;
	Display *d;
	Window win;
	GC gc;
	XSetWindowAttributes at;

	for (i = 1; i < argc; i++) {
		if (!strcmp(argv[i], "--seq") && i + 1 < argc) {
			char *s = argv[++i], *t;

			for (t = strtok(s, ","); t && nseq < MAX_SEQ; t = strtok(NULL, ","))
				seq[nseq++] = parse_hex(t);
		} else if (!strcmp(argv[i], "--hold") && i + 1 < argc) {
			hold_ms = atoi(argv[++i]);
		} else if (!strcmp(argv[i], "--geometry") && i + 1 < argc) {
			geom = argv[++i];
		} else if (nseq < MAX_SEQ) {
			seq[nseq++] = parse_hex(argv[i]);
		}
	}
	if (!nseq) {
		fprintf(stderr, "usage: paintprobe <rrggbb>... [--seq a,b,c] [--hold ms] [--geometry WxH+X+Y]\n");
		return 2;
	}
	if (geom && sscanf(geom, "%dx%d+%d+%d", &wd, &ht, &x, &y) != 4) {
		fprintf(stderr, "paintprobe: geometry must be WxH+X+Y\n");
		return 2;
	}

	d = XOpenDisplay(NULL);
	if (!d) {
		fprintf(stderr, "paintprobe: cannot open display %s\n",
			getenv("DISPLAY") ? getenv("DISPLAY") : "(unset)");
		return 1;
	}

	/* override_redirect: there is no window manager on this path, and a
	 * window that waits to be managed is a window that never maps. */
	memset(&at, 0, sizeof(at));
	at.override_redirect = True;
	at.background_pixel = 0;
	win = XCreateWindow(d, DefaultRootWindow(d), x, y, wd, ht, 0,
			    CopyFromParent, InputOutput, CopyFromParent,
			    CWOverrideRedirect | CWBackPixel, &at);
	XSelectInput(d, win, ExposureMask);
	XMapRaised(d, win);
	gc = XCreateGC(d, win, 0, NULL);

	/* The first paint has to wait for the map to have happened, or it
	 * lands on a window the server is not showing yet. */
	for (;;) {
		XEvent e;

		XNextEvent(d, &e);
		if (e.type == Expose)
			break;
	}

	printf("paintprobe %dx%d+%d+%d, %d colour(s)\n", wd, ht, x, y, nseq);
	fflush(stdout);

	if (nseq == 1) {
		paint(d, win, gc, seq[0], wd, ht);
		/* Hold, repainting whenever the server asks. */
		for (;;) {
			XEvent e;

			XNextEvent(d, &e);
			if (e.type == Expose)
				paint(d, win, gc, seq[0], wd, ht);
		}
	}

	for (i = 0; i < nseq; i++) {
		paint(d, win, gc, seq[i], wd, ht);
		printf("colour %d: %06lx\n", i, seq[i]);
		fflush(stdout);
		usleep((useconds_t)hold_ms * 1000);
	}
	XCloseDisplay(d);
	return 0;
}
