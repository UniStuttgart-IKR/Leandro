// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
/*
 * fbprobe -- does the scanout buffer actually CARRY PIXELS?
 *
 *   gcc -O2 -Wall -Wextra -o fbprobe fbprobe.c -ldl -lEGL
 *   sudo ./fbprobe                       # auto-find the nvidia-drm node
 *   sudo ./fbprobe --loop 20 --interval 250
 *   sudo ./fbprobe --dump /tmp/scanout.ppm   # and LOOK at it
 *
 * -lEGL is not optional: the line here said `-ldl` alone until
 * 2026-08-17, and the link fails on eglInitialize. GL is loaded through
 * eglGetProcAddress, but EGL itself is called directly.
 *
 * Why this exists (OPEN-QUESTIONS 17). Sunshine's `capture = kms` path
 * initialises, finds NVENC, and logs "width and height: w 2560 h 1440"
 * sixty times a second -- and the receiver still showed BLACK. Every reader
 * we had answered a different question: FPS, latency, "did the ioctl
 * return 0". None of them answered the only one that matters, which is
 * whether the bytes behind that framebuffer are a desktop or zeroes.
 *
 * So this walks the exact chain Sunshine walks, and reports each link
 * separately:
 *
 *   plane -> GETFB2 -> PRIME export -> (a) CPU mmap of the dmabuf
 *                                     (b) EGL dmabuf import + GL readback
 *                                     (c) EGL image -> CUDA -> device copy
 *
 * What the three ways actually did, measured 2026-08-16 on a moving
 * weston desktop -- and two of the three contradicted the guess this file
 * was written on:
 *
 *   (a) mmap  WORKS, on the primary plane and on the cursor plane alike.
 *             The "Failed to mmap cursor FB: Cannot allocate memory" that
 *             Sunshine prints every frame is therefore a property of ITS
 *             mapping, not of this layer.
 *   (b) gl    works, and is the control: it proves the dmabuf, its format
 *             and its MODIFIER import correctly and hold an image.
 *   (c) cuda  works, but NOT through cuGraphicsEGLRegisterImage. That call
 *             is effectively a Tegra interface and answers "invalid
 *             argument" for a good dmabuf EGLImage even on a healthy
 *             desktop host. The route that carries is Sunshine's own:
 *             EGLImage -> a texture we own -> cuGraphicsGLRegisterImage.
 *             Both are tried, and the summary names the one that worked.
 *
 * "nonzero" alone is a weak reader: a buffer full of stale garbage is
 * nonzero too, and a legitimately black desktop is not. So motion is
 * reported as well -- but on a HASH OF THE WHOLE FRAME, not on the sample
 * grid. That distinction is not pedantry: with the grid, three small demo
 * clients on a 2560x1440 desktop moved only 10-14 of 1024 sample points,
 * and a single small client moved none at all, so a perfectly live
 * compositor read as frozen. The grid decides black or not black; the hash
 * decides moving or still.
 *
 * A way is CONTENT when it reads nonzero pixels AND the frame changes,
 * STATIC when it reads pixels that never move, BLACK when it reads zeroes.
 * STATIC on an animating desktop is itself the finding -- it is what
 * Sunshine encodes sixty times a second while the receiver sees a still.
 *
 * No libdrm, no cuda.h, no GLES headers: the guest image has none of them,
 * and a probe that needs three -dev packages before it can answer is a
 * probe that does not get run. Everything below is declared here and
 * resolved with dlopen/dlsym, so a missing libcuda downgrades way (c) to
 * "unavailable" instead of breaking the build for (a) and (b).
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdarg.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <drm/drm.h>
#include <drm/drm_mode.h>
#include <drm/drm_fourcc.h>
#include <EGL/egl.h>
#include <EGL/eglext.h>

/* ------------------------------------------------------------------ misc */

#define MAX_SAMPLES 4096

static int verbose;

static void vlog(const char *fmt, ...)
{
	va_list ap;
	if (!verbose)
		return;
	va_start(ap, fmt);
	vfprintf(stderr, fmt, ap);
	va_end(ap);
}

static const char *fourcc_str(uint32_t f, char *buf)
{
	buf[0] = f & 0xff; buf[1] = (f >> 8) & 0xff;
	buf[2] = (f >> 16) & 0xff; buf[3] = (f >> 24) & 0xff;
	buf[4] = '\0';
	return buf;
}

static void msleep(long ms)
{
	struct timespec ts = { ms / 1000, (ms % 1000) * 1000000L };
	nanosleep(&ts, NULL);
}

/* The sample grid. Fixed and deterministic on purpose: the same pixels are
 * compared across polls, so CHANGED means the content moved rather than the
 * sampler having wandered. */
struct grid { uint32_t x[MAX_SAMPLES], y[MAX_SAMPLES]; int n; };

static void grid_build(struct grid *g, uint32_t w, uint32_t h, int want)
{
	int i, cols, rows, c, r;

	/* A coprime-ish stride walk beats a plain lattice: a lattice can land
	 * entirely inside a letterboxed black band and report a black desktop
	 * for a picture that is merely centred. */
	cols = 1; while (cols * cols < want) cols++;
	rows = cols;
	g->n = 0;
	for (r = 0; r < rows; r++) {
		for (c = 0; c < cols; c++) {
			if (g->n >= want || (uint32_t)g->n >= (uint64_t)w * h)
				break;
			i = g->n;
			g->x[i] = (uint32_t)(((uint64_t)c * w) / cols
					     + ((uint64_t)r * 7919) % (w / cols ? w / cols : 1));
			g->y[i] = (uint32_t)(((uint64_t)r * h) / rows
					     + ((uint64_t)c * 6271) % (h / rows ? h / rows : 1));
			if (g->x[i] >= w) g->x[i] = w - 1;
			if (g->y[i] >= h) g->y[i] = h - 1;
			g->n++;
		}
	}
}

/* FNV-1a over an entire frame. This exists because the sampled grid is
 * not a sufficient motion detector, and believing it nearly produced a
 * wrong headline: with three small demo clients on a 2560x1440 desktop only
 * 10-14 of 1024 sample points ever land on an animating window, and with a
 * single small client the honest expected value is ZERO. A grid that misses
 * the moving pixels reports "changed 0" for a perfectly live compositor,
 * which reads as "the desktop is frozen" -- the exact false finding this
 * probe was built to prevent. So motion is decided on the whole frame and
 * only the nonzero/black question is decided on the grid. */
static uint64_t frame_hash(const void *p, size_t bytes)
{
	const uint32_t *w = p;
	size_t words = bytes / 4;
	uint64_t h = 1469598103934665603ULL;
	size_t i;

	/* Every 8th word, not every byte. The byte loop this replaces
	 * took over two minutes per run: way (a) hashes an mmap of the
	 * scanout buffer directly, and byte-at-a-time reads across that
	 * mapping are uncached device traffic. Word reads at a stride of 8
	 * cut it to something interactive while still sampling 32k points of
	 * a 2560x1440 frame -- any moving region bigger than a few pixels
	 * still lands on many of them. */
	for (i = 0; i < words; i += 8) {
		h ^= w[i];
		h *= 1099511628211ULL;
	}
	return h;
}

/* One way's running tally. */
struct way {
	const char *name;
	int available;          /* the machinery exists at all */
	const char *why_not;    /* if it does not */
	int attempts, ok, failed;
	char last_err[240];
	int last_nonzero, last_changed, last_total;
	int polls_nonzero, polls_changed;   /* polls with >0 of each */
	int best_nonzero, best_changed;
	uint32_t prev[MAX_SAMPLES];
	int have_prev;
	uint64_t prev_hash;
	int have_hash;
	uint64_t prev_hash_pending;
	int have_pending;
	int polls_frame_differs;   /* the real motion reader */
};

static void way_fail(struct way *w, const char *fmt, ...)
{
	va_list ap;
	w->failed++;
	va_start(ap, fmt);
	vsnprintf(w->last_err, sizeof(w->last_err), fmt, ap);
	va_end(ap);
	vlog("    %-6s FAIL %s\n", w->name, w->last_err);
}

/* Score a freshly read sample set against the previous one. This is the
 * whole point of the program, so it is deliberately the simplest code in
 * it: count what is not zero, and count what moved. */
