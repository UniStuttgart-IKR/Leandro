// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
/*
 * vdisp-frame -- write one deterministic frame to the virtual display and
 * read it back out through the driver's own export path.
 *
 *   gcc -O2 -Wall -Wextra -o vdisp-frame vdisp-frame.c
 *   sudo ./vdisp-frame /dev/dri/card0
 *   sudo ./vdisp-frame --find                 # print the nvidia-drm node
 *   ./vdisp-frame --reference 1600x900        # the expected hash, no DRM
 *
 * Exit 0 = the frame came back byte for byte. 1 = it came back different.
 * 2 = something in the chain refused (no node, no mode, an ioctl failed) --
 * a different answer from "the pixels are wrong", and the gate reports it
 * as such.
 *
 * WHAT IT PROVES, and what it does not. The pattern is written by the CPU
 * into a dumb buffer, ADDFB'd, put on the CRTC with SETCRTC, and then read
 * back by asking the CRTC which framebuffer it is showing (GETCRTC), asking
 * that framebuffer for its GEM handle (GETFB2), exporting the handle as a
 * dmabuf (PRIME_HANDLE_TO_FD) and mapping THAT. So the buffer makes the
 * whole trip through virtio-nvrm into the host's VRAM and back through a
 * second, independent path.
 *
 * It is NOT a scanout capture. A virtual display has no scanout -- there is
 * no cable and no panel -- so nothing here proves a compositor would see
 * anything. Rendering is not measured either: no GPU touches these pixels.
 * Both of those belong to the display gate, scripts/test.sh display (swapchain, present, the
 * capture stages). This is the cheap round-trip underneath them.
 *
 * WHY A HASH AND NOT A COMPARISON. The buffer is potentially uncached device
 * memory; a memcmp against a host-side copy would mean a second full-frame
 * read. FNV-1a over the visible pixels, row by row, honouring the pitch, is
 * one pass and gives the gate a single number to record as a fact.
 *
 * The PITCH is honoured on both sides, which is why the number is comparable
 * at all: the driver may hand out a stride wider than the width, and hashing
 * the padding would make the answer depend on the allocator rather than on
 * the pixels.
 *
 * WARNING: needs DRM master, so nothing else may hold it -- stop gdm3 first.
 * GETFB2 hands back GEM handles only to a client with CAP_SYS_ADMIN or DRM
 * master; without either the call SUCCEEDS and every handle is 0, which
 * looks like an empty framebuffer rather than a permissions problem
 * (measured in scripts/guest/fbprobe.c, same trap).
 *
 * Raw ioctls, no libdrm: the guest image has the kernel's <drm/drm_mode.h>
 * but not libdrm-dev, and one less package is one less thing to install
 * before a measurement. Patterned on scripts/guest/drm-modeset.c for
 * the modeset half and on fbprobe.c way (a) for the export-and-map half.
 */
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <drm/drm.h>
#include <drm/drm_mode.h>

/* Both are recent additions to the UAPI headers. Defined here so the probe
 * builds on a guest whose linux-libc-dev predates them -- fbprobe.c carries
 * the same two fallbacks for the same reason. */
#ifndef DRM_IOCTL_MODE_GETFB2
#define DRM_IOCTL_MODE_GETFB2 DRM_IOWR(0xCE, struct drm_mode_fb_cmd2)
#endif
#ifndef DRM_RDWR
#define DRM_RDWR O_RDWR
#endif

static int fd = -1;

/*
 * The pattern. Deterministic, and a pure function of the pixel position, so
 * the expected hash can be computed on a host with no GPU at all
 * (--reference) and committed beside the gate.
 *
 * Three channels that vary at three different rates, on purpose: a frame
 * that arrives shifted by one pixel, transposed, or with red and blue
 * swapped produces a different hash from one that arrives intact. A solid
 * colour would survive all three.
 */
static uint32_t pattern(uint32_t x, uint32_t y)
{
	return 0xff000000u
	     | ((x * 7u) & 0xffu) << 16
	     | ((y * 13u) & 0xffu) << 8
	     | ((x ^ y) & 0xffu);
}

