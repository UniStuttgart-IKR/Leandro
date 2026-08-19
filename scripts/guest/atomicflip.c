// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
/*
 * atomicflip -- do ATOMIC page flips keep completing, or only the first few?
 *
 *   gcc -O2 -Wall -Wextra -o atomicflip atomicflip.c
 *   sudo ./atomicflip --flips 120
 *
 * Why this exists, and why drm-modeset does not answer it. drm-modeset
 * flips through the LEGACY ioctl (DRM_IOCTL_MODE_PAGE_FLIP) and reports
 * 60 issued, 60 completed, 0 timed out on this rig. weston does not use
 * that ioctl: its log says "DRM: supports atomic modesetting", so every
 * frame it shows goes through DRM_IOCTL_MODE_ATOMIC instead. Measured
 * 2026-08-16: weston brings the output up, composites for a few seconds
 * and then stops dead -- zero CPU, no further commits, clients starved of
 * frame callbacks -- while Sunshine happily encodes the same still frame
 * sixty times a second. A legacy-only probe reports green on exactly that
 * arrangement, which is how the legacy result and the observed hang can
 * both be true at once.
 *
 * So this asks the same question through the same door weston uses:
 * modeset once with ALLOW_MODESET, then flip the primary plane's FB_ID in
 * a loop, each commit carrying PAGE_FLIP_EVENT, and count the completions
 * that come back. If the count stops climbing while the loop keeps
 * issuing, the missing completion is reproduced in about eighty lines of
 * client code and weston is off the hook.
 *
 * Raw ioctls, no libdrm: same reason as drm-modeset.c -- the guest image
 * has the kernel headers and not the library, and a probe that needs a
 * package installed before it can answer is a probe that does not get run.
 */
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <time.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <drm/drm.h>
#include <drm/drm_mode.h>

#ifndef DRM_CLIENT_CAP_UNIVERSAL_PLANES
#define DRM_CLIENT_CAP_UNIVERSAL_PLANES 2
#endif
#ifndef DRM_CLIENT_CAP_ATOMIC
#define DRM_CLIENT_CAP_ATOMIC 3
#endif
#ifndef DRM_MODE_ATOMIC_NONBLOCK
#define DRM_MODE_ATOMIC_NONBLOCK   0x0200
#endif
#ifndef DRM_MODE_ATOMIC_ALLOW_MODESET
#define DRM_MODE_ATOMIC_ALLOW_MODESET 0x0400
#endif
#define PLANE_TYPE_PRIMARY 1

static int fd;
static int verbose;

/* ------------------------------------------------------------- plumbing */

static int drm_name(int f, char *out, size_t len)
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

static int open_node(const char *want, char *chosen, size_t len)
{
	char path[64], name[64];
	int i, f;

	for (i = 0; i < 16; i++) {
		snprintf(path, sizeof(path), "/dev/dri/card%d", i);
		f = open(path, O_RDWR | O_CLOEXEC);
		if (f < 0)
			continue;
		if (drm_name(f, name, sizeof(name)) == 0 && !strcmp(name, want)) {
			snprintf(chosen, len, "%s", path);
			return f;
		}
		close(f);
	}
	return -1;
}

/* The property id of `name` on one object, plus optionally its current
 * value. Atomic is all property ids, and they are not constants -- they
 * are allocated per driver instance, so every one of them has to be looked
 * up by name before the first commit. */
static uint32_t prop_id(uint32_t obj_id, uint32_t obj_type, const char *name,
			uint64_t *value_out)
{
	struct drm_mode_obj_get_properties op;
	uint32_t props[128];
	uint64_t vals[128];
	uint32_t i;

	memset(&op, 0, sizeof(op));
	op.obj_id = obj_id;
	op.obj_type = obj_type;
	op.props_ptr = (uint64_t)(uintptr_t)props;
	op.prop_values_ptr = (uint64_t)(uintptr_t)vals;
	op.count_props = 128;
	if (ioctl(fd, DRM_IOCTL_MODE_OBJ_GETPROPERTIES, &op) < 0)
		return 0;
	if (op.count_props > 128)
		op.count_props = 128;

	for (i = 0; i < op.count_props; i++) {
		struct drm_mode_get_property gp;

		memset(&gp, 0, sizeof(gp));
		gp.prop_id = props[i];
		if (ioctl(fd, DRM_IOCTL_MODE_GETPROPERTY, &gp) < 0)
			continue;
		if (!strcmp(gp.name, name)) {
			if (value_out)
				*value_out = vals[i];
			return props[i];
		}
	}
	return 0;
}

