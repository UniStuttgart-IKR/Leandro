// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
/*
 * drm-modeset -- set a mode on a DRM connector and prove a frame was taken.
 *
 *   gcc -O2 -Wall -Wextra -o drm-modeset drm-modeset.c
 *   ./drm-modeset /dev/dri/card1
 *   ./drm-modeset --find            # print the nvidia-drm node and exit
 *   ./drm-modeset --flips 30 /dev/dri/card0     # 30 flips, count completions
 *   ./drm-modeset --flip-wait 500               # ms to wait per completion
 *
 * --find exists because the node NUMBER is not a constant: which cardN
 * the virtual display gets depends on what other DRM devices the guest
 * carries, and every script that hard-types a number measures the wrong
 * device or nothing at all. The only reliable answer is the DRM
 * driver's own name (DRM_IOCTL_VERSION): the PCI parent says `virtio-pci`
 * for both, and debugfs is not mounted in the guest image.
 *
 * Raw ioctls, no libdrm: the guest image has the kernel's <drm/drm_mode.h>
 * but not libdrm-dev, and one less package is one less thing to install
 * before a measurement.
 *
 * Why this exists rather than modetest: the question on the virtual display
 * path is never "did the call return 0", it is WHO READ THE BUFFER. So this
 * does the whole chain -- dumb buffer, fill it with a known pattern, SETCRTC,
 * then PAGE_FLIP to a second buffer -- and reports what the kernel says the
 * CRTC is showing afterwards. A modeset that returns success while the CRTC
 * stays blank is the failure this is built to catch.
 */
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <unistd.h>
#include <poll.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <drm/drm.h>
#include <drm/drm_mode.h>

static int fd;

static int try_ioctl(unsigned long req, void *arg, const char *what)
{
	if (ioctl(fd, req, arg) < 0) {
		fprintf(stderr, "  %s: %s\n", what, strerror(errno));
		return -1;
	}
	return 0;
}

#define MUST(req, arg, what) do { if (try_ioctl(req, arg, what)) return 1; } while (0)

/* One dumb buffer, filled with a solid colour, registered as a framebuffer. */
struct buf {
	uint32_t handle, fb_id, pitch;
	uint64_t size;
	uint32_t *px;
};

static int make_buf(struct buf *b, uint32_t w, uint32_t h, uint32_t colour)
{
	struct drm_mode_create_dumb create = { .width = w, .height = h, .bpp = 32 };
	struct drm_mode_map_dumb map = { 0 };
	struct drm_mode_fb_cmd fb = { 0 };
	void *p;
	uint64_t i;

	MUST(DRM_IOCTL_MODE_CREATE_DUMB, &create, "CREATE_DUMB");
	b->handle = create.handle;
	b->pitch  = create.pitch;
	b->size   = create.size;

	fb.width = w; fb.height = h; fb.bpp = 32; fb.depth = 24;
	fb.pitch = b->pitch; fb.handle = b->handle;
	MUST(DRM_IOCTL_MODE_ADDFB, &fb, "ADDFB");
	b->fb_id = fb.fb_id;

	map.handle = b->handle;
	MUST(DRM_IOCTL_MODE_MAP_DUMB, &map, "MAP_DUMB");

	p = mmap(NULL, b->size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, map.offset);
	if (p == MAP_FAILED) {
		fprintf(stderr, "  mmap of the dumb buffer: %s\n", strerror(errno));
		return 1;
	}
	b->px = p;
	for (i = 0; i < b->size / 4; i++)
		b->px[i] = colour;
	return 0;
}

/* The DRM driver's own name for an open node, e.g. "nvidia-drm". */
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

/* Print the first /dev/dri/cardN whose driver is `want`. */
static int find_node(const char *want)
{
	char path[64], name[64];
	int i, f;

	for (i = 0; i < 16; i++) {
		snprintf(path, sizeof(path), "/dev/dri/card%d", i);
		f = open(path, O_RDWR | O_CLOEXEC);
		if (f < 0)
			continue;
		if (drm_name(f, name, sizeof(name)) == 0 && strcmp(name, want) == 0) {
			close(f);
			printf("%s\n", path);
			return 0;
		}
		close(f);
	}
	fprintf(stderr, "no DRM node with driver \"%s\"\n", want);
	return 1;
}