static void way_score(struct way *w, const uint32_t *px, int n,
		      const void *frame, size_t frame_bytes)
{
	int i, nz = 0, ch = 0;

	/* The frame fingerprint first: it, not the grid, is what answers
	 * "did anything move". */
	if (frame && frame_bytes) {
		uint64_t h = frame_hash(frame, frame_bytes);

		if (w->have_hash && h != w->prev_hash)
			w->polls_frame_differs++;
		w->prev_hash = h;
		w->have_hash = 1;
	}

	for (i = 0; i < n; i++) {
		/* Alpha is ignored: an XRGB buffer has 0xff there on every
		 * pixel including the black ones, and counting it would make
		 * a completely black screen read as 100% nonzero. That exact
		 * mistake would have declared the black stream green. */
		uint32_t v = px[i] & 0x00ffffff;

		if (v)
			nz++;
		if (w->have_prev && v != (w->prev[i] & 0x00ffffff))
			ch++;
	}
	memcpy(w->prev, px, (size_t)n * sizeof(uint32_t));
	w->have_prev = 1;

	w->ok++;
	w->last_nonzero = nz;
	w->last_changed = ch;
	w->last_total = n;
	if (nz > 0) w->polls_nonzero++;
	if (ch > 0) w->polls_changed++;
	if (nz > w->best_nonzero) w->best_nonzero = nz;
	if (ch > w->best_changed) w->best_changed = ch;
	vlog("    %-6s nonzero %d/%d  sampled-changed %d/%d  frame-differs %d\n",
	     w->name, nz, n, ch, n, w->polls_frame_differs);
}

/* --------------------------------------------------------------- the DRM */

#ifndef DRM_IOCTL_MODE_GETFB2
#define DRM_IOCTL_MODE_GETFB2 DRM_IOWR(0xCE, struct drm_mode_fb_cmd2)
#endif
#ifndef DRM_CLIENT_CAP_UNIVERSAL_PLANES
#define DRM_CLIENT_CAP_UNIVERSAL_PLANES 2
#endif
#ifndef DRM_CLIENT_CAP_ATOMIC
#define DRM_CLIENT_CAP_ATOMIC 3
#endif
#ifndef DRM_RDWR
#define DRM_RDWR O_RDWR
#endif

static int drm_fd = -1;

static int drm_name_of(int f, char *out, size_t len)
{
	struct drm_version v;

	memset(&v, 0, sizeof(v));
	v.name = out;
	v.name_len = len - 1;
	memset(out, 0, len);
	if (ioctl(f, DRM_IOCTL_VERSION, &v) < 0)
		return -1;
	out[len - 1] = '\0';
	return 0;
}

/* Open the first /dev/dri/cardN whose DRM driver name is `want`. The node
 * number is NOT a constant -- see the same note in drm-modeset.c; with a
 * virtio-gpu beside us it is card1, without one card0. */
static int open_node(const char *want, char *chosen, size_t len)
{
	char path[64], name[64];
	int i, f;

	for (i = 0; i < 16; i++) {
		snprintf(path, sizeof(path), "/dev/dri/card%d", i);
		f = open(path, O_RDWR | O_CLOEXEC);
		if (f < 0)
			continue;
		if (drm_name_of(f, name, sizeof(name)) == 0 && !strcmp(name, want)) {
			snprintf(chosen, len, "%s", path);
			return f;
		}
		close(f);
	}
	return -1;
}

struct fbinfo {
	uint32_t plane_id, crtc_id, fb_id;
	uint32_t w, h, fourcc;
	uint64_t modifier;
	uint32_t handles[4], pitches[4], offsets[4];
	int planes;
};

/* Every plane that currently scans out something, largest first. The cursor
 * plane is in here too, and on purpose: "cursor arrives, main plane does
 * not" was an observed failure, and a probe that only looks at the primary
 * cannot tell that story. */
static int planes_total, planes_dark;

static int collect_planes(struct fbinfo *out, int max)
{
	struct drm_mode_get_plane_res pres;
	uint32_t ids[64];
	int n = 0, seen_dark = 0;
	uint32_t i;

	memset(&pres, 0, sizeof(pres));
	pres.plane_id_ptr = (uint64_t)(uintptr_t)ids;
	pres.count_planes = 64;
	if (ioctl(drm_fd, DRM_IOCTL_MODE_GETPLANERESOURCES, &pres) < 0) {
		fprintf(stderr, "GETPLANERESOURCES: %s\n", strerror(errno));
		return -1;
	}
	if (pres.count_planes > 64)
		pres.count_planes = 64;

	for (i = 0; i < pres.count_planes && n < max; i++) {
		struct drm_mode_get_plane p;
		struct drm_mode_fb_cmd2 fb;

		memset(&p, 0, sizeof(p));
		p.plane_id = ids[i];
		if (ioctl(drm_fd, DRM_IOCTL_MODE_GETPLANE, &p) < 0) {
			vlog("  plane %u: GETPLANE: %s\n", ids[i], strerror(errno));
			continue;
		}
		if (!p.fb_id) {
			/* An idle plane is not an error -- but "how many
			 * planes exist and how many are lit" is the first
			 * thing to know when nothing is scanning out, and
			 * a probe that only counts the lit ones cannot
			 * tell "no planes" from "planes, all dark". */
			seen_dark++;
			vlog("  plane %u: no fb (crtc %u)\n", ids[i], p.crtc_id);
			continue;
		}

		memset(&fb, 0, sizeof(fb));
		fb.fb_id = p.fb_id;
		/* GETFB2 hands back GEM handles only to a client with
		 * CAP_SYS_ADMIN or DRM master. Run as root. Without it the
		 * call SUCCEEDS and every handle is 0, which looks like an
		 * empty framebuffer rather than a permissions problem. */
		if (ioctl(drm_fd, DRM_IOCTL_MODE_GETFB2, &fb) < 0) {
			fprintf(stderr, "  plane %u fb %u: GETFB2: %s\n",
				ids[i], p.fb_id, strerror(errno));
			continue;
		}
		if (!fb.handles[0]) {
			fprintf(stderr, "  plane %u fb %u: GETFB2 gave handle 0 "
				"-- not root? (needs CAP_SYS_ADMIN)\n",
				ids[i], p.fb_id);
			continue;
		}

		out[n].plane_id = ids[i];
		out[n].crtc_id  = p.crtc_id;
		out[n].fb_id    = p.fb_id;
		out[n].w        = fb.width;
		out[n].h        = fb.height;
		out[n].fourcc   = fb.pixel_format;
		out[n].modifier = (fb.flags & DRM_MODE_FB_MODIFIERS)
				  ? fb.modifier[0] : DRM_FORMAT_MOD_INVALID;
		memcpy(out[n].handles, fb.handles, sizeof(fb.handles));
		memcpy(out[n].pitches, fb.pitches, sizeof(fb.pitches));
		memcpy(out[n].offsets, fb.offsets, sizeof(fb.offsets));
		out[n].planes = 0;
		{
			int k;
			for (k = 0; k < 4; k++)
				if (fb.handles[k])
					out[n].planes = k + 1;
		}
		n++;
	}
	planes_total = (int)pres.count_planes;
	planes_dark = seen_dark;
	return n;
}

/* What the CRTCs think they are showing. When no plane holds a framebuffer
 * this is the difference between "the head is off" and "the head is lit by
 * a path that does not go through DRM planes". */
static void dump_crtcs(void)
{
	struct drm_mode_card_res res;
	uint32_t conn[32], crtc[32], enc[32], fb[32];
	uint32_t i;

	memset(&res, 0, sizeof(res));
	res.connector_id_ptr = (uint64_t)(uintptr_t)conn;
	res.crtc_id_ptr      = (uint64_t)(uintptr_t)crtc;
	res.encoder_id_ptr   = (uint64_t)(uintptr_t)enc;
	res.fb_id_ptr        = (uint64_t)(uintptr_t)fb;
	res.count_connectors = res.count_crtcs = res.count_encoders = res.count_fbs = 32;
	if (ioctl(drm_fd, DRM_IOCTL_MODE_GETRESOURCES, &res) < 0) {
		fprintf(stderr, "  GETRESOURCES: %s\n", strerror(errno));
		return;
	}
	fprintf(stderr, "  %u crtc(s), %u connector(s)\n",
		res.count_crtcs, res.count_connectors);
	for (i = 0; i < res.count_crtcs && i < 32; i++) {
		struct drm_mode_crtc c;

		memset(&c, 0, sizeof(c));
		c.crtc_id = crtc[i];
		if (ioctl(drm_fd, DRM_IOCTL_MODE_GETCRTC, &c) < 0)
			continue;
		fprintf(stderr, "    crtc %u: fb %u, mode_valid %u, %ux%u\n",
			crtc[i], c.fb_id, c.mode_valid,
			c.mode.hdisplay, c.mode.vdisplay);
	}
	for (i = 0; i < res.count_connectors && i < 32; i++) {
		struct drm_mode_get_connector c;

		memset(&c, 0, sizeof(c));
		c.connector_id = conn[i];
		if (ioctl(drm_fd, DRM_IOCTL_MODE_GETCONNECTOR, &c) < 0)
			continue;
		fprintf(stderr, "    connector %u: connection %u (1 = connected), "
			"%u mode(s), encoder %u\n", conn[i], c.connection,
			c.count_modes, c.encoder_id);
	}
}