/* FNV-1a over 64 bits, fed one 32-bit pixel at a time. */
static uint64_t fnv_init(void)
{
	return 1469598103934665603ULL;
}

static void fnv_pixel(uint64_t *h, uint32_t px)
{
	*h ^= px;
	*h *= 1099511628211ULL;
}

/* The hash the pattern SHOULD produce: w*h pixels, row-major, no padding. */
static uint64_t hash_expected(uint32_t w, uint32_t h)
{
	uint64_t hash = fnv_init();
	uint32_t x, y;

	for (y = 0; y < h; y++)
		for (x = 0; x < w; x++)
			fnv_pixel(&hash, pattern(x, y));
	return hash;
}

/* The hash of what is actually in a mapping, skipping the stride padding. */
static uint64_t hash_mapping(const uint8_t *p, uint32_t w, uint32_t h,
			     uint32_t pitch)
{
	uint64_t hash = fnv_init();
	uint32_t x, y;

	for (y = 0; y < h; y++) {
		const uint32_t *row = (const uint32_t *)(const void *)(p + (size_t)y * pitch);

		for (x = 0; x < w; x++)
			fnv_pixel(&hash, row[x]);
	}
	return hash;
}

static int try_ioctl(unsigned long req, void *arg, const char *what)
{
	if (ioctl(fd, req, arg) < 0) {
		fprintf(stderr, "  %s: %s\n", what, strerror(errno));
		return -1;
	}
	return 0;
}

#define MUST(req, arg, what) do { if (try_ioctl(req, arg, what)) return 2; } while (0)

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

/*
 * Print the first /dev/dri/cardN whose driver is `want`.
 *
 * The node NUMBER is not a constant: with a virtio-gpu beside us the virtual
 * display was card1, without one it is card0. The PCI parent
 * says virtio-pci for both and debugfs is not mounted in the guest image, so
 * the driver name is the only answer that holds in both shapes.
 */
static int find_node(const char *want, char *out, size_t len)
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
			snprintf(out, len, "%s", path);
			return 0;
		}
		close(f);
	}
	fprintf(stderr, "no DRM node with driver \"%s\"\n", want);
	return 2;
}

static int prime_export(uint32_t handle, int *out_fd)
{
	struct drm_prime_handle ph;

	memset(&ph, 0, sizeof(ph));
	ph.handle = handle;
	ph.flags = DRM_CLOEXEC | DRM_RDWR;
	if (ioctl(fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &ph) < 0) {
		/* Some drivers refuse an RDWR export but allow a read-only
		 * one. Ask again rather than reporting a failure that is
		 * really a flag mismatch. */
		memset(&ph, 0, sizeof(ph));
		ph.handle = handle;
		ph.flags = DRM_CLOEXEC;
		if (ioctl(fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &ph) < 0)
			return -1;
	}
	*out_fd = ph.fd;
	return 0;
}

static int usage(const char *me)
{
	fprintf(stderr,
		"usage: %s [<node>]            write a frame and read it back\n"
		"       %s --find              print the nvidia-drm node\n"
		"       %s --reference WxH     print the expected hash, no DRM\n",
		me, me, me);
	return 2;
}

