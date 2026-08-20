// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//
// OpenCL, the smallest run that actually computes: pick NVIDIA's platform,
// build a vector-add kernel from source, run it, verify every element.
//
// WHY IT EXISTS: libnvidia-opencl is staged into the guest and no probe in
// this tree has ever exercised it. Its escape surface is not derivable from
// libcuda's -- OpenCL is a separate userspace on the same driver, the way
// NVML is, and nvidia-smi got its own probe for exactly that reason.
//
// WHY THE RESULT IS CHECKED and not just the status codes: a missing
// libnvidia-opencl is not an error here at all. The ICD loader
// (libOpenCL.so.1) reads /etc/OpenCL/vendors/*.icd, fails to dlopen the
// vendor library and reports "no platforms" -- clean status codes, zero
// ioctls, and a trace that looks like a feature nobody used. Only the
// computed bytes tell "denied" from "absent".
//
// Deterministic: fixed length, fixed inputs, integer arithmetic, no clock,
// no network, no files.
#define CL_TARGET_OPENCL_VERSION 120
#include <CL/cl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define N 4096

static const char *SRC =
    "__kernel void vecadd(__global const int *a, __global const int *b,\n"
    "                     __global int *c) {\n"
    "    int i = get_global_id(0);\n"
    "    c[i] = a[i] + b[i];\n"
    "}\n";

static int fail(const char *what, cl_int e)
{
    fprintf(stderr, "oclprobe: %s failed (%d)\n", what, (int)e);
    return 1;
}

int main(void)
{
    cl_uint nplat = 0;
    cl_int e = clGetPlatformIDs(0, NULL, &nplat);
    if (e != CL_SUCCESS || nplat == 0) {
        // The interesting failure, and the reason this probe reports it by
        // name: no platform means the ICD loader found no vendor library.
        fprintf(stderr, "oclprobe: no OpenCL platform (loader found no vendor library)\n");
        return 1;
    }

    cl_platform_id plats[16];
    if (nplat > 16)
        nplat = 16;
    if ((e = clGetPlatformIDs(nplat, plats, NULL)) != CL_SUCCESS)
        return fail("clGetPlatformIDs", e);

    cl_platform_id plat = NULL;
    char vendor[256] = { 0 };
    for (cl_uint i = 0; i < nplat; i++) {
        char v[256] = { 0 };
        if (clGetPlatformInfo(plats[i], CL_PLATFORM_VENDOR, sizeof v, v, NULL) != CL_SUCCESS)
            continue;
        if (strstr(v, "NVIDIA")) {
            plat = plats[i];
            snprintf(vendor, sizeof vendor, "%s", v);
            break;
        }
    }
    if (!plat) {
        fprintf(stderr, "oclprobe: %u platform(s), none of them NVIDIA\n", nplat);
        return 1;
    }

    cl_device_id dev;
    if ((e = clGetDeviceIDs(plat, CL_DEVICE_TYPE_GPU, 1, &dev, NULL)) != CL_SUCCESS)
        return fail("clGetDeviceIDs", e);

    char devname[256] = { 0 };
    clGetDeviceInfo(dev, CL_DEVICE_NAME, sizeof devname, devname, NULL);

    cl_context ctx = clCreateContext(NULL, 1, &dev, NULL, NULL, &e);
    if (!ctx)
        return fail("clCreateContext", e);
    cl_command_queue q = clCreateCommandQueue(ctx, dev, 0, &e);
    if (!q)
        return fail("clCreateCommandQueue", e);

    cl_program prog = clCreateProgramWithSource(ctx, 1, &SRC, NULL, &e);
    if (!prog)
        return fail("clCreateProgramWithSource", e);
    // Source, not SPIR-V, deliberately: the online compiler is part of what
    // the guest would have to run, the same argument kernels.ptx makes for
    // the CUDA JIT.
    if ((e = clBuildProgram(prog, 1, &dev, NULL, NULL, NULL)) != CL_SUCCESS) {
        char log[4096] = { 0 };
        clGetProgramBuildInfo(prog, dev, CL_PROGRAM_BUILD_LOG, sizeof log, log, NULL);
        fprintf(stderr, "oclprobe: build log:\n%s\n", log);
        return fail("clBuildProgram", e);
    }
    cl_kernel k = clCreateKernel(prog, "vecadd", &e);
    if (!k)
        return fail("clCreateKernel", e);

    int *a = malloc(N * sizeof(int)), *b = malloc(N * sizeof(int)), *c = malloc(N * sizeof(int));
    if (!a || !b || !c)
        return fail("malloc", 0);
    for (int i = 0; i < N; i++) {
        a[i] = i;
        b[i] = 2 * i + 1;
        c[i] = -1;
    }

    cl_mem da = clCreateBuffer(ctx, CL_MEM_READ_ONLY | CL_MEM_COPY_HOST_PTR,
                               N * sizeof(int), a, &e);
    if (!da)
        return fail("clCreateBuffer a", e);
    cl_mem db = clCreateBuffer(ctx, CL_MEM_READ_ONLY | CL_MEM_COPY_HOST_PTR,
                               N * sizeof(int), b, &e);
    if (!db)
        return fail("clCreateBuffer b", e);
    cl_mem dc = clCreateBuffer(ctx, CL_MEM_WRITE_ONLY, N * sizeof(int), NULL, &e);
    if (!dc)
        return fail("clCreateBuffer c", e);

    clSetKernelArg(k, 0, sizeof da, &da);
    clSetKernelArg(k, 1, sizeof db, &db);
    clSetKernelArg(k, 2, sizeof dc, &dc);

    size_t global = N;
    if ((e = clEnqueueNDRangeKernel(q, k, 1, NULL, &global, NULL, 0, NULL, NULL)) != CL_SUCCESS)
        return fail("clEnqueueNDRangeKernel", e);
    if ((e = clEnqueueReadBuffer(q, dc, CL_TRUE, 0, N * sizeof(int), c, 0, NULL, NULL)) != CL_SUCCESS)
        return fail("clEnqueueReadBuffer", e);
    clFinish(q);

    int bad = 0;
    for (int i = 0; i < N; i++)
        if (c[i] != a[i] + b[i])
            bad++;

    // Teardown is explicit for the same reason nvprobe's is: the free order
    // is part of what a forwarding implementation has to reproduce, and an
    // exit() would let the driver clean up invisibly.
    clReleaseMemObject(da);
    clReleaseMemObject(db);
    clReleaseMemObject(dc);
    clReleaseKernel(k);
    clReleaseProgram(prog);
    clReleaseCommandQueue(q);
    clReleaseContext(ctx);
    free(a);
    free(b);
    free(c);

    if (bad) {
        fprintf(stderr, "oclprobe: %d of %d elements wrong\n", bad, N);
        return 1;
    }
    printf("OCLPLATFORM=%s\n", vendor);
    printf("OCLDEVICE=%s\n", devname);
    printf("OCLVERIFIED=%d/%d\n", N, N);
    printf("opencl ok\n");
    return 0;
}