/* One atomic request, built as three parallel arrays the way the ioctl
 * wants them: objects, how many properties each carries, then the
 * properties and values run together in object order. */
#define MAX_ITEMS 32
struct req {
	uint32_t objs[MAX_ITEMS];
	uint32_t counts[MAX_ITEMS];
	uint32_t props[MAX_ITEMS];
	uint64_t vals[MAX_ITEMS];
	int nobj, nprop;
};

static void req_reset(struct req *r)
{
	r->nobj = 0;
	r->nprop = 0;
}

static void req_obj(struct req *r, uint32_t obj)
{
	r->objs[r->nobj] = obj;
	r->counts[r->nobj] = 0;
	r->nobj++;
}

static void req_prop(struct req *r, uint32_t prop, uint64_t val)
{
	r->props[r->nprop] = prop;
	r->vals[r->nprop] = val;
	r->nprop++;
	r->counts[r->nobj - 1]++;
}

static int req_commit(struct req *r, uint32_t flags, uint64_t user_data)
{
	struct drm_mode_atomic a;

	memset(&a, 0, sizeof(a));
	a.flags = flags;
	a.count_objs = (uint32_t)r->nobj;
	a.objs_ptr = (uint64_t)(uintptr_t)r->objs;
	a.count_props_ptr = (uint64_t)(uintptr_t)r->counts;
	a.props_ptr = (uint64_t)(uintptr_t)r->props;
	a.prop_values_ptr = (uint64_t)(uintptr_t)r->vals;
	a.user_data = user_data;
	return ioctl(fd, DRM_IOCTL_MODE_ATOMIC, &a);
}

/* A dumb buffer with a solid colour, registered as a framebuffer. */
struct buf { uint32_t handle, fb_id, pitch; uint64_t size; };

static int make_buf(struct buf *b, uint32_t w, uint32_t h, uint32_t colour)
{
	struct drm_mode_create_dumb create;
	struct drm_mode_map_dumb map;
	struct drm_mode_fb_cmd fb;
	uint32_t *px;
	uint64_t i;
	void *p;

	memset(&create, 0, sizeof(create));
	create.width = w; create.height = h; create.bpp = 32;
	if (ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &create) < 0) {
		fprintf(stderr, "CREATE_DUMB: %s\n", strerror(errno));
		return -1;
	}
	b->handle = create.handle; b->pitch = create.pitch; b->size = create.size;

	memset(&fb, 0, sizeof(fb));
	fb.width = w; fb.height = h; fb.bpp = 32; fb.depth = 24;
	fb.pitch = b->pitch; fb.handle = b->handle;
	if (ioctl(fd, DRM_IOCTL_MODE_ADDFB, &fb) < 0) {
		fprintf(stderr, "ADDFB: %s\n", strerror(errno));
		return -1;
	}
	b->fb_id = fb.fb_id;

	memset(&map, 0, sizeof(map));
	map.handle = b->handle;
	if (ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &map) < 0) {
		fprintf(stderr, "MAP_DUMB: %s\n", strerror(errno));
		return -1;
	}
	p = mmap(NULL, b->size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, map.offset);
	if (p == MAP_FAILED) {
		fprintf(stderr, "mmap dumb: %s\n", strerror(errno));
		return -1;
	}
	px = p;
	for (i = 0; i < b->size / 4; i++)
		px[i] = colour;
	munmap(p, b->size);
	return 0;
}

/* Ask the DRM core what the current vblank count is. This needs no
 * master and no modeset, which is the whole point: it is the only way to
 * read the frame counter WHILE a compositor owns the display, and "does the
 * counter advance under weston" is the question that separates a broken
 * compositor from a display that never reports a frame. _DRM_VBLANK_RELATIVE
 * with sequence 0 means "return the current value now" rather than sleeping
 * for a future one. */
