// SPDX-License-Identifier: MIT
// Small CUDA driver-API program for the manual vhost-user VM check: one
// context, a PTX kernel (c[i] = a[i] + b[i]) JIT-compiled by the driver, 1 Mi
// elements, result checked on the host. Needs no CUDA toolkit:
//   gcc -O2 -o vecadd vecadd.c -ldl && ./vecadd
// VECADD_HOLD=N keeps the context and its mappings for N seconds after the
// kernel ran (used to reset the device under live mappings).
#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

typedef int CUresult;
typedef int CUdevice;
typedef void *CUcontext, *CUmodule, *CUfunction, *CUstream;
typedef unsigned long long CUdeviceptr;

static const char ptx[] =
	".version 6.4\n.target sm_75\n.address_size 64\n"
	".visible .entry vecadd(.param .u64 pa, .param .u64 pb, .param .u64 pc, .param .u32 pn)\n{\n"
	"  .reg .pred %p; .reg .b32 %r<6>; .reg .b64 %rd<11>;\n"
	"  ld.param.u64 %rd1, [pa]; ld.param.u64 %rd2, [pb]; ld.param.u64 %rd3, [pc]; ld.param.u32 %r1, [pn];\n"
	"  mov.u32 %r2, %ctaid.x; mov.u32 %r3, %ntid.x; mov.u32 %r4, %tid.x; mad.lo.s32 %r5, %r2, %r3, %r4;\n"
	"  setp.ge.u32 %p, %r5, %r1; @%p bra DONE;\n"
	"  cvta.to.global.u64 %rd4, %rd1; cvta.to.global.u64 %rd5, %rd2; cvta.to.global.u64 %rd6, %rd3;\n"
	"  mul.wide.u32 %rd7, %r5, 4; add.s64 %rd8, %rd4, %rd7; add.s64 %rd9, %rd5, %rd7; add.s64 %rd10, %rd6, %rd7;\n"
	"  ld.global.u32 %r2, [%rd8]; ld.global.u32 %r3, [%rd9]; add.s32 %r4, %r2, %r3; st.global.u32 [%rd10], %r4;\n"
	"DONE:\n  ret;\n}\n";

static void *h;
static void *sym(const char *n)
{
	void *p = dlsym(h, n);
	if (!p) {
		fprintf(stderr, "missing %s\n", n);
		exit(2);
	}
	return p;
}
#define CK(call)                                                             \
	do {                                                                 \
		CUresult r_ = (call);                                        \
		if (r_) {                                                    \
			fprintf(stderr, "%s -> CUDA error %d\n", #call, r_); \
			return 1;                                            \
		}                                                            \
	} while (0)

int main(void)
{
	h = dlopen("libcuda.so.1", RTLD_NOW);
	if (!h) {
		fprintf(stderr, "%s\n", dlerror());
		return 2;
	}
	CUresult (*cuInit)(unsigned) = sym("cuInit");
	CUresult (*cuDeviceGet)(CUdevice *, int) = sym("cuDeviceGet");
	CUresult (*cuDeviceGetName)(char *, int, CUdevice) =
		sym("cuDeviceGetName");
	CUresult (*cuDevicePrimaryCtxRetain)(CUcontext *, CUdevice) =
		sym("cuDevicePrimaryCtxRetain");
	CUresult (*cuDevicePrimaryCtxRelease)(CUdevice) =
		sym("cuDevicePrimaryCtxRelease_v2");
	CUresult (*cuCtxSetCurrent)(CUcontext) = sym("cuCtxSetCurrent");
	CUresult (*cuModuleLoadData)(CUmodule *, const void *) =
		sym("cuModuleLoadData");
	CUresult (*cuModuleGetFunction)(CUfunction *, CUmodule, const char *) =
		sym("cuModuleGetFunction");
	CUresult (*cuMemAlloc)(CUdeviceptr *, size_t) = sym("cuMemAlloc_v2");
	CUresult (*cuMemFree)(CUdeviceptr) = sym("cuMemFree_v2");
	CUresult (*cuMemcpyHtoD)(CUdeviceptr, const void *, size_t) =
		sym("cuMemcpyHtoD_v2");
	CUresult (*cuMemcpyDtoH)(void *, CUdeviceptr, size_t) =
		sym("cuMemcpyDtoH_v2");
	CUresult (*cuLaunchKernel)(CUfunction, unsigned, unsigned, unsigned,
				   unsigned, unsigned, unsigned, unsigned,
				   CUstream, void **,
				   void **) = sym("cuLaunchKernel");
	CUresult (*cuCtxSynchronize)(void) = sym("cuCtxSynchronize");

	enum { N = 1 << 20 };
	uint32_t *a = malloc(N * 4), *b = malloc(N * 4), *c = malloc(N * 4);
	for (uint32_t i = 0; i < N; i++) {
		a[i] = i;
		b[i] = 3 * i + 7;
		c[i] = 0;
	}

	CUdevice dev;
	CUcontext ctx;
	CUmodule mod;
	CUfunction fn;
	CUdeviceptr da, db, dc;
	char name[128];
	CK(cuInit(0));
	CK(cuDeviceGet(&dev, 0));
	CK(cuDeviceGetName(name, sizeof name, dev));
	CK(cuDevicePrimaryCtxRetain(&ctx, dev));
	CK(cuCtxSetCurrent(ctx));
	CK(cuModuleLoadData(&mod, ptx));
	CK(cuModuleGetFunction(&fn, mod, "vecadd"));
	CK(cuMemAlloc(&da, N * 4));
	CK(cuMemAlloc(&db, N * 4));
	CK(cuMemAlloc(&dc, N * 4));
	CK(cuMemcpyHtoD(da, a, N * 4));
	CK(cuMemcpyHtoD(db, b, N * 4));
	unsigned n = N;
	void *args[] = { &da, &db, &dc, &n };
	CK(cuLaunchKernel(fn, N / 256, 1, 1, 256, 1, 1, 0, NULL, args, NULL));
	CK(cuCtxSynchronize());
	CK(cuMemcpyDtoH(c, dc, N * 4));
	if (getenv("VECADD_HOLD")) {
		printf("holding the context for %s s\n", getenv("VECADD_HOLD"));
		fflush(stdout);
		sleep(atoi(getenv("VECADD_HOLD")));
	}
	unsigned bad = 0;
	for (uint32_t i = 0; i < N; i++)
		bad += c[i] != 4 * i + 7;
	CK(cuMemFree(da));
	CK(cuMemFree(db));
	CK(cuMemFree(dc));
	CK(cuDevicePrimaryCtxRelease(dev));
	printf("vecadd on %s: %u elements, %u wrong -> %s\n", name, N, bad,
	       bad ? "FAIL" : "PASS");
	return bad != 0;
}