static void close_handles(const struct fbinfo *f)
{
	struct drm_gem_close c;
	int i, j;

	for (i = 0; i < 4; i++) {
		if (!f->handles[i])
			continue;
		for (j = 0; j < i; j++)
			if (f->handles[j] == f->handles[i])
				break;
		if (j < i)
			continue;   /* same BO behind several planes */
		memset(&c, 0, sizeof(c));
		c.handle = f->handles[i];
		ioctl(drm_fd, DRM_IOCTL_GEM_CLOSE, &c);
	}
}

static int prime_export(uint32_t handle, int *out_fd)
{
	struct drm_prime_handle ph;

	memset(&ph, 0, sizeof(ph));
	ph.handle = handle;
	ph.flags = DRM_CLOEXEC | DRM_RDWR;
	if (ioctl(drm_fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &ph) < 0) {
		/* Some drivers refuse RDWR export but allow read-only. Ask
		 * again rather than reporting a failure that is really a
		 * flag mismatch. */
		memset(&ph, 0, sizeof(ph));
		ph.handle = handle;
		ph.flags = DRM_CLOEXEC;
		if (ioctl(drm_fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &ph) < 0)
			return -1;
	}
	*out_fd = ph.fd;
	return 0;
}

/* ------------------------------------------------------------ way (a) CPU */

static void read_mmap(struct way *w, const struct fbinfo *f, int dmabuf,
		      const struct grid *g)
{
	static uint32_t px[MAX_SAMPLES];
	off_t size;
	uint8_t *p;
	int i;

	w->attempts++;
	size = lseek(dmabuf, 0, SEEK_END);
	if (size <= 0) {
		way_fail(w, "lseek(SEEK_END) on the dmabuf: %s", strerror(errno));
		return;
	}
	p = mmap(NULL, (size_t)size, PROT_READ, MAP_SHARED, dmabuf, 0);
	if (p == MAP_FAILED) {
		/* THE measurement. Sunshine reports exactly this for the
		 * cursor FB, once per frame: "Failed to mmap cursor FB:
		 * Cannot allocate memory". If it says ENOMEM here too, then
		 * CPU mapping of our dmabufs is simply not implemented, and
		 * no amount of Sunshine configuration will change it. */
		way_fail(w, "mmap %lld bytes: %s", (long long)size, strerror(errno));
		return;
	}
	for (i = 0; i < g->n; i++) {
		uint64_t off = (uint64_t)f->offsets[0]
			     + (uint64_t)g->y[i] * f->pitches[0]
			     + (uint64_t)g->x[i] * 4;
		px[i] = (off + 4 <= (uint64_t)size)
			? *(const uint32_t *)(p + off) : 0;
	}
	{
		/* Hash before unmapping. Striding by nothing: the whole
		 * buffer, because a cursor-sized change in a corner is
		 * exactly what must not be missed. */
		uint64_t h = frame_hash(p, (size_t)size);

		w->prev_hash_pending = h;
		w->have_pending = 1;
	}
	munmap(p, (size_t)size);
	/* For a block-linear (modifier != 0) buffer this addressing is
	 * NOT the pixel at (x, y) -- the bytes are tiled. It does not matter
	 * for the question being asked: zero bytes are zero wherever they
	 * sit, and a desktop's bytes are not zero. Read the geometry of this
	 * way's output as "somewhere in the buffer", not as an image. */
	/* mmap hashed its own mapping above, so pass the pending value
	 * through rather than a pointer to memory that is already gone. */
	if (w->have_pending) {
		uint64_t h = w->prev_hash_pending;

		if (w->have_hash && h != w->prev_hash)
			w->polls_frame_differs++;
		w->prev_hash = h;
		w->have_hash = 1;
		w->have_pending = 0;
	}
	way_score(w, px, g->n, NULL, 0);
}

/* ---------------------------------------------------------- EGL machinery */

#ifndef EGL_LINUX_DMA_BUF_EXT
#define EGL_LINUX_DMA_BUF_EXT                 0x3270
#define EGL_LINUX_DRM_FOURCC_EXT              0x3271
#define EGL_DMA_BUF_PLANE0_FD_EXT             0x3272
#define EGL_DMA_BUF_PLANE0_OFFSET_EXT         0x3273
#define EGL_DMA_BUF_PLANE0_PITCH_EXT          0x3274
#endif
#ifndef EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT
#define EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT    0x3443
#define EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT    0x3444
#endif

/* GL, by hand. GLES2/gl2.h is not in the guest image and libGLESv2 is, so
 * the header is the only thing missing and it is three typedefs deep. */
#define GL_TEXTURE_2D            0x0DE1
#define GL_TEXTURE_MIN_FILTER    0x2801
#define GL_TEXTURE_MAG_FILTER    0x2800
#define GL_NEAREST               0x2600
#define GL_FRAMEBUFFER           0x8D40
#define GL_COLOR_ATTACHMENT0     0x8CE0
#define GL_FRAMEBUFFER_COMPLETE  0x8CD5
#define GL_RGBA                  0x1908
#define GL_BGRA_EXT              0x80E1
#define GL_UNSIGNED_BYTE         0x1401
#define GL_NO_ERROR              0
#define GL_VERTEX_SHADER         0x8B31
#define GL_FRAGMENT_SHADER       0x8B30
#define GL_COMPILE_STATUS        0x8B81
#define GL_LINK_STATUS           0x8B82
#define GL_FLOAT                 0x1406
#define GL_TRIANGLE_STRIP        0x0005
#define GL_TEXTURE0              0x84C0

typedef unsigned int GLenum, GLuint, GLbitfield;
typedef int GLint, GLsizei;
typedef void GLvoid;

static struct {
	void (*GenTextures)(GLsizei, GLuint *);
	void (*BindTexture)(GLenum, GLuint);
	void (*DeleteTextures)(GLsizei, const GLuint *);
	void (*TexParameteri)(GLenum, GLenum, GLint);
	void (*GenFramebuffers)(GLsizei, GLuint *);
	void (*BindFramebuffer)(GLenum, GLuint);
	void (*DeleteFramebuffers)(GLsizei, const GLuint *);
	void (*FramebufferTexture2D)(GLenum, GLenum, GLenum, GLuint, GLint);
	GLenum (*CheckFramebufferStatus)(GLenum);
	void (*ReadPixels)(GLint, GLint, GLsizei, GLsizei, GLenum, GLenum, GLvoid *);
	void (*TexImage2D)(GLenum, GLint, GLint, GLsizei, GLsizei, GLint, GLenum, GLenum, const GLvoid *);
	GLuint (*CreateShader)(GLenum);
	void (*ShaderSource)(GLuint, GLsizei, const char *const *, const GLint *);
	void (*CompileShader)(GLuint);
	void (*GetShaderiv)(GLuint, GLenum, GLint *);
	void (*GetShaderInfoLog)(GLuint, GLsizei, GLsizei *, char *);
	GLuint (*CreateProgram)(void);
	void (*AttachShader)(GLuint, GLuint);
	void (*LinkProgram)(GLuint);
	void (*GetProgramiv)(GLuint, GLenum, GLint *);
	void (*GetProgramInfoLog)(GLuint, GLsizei, GLsizei *, char *);
	void (*UseProgram)(GLuint);
	GLint (*GetAttribLocation)(GLuint, const char *);
	GLint (*GetUniformLocation)(GLuint, const char *);
	void (*Uniform1i)(GLint, GLint);
	void (*VertexAttribPointer)(GLuint, GLint, GLenum, unsigned char, GLsizei, const GLvoid *);
	void (*EnableVertexAttribArray)(GLuint);
	void (*DrawArrays)(GLenum, GLint, GLsizei);
	void (*Viewport)(GLint, GLint, GLsizei, GLsizei);
	void (*ActiveTexture)(GLenum);
	void (*Finish)(void);
	GLenum (*GetError)(void);
	void (*EGLImageTargetTexture2DOES)(GLenum, void *);
} gl;

static EGLDisplay egl_dpy = EGL_NO_DISPLAY;
static EGLContext egl_ctx = EGL_NO_CONTEXT;
static int egl_have_modifiers;
static PFNEGLCREATEIMAGEKHRPROC pCreateImage;
static PFNEGLDESTROYIMAGEKHRPROC pDestroyImage;