static int watch_vblank(unsigned samples, int gap_ms)
{
	union drm_wait_vblank vb;
	unsigned i, advanced = 0;
	uint32_t first = 0, last = 0;

	for (i = 0; i < samples; i++) {
		memset(&vb, 0, sizeof(vb));
		vb.request.type = _DRM_VBLANK_RELATIVE;
		vb.request.sequence = 0;
		if (ioctl(fd, DRM_IOCTL_WAIT_VBLANK, &vb) < 0) {
			fprintf(stderr, "WAIT_VBLANK: %s\n", strerror(errno));
			return 2;
		}
		if (i == 0)
			first = vb.reply.sequence;
		else if (vb.reply.sequence != last)
			advanced++;
		last = vb.reply.sequence;
		printf("  sample %2u: sequence %u, stamp %ld.%06ld\n",
		       i, vb.reply.sequence, (long)vb.reply.tval_sec,
		       (long)vb.reply.tval_usec);
		if (gap_ms > 0) {
			struct timespec ts = { gap_ms / 1000,
					       (long)(gap_ms % 1000) * 1000000L };
			nanosleep(&ts, NULL);
		}
	}
	printf("vblank counter: %u -> %u over %u samples, changed %u time(s)\n",
	       first, last, samples, advanced);
	if (!advanced)
		printf("the DRM frame counter NEVER advanced -- nothing is calling "
		       "drm_handle_vblank, so every compositor that paces on it is "
		       "flying blind\n");
	return advanced ? 0 : 1;
}

/* ------------------------------------------------------------------ main */