int main(int argc, char **argv)
{
	const char *node;
	char drv[64];

	unsigned flips = 1, last_fb;
	int flip_wait_ms = 500;
	int ai;

	if (argc > 1 && strcmp(argv[1], "--find") == 0)
		return find_node(argc > 2 ? argv[2] : "nvidia-drm");

	node = "/dev/dri/card1";
	for (ai = 1; ai < argc; ai++) {
		if (!strcmp(argv[ai], "--flips") && ai + 1 < argc)
			flips = (unsigned)atoi(argv[++ai]);
		else if (!strcmp(argv[ai], "--flip-wait") && ai + 1 < argc)
			flip_wait_ms = atoi(argv[++ai]);
		else
			node = argv[ai];
	}
	struct drm_mode_card_res res = { 0 };
	uint32_t conn_ids[32], crtc_ids[32], enc_ids[32], fb_ids[32];
	struct drm_mode_get_connector conn = { 0 };
	struct drm_mode_modeinfo modes[64];
	uint32_t conn_enc_ids[32], props[64];
	uint64_t prop_vals[64];
	struct drm_mode_crtc set = { 0 };
	struct drm_mode_crtc got = { 0 };
	struct buf a, bb;
	unsigned i;

	fd = open(node, O_RDWR | O_CLOEXEC);
	if (fd < 0) {
		fprintf(stderr, "open %s: %s\n", node, strerror(errno));
		return 2;
	}
	if (drm_name(fd, drv, sizeof(drv)) == 0)
		printf("node %s, driver \"%s\"\n", node, drv);
	else
		printf("node %s (DRM_IOCTL_VERSION failed)\n", node);

	/* Without master, SETCRTC is EACCES. Nothing else may hold it -- that
	 * is what "stop gdm3 first" in display-rig.sh is about. */
	if (ioctl(fd, DRM_IOCTL_SET_MASTER, 0) < 0)
		fprintf(stderr, "  SET_MASTER: %s (someone else holds it?)\n",
			strerror(errno));

	res.connector_id_ptr = (uint64_t)(uintptr_t)conn_ids;
	res.crtc_id_ptr      = (uint64_t)(uintptr_t)crtc_ids;
	res.encoder_id_ptr   = (uint64_t)(uintptr_t)enc_ids;
	res.fb_id_ptr        = (uint64_t)(uintptr_t)fb_ids;
	res.count_connectors = res.count_crtcs = 32;
	res.count_encoders   = res.count_fbs   = 32;
	MUST(DRM_IOCTL_MODE_GETRESOURCES, &res, "GETRESOURCES");
	printf("crtcs %u, connectors %u\n", res.count_crtcs, res.count_connectors);
	if (!res.count_crtcs || !res.count_connectors)
		return 1;

	conn.connector_id  = conn_ids[0];
	conn.modes_ptr     = (uint64_t)(uintptr_t)modes;
	conn.props_ptr     = (uint64_t)(uintptr_t)props;
	conn.prop_values_ptr = (uint64_t)(uintptr_t)prop_vals;
	conn.encoders_ptr  = (uint64_t)(uintptr_t)conn_enc_ids;
	conn.count_modes   = 64;
	conn.count_props   = 64;
	conn.count_encoders = 32;
	MUST(DRM_IOCTL_MODE_GETCONNECTOR, &conn, "GETCONNECTOR");
	printf("connector %u: connection %u (1 = connected), %u modes\n",
	       conn.connector_id, conn.connection, conn.count_modes);
	if (!conn.count_modes) {
		fprintf(stderr, "  no modes -- nothing to set\n");
		return 1;
	}
	for (i = 0; i < conn.count_modes && i < 4; i++)
		printf("  mode %u: %ux%u@%u\n", i, modes[i].hdisplay,
		       modes[i].vdisplay, modes[i].vrefresh);

	/* Two buffers so the flip has somewhere to go. Distinct colours, so a
	 * reader that shows the wrong one is visibly wrong rather than
	 * plausibly right. */
	printf("buffers %ux%u\n", modes[0].hdisplay, modes[0].vdisplay);
	if (make_buf(&a, modes[0].hdisplay, modes[0].vdisplay, 0x00204080))
		return 1;
	if (make_buf(&bb, modes[0].hdisplay, modes[0].vdisplay, 0x00c05020))
		return 1;
	printf("  fb %u and fb %u, pitch %u, %llu bytes each\n",
	       a.fb_id, bb.fb_id, a.pitch, (unsigned long long)a.size);

	set.crtc_id   = crtc_ids[0];
	set.fb_id     = a.fb_id;
	set.set_connectors_ptr = (uint64_t)(uintptr_t)&conn.connector_id;
	set.count_connectors   = 1;
	set.mode      = modes[0];
	set.mode_valid = 1;
	MUST(DRM_IOCTL_MODE_SETCRTC, &set, "SETCRTC");
	printf("SETCRTC ok\n");

	/* The read-back is the point. A CRTC that reports mode_valid = 0 or a
	 * different fb has not taken the frame, whatever SETCRTC returned. */
	got.crtc_id = crtc_ids[0];
	MUST(DRM_IOCTL_MODE_GETCRTC, &got, "GETCRTC");
	printf("GETCRTC: fb %u, mode_valid %u, %ux%u\n",
	       got.fb_id, got.mode_valid, got.mode.hdisplay, got.mode.vdisplay);

	/* One flip is enough to show the path works at all; it is NOT enough
	 * to show it keeps working. Measured 2026-08-16: weston brings the
	 * output up, composites a handful of frames and then stops dead --
	 * zero CPU, no atomic commits, clients starved of frame callbacks --
	 * while the invented vblank keeps ticking at 120 Hz underneath. The
	 * failure is therefore not "no completion" but "completions stop after
	 * the first few", and a single-flip probe reports green on exactly the
	 * configuration that hangs. So flip in a loop and count.
	 *
	 * The read is polled with a timeout rather than blocking: a blocking
	 * read on a completion that never comes is indistinguishable from a
	 * probe that crashed, and this bug's whole signature is a completion
	 * that never comes. */
	{
		struct drm_mode_crtc_page_flip flip = { 0 };
		unsigned issued = 0, completed = 0, timedout = 0;
		unsigned f;

		/* Which buffer the last SUCCESSFUL flip named. The read-back
		 * below compares against this rather than against a fixed
		 * buffer: with an even number of flips the sequence ends on
		 * the other one, and a hard-coded expectation would fail a
		 * perfectly good run. */
		last_fb = a.fb_id;

		printf("flipping %u times, %d ms timeout each\n", flips, flip_wait_ms);
		for (f = 0; f < flips; f++) {
			struct pollfd pfd = { .fd = fd, .events = POLLIN };
			struct drm_event_vblank ev;
			int pr;

			flip.crtc_id = crtc_ids[0];
			flip.fb_id   = (f & 1) ? a.fb_id : bb.fb_id;
			flip.flags   = DRM_MODE_PAGE_FLIP_EVENT;
			if (ioctl(fd, DRM_IOCTL_MODE_PAGE_FLIP, &flip) < 0) {
				printf("  flip %u: PAGE_FLIP: %s\n", f, strerror(errno));
				break;
			}
			issued++;
			last_fb = flip.fb_id;

			pr = poll(&pfd, 1, flip_wait_ms);
			if (pr < 0) {
				printf("  flip %u: poll: %s\n", f, strerror(errno));
				break;
			}
			if (pr == 0) {
				timedout++;
				printf("  flip %u: NO COMPLETION within %d ms"
				       "  (issued %u, completed %u so far)\n",
				       f, flip_wait_ms, issued, completed);
				/* Once one completion is lost the CRTC has a
				 * flip outstanding forever and every later
				 * flip returns EBUSY, so stop: the number
				 * that matters is WHICH flip went missing. */
				break;
			}
			if (read(fd, &ev, sizeof(ev)) == (ssize_t)sizeof(ev))
				completed++;
		}
		printf("PAGE_FLIP: %u issued, %u completed, %u timed out\n",
		       issued, completed, timedout);
		if (timedout)
			printf("the completion for flip %u never arrived -- "
			       "this is what strands a compositor's repaint loop\n",
			       completed);
	}

	got.crtc_id = crtc_ids[0];
	if (try_ioctl(DRM_IOCTL_MODE_GETCRTC, &got, "GETCRTC") == 0)
		printf("after flip: fb %u (expected %u)\n", got.fb_id, last_fb);

	printf("done\n");
	return 0;
}