/* An EGL display on the GPU itself, not on a window system: the probe has to
 * run with a compositor already holding the DRM master, so it must not need
 * a surface, a socket or an X connection of its own. EGL_PLATFORM_DEVICE_EXT
 * plus a surfaceless context is the only combination that asks nothing of
 * the session. */
static const char *egl_setup(void)
{
	static char err[160];
	PFNEGLQUERYDEVICESEXTPROC qDevices;
	PFNEGLGETPLATFORMDISPLAYEXTPROC gPlatform;
	EGLDeviceEXT devs[16];
	EGLint ndev = 0, major = 0, minor = 0, nconf = 0;
	EGLConfig conf;
	const char *exts;
	static const EGLint cfg_attr[] = {
		EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
		EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
		EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8,
		EGL_NONE
	};
	static const EGLint ctx_attr[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
	void *libgles;

	qDevices = (PFNEGLQUERYDEVICESEXTPROC)eglGetProcAddress("eglQueryDevicesEXT");
	gPlatform = (PFNEGLGETPLATFORMDISPLAYEXTPROC)
		eglGetProcAddress("eglGetPlatformDisplayEXT");
	if (!qDevices || !gPlatform)
		return "EGL_EXT_platform_device is not available";
	if (!qDevices(16, devs, &ndev) || ndev < 1)
		return "eglQueryDevicesEXT found no EGL device";

	egl_dpy = gPlatform(EGL_PLATFORM_DEVICE_EXT, devs[0], NULL);
	if (egl_dpy == EGL_NO_DISPLAY)
		return "eglGetPlatformDisplayEXT(DEVICE) gave EGL_NO_DISPLAY";
	if (!eglInitialize(egl_dpy, &major, &minor)) {
		snprintf(err, sizeof(err), "eglInitialize: %#x", eglGetError());
		return err;
	}

	exts = eglQueryString(egl_dpy, EGL_EXTENSIONS);
	if (!exts || !strstr(exts, "EGL_EXT_image_dma_buf_import"))
		return "EGL_EXT_image_dma_buf_import is missing -- no dmabuf import";
	egl_have_modifiers = exts && strstr(exts, "EGL_EXT_image_dma_buf_import_modifiers") != NULL;
	if (!strstr(exts, "EGL_KHR_surfaceless_context"))
		return "EGL_KHR_surfaceless_context is missing";

	pCreateImage = (PFNEGLCREATEIMAGEKHRPROC)eglGetProcAddress("eglCreateImageKHR");
	pDestroyImage = (PFNEGLDESTROYIMAGEKHRPROC)eglGetProcAddress("eglDestroyImageKHR");
	if (!pCreateImage || !pDestroyImage)
		return "eglCreateImageKHR is not exported";

	if (!eglBindAPI(EGL_OPENGL_ES_API))
		return "eglBindAPI(ES) failed";
	if (!eglChooseConfig(egl_dpy, cfg_attr, &conf, 1, &nconf) || nconf < 1)
		return "eglChooseConfig found no ES2 config";
	egl_ctx = eglCreateContext(egl_dpy, conf, EGL_NO_CONTEXT, ctx_attr);
	if (egl_ctx == EGL_NO_CONTEXT) {
		snprintf(err, sizeof(err), "eglCreateContext: %#x", eglGetError());
		return err;
	}
	if (!eglMakeCurrent(egl_dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, egl_ctx)) {
		snprintf(err, sizeof(err), "eglMakeCurrent(surfaceless): %#x", eglGetError());
		return err;
	}

	libgles = dlopen("libGLESv2.so.2", RTLD_NOW | RTLD_GLOBAL);
	if (!libgles)
		return "libGLESv2.so.2 not loadable";
#define GET(n) do { *(void **)&gl.n = dlsym(libgles, "gl" #n); \
		    if (!gl.n) *(void **)&gl.n = (void *)eglGetProcAddress("gl" #n); \
		    if (!gl.n) return "missing gl" #n; } while (0)
	GET(GenTextures); GET(BindTexture); GET(DeleteTextures); GET(TexParameteri);
	GET(GenFramebuffers); GET(BindFramebuffer); GET(DeleteFramebuffers);
	GET(FramebufferTexture2D); GET(CheckFramebufferStatus);
	GET(ReadPixels); GET(Finish); GET(GetError);
	GET(TexImage2D); GET(CreateShader); GET(ShaderSource); GET(CompileShader);
	GET(GetShaderiv); GET(GetShaderInfoLog); GET(CreateProgram); GET(AttachShader);
	GET(LinkProgram); GET(GetProgramiv); GET(GetProgramInfoLog); GET(UseProgram);
	GET(GetAttribLocation); GET(GetUniformLocation); GET(Uniform1i);
	GET(VertexAttribPointer); GET(EnableVertexAttribArray); GET(DrawArrays);
	GET(Viewport); GET(ActiveTexture);
#undef GET
	*(void **)&gl.EGLImageTargetTexture2DOES =
		(void *)eglGetProcAddress("glEGLImageTargetTexture2DOES");
	if (!gl.EGLImageTargetTexture2DOES)
		return "glEGLImageTargetTexture2DOES is not exported";

	printf("EGL %d.%d, vendor \"%s\", dmabuf modifiers %s\n", major, minor,
	       eglQueryString(egl_dpy, EGL_VENDOR),
	       egl_have_modifiers ? "yes" : "no");
	return NULL;
}

/* Import one dmabuf as an EGLImage, exactly as Sunshine's egl.cpp does. */
static EGLImageKHR import_image(const struct fbinfo *f, int dmabuf)
{
	EGLint a[32];
	int n = 0;

	a[n++] = EGL_WIDTH;                    a[n++] = (EGLint)f->w;
	a[n++] = EGL_HEIGHT;                   a[n++] = (EGLint)f->h;
	a[n++] = EGL_LINUX_DRM_FOURCC_EXT;     a[n++] = (EGLint)f->fourcc;
	a[n++] = EGL_DMA_BUF_PLANE0_FD_EXT;    a[n++] = dmabuf;
	a[n++] = EGL_DMA_BUF_PLANE0_OFFSET_EXT; a[n++] = (EGLint)f->offsets[0];
	a[n++] = EGL_DMA_BUF_PLANE0_PITCH_EXT;  a[n++] = (EGLint)f->pitches[0];
	if (egl_have_modifiers && f->modifier != DRM_FORMAT_MOD_INVALID) {
		a[n++] = EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT;
		a[n++] = (EGLint)(f->modifier & 0xffffffffu);
		a[n++] = EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT;
		a[n++] = (EGLint)(f->modifier >> 32);
	}
	a[n++] = EGL_NONE;

	return pCreateImage(egl_dpy, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT,
			    (EGLClientBuffer)NULL, a);
}

/* ------------------------------------------------------------- way (b) GL */

/* Bind the imported EGLImage to a GL texture. Both GPU ways share it: the
 * GL one reads it back through an FBO, the CUDA one registers this very
 * texture object. Sharing matters -- two textures would be two imports, and
 * "one worked, the other did not" would then be ambiguous. */
static GLuint make_tex(EGLImageKHR img, const char **err)
{
	GLuint tex = 0;
	GLenum st;

	*err = NULL;
	gl.GenTextures(1, &tex);
	gl.BindTexture(GL_TEXTURE_2D, tex);
	gl.TexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
	gl.TexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
	gl.EGLImageTargetTexture2DOES(GL_TEXTURE_2D, img);
	if ((st = gl.GetError()) != GL_NO_ERROR) {
		static char b[80];
		snprintf(b, sizeof(b), "glEGLImageTargetTexture2DOES: GL error %#x", st);
		*err = b;
		gl.DeleteTextures(1, &tex);
		return 0;
	}
	return tex;
}

/*
 * `--dump PATH`: write ONE frame out as a PPM, from the GL way.
 *
 * Why the GL way and not mmap: the scanout is block-linear (modifier
 * != 0), so the mmap way's (x, y) addressing is not the pixel at (x, y) --
 * its own comment says so. The EGL import de-tiles, which makes the GL
 * readback the only one of the three that is an IMAGE rather than a bag of
 * bytes.
 *
 * And why it exists at all: every verdict this program prints is a COUNT --
 * nonzero pixels, changed frames. A desktop whose windows never reach the
 * scanout still counts as "content" as long as a clock ticks somewhere in
 * it. Measured 2026-08-17: the counters said GREEN while a human on the
 * other end of the stream saw no window at all. A count cannot settle that;
 * a picture can.
 */
static const char *dump_path;
static int dump_done;

static void dump_ppm(const uint8_t *rgba, uint32_t w, uint32_t h)
{
	FILE *fp;
	uint8_t *row;
	uint32_t y, x;

	if (!dump_path || dump_done)
		return;
	fp = fopen(dump_path, "wb");
	if (!fp) {
		fprintf(stderr, "--dump %s: %s\n", dump_path, strerror(errno));
		dump_done = 1;
		return;
	}
	row = malloc((size_t)w * 3);
	if (!row) { fclose(fp); dump_done = 1; return; }
	fprintf(fp, "P6\n%u %u\n255\n", w, h);
	for (y = 0; y < h; y++) {
		const uint8_t *src = rgba + (size_t)y * w * 4;

		for (x = 0; x < w; x++) {
			row[x * 3 + 0] = src[x * 4 + 0];
			row[x * 3 + 1] = src[x * 4 + 1];
			row[x * 3 + 2] = src[x * 4 + 2];
		}
		fwrite(row, 3, w, fp);
	}
	free(row);
	fclose(fp);
	dump_done = 1;
	printf("  dumped one %ux%u frame to %s\n", w, h, dump_path);
}

static void read_gl(struct way *w, const struct fbinfo *f, GLuint tex,
		    const struct grid *g)
{
	static uint32_t px[MAX_SAMPLES];
	static uint8_t *buf;
	static size_t bufsz;
	GLuint fbo = 0;
	GLenum st;
	size_t need;
	int i;

	w->attempts++;
	need = (size_t)f->w * f->h * 4;
	if (need > bufsz) {
		uint8_t *nb = realloc(buf, need);
		if (!nb) { way_fail(w, "out of memory for the readback buffer"); return; }
		buf = nb; bufsz = need;
	}

	gl.GenFramebuffers(1, &fbo);
	gl.BindFramebuffer(GL_FRAMEBUFFER, fbo);
	gl.FramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
				GL_TEXTURE_2D, tex, 0);
	if ((st = gl.CheckFramebufferStatus(GL_FRAMEBUFFER)) != GL_FRAMEBUFFER_COMPLETE) {
		way_fail(w, "framebuffer incomplete: %#x", st);
		goto out;
	}
	gl.ReadPixels(0, 0, (GLsizei)f->w, (GLsizei)f->h,
		      GL_RGBA, GL_UNSIGNED_BYTE, buf);
	gl.Finish();
	if ((st = gl.GetError()) != GL_NO_ERROR) {
		way_fail(w, "glReadPixels: GL error %#x", st);
		goto out;
	}
	for (i = 0; i < g->n; i++)
		px[i] = *(const uint32_t *)(buf + ((size_t)g->y[i] * f->w + g->x[i]) * 4);
	dump_ppm(buf, f->w, f->h);
	way_score(w, px, g->n, buf, need);
out:
	gl.BindFramebuffer(GL_FRAMEBUFFER, 0);
	if (fbo) gl.DeleteFramebuffers(1, &fbo);
}

/* ----------------------------------------------------------- way (c) CUDA */

static const char *blit_setup(void);   /* defined below, next to its shaders */

/* cuda.h is not in the guest, and installing a CUDA toolkit to ask a
 * yes/no question is out of proportion. These are the pieces Sunshine's
 * cuda.cpp uses, declared by hand. Every field the probe reads is PRINTED,
 * so a struct that drifted against the driver shows up as nonsense
 * dimensions rather than as a silently wrong answer. */
typedef int CUresult_t;
typedef unsigned long long CUdeviceptr_t;
typedef void *CUarray_t;
typedef void *CUcontext_t;
typedef void *CUgraphicsResource_t;
typedef int CUdevice_t;

#define CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY 0x01
#define CU_MEMORYTYPE_HOST_T   1
#define CU_MEMORYTYPE_DEVICE_T 2
#define CU_MEMORYTYPE_ARRAY_T  3
#define CU_EGL_FRAME_TYPE_ARRAY 0
#define CU_EGL_FRAME_TYPE_PITCH 1

typedef struct {
	union { CUarray_t pArray[3]; void *pPitch[3]; } frame;
	unsigned int width, height, depth, pitch, planeCount, numChannels;
	int frameType;        /* CUeglFrameType   */
	int eglColorFormat;   /* CUeglColorFormat */
	int cuFormat;         /* CUarray_format   */
} CUeglFrame_t;

typedef struct {
	size_t srcXInBytes, srcY;
	int srcMemoryType;
	const void *srcHost;
	CUdeviceptr_t srcDevice;
	CUarray_t srcArray;
	size_t srcPitch;
	size_t dstXInBytes, dstY;
	int dstMemoryType;
	void *dstHost;
	CUdeviceptr_t dstDevice;
	CUarray_t dstArray;
	size_t dstPitch;
	size_t WidthInBytes, Height;
} CUDA_MEMCPY2D_t;

static struct {
	CUresult_t (*Init)(unsigned);
	CUresult_t (*DeviceGetCount)(int *);
	CUresult_t (*DeviceGet)(CUdevice_t *, int);
	CUresult_t (*CtxCreate)(CUcontext_t *, unsigned, CUdevice_t);
	CUresult_t (*CtxDestroy)(CUcontext_t);
	CUresult_t (*GraphicsEGLRegisterImage)(CUgraphicsResource_t *, void *, unsigned);
	CUresult_t (*GraphicsResourceGetMappedEglFrame)(CUeglFrame_t *, CUgraphicsResource_t,
							unsigned, unsigned);
	CUresult_t (*GraphicsUnregisterResource)(CUgraphicsResource_t);
	CUresult_t (*Memcpy2D)(const CUDA_MEMCPY2D_t *);
	CUresult_t (*GetErrorString)(CUresult_t, const char **);
	/* The desktop route. cuGraphicsEGLRegisterImage is in practice a
	 * Tegra/L4T interface: on this very host -- a working Hyprland on a
	 * real RTX 2070 -- it answers CUDA_ERROR_INVALID_VALUE for a
	 * perfectly good dmabuf EGLImage. Measured 2026-08-16, and it is
	 * the reason this second route exists at all: without it the probe
	 * would have reported "CUDA cannot read the scanout buffer" as a
	 * finding about the GUEST, when it is a finding about the API. */
	CUresult_t (*GraphicsGLRegisterImage)(CUgraphicsResource_t *, GLuint, GLenum, unsigned);
	CUresult_t (*GraphicsMapResources)(unsigned, CUgraphicsResource_t *, void *);
	CUresult_t (*GraphicsUnmapResources)(unsigned, CUgraphicsResource_t *, void *);
	CUresult_t (*GraphicsSubResourceGetMappedArray)(CUarray_t *, CUgraphicsResource_t,
							unsigned, unsigned);
} cu;

static CUcontext_t cu_ctx;

static const char *cu_err(CUresult_t r)
{
	const char *s = NULL;
	if (cu.GetErrorString && cu.GetErrorString(r, &s) == 0 && s)
		return s;
	return "(no string)";
}

static const char *cuda_setup(void)
{
	static char err[160];
	void *lib;
	CUresult_t r;
	int count = 0;
	CUdevice_t dev;

	lib = dlopen("libcuda.so.1", RTLD_NOW);
	if (!lib)
		return "libcuda.so.1 not loadable";
#define GETCU(n) do { *(void **)&cu.n = dlsym(lib, "cu" #n); } while (0)
	GETCU(Init); GETCU(DeviceGetCount); GETCU(GraphicsEGLRegisterImage);
	GETCU(GraphicsResourceGetMappedEglFrame); GETCU(GraphicsUnregisterResource);
	GETCU(GetErrorString);
#undef GETCU
	/* The _v2 spellings are what the driver actually exports for these. */
	*(void **)&cu.DeviceGet = dlsym(lib, "cuDeviceGet");
	*(void **)&cu.CtxCreate = dlsym(lib, "cuCtxCreate_v2");
	*(void **)&cu.CtxDestroy = dlsym(lib, "cuCtxDestroy_v2");
	*(void **)&cu.Memcpy2D = dlsym(lib, "cuMemcpy2D_v2");
	*(void **)&cu.GraphicsGLRegisterImage = dlsym(lib, "cuGraphicsGLRegisterImage");
	*(void **)&cu.GraphicsMapResources = dlsym(lib, "cuGraphicsMapResources");
	*(void **)&cu.GraphicsUnmapResources = dlsym(lib, "cuGraphicsUnmapResources");
	*(void **)&cu.GraphicsSubResourceGetMappedArray =
		dlsym(lib, "cuGraphicsSubResourceGetMappedArray");
	if (!cu.Init || !cu.DeviceGet || !cu.CtxCreate || !cu.Memcpy2D)
		return "libcuda is missing cuInit/cuCtxCreate/cuMemcpy2D";
	if (!cu.GraphicsEGLRegisterImage && !cu.GraphicsGLRegisterImage)
		return "libcuda exports neither the EGL nor the GL interop entry point";

	if ((r = cu.Init(0)) != 0) {
		snprintf(err, sizeof(err), "cuInit: %s", cu_err(r)); return err;
	}
	if ((r = cu.DeviceGetCount(&count)) != 0 || count < 1) {
		snprintf(err, sizeof(err), "cuDeviceGetCount: %d device(s), %s",
			 count, cu_err(r));
		return err;
	}
	if ((r = cu.DeviceGet(&dev, 0)) != 0) {
		snprintf(err, sizeof(err), "cuDeviceGet: %s", cu_err(r)); return err;
	}
	if ((r = cu.CtxCreate(&cu_ctx, 0, dev)) != 0) {
		snprintf(err, sizeof(err), "cuCtxCreate: %s", cu_err(r)); return err;
	}
	if (cu.GraphicsGLRegisterImage) {
		const char *b = blit_setup();

		if (b) {
			snprintf(err, sizeof(err), "the blit shader: %.130s", b);
			return err;
		}
	}
	printf("CUDA: context on device 0\n");
	return NULL;
}

/* A full-screen textured triangle strip. A raw glCopyTexSubImage2D
 * cannot do this job: the scanout is XR24, so the source framebuffer has no
 * alpha channel, and copying it into an RGBA texture is GL_INVALID_OPERATION
 * (0x502, measured on this host). Writing alpha = 1.0 in a fragment shader
 * is what makes the destination a legal 4-channel texture -- which is in
 * turn what CUDA will register. Sunshine reaches the same place through its
 * colour-conversion shader. */
static GLuint blit_prog, blit_pos, blit_tex_u;

static const char *blit_setup(void)
{
	static const char *vs =
		"attribute vec2 p;\nvarying vec2 uv;\n"
		"void main() { uv = p * 0.5 + 0.5; gl_Position = vec4(p, 0.0, 1.0); }\n";
	static const char *fs =
		"precision mediump float;\nvarying vec2 uv;\nuniform sampler2D t;\n"
		"void main() { gl_FragColor = vec4(texture2D(t, uv).rgb, 1.0); }\n";
	static char log[512];
	GLuint v, f;
	GLint ok = 0;

	v = gl.CreateShader(GL_VERTEX_SHADER);
	gl.ShaderSource(v, 1, &vs, NULL);
	gl.CompileShader(v);
	gl.GetShaderiv(v, GL_COMPILE_STATUS, &ok);
	if (!ok) { gl.GetShaderInfoLog(v, sizeof(log), NULL, log); return log; }
	f = gl.CreateShader(GL_FRAGMENT_SHADER);
	gl.ShaderSource(f, 1, &fs, NULL);
	gl.CompileShader(f);
	gl.GetShaderiv(f, GL_COMPILE_STATUS, &ok);
	if (!ok) { gl.GetShaderInfoLog(f, sizeof(log), NULL, log); return log; }
	blit_prog = gl.CreateProgram();
	gl.AttachShader(blit_prog, v);
	gl.AttachShader(blit_prog, f);
	gl.LinkProgram(blit_prog);
	gl.GetProgramiv(blit_prog, GL_LINK_STATUS, &ok);
	if (!ok) { gl.GetProgramInfoLog(blit_prog, sizeof(log), NULL, log); return log; }
	blit_pos = (GLuint)gl.GetAttribLocation(blit_prog, "p");
	blit_tex_u = (GLuint)gl.GetUniformLocation(blit_prog, "t");
	return NULL;
}

/* Which interop route actually worked, so the summary can say so. */
static const char *cuda_route = "(none yet)";

/* The desktop route: register the GL texture that already holds the
 * imported dmabuf, map it, and copy the mipmap level 0 array to the host.
 * Same question as the EGL route, asked through the interface this platform
 * actually implements. */
static int read_cuda_via_gl(struct way *w, const struct fbinfo *f, GLuint tex,
			    uint8_t *buf)
{
	/* Registered once and kept: cuGraphicsGLRegisterImage is expensive,
	 * and Sunshine registers its encoder textures once per session too. */
	static CUgraphicsResource_t reg;
	static GLuint owned, owned_fbo;
	static uint32_t owned_w, owned_h;
	CUgraphicsResource_t res;
	CUarray_t arr = NULL;
	CUDA_MEMCPY2D_t m;
	CUresult_t r;
	GLenum st;
	int rc = -1;

	if (!cu.GraphicsGLRegisterImage || !cu.GraphicsMapResources ||
	    !cu.GraphicsSubResourceGetMappedArray || !cu.GraphicsUnmapResources) {
		way_fail(w, "no GL-interop entry points in libcuda");
		return -1;
	}

	/* CUDA will NOT register a texture whose storage is an imported
	 * EGLImage -- cuGraphicsGLRegisterImage answers "invalid argument",
	 * measured on this host's working Hyprland as well as in the guest.
	 * Sunshine has the same constraint and solves it the same way: it
	 * copies the imported image into a texture it allocated itself and
	 * registers THAT. The copy here is a framebuffer-to-texture blit
	 * rather than Sunshine's colour-conversion shader, because the
	 * question is "are the pixels there", not "are they NV12 yet". */
	if (!owned || owned_w != f->w || owned_h != f->h) {
		if (owned) {
			if (reg) { cu.GraphicsUnregisterResource(reg); reg = NULL; }
			gl.DeleteTextures(1, &owned);
			gl.DeleteFramebuffers(1, &owned_fbo);
		}
		gl.GenTextures(1, &owned);
		gl.BindTexture(GL_TEXTURE_2D, owned);
		gl.TexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
		gl.TexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
		gl.TexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, (GLsizei)f->w, (GLsizei)f->h,
			      0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
		if ((st = gl.GetError()) != GL_NO_ERROR) {
			way_fail(w, "glTexImage2D for the owned texture: %#x", st);
			gl.DeleteTextures(1, &owned); owned = 0;
			return -1;
		}
		gl.GenFramebuffers(1, &owned_fbo);
		owned_w = f->w; owned_h = f->h;
	}

	/* Draw the imported texture into the owned one. */
	{
		static const float quad[8] = { -1, -1,  1, -1, -1,  1,  1,  1 };

		gl.BindFramebuffer(GL_FRAMEBUFFER, owned_fbo);
		gl.FramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
					GL_TEXTURE_2D, owned, 0);
		if ((st = gl.CheckFramebufferStatus(GL_FRAMEBUFFER)) != GL_FRAMEBUFFER_COMPLETE) {
			way_fail(w, "blit destination framebuffer incomplete: %#x", st);
			gl.BindFramebuffer(GL_FRAMEBUFFER, 0);
			return -1;
		}
		gl.Viewport(0, 0, (GLsizei)f->w, (GLsizei)f->h);
		gl.UseProgram(blit_prog);
		gl.ActiveTexture(GL_TEXTURE0);
		gl.BindTexture(GL_TEXTURE_2D, tex);
		gl.Uniform1i((GLint)blit_tex_u, 0);
		gl.EnableVertexAttribArray(blit_pos);
		gl.VertexAttribPointer(blit_pos, 2, GL_FLOAT, 0, 0, quad);
		gl.DrawArrays(GL_TRIANGLE_STRIP, 0, 4);
		gl.Finish();
		gl.BindFramebuffer(GL_FRAMEBUFFER, 0);
		if ((st = gl.GetError()) != GL_NO_ERROR) {
			way_fail(w, "the blit into the owned texture: GL error %#x", st);
			return -1;
		}
	}

	if (!reg && (r = cu.GraphicsGLRegisterImage(&reg, owned, GL_TEXTURE_2D,
			CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY)) != 0) {
		reg = NULL;
		way_fail(w, "cuGraphicsGLRegisterImage: %s", cu_err(r));
		return -1;
	}
	res = reg;
	if ((r = cu.GraphicsMapResources(1, &res, NULL)) != 0) {
		way_fail(w, "cuGraphicsMapResources: %s", cu_err(r));
		return -1;
	}
	if ((r = cu.GraphicsSubResourceGetMappedArray(&arr, res, 0, 0)) != 0) {
		way_fail(w, "cuGraphicsSubResourceGetMappedArray: %s", cu_err(r));
		goto unmap;
	}
	(void)arr;
	memset(&m, 0, sizeof(m));
	m.srcMemoryType = CU_MEMORYTYPE_ARRAY_T;
	m.srcArray = arr;
	m.dstMemoryType = CU_MEMORYTYPE_HOST_T;
	m.dstHost = buf;
	m.dstPitch = (size_t)f->w * 4;
	m.WidthInBytes = (size_t)f->w * 4;
	m.Height = f->h;
	if ((r = cu.Memcpy2D(&m)) != 0) {
		way_fail(w, "cuMemcpy2D (GL route): %s", cu_err(r));
		goto unmap;
	}
	cuda_route = "GL interop (EGLImage -> owned texture -> cuGraphicsGLRegisterImage)";
	rc = 0;
unmap:
	cu.GraphicsUnmapResources(1, &res, NULL);
	return rc;
}