int main(int argc, char **argv)
{
	const char *node = NULL;
	char chosen[64] = "";
	unsigned flips = 120, watch = 0;
	int wait_ms = 500, gap_ms = 0, i;

	struct drm_mode_card_res res;
	uint32_t conn_ids[32], crtc_ids[32], enc_ids[32], fb_ids[32];
	struct drm_mode_get_connector conn;
	struct drm_mode_modeinfo modes[64];
	uint32_t conn_enc[32], cprops[64];
	uint64_t cvals[64];
	struct drm_mode_get_plane_res pres;
	uint32_t plane_ids[64];
	uint32_t plane = 0, crtc, connector;
	struct drm_mode_create_blob blob;
	struct req r;

	uint32_t p_fb, p_crtc_id, p_src_x, p_src_y, p_src_w, p_src_h;
	uint32_t p_crtc_x, p_crtc_y, p_crtc_w, p_crtc_h;
	uint32_t c_mode_id, c_active, k_crtc_id;
	struct buf a, b;
	unsigned issued = 0, completed = 0, timedout = 0, f;
	uint32_t first_seq = 0, last_seq = 0;
	unsigned seq_stuck = 0, seq_back = 0;
	double min_skew = 0, max_skew = 0;
	double prev_done = 0, gap_sum = 0, gap_min = 0, gap_max = 0;
	unsigned gap_n = 0;

	for (i = 1; i < argc; i++) {
		if (!strcmp(argv[i], "--flips") && i + 1 < argc) flips = (unsigned)atoi(argv[++i]);
		else if (!strcmp(argv[i], "--wait") && i + 1 < argc) wait_ms = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--gap") && i + 1 < argc) gap_ms = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--watch") && i + 1 < argc) watch = (unsigned)atoi(argv[++i]);
		else if (!strcmp(argv[i], "-v")) verbose = 1;
		else node = argv[i];
	}

	fd = node ? open(node, O_RDWR | O_CLOEXEC) : open_node("nvidia-drm", chosen, sizeof(chosen));
	if (node && fd >= 0) snprintf(chosen, sizeof(chosen), "%s", node);
	if (fd < 0) {
		fprintf(stderr, "no nvidia-drm node (%s)\n", strerror(errno));
		return 2;
	}
	printf("node %s\n", chosen);

	/* The watch runs before any capability or master business: it must be
	 * usable against a display somebody else is driving. */
	if (watch)
		return watch_vblank(watch, gap_ms ? gap_ms : 200);

	/* Atomic is opt-in per client, and asking for it also turns on
	 * universal planes -- without which the primary plane is invisible
	 * and there is nothing to put an FB_ID on. */
	{
		struct drm_set_client_cap cap;

		cap.capability = DRM_CLIENT_CAP_UNIVERSAL_PLANES; cap.value = 1;
		if (ioctl(fd, DRM_IOCTL_SET_CLIENT_CAP, &cap) < 0)
			fprintf(stderr, "UNIVERSAL_PLANES: %s\n", strerror(errno));
		cap.capability = DRM_CLIENT_CAP_ATOMIC; cap.value = 1;
		if (ioctl(fd, DRM_IOCTL_SET_CLIENT_CAP, &cap) < 0) {
			fprintf(stderr, "ATOMIC cap refused: %s -- this driver has no "
				"atomic path for a client, so weston cannot be using "
				"one either\n", strerror(errno));
			return 1;
		}
	}
	if (ioctl(fd, DRM_IOCTL_SET_MASTER, 0) < 0)
		fprintf(stderr, "SET_MASTER: %s (is a compositor still up?)\n",
			strerror(errno));

	memset(&res, 0, sizeof(res));
	res.connector_id_ptr = (uint64_t)(uintptr_t)conn_ids;
	res.crtc_id_ptr = (uint64_t)(uintptr_t)crtc_ids;
	res.encoder_id_ptr = (uint64_t)(uintptr_t)enc_ids;
	res.fb_id_ptr = (uint64_t)(uintptr_t)fb_ids;
	res.count_connectors = res.count_crtcs = res.count_encoders = res.count_fbs = 32;
	if (ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &res) < 0) {
		fprintf(stderr, "GETRESOURCES: %s\n", strerror(errno));
		return 1;
	}
	if (!res.count_crtcs || !res.count_connectors) {
		fprintf(stderr, "no crtc/connector\n");
		return 1;
	}
	crtc = crtc_ids[0];
	connector = conn_ids[0];

	memset(&conn, 0, sizeof(conn));
	conn.connector_id = connector;
	conn.modes_ptr = (uint64_t)(uintptr_t)modes;
	conn.props_ptr = (uint64_t)(uintptr_t)cprops;
	conn.prop_values_ptr = (uint64_t)(uintptr_t)cvals;
	conn.encoders_ptr = (uint64_t)(uintptr_t)conn_enc;
	conn.count_modes = 64; conn.count_props = 64; conn.count_encoders = 32;
	if (ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &conn) < 0) {
		fprintf(stderr, "GETCONNECTOR: %s\n", strerror(errno));
		return 1;
	}
	if (conn.connection != 1 || !conn.count_modes) {
		fprintf(stderr, "connector %u: connection %u, %u modes -- nothing to drive\n",
			connector, conn.connection, conn.count_modes);
		return 1;
	}
	printf("connector %u, crtc %u, mode %ux%u@%u\n", connector, crtc,
	       modes[0].hdisplay, modes[0].vdisplay, modes[0].vrefresh);

	/* The primary plane of this crtc. */
	memset(&pres, 0, sizeof(pres));
	pres.plane_id_ptr = (uint64_t)(uintptr_t)plane_ids;
	pres.count_planes = 64;
	if (ioctl(fd, DRM_IOCTL_MODE_GETPLANERESOURCES, &pres) < 0) {
		fprintf(stderr, "GETPLANERESOURCES: %s\n", strerror(errno));
		return 1;
	}
	if (pres.count_planes > 64) pres.count_planes = 64;
	for (i = 0; i < (int)pres.count_planes; i++) {
		uint64_t type = 0;

		if (!prop_id(plane_ids[i], DRM_MODE_OBJECT_PLANE, "type", &type))
			continue;
		if (type == PLANE_TYPE_PRIMARY) { plane = plane_ids[i]; break; }
	}
	if (!plane) {
		fprintf(stderr, "no primary plane\n");
		return 1;
	}
	printf("primary plane %u\n", plane);

#define NEED(var, obj, otype, name) do { \
		var = prop_id(obj, otype, name, NULL); \
		if (!var) { fprintf(stderr, "missing property \"%s\"\n", name); return 1; } \
	} while (0)
	NEED(p_fb,      plane, DRM_MODE_OBJECT_PLANE, "FB_ID");
	NEED(p_crtc_id, plane, DRM_MODE_OBJECT_PLANE, "CRTC_ID");
	NEED(p_src_x,   plane, DRM_MODE_OBJECT_PLANE, "SRC_X");
	NEED(p_src_y,   plane, DRM_MODE_OBJECT_PLANE, "SRC_Y");
	NEED(p_src_w,   plane, DRM_MODE_OBJECT_PLANE, "SRC_W");
	NEED(p_src_h,   plane, DRM_MODE_OBJECT_PLANE, "SRC_H");
	NEED(p_crtc_x,  plane, DRM_MODE_OBJECT_PLANE, "CRTC_X");
	NEED(p_crtc_y,  plane, DRM_MODE_OBJECT_PLANE, "CRTC_Y");
	NEED(p_crtc_w,  plane, DRM_MODE_OBJECT_PLANE, "CRTC_W");
	NEED(p_crtc_h,  plane, DRM_MODE_OBJECT_PLANE, "CRTC_H");
	NEED(c_mode_id, crtc,  DRM_MODE_OBJECT_CRTC,  "MODE_ID");
	NEED(c_active,  crtc,  DRM_MODE_OBJECT_CRTC,  "ACTIVE");
	NEED(k_crtc_id, connector, DRM_MODE_OBJECT_CONNECTOR, "CRTC_ID");