int main(int argc, char **argv)
{
	char node[64] = "";
	char drv[64];
	struct drm_mode_card_res res;
	uint32_t conn_ids[32], crtc_ids[32], enc_ids[32], fb_ids[32];
	struct drm_mode_get_connector conn;
	struct drm_mode_modeinfo modes[64];
	uint32_t conn_enc_ids[32], props[64];
	uint64_t prop_vals[64];
	struct drm_mode_create_dumb create;
	struct drm_mode_map_dumb map;
	struct drm_mode_fb_cmd addfb;
	struct drm_mode_fb_cmd2 getfb;
	struct drm_mode_crtc set, got;
	uint64_t want_hash, read_hash;
	const char *via = "prime";
	uint32_t w, h, pitch;
	uint8_t *dumb = NULL;
	uint8_t *view = NULL;
	size_t view_len = 0;
	int dmabuf = -1;
	int ai, rc;
	off_t dsize;

	if (argc > 1 && !strcmp(argv[1], "--reference")) {
		unsigned rw = 0, rh = 0;

		if (argc != 3 || sscanf(argv[2], "%ux%u", &rw, &rh) != 2 || !rw || !rh)
			return usage(argv[0]);
		/* No DRM at all: this is what generates probe/data/vdisp-frame.ref
		 * on a machine that has no virtual display. */
		printf("%ux%u 0x%016llx\n", rw, rh,
		       (unsigned long long)hash_expected(rw, rh));
		return 0;
	}
	if (argc > 1 && !strcmp(argv[1], "--find")) {
		if (find_node(argc > 2 ? argv[2] : "nvidia-drm", node, sizeof(node)))
			return 2;
		printf("%s\n", node);
		return 0;
	}
	for (ai = 1; ai < argc; ai++) {
		if (argv[ai][0] == '-')
			return usage(argv[0]);
		snprintf(node, sizeof(node), "%s", argv[ai]);
	}
	if (!node[0] && find_node("nvidia-drm", node, sizeof(node)))
		return 2;

	fd = open(node, O_RDWR | O_CLOEXEC);
	if (fd < 0) {
		fprintf(stderr, "open %s: %s\n", node, strerror(errno));
		return 2;
	}
	if (drm_name(fd, drv, sizeof(drv)) != 0) {
		fprintf(stderr, "%s: DRM_IOCTL_VERSION failed\n", node);
		return 2;
	}

	/* Without master, SETCRTC is EACCES and GETFB2 silently returns zero
	 * handles. Nothing else may hold it -- that is what "stop gdm3 first"
	 * is about. */
	if (ioctl(fd, DRM_IOCTL_SET_MASTER, 0) < 0) {
		fprintf(stderr, "SET_MASTER: %s (someone else holds it -- "
			"stop gdm3)\n", strerror(errno));
		return 2;
	}

	memset(&res, 0, sizeof(res));
	res.connector_id_ptr = (uint64_t)(uintptr_t)conn_ids;
	res.crtc_id_ptr      = (uint64_t)(uintptr_t)crtc_ids;
	res.encoder_id_ptr   = (uint64_t)(uintptr_t)enc_ids;
	res.fb_id_ptr        = (uint64_t)(uintptr_t)fb_ids;
	res.count_connectors = res.count_crtcs = 32;
	res.count_encoders   = res.count_fbs   = 32;
	MUST(DRM_IOCTL_MODE_GETRESOURCES, &res, "GETRESOURCES");
	if (!res.count_crtcs || !res.count_connectors) {
		fprintf(stderr, "  %u crtc(s), %u connector(s) -- nothing to set\n",
			res.count_crtcs, res.count_connectors);
		return 2;
	}

	memset(&conn, 0, sizeof(conn));
	conn.connector_id    = conn_ids[0];
	conn.modes_ptr       = (uint64_t)(uintptr_t)modes;
	conn.props_ptr       = (uint64_t)(uintptr_t)props;
	conn.prop_values_ptr = (uint64_t)(uintptr_t)prop_vals;
	conn.encoders_ptr    = (uint64_t)(uintptr_t)conn_enc_ids;
	conn.count_modes     = 64;
	conn.count_props     = 64;
	conn.count_encoders  = 32;
	MUST(DRM_IOCTL_MODE_GETCONNECTOR, &conn, "GETCONNECTOR");
	if (!conn.count_modes) {
		fprintf(stderr, "  connector %u has no modes\n", conn.connector_id);
		return 2;
	}
	/* Mode 0 is the preferred one -- the EDID's first detailed timing,
	 * i.e. exactly the size the module was asked to invent. */
	w = modes[0].hdisplay;
	h = modes[0].vdisplay;

	memset(&create, 0, sizeof(create));
	create.width = w;
	create.height = h;
	create.bpp = 32;
	MUST(DRM_IOCTL_MODE_CREATE_DUMB, &create, "CREATE_DUMB");

	memset(&addfb, 0, sizeof(addfb));
	addfb.width = w; addfb.height = h;
	addfb.bpp = 32; addfb.depth = 24;
	addfb.pitch = create.pitch;
	addfb.handle = create.handle;
	MUST(DRM_IOCTL_MODE_ADDFB, &addfb, "ADDFB");

	memset(&map, 0, sizeof(map));
	map.handle = create.handle;
	MUST(DRM_IOCTL_MODE_MAP_DUMB, &map, "MAP_DUMB");

	dumb = mmap(NULL, create.size, PROT_READ | PROT_WRITE, MAP_SHARED,
		    fd, map.offset);
	if (dumb == MAP_FAILED) {
		fprintf(stderr, "  mmap of the dumb buffer: %s\n", strerror(errno));
		return 2;
	}
	{
		uint32_t x, y;

		for (y = 0; y < h; y++) {
			uint32_t *row = (uint32_t *)(void *)(dumb + (size_t)y * create.pitch);

			for (x = 0; x < w; x++)
				row[x] = pattern(x, y);
		}
	}
	want_hash = hash_expected(w, h);

	memset(&set, 0, sizeof(set));
	set.crtc_id = crtc_ids[0];
	set.fb_id   = addfb.fb_id;
	set.set_connectors_ptr = (uint64_t)(uintptr_t)&conn.connector_id;
	set.count_connectors   = 1;
	set.mode       = modes[0];
	set.mode_valid = 1;
	MUST(DRM_IOCTL_MODE_SETCRTC, &set, "SETCRTC");

	/* The read-back begins here, and the first question is the CRTC's own:
	 * a SETCRTC that returns 0 while the CRTC shows something else is
	 * exactly the failure this is built to catch. */
	memset(&got, 0, sizeof(got));
	got.crtc_id = crtc_ids[0];
	MUST(DRM_IOCTL_MODE_GETCRTC, &got, "GETCRTC");

	pitch = create.pitch;
	memset(&getfb, 0, sizeof(getfb));
	getfb.fb_id = got.fb_id;
	if (got.fb_id && ioctl(fd, DRM_IOCTL_MODE_GETFB2, &getfb) == 0
	    && getfb.handles[0]
	    && prime_export(getfb.handles[0], &dmabuf) == 0) {
		dsize = lseek(dmabuf, 0, SEEK_END);
		if (dsize > 0) {
			view = mmap(NULL, (size_t)dsize, PROT_READ, MAP_SHARED,
				    dmabuf, 0);
			if (view == MAP_FAILED) {
				view = NULL;
			} else {
				view_len = (size_t)dsize;
				if (getfb.pitches[0])
					pitch = getfb.pitches[0];
			}
		}
	}
	if (!view) {
		/* The PRIME path was measured to work for the primary plane
		 * (fbprobe way (a)), but not for a dumb buffer of our own --
		 * so the fallback is real rather than defensive. It reads the
		 * SAME memory through the mapping we already hold, which is a
		 * weaker statement (it does not exercise the export path) and
		 * the line says so, so a run that quietly lost the stronger
		 * check is still visible as one.
		 *
		 * Also the honest answer on a driver that refuses PRIME
		 * export outright: reporting "the pixels are wrong" there
		 * would name the wrong defect. */
		via = "dumb";
		view = dumb;
		view_len = create.size;
		pitch = create.pitch;
	}
	if ((size_t)h * pitch > view_len) {
		fprintf(stderr, "  mapping is %zu bytes, %ux%u at pitch %u needs %zu\n",
			view_len, w, h, pitch, (size_t)h * pitch);
		return 2;
	}
	read_hash = hash_mapping(view, w, h, pitch);

	rc = (read_hash == want_hash) ? 0 : 1;
	printf("vdisp-frame %ux%u node=%s driver=%s fb=%u crtc_fb=%u "
	       "pitch=%u readback_via=%s written=0x%016llx readback=0x%016llx %s\n",
	       w, h, node, drv, addfb.fb_id, got.fb_id, pitch, via,
	       (unsigned long long)want_hash, (unsigned long long)read_hash,
	       rc ? "MISMATCH" : "match");
	if (got.fb_id != addfb.fb_id) {
		fprintf(stderr, "  the CRTC names fb %u, not the fb %u that was set\n",
			got.fb_id, addfb.fb_id);
		rc = 1;
	}
	if (dmabuf >= 0)
		close(dmabuf);
	return rc;
}