static void read_cuda(struct way *w, const struct fbinfo *f, EGLImageKHR img,
		      GLuint tex, const struct grid *g)
{
	static uint32_t px[MAX_SAMPLES];
	static uint8_t *buf;
	static size_t bufsz;
	static int shown;
	CUgraphicsResource_t res = NULL;
	CUeglFrame_t ef;
	CUDA_MEMCPY2D_t m;
	CUresult_t r;
	size_t need;
	int i;

	w->attempts++;
	need = (size_t)f->w * f->h * 4;
	if (need > bufsz) {
		uint8_t *nb = realloc(buf, need);
		if (!nb) { way_fail(w, "out of memory for the readback buffer"); return; }
		buf = nb; bufsz = need;
	}

	/* This is the call. If the black stream is a CUDA-interop problem,
	 * it fails or lies right here. */
	if (!cu.GraphicsEGLRegisterImage ||
	    (r = cu.GraphicsEGLRegisterImage(&res, img,
			CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY)) != 0) {
		/* Expected on desktop x86: see the note on
		 * GraphicsGLRegisterImage above. Fall through to the route
		 * this platform does implement rather than calling the whole
		 * way dead -- a probe that gives up here would have blamed
		 * the guest for an API that is missing everywhere. */
		char egl_why[100], gl_why[100];

		snprintf(egl_why, sizeof(egl_why), "%s",
			 cu.GraphicsEGLRegisterImage ? cu_err(r)
						     : "not exported");
		if (read_cuda_via_gl(w, f, tex, buf) == 0) {
			/* read_cuda_via_gl counted no failure, so nothing to
			 * undo; it scores below. */
			goto score;
		}
		/* Both routes are out. Report BOTH reasons: "invalid
		 * argument" from the Tegra-only entry point is expected
		 * noise, and hiding the GL route's reason behind it would
		 * bury the finding. snprintf must not read and write the
		 * same buffer, hence the copy. */
		snprintf(gl_why, sizeof(gl_why), "%.99s", w->last_err);
		snprintf(w->last_err, sizeof(w->last_err),
			 "EGL route: %.90s; GL route: %.110s", egl_why, gl_why);
		return;
	}
	cuda_route = "EGL interop (cuGraphicsEGLRegisterImage)";
	memset(&ef, 0, sizeof(ef));
	if ((r = cu.GraphicsResourceGetMappedEglFrame(&ef, res, 0, 0)) != 0) {
		way_fail(w, "cuGraphicsResourceGetMappedEglFrame: %s", cu_err(r));
		goto out;
	}
	if (!shown) {
		/* Printed once: these numbers are the proof that the
		 * hand-written CUeglFrame above matches the driver's. If
		 * width/height are not the framebuffer's, stop reading the
		 * rest of this way's output -- the struct has drifted. */
		printf("CUDA eglFrame: %ux%u, planes %u, channels %u, "
		       "type %s, pitch %u, cuFormat %d, colorFormat %d\n",
		       ef.width, ef.height, ef.planeCount, ef.numChannels,
		       ef.frameType == CU_EGL_FRAME_TYPE_ARRAY ? "ARRAY" : "PITCH",
		       ef.pitch, ef.cuFormat, ef.eglColorFormat);
		if (ef.width != f->w || ef.height != f->h)
			printf("  eglFrame dimensions %ux%u != framebuffer %ux%u"
			       " -- treat way (c) as unreliable\n",
			       ef.width, ef.height, f->w, f->h);
		shown = 1;
	}

	memset(&m, 0, sizeof(m));
	if (ef.frameType == CU_EGL_FRAME_TYPE_ARRAY) {
		m.srcMemoryType = CU_MEMORYTYPE_ARRAY_T;
		m.srcArray = ef.frame.pArray[0];
	} else {
		m.srcMemoryType = CU_MEMORYTYPE_DEVICE_T;
		m.srcDevice = (CUdeviceptr_t)(uintptr_t)ef.frame.pPitch[0];
		m.srcPitch = ef.pitch;
	}
	m.dstMemoryType = CU_MEMORYTYPE_HOST_T;
	m.dstHost = buf;
	m.dstPitch = (size_t)f->w * 4;
	m.WidthInBytes = (size_t)f->w * 4;
	m.Height = f->h;
	if ((r = cu.Memcpy2D(&m)) != 0) {
		way_fail(w, "cuMemcpy2D: %s", cu_err(r));
		goto out;
	}
score:
	for (i = 0; i < g->n; i++)
		px[i] = *(const uint32_t *)(buf + ((size_t)g->y[i] * f->w + g->x[i]) * 4);
	way_score(w, px, g->n, buf, need);
out:
	if (res)
		cu.GraphicsUnregisterResource(res);
}