#undef NEED

	if (make_buf(&a, modes[0].hdisplay, modes[0].vdisplay, 0x00204080))
		return 1;
	if (make_buf(&b, modes[0].hdisplay, modes[0].vdisplay, 0x00c05020))
		return 1;
	printf("fb %u and fb %u\n", a.fb_id, b.fb_id);

	memset(&blob, 0, sizeof(blob));
	blob.data = (uint64_t)(uintptr_t)&modes[0];
	blob.length = sizeof(modes[0]);
	if (ioctl(fd, DRM_IOCTL_MODE_CREATEPROPBLOB, &blob) < 0) {
		fprintf(stderr, "CREATEPROPBLOB: %s\n", strerror(errno));
		return 1;
	}

	/* The modeset, once, blocking and with ALLOW_MODESET. */
	req_reset(&r);
	req_obj(&r, connector);
	req_prop(&r, k_crtc_id, crtc);
	req_obj(&r, crtc);
	req_prop(&r, c_mode_id, blob.blob_id);
	req_prop(&r, c_active, 1);
	req_obj(&r, plane);
	req_prop(&r, p_fb, a.fb_id);
	req_prop(&r, p_crtc_id, crtc);
	req_prop(&r, p_src_x, 0);
	req_prop(&r, p_src_y, 0);
	req_prop(&r, p_src_w, (uint64_t)modes[0].hdisplay << 16);
	req_prop(&r, p_src_h, (uint64_t)modes[0].vdisplay << 16);
	req_prop(&r, p_crtc_x, 0);
	req_prop(&r, p_crtc_y, 0);
	req_prop(&r, p_crtc_w, modes[0].hdisplay);
	req_prop(&r, p_crtc_h, modes[0].vdisplay);
	if (req_commit(&r, DRM_MODE_ATOMIC_ALLOW_MODESET, 0) < 0) {
		fprintf(stderr, "atomic modeset: %s\n", strerror(errno));
		return 1;
	}
	printf("atomic modeset ok\n");

	/* And now the question: does every flip get its event back? */
	printf("flipping %u times through DRM_IOCTL_MODE_ATOMIC, %d ms timeout each, "
	       "%d ms gap\n", flips, wait_ms, gap_ms);
	for (f = 0; f < flips; f++) {
		struct pollfd pfd = { .fd = fd, .events = POLLIN };
		struct drm_event_vblank ev;
		int pr;

		/* An optional pause, and it is a measuring instrument rather
		 * than politeness. If the completion timestamp is FROZEN --
		 * the same value handed back forever -- then its distance
		 * from CLOCK_MONOTONIC grows by exactly the elapsed time, so
		 * spreading the flips over seconds turns a constant into an
		 * unmistakable linear drift. Back-to-back flips finish in a
		 * couple of milliseconds and cannot tell the two apart. */
		if (gap_ms > 0) {
			struct timespec ts = { gap_ms / 1000,
					       (long)(gap_ms % 1000) * 1000000L };
			nanosleep(&ts, NULL);
		}

		req_reset(&r);
		req_obj(&r, plane);
		req_prop(&r, p_fb, (f & 1) ? a.fb_id : b.fb_id);
		if (req_commit(&r, DRM_MODE_PAGE_FLIP_EVENT | DRM_MODE_ATOMIC_NONBLOCK,
			       (uint64_t)f) < 0) {
			printf("  flip %u: ATOMIC: %s  (issued %u, completed %u)\n",
			       f, strerror(errno), issued, completed);
			break;
		}
		issued++;

		pr = poll(&pfd, 1, wait_ms);
		if (pr < 0) { printf("  poll: %s\n", strerror(errno)); break; }
		if (pr == 0) {
			timedout++;
			printf("  flip %u: NO COMPLETION within %d ms  "
			       "(issued %u, completed %u so far)\n",
			       f, wait_ms, issued, completed);
			/* One lost completion leaves a flip outstanding
			 * forever and every later commit answers EBUSY, so
			 * the interesting number is WHICH flip went missing. */
			break;
		}
		if (read(fd, &ev, sizeof(ev)) == (ssize_t)sizeof(ev)) {
			/* The two fields a compositor SCHEDULES on. weston takes
			 * the completion timestamp, adds the refresh interval and
			 * arms an absolute timer for the next repaint; it takes
			 * the sequence as the frame counter. Measured
			 * 2026-08-16 via strace: weston armed that timer 281
			 * seconds into the future and then slept, which is
			 * exactly the observed 0 %-CPU stall. So the question is
			 * whether these two numbers are sane -- a timestamp that
			 * runs ahead of CLOCK_MONOTONIC, or a sequence that never
			 * advances, is enough to strand every compositor on this
			 * display while every ioctl still returns success. */
			struct timespec now;
			double evt, nowt, skew;

			clock_gettime(CLOCK_MONOTONIC, &now);
			evt = (double)ev.tv_sec + (double)ev.tv_usec / 1e6;
			nowt = (double)now.tv_sec + (double)now.tv_nsec / 1e9;
			skew = evt - nowt;

			completed++;
			if (completed == 1) {
				first_seq = ev.sequence;
				min_skew = max_skew = skew;
			} else {
				if (skew < min_skew) min_skew = skew;
				if (skew > max_skew) max_skew = skew;
				if (ev.sequence == last_seq)
					seq_stuck++;
				else if (ev.sequence < last_seq)
					seq_back++;
			}
			/* And the pacing. A page flip is supposed to complete AT
			 * a vertical blank, so completions on a 120 Hz display
			 * arrive 8.33 ms apart. Measured here: 20 flips
			 * completed in 0.6 ms, roughly 165x too fast, because
			 * nothing on this path waits for the invented vblank.
			 * A compositor that paces itself on flip completions is
			 * therefore running against no clock at all. */
			if (completed > 1) {
				double d = nowt - prev_done;

				gap_sum += d;
				gap_n++;
				if (d < gap_min || gap_n == 1) gap_min = d;
				if (d > gap_max) gap_max = d;
			}
			prev_done = nowt;
			last_seq = ev.sequence;
			if (verbose)
				printf("  flip %u: seq %u, stamp %.6f, now %.6f, skew %+.6f s\n",
				       f, ev.sequence, evt, nowt, skew);
		}
	}

	printf("ATOMIC: %u issued, %u completed, %u timed out\n",
	       issued, completed, timedout);
	if (completed) {
		printf("sequence: first %u, last %u, %u repeat(s), %u backward step(s)\n",
		       first_seq, last_seq, seq_stuck, seq_back);
		printf("completion stamp vs CLOCK_MONOTONIC: skew %+.6f s .. %+.6f s "
		       "(drift %.6f s)\n", min_skew, max_skew, max_skew - min_skew);
		/* A compositor arms an ABSOLUTE timer at stamp + refresh. A
		 * stamp ahead of the clock therefore becomes dead sleep of
		 * exactly that size, once per frame, with no error anywhere. */
		if (max_skew > 0.001)
			printf("the completion timestamp RUNS AHEAD of the monotonic "
			       "clock by up to %.3f s -- a compositor that schedules on "
			       "it sleeps that long\n", max_skew);
		if (seq_stuck)
			printf("the sequence did not advance on %u of %u completions\n",
			       seq_stuck, completed - 1);
		if (gap_n) {
			double mean = gap_sum / gap_n;
			double refresh = modes[0].vrefresh ? 1.0 / modes[0].vrefresh : 0;

			printf("completion spacing: mean %.3f ms (min %.3f, max %.3f) "
			       "-- one refresh at %u Hz is %.3f ms\n",
			       mean * 1e3, gap_min * 1e3, gap_max * 1e3,
			       modes[0].vrefresh, refresh * 1e3);
			if (refresh > 0 && mean < refresh / 2)
				printf("completions arrive %.0fx faster than the refresh "
				       "rate -- the flip path is NOT paced by the vblank\n",
				       refresh / mean);
		}
	}
	if (timedout)
		printf("the completion for atomic flip %u never arrived -- "
		       "this is what strands a compositor's repaint loop\n", completed);
	else
		printf("every atomic flip got its event back\n");
	return timedout ? 1 : 0;
}