/* -------------------------------------------------------------------- main */

static void usage(void)
{
	puts("fbprobe [--node /dev/dri/cardN] [--plane ID] [--loop N]\n"
	     "        [--interval MS] [--samples N] [--dump FILE.ppm] [--list] [-v]\n"
	     "\n"
	     "Reads the scanout framebuffer the same way Sunshine's kms capture\n"
	     "does and reports, per way, how many sampled pixels are nonzero and\n"
	     "how many changed since the previous poll. Needs root for GETFB2.\n"
	     "\n"
	     "--dump writes the FIRST frame the GL way reads out as a PPM, so the\n"
	     "picture can be LOOKED AT. The counters above cannot tell a desktop\n"
	     "whose windows never reach the scanout from one that composites.");
}

int main(int argc, char **argv)
{
	const char *node = NULL;
	char chosen[64] = "";
	int loops = 10, interval = 250, samples = 1024, list_only = 0;
	uint32_t want_plane = 0;
	struct fbinfo planes[8];
	struct grid g;
	struct way w_mmap = { .name = "mmap" }, w_gl = { .name = "gl" },
		   w_cuda = { .name = "cuda" };
	const char *e;
	uint32_t target_plane = 0, geom_w = 0, geom_h = 0;
	int i, n, iter, no_plane_polls = 0;
	char fcc[5];

	for (i = 1; i < argc; i++) {
		if (!strcmp(argv[i], "--node") && i + 1 < argc) node = argv[++i];
		else if (!strcmp(argv[i], "--plane") && i + 1 < argc) want_plane = (uint32_t)atoi(argv[++i]);
		else if (!strcmp(argv[i], "--loop") && i + 1 < argc) loops = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--interval") && i + 1 < argc) interval = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--samples") && i + 1 < argc) samples = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--dump") && i + 1 < argc) dump_path = argv[++i];
		else if (!strcmp(argv[i], "--list")) list_only = 1;
		else if (!strcmp(argv[i], "-v")) verbose = 1;
		else if (!strcmp(argv[i], "-h") || !strcmp(argv[i], "--help")) { usage(); return 0; }
		else { usage(); return 2; }
	}
	if (samples > MAX_SAMPLES) samples = MAX_SAMPLES;

	if (node) {
		drm_fd = open(node, O_RDWR | O_CLOEXEC);
		if (drm_fd >= 0) snprintf(chosen, sizeof(chosen), "%s", node);
	} else {
		drm_fd = open_node("nvidia-drm", chosen, sizeof(chosen));
	}
	if (drm_fd < 0) {
		fprintf(stderr, "no usable DRM node (%s)\n",
			node ? strerror(errno) : "no node with driver \"nvidia-drm\"");
		return 2;
	}
	if (geteuid() != 0)
		fprintf(stderr, "not root -- GETFB2 will return handle 0 "
			"and nothing below will work\n");
	/* Without this the plane list holds only overlays: primary and cursor
	 * are hidden from a legacy client, and the probe would report "no
	 * plane is scanning out" on a perfectly good desktop. */
	{
		struct drm_set_client_cap cap = { DRM_CLIENT_CAP_UNIVERSAL_PLANES, 1 };
		if (ioctl(drm_fd, DRM_IOCTL_SET_CLIENT_CAP, &cap) < 0)
			fprintf(stderr, "UNIVERSAL_PLANES cap refused: %s\n",
				strerror(errno));
		/* Atomic implies universal planes and is what every modern
		 * compositor sets. Asking for it costs nothing if it is
		 * refused, and some drivers only populate plane state for a
		 * client that has it. */
		cap.capability = DRM_CLIENT_CAP_ATOMIC;
		cap.value = 1;
		if (ioctl(drm_fd, DRM_IOCTL_SET_CLIENT_CAP, &cap) < 0)
			vlog("  ATOMIC cap refused: %s\n", strerror(errno));
	}
	printf("node %s\n", chosen);

	n = collect_planes(planes, 8);
	if (n <= 0) {
		fprintf(stderr, "no plane is scanning out: %d plane(s) exist, "
			"%d of them dark\n", planes_total, planes_dark);
		dump_crtcs();
		if (planes_total > 0)
			fprintf(stderr,
			  "planes exist but none holds a framebuffer. Under the\n"
			  "    NVIDIA X driver that is NORMAL: Xorg programs the head\n"
			  "    through NVKMS and never publishes DRM plane state, so\n"
			  "    there is nothing here for a kms grabber to read either.\n"
			  "    A Wayland compositor on the DRM backend does publish it.\n");
		return 1;
	}
	printf("planes scanning out: %d\n", n);
	for (i = 0; i < n; i++)
		printf("  plane %u (crtc %u) fb %u  %ux%u  %s  modifier %#llx  %d dmabuf plane(s)\n",
		       planes[i].plane_id, planes[i].crtc_id, planes[i].fb_id,
		       planes[i].w, planes[i].h, fourcc_str(planes[i].fourcc, fcc),
		       (unsigned long long)planes[i].modifier, planes[i].planes);

	/* Biggest wins: the 64x64 one is the cursor, and a probe that latches
	 * onto it reports a healthy stream for a black desktop -- which is
	 * exactly the failure this program exists to tell apart. */
	if (want_plane) {
		target_plane = want_plane;
	} else {
		uint64_t best = 0;
		for (i = 0; i < n; i++) {
			uint64_t area = (uint64_t)planes[i].w * planes[i].h;
			if (area > best) { best = area; target_plane = planes[i].plane_id; }
		}
	}
	for (i = 0; i < n; i++)
		if (planes[i].plane_id == target_plane) { geom_w = planes[i].w; geom_h = planes[i].h; }
	for (i = 0; i < n; i++)
		close_handles(&planes[i]);
	if (!geom_w) {
		fprintf(stderr, "plane %u is not scanning out\n", target_plane);
		return 1;
	}
	printf("watching plane %u (%ux%u)\n", target_plane, geom_w, geom_h);
	if (list_only)
		return 0;

	grid_build(&g, geom_w, geom_h, samples);
	printf("%d sample points, %d polls, %d ms apart\n\n", g.n, loops, interval);

	e = egl_setup();
	w_gl.available = (e == NULL); w_gl.why_not = e;
	if (e) printf("EGL import unavailable: %s\n", e);
	if (w_gl.available) {
		e = cuda_setup();
		w_cuda.available = (e == NULL); w_cuda.why_not = e;
		if (e) printf("CUDA import unavailable: %s\n", e);
	} else {
		w_cuda.available = 0;
		w_cuda.why_not = "EGL is unavailable, so there is no image to register";
	}
	w_mmap.available = 1;
	putchar('\n');

	for (iter = 0; iter < loops; iter++) {
		struct fbinfo *f = NULL;
		int dmabuf = -1;
		EGLImageKHR img = EGL_NO_IMAGE_KHR;

		n = collect_planes(planes, 8);
		for (i = 0; i < n; i++)
			if (planes[i].plane_id == target_plane) f = &planes[i];
		if (!f) {
			no_plane_polls++;
			for (i = 0; i < n; i++) close_handles(&planes[i]);
			msleep(interval);
			continue;
		}
		vlog("  poll %d: fb %u\n", iter, f->fb_id);

		if (prime_export(f->handles[0], &dmabuf) < 0) {
			fprintf(stderr, "  PRIME_HANDLE_TO_FD: %s\n", strerror(errno));
			w_mmap.attempts++; way_fail(&w_mmap, "no dmabuf: PRIME export failed");
			goto next;
		}
		if (w_mmap.available)
			read_mmap(&w_mmap, f, dmabuf, &g);
		if (w_gl.available || w_cuda.available) {
			img = import_image(f, dmabuf);
			if (img == EGL_NO_IMAGE_KHR) {
				EGLint err = eglGetError();
				if (w_gl.available) { w_gl.attempts++;
					way_fail(&w_gl, "eglCreateImageKHR(dmabuf): %#x", err); }
				if (w_cuda.available) { w_cuda.attempts++;
					way_fail(&w_cuda, "eglCreateImageKHR(dmabuf): %#x", err); }
			} else {
				const char *terr = NULL;
				GLuint tex = make_tex(img, &terr);

				if (!tex) {
					if (w_gl.available) { w_gl.attempts++;
						way_fail(&w_gl, "%s", terr); }
					if (w_cuda.available) { w_cuda.attempts++;
						way_fail(&w_cuda, "%s", terr); }
				} else {
					if (w_gl.available)
						read_gl(&w_gl, f, tex, &g);
					if (w_cuda.available)
						read_cuda(&w_cuda, f, img, tex, &g);
					gl.DeleteTextures(1, &tex);
				}
				pDestroyImage(egl_dpy, img);
			}
		}
next:
		if (dmabuf >= 0) close(dmabuf);
		for (i = 0; i < n; i++) close_handles(&planes[i]);
		if (iter + 1 < loops) msleep(interval);
	}

	/* The verdict. One line per way, and it says what it measured rather
	 * than PASS/FAIL: "the import worked and the picture is moving" and
	 * "the import worked and the picture is frozen black" are different
	 * findings and must not collapse into one word. */
	printf("\n== fbprobe verdict (plane %u, %ux%u) ==\n", target_plane, geom_w, geom_h);
	if (no_plane_polls)
		printf("  %d/%d polls found no framebuffer on that plane\n",
		       no_plane_polls, loops);
	{
		struct way *ways[3] = { &w_mmap, &w_gl, &w_cuda };
		int k, green = 0, amber = 0;

		for (k = 0; k < 3; k++) {
			struct way *w = ways[k];

			if (!w->available) {
				printf("  %-5s UNAVAILABLE  %s\n", w->name,
				       w->why_not ? w->why_not : "");
				continue;
			}
			if (!w->ok) {
				printf("  %-5s BROKEN       %d/%d polls failed; last: %s\n",
				       w->name, w->failed, w->attempts,
				       w->last_err[0] ? w->last_err : "(no detail)");
				continue;
			}
			printf("  %-5s %s  reads %d/%d  nonzero in %d polls (peak %d/%d)"
			       "  frame changed in %d/%d polls\n",
			       w->name,
			       w->polls_nonzero && w->polls_frame_differs ? "CONTENT " :
			       w->polls_nonzero ? "STATIC  " : "BLACK   ",
			       w->ok, w->attempts,
			       w->polls_nonzero, w->best_nonzero, w->last_total,
			       w->polls_frame_differs, w->ok > 0 ? w->ok - 1 : 0);
			if (w->failed)
				printf("        (%d poll(s) failed; last: %s)\n",
				       w->failed, w->last_err);
			if (w == &w_cuda && w->ok)
				printf("        route: %s\n", cuda_route);
			if (w->polls_nonzero && w->polls_frame_differs)
				green++;
			else if (w->polls_nonzero)
				amber++;
		}
		/* Three outcomes, not two. "nonzero but never changing" is a
		 * perfectly good reading of a desktop that is standing still,
		 * and calling it RED would fail every run taken against an
		 * idle screen -- so it gets its own answer and its own exit
		 * code, and the caller says whether it was expecting motion. */
		if (green) {
			printf("\nREADER GREEN: %d way(s) see moving, nonzero content.\n",
			       green);
			return 0;
		}
		if (amber) {
			printf("\nREADER AMBER: %d way(s) read nonzero pixels, but nothing\n"
			       "moved across the polls. On an animating desktop that is a\n"
			       "finding (a frozen or wrongly mapped import); on an idle one\n"
			       "it is simply an idle desktop.\n", amber);
			return 2;
		}
		printf("\nREADER RED: no way read a nonzero pixel -- "
		       "the capture chain is not carrying the desktop.\n");
		return 1;
	}
}
