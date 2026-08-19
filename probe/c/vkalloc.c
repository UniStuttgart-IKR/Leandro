// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
/* vkalloc: which Vulkan memory types can the guest actually allocate?
 *
 * THE QUESTION. Since 2026-08-07 the guest's `vulkaninfo`
 * lists the NVIDIA card in full -- device creation, queues, limits, all of
 * it. What it cannot do is put anything in memory: ffmpeg's Vulkan upload
 * ends in VK_ERROR_OUT_OF_DEVICE_MEMORY, and the backend logs NO failing RM
 * call for it. So the refusal is either a decision inside the ICD or a path
 * that never reaches RM, and "the driver says out of memory" is not a
 * diagnosis -- the card has 8 GiB free.
 *
 * This walks every memory type the device advertises and allocates a small
 * block from each, then tries an image and a buffer, then a host mapping.
 * The point is the TABLE it prints: which property combinations work and
 * which do not is a far sharper statement than one error code from ffmpeg,
 * and it is directly comparable against the same run on the host.
 *
 * Deliberately no external memory, no dma-buf, no swapchain: those are the
 * next questions, and mixing them in would make a failure ambiguous.
 *
 * Uses vkGetInstanceProcAddr for everything, so it links against nothing
 * but libvulkan and runs wherever the loader does.
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <vulkan/vulkan.h>

static const char *res(VkResult r)
{
    switch (r) {
    case VK_SUCCESS:                        return "ok";
    case VK_ERROR_OUT_OF_HOST_MEMORY:       return "OUT_OF_HOST_MEMORY";
    case VK_ERROR_OUT_OF_DEVICE_MEMORY:     return "OUT_OF_DEVICE_MEMORY";
    case VK_ERROR_INVALID_EXTERNAL_HANDLE:  return "INVALID_EXTERNAL_HANDLE";
    case VK_ERROR_MEMORY_MAP_FAILED:        return "MEMORY_MAP_FAILED";
    case VK_ERROR_INITIALIZATION_FAILED:    return "INITIALIZATION_FAILED";
    case VK_ERROR_FEATURE_NOT_PRESENT:      return "FEATURE_NOT_PRESENT";
    case VK_ERROR_TOO_MANY_OBJECTS:         return "TOO_MANY_OBJECTS";
    default:                                break;
    }
    static char b[32];
    snprintf(b, sizeof b, "VkResult %d", (int)r);
    return b;
}

static void flags_str(VkMemoryPropertyFlags f, char *out, size_t n)
{
    out[0] = 0;
    struct { VkMemoryPropertyFlags bit; const char *name; } t[] = {
        { VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT,     "DEVICE_LOCAL" },
        { VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT,     "HOST_VISIBLE" },
        { VK_MEMORY_PROPERTY_HOST_COHERENT_BIT,    "HOST_COHERENT" },
        { VK_MEMORY_PROPERTY_HOST_CACHED_BIT,      "HOST_CACHED" },
        { VK_MEMORY_PROPERTY_LAZILY_ALLOCATED_BIT, "LAZY" },
    };
    for (size_t i = 0; i < sizeof t / sizeof *t; i++)
        if (f & t[i].bit) {
            if (out[0]) strncat(out, "|", n - strlen(out) - 1);
            strncat(out, t[i].name, n - strlen(out) - 1);
        }
    if (!out[0]) strncpy(out, "-", n);
}


/* Pick a memory type from `bits`, preferring DEVICE_LOCAL.
 *
 * WARNING: __builtin_ctz(bits) -- the obvious choice -- takes the LOWEST
 * allowed type, and on this driver that is type 0 on the 23 GiB system heap.
 * An export test that uses it answers the easy half of DISPLAY.md 10.1 (the
 * importable case) and silently skips the half that decides the cost: a
 * render target in VRAM.
 */
static uint32_t pick_type(const VkPhysicalDeviceMemoryProperties *mp,
                          uint32_t bits, int want_device_local)
{
    for (uint32_t i = 0; i < mp->memoryTypeCount; i++) {
        if (!(bits & (1u << i)))
            continue;
        int dl = (mp->memoryTypes[i].propertyFlags &
                  VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT) != 0;
        if (dl == want_device_local)
            return i;
    }
    return (uint32_t)__builtin_ctz(bits);
}

int main(int argc, char **argv)
{
    /* Which device: by substring of the name, so a guest with Venus AND
     * NVIDIA can be pointed at either without counting indices. */
    const char *want = argc > 1 ? argv[1] : "NVIDIA";

    VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                              .pApplicationName = "vkalloc",
                              .apiVersion = VK_API_VERSION_1_1 };
    VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                                 .pApplicationInfo = &app };
    VkInstance inst;
    VkResult r = vkCreateInstance(&ici, NULL, &inst);
    printf("  %-40s %s\n", "vkCreateInstance", res(r));
    if (r != VK_SUCCESS) return 1;

    uint32_t n = 0;
    vkEnumeratePhysicalDevices(inst, &n, NULL);
    VkPhysicalDevice *devs = calloc(n, sizeof *devs);
    vkEnumeratePhysicalDevices(inst, &n, devs);
    printf("  %-40s %u\n", "physical devices", n);

    VkPhysicalDevice pick = VK_NULL_HANDLE;
    VkPhysicalDeviceProperties props;
    for (uint32_t i = 0; i < n; i++) {
        vkGetPhysicalDeviceProperties(devs[i], &props);
        printf("    [%u] %s\n", i, props.deviceName);
        if (!pick && strstr(props.deviceName, want) && !strstr(props.deviceName, "Venus"))
            pick = devs[i];
    }
    if (!pick) { printf("  no device matching '%s'\n", want); return 1; }
    vkGetPhysicalDeviceProperties(pick, &props);
    printf("  %-40s %s\n", "chosen", props.deviceName);

    /* One queue, no extensions: the smallest device that can own memory. */
    float prio = 1.0f;
    VkDeviceQueueCreateInfo q = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                  .queueFamilyIndex = 0, .queueCount = 1,
                                  .pQueuePriorities = &prio };
    /* The export test needs these two; without them vkGetMemoryFdKHR is not
     * even a symbol.
     *
     * WARNING: enable only what the device ADVERTISES. Asking for an absent
     * extension makes vkCreateDevice return VK_ERROR_EXTENSION_NOT_PRESENT
     * and the whole probe reports nothing -- which is exactly what happened
     * on the first guest run, and it hid the real finding: the NVIDIA device
     * in a guest has VK_KHR_external_memory_fd but NOT
     * VK_EXT_external_memory_dma_buf, while natively it has both. A probe
     * that dies on the missing one cannot say which one is missing. */
    uint32_t nx = 0;
    vkEnumerateDeviceExtensionProperties(pick, NULL, &nx, NULL);
    VkExtensionProperties *xs = calloc(nx ? nx : 1, sizeof *xs);
    vkEnumerateDeviceExtensionProperties(pick, NULL, &nx, xs);
    int have_fd = 0, have_dmabuf = 0, have_hostptr = 0;
    for (uint32_t i = 0; i < nx; i++) {
        if (!strcmp(xs[i].extensionName, VK_KHR_EXTERNAL_MEMORY_FD_EXTENSION_NAME))
            have_fd = 1;
        if (!strcmp(xs[i].extensionName, VK_EXT_EXTERNAL_MEMORY_DMA_BUF_EXTENSION_NAME))
            have_dmabuf = 1;
        if (!strcmp(xs[i].extensionName, VK_EXT_EXTERNAL_MEMORY_HOST_EXTENSION_NAME))
            have_hostptr = 1;
    }
    printf("  %-40s %s\n", VK_KHR_EXTERNAL_MEMORY_FD_EXTENSION_NAME,
           have_fd ? "present" : "ABSENT");
    printf("  %-40s %s\n", VK_EXT_EXTERNAL_MEMORY_DMA_BUF_EXTENSION_NAME,
           have_dmabuf ? "present" : "ABSENT");
    printf("  %-40s %s\n", VK_EXT_EXTERNAL_MEMORY_HOST_EXTENSION_NAME,
           have_hostptr ? "present" : "ABSENT");

    const char *devext[3];
    uint32_t nde = 0;
    if (have_fd)      devext[nde++] = VK_KHR_EXTERNAL_MEMORY_FD_EXTENSION_NAME;
    if (have_dmabuf)  devext[nde++] = VK_EXT_EXTERNAL_MEMORY_DMA_BUF_EXTENSION_NAME;
    if (have_hostptr) devext[nde++] = VK_EXT_EXTERNAL_MEMORY_HOST_EXTENSION_NAME;
    VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                               .queueCreateInfoCount = 1, .pQueueCreateInfos = &q,
                               .enabledExtensionCount = nde,
                               .ppEnabledExtensionNames = devext };
    VkDevice dev;
    r = vkCreateDevice(pick, &dci, NULL, &dev);
    printf("  %-40s %s\n", "vkCreateDevice", res(r));
    if (r != VK_SUCCESS) return 1;

    VkPhysicalDeviceMemoryProperties mp;
    vkGetPhysicalDeviceMemoryProperties(pick, &mp);

    printf("\n  %-4s %-6s %-46s %-10s %s\n", "type", "heap", "properties", "1 MiB", "map");
    for (uint32_t i = 0; i < mp.memoryTypeCount; i++) {
        char f[128];
        flags_str(mp.memoryTypes[i].propertyFlags, f, sizeof f);
        VkMemoryAllocateInfo ai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                                    .allocationSize = 1u << 20,
                                    .memoryTypeIndex = i };
        VkDeviceMemory mem;
        VkResult a = vkAllocateMemory(dev, &ai, NULL, &mem);
        const char *mapres = "-";
        if (a == VK_SUCCESS) {
            if (mp.memoryTypes[i].propertyFlags & VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT) {
                void *p = NULL;
                VkResult m = vkMapMemory(dev, mem, 0, VK_WHOLE_SIZE, 0, &p);
                mapres = res(m);
                if (m == VK_SUCCESS) vkUnmapMemory(dev, mem);
            }
            vkFreeMemory(dev, mem, NULL);
        }
        printf("  %-4u %-6u %-46s %-10s %s\n",
               i, mp.memoryTypes[i].heapIndex, f, res(a), mapres);
    }

    /* And the two objects a renderer actually needs, each with the memory
     * type the driver itself asks for -- a type that allocates in isolation
     * can still be refused for a real resource. */
    printf("\n");
    VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
                               .size = 1u << 20,
                               .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT,
                               .sharingMode = VK_SHARING_MODE_EXCLUSIVE };
    VkBuffer buf;
    r = vkCreateBuffer(dev, &bci, NULL, &buf);
    printf("  %-40s %s\n", "vkCreateBuffer 1 MiB", res(r));
    if (r == VK_SUCCESS) {
        VkMemoryRequirements mr;
        vkGetBufferMemoryRequirements(dev, buf, &mr);
        VkMemoryAllocateInfo ai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                                    .allocationSize = mr.size,
                                    .memoryTypeIndex = (uint32_t)__builtin_ctz(mr.memoryTypeBits) };
        VkDeviceMemory mem;
        VkResult a = vkAllocateMemory(dev, &ai, NULL, &mem);
        printf("  %-40s %s (type %u of mask %#x)\n", "  bind memory", res(a),
               (uint32_t)__builtin_ctz(mr.memoryTypeBits), mr.memoryTypeBits);
        if (a == VK_SUCCESS) {
            printf("  %-40s %s\n", "  vkBindBufferMemory",
                   res(vkBindBufferMemory(dev, buf, mem, 0)));
            vkFreeMemory(dev, mem, NULL);
        }
        vkDestroyBuffer(dev, buf, NULL);
    }

    VkImageCreateInfo ici2 = { .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
                               .imageType = VK_IMAGE_TYPE_2D,
                               .format = VK_FORMAT_R8G8B8A8_UNORM,
                               .extent = { 1920, 1080, 1 },
                               .mipLevels = 1, .arrayLayers = 1,
                               .samples = VK_SAMPLE_COUNT_1_BIT,
                               .tiling = VK_IMAGE_TILING_OPTIMAL,
                               .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT |
                                        VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
                               .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
                               .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED };
    VkImage img;
    r = vkCreateImage(dev, &ici2, NULL, &img);
    printf("  %-40s %s\n", "vkCreateImage 1920x1080 RGBA8", res(r));
    if (r == VK_SUCCESS) {
        VkMemoryRequirements mr;
        vkGetImageMemoryRequirements(dev, img, &mr);
        VkMemoryAllocateInfo ai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                                    .allocationSize = mr.size,
                                    .memoryTypeIndex = (uint32_t)__builtin_ctz(mr.memoryTypeBits) };
        VkDeviceMemory mem;
        VkResult a = vkAllocateMemory(dev, &ai, NULL, &mem);
        printf("  %-40s %s (%llu bytes, type %u of mask %#x)\n", "  bind memory", res(a),
               (unsigned long long)mr.size,
               (uint32_t)__builtin_ctz(mr.memoryTypeBits), mr.memoryTypeBits);
        if (a == VK_SUCCESS) {
            printf("  %-40s %s\n", "  vkBindImageMemory",
                   res(vkBindImageMemory(dev, img, mem, 0)));
            vkFreeMemory(dev, mem, NULL);
        }
        vkDestroyImage(dev, img, NULL);
    }

    /* ---- and the one that decides the display path ------------------
     * A VRAM-backed image EXPORTED as an fd. The display-path notes list
     * this as the thing never demonstrated, and as what decides
     * between "import, no copy" and "one copy per frame". The early traces
     * showed no client doing it at all, so it had to be asked directly.
     *
     * Two handle types, because they fail for different reasons:
     *   OPAQUE_FD  NVIDIA's own external-memory fd. Nothing outside the
     *              driver can read it, but it proves the mechanism.
     *   DMA_BUF    the one a compositor could import. If the driver offers
     *              it, PRIME becomes a question of plumbing rather than of
     *              capability.
     *
     * WARNING: the fd comes back from the HOST driver in a guest, which is
     * the out-direction problem (an fd is a process resource). So a
     * SUCCESS here is not yet a usable buffer -- it is the statement that
     * the driver would export one.
     */
    printf("\n");
    for (int pass = 0; pass < 2; pass++) {
        VkExternalMemoryHandleTypeFlagBits ht = pass == 0
            ? VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT
            : VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT;
        const char *hname = pass == 0 ? "OPAQUE_FD" : "DMA_BUF";
        if (pass == 0 ? !have_fd : !have_dmabuf) {
            printf("  %-40s extension absent -- not asked\n", hname);
            continue;
        }

        /* Ask first, allocate second: the driver says per format and usage
         * whether it can export at all, and a refusal here is a different
         * statement from a failed allocation. */
        VkPhysicalDeviceExternalImageFormatInfo eifi = {
            .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO,
            .handleType = ht };
        VkPhysicalDeviceImageFormatInfo2 ifi = {
            .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2,
            .pNext = &eifi,
            .format = VK_FORMAT_R8G8B8A8_UNORM,
            .type = VK_IMAGE_TYPE_2D,
            .tiling = VK_IMAGE_TILING_OPTIMAL,
            .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT |
                     VK_IMAGE_USAGE_TRANSFER_SRC_BIT };
        VkExternalImageFormatProperties eifp = {
            .sType = VK_STRUCTURE_TYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES };
        VkImageFormatProperties2 ifp = {
            .sType = VK_STRUCTURE_TYPE_IMAGE_FORMAT_PROPERTIES_2, .pNext = &eifp };
        VkResult q2 = vkGetPhysicalDeviceImageFormatProperties2(pick, &ifi, &ifp);
        VkExternalMemoryFeatureFlags feat =
            eifp.externalMemoryProperties.externalMemoryFeatures;
        printf("  %-40s %s%s%s%s\n", hname, res(q2),
               (feat & VK_EXTERNAL_MEMORY_FEATURE_EXPORTABLE_BIT) ? "  exportable" : "",
               (feat & VK_EXTERNAL_MEMORY_FEATURE_IMPORTABLE_BIT) ? "  importable" : "",
               (feat & VK_EXTERNAL_MEMORY_FEATURE_DEDICATED_ONLY_BIT) ? "  dedicated-only" : "");
        if (q2 != VK_SUCCESS ||
            !(feat & VK_EXTERNAL_MEMORY_FEATURE_EXPORTABLE_BIT))
            continue;

        VkExternalMemoryImageCreateInfo emici = {
            .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
            .handleTypes = ht };
        VkImageCreateInfo xi = ici2;
        xi.pNext = &emici;
        VkImage ximg;
        VkResult xr = vkCreateImage(dev, &xi, NULL, &ximg);
        printf("    %-38s %s\n", "vkCreateImage (external)", res(xr));
        if (xr != VK_SUCCESS) continue;

        VkMemoryRequirements mr;
        vkGetImageMemoryRequirements(dev, ximg, &mr);
        uint32_t xt = pick_type(&mp, mr.memoryTypeBits, 1);
        VkExportMemoryAllocateInfo emai = {
            .sType = VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO,
            .handleTypes = ht };
        VkMemoryDedicatedAllocateInfo mdai = {
            .sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
            .pNext = &emai, .image = ximg };
        VkMemoryAllocateInfo xai = {
            .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
            .pNext = &mdai, .allocationSize = mr.size,
            .memoryTypeIndex = xt };
        VkDeviceMemory xmem;
        VkResult xa = vkAllocateMemory(dev, &xai, NULL, &xmem);
        char pf[128];
        flags_str(mp.memoryTypes[xt].propertyFlags, pf, sizeof pf);
        printf("    %-38s %s (%llu bytes, type %u heap %u: %s)\n",
               "vkAllocateMemory (exportable)", res(xa),
               (unsigned long long)mr.size, xt, mp.memoryTypes[xt].heapIndex, pf);
        if (xa == VK_SUCCESS) {
            printf("    %-38s %s\n", "vkBindImageMemory",
                   res(vkBindImageMemory(dev, ximg, xmem, 0)));
            PFN_vkGetMemoryFdKHR getfd = (PFN_vkGetMemoryFdKHR)
                vkGetDeviceProcAddr(dev, "vkGetMemoryFdKHR");
            if (!getfd) {
                printf("    %-38s %s\n", "vkGetMemoryFdKHR", "symbol missing");
            } else {
                VkMemoryGetFdInfoKHR gi = {
                    .sType = VK_STRUCTURE_TYPE_MEMORY_GET_FD_INFO_KHR,
                    .memory = xmem, .handleType = ht };
                int fd = -1;
                VkResult g = getfd(dev, &gi, &fd);
                char lp[64], tgt[128] = "-";
                if (g == VK_SUCCESS && fd >= 0) {
                    snprintf(lp, sizeof lp, "/proc/self/fd/%d", fd);
                    ssize_t rl = readlink(lp, tgt, sizeof tgt - 1);
                    if (rl > 0) tgt[rl] = 0;
                }
                printf("    %-38s %s  fd=%d (%s)\n", "vkGetMemoryFdKHR",
                       res(g), fd, tgt);
                if (fd >= 0) close(fd);
            }
            vkFreeMemory(dev, xmem, NULL);
        }
        vkDestroyImage(dev, ximg, NULL);
    }

    /* ---- the other direction: IMPORT a pointer this process owns ------
     * `VK_EXT_external_memory_host` lets the driver take an ordinary
     * userspace mapping and treat it as device memory. That is the shape
     * a shared display buffer would have WITHOUT any dma-buf and without a
     * DRM node: whatever the guest can mmap -- a virtio-gpu blob among
     * other things -- becomes something NVIDIA can render into.
     *
     * Underneath it is the path this project already runs for pinned host
     * memory: NV01_MEMORY_SYSTEM_OS_DESCRIPTOR, guest pages resolved to
     * GPA runs, pinned on the host (`stat_osdesc_pins` in the gate). So
     * the question is not whether the plumbing exists but whether the
     * driver offers this door in a guest at all.
     *
     * WARNING: this proves IMPORT, nothing more. Whether virtio-gpu would
     * accept the same pages, and under which tiling, is a separate
     * question and not answered here.
     */
    if (have_hostptr) {
        printf("\n");
        VkPhysicalDeviceExternalMemoryHostPropertiesEXT hp = {
            .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_MEMORY_HOST_PROPERTIES_EXT };
        VkPhysicalDeviceProperties2 p2 = {
            .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2, .pNext = &hp };
        vkGetPhysicalDeviceProperties2(pick, &p2);
        printf("  %-40s %llu bytes\n", "minImportedHostPointerAlignment",
               (unsigned long long)hp.minImportedHostPointerAlignment);

        size_t align = hp.minImportedHostPointerAlignment ?
                       (size_t)hp.minImportedHostPointerAlignment : 4096;
        /* 8 MiB, a 1080p RGBA8 frame with room to spare, rounded up to
         * the alignment the driver demands. */
        size_t len = (((size_t)8 << 20) + align - 1) / align * align;
        void *host = NULL;
        if (posix_memalign(&host, align, len) != 0 || !host) {
            printf("  %-40s posix_memalign failed\n", "host buffer");
        } else {
            memset(host, 0xA5, len);
            PFN_vkGetMemoryHostPointerPropertiesEXT gethp =
                (PFN_vkGetMemoryHostPointerPropertiesEXT)
                vkGetDeviceProcAddr(dev, "vkGetMemoryHostPointerPropertiesEXT");
            VkMemoryHostPointerPropertiesEXT hpp = {
                .sType = VK_STRUCTURE_TYPE_MEMORY_HOST_POINTER_PROPERTIES_EXT };
            VkResult hr = gethp
                ? gethp(dev, VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT,
                        host, &hpp)
                : VK_ERROR_EXTENSION_NOT_PRESENT;
            printf("  %-40s %s (types %#x)\n", "vkGetMemoryHostPointerProperties",
                   res(hr), hpp.memoryTypeBits);

            if (hr == VK_SUCCESS && hpp.memoryTypeBits) {
                uint32_t ht2 = (uint32_t)__builtin_ctz(hpp.memoryTypeBits);
                char pf2[128];
                flags_str(mp.memoryTypes[ht2].propertyFlags, pf2, sizeof pf2);
                VkImportMemoryHostPointerInfoEXT imp = {
                    .sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_HOST_POINTER_INFO_EXT,
                    .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT,
                    .pHostPointer = host };
                VkMemoryAllocateInfo iai = {
                    .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                    .pNext = &imp, .allocationSize = len,
                    .memoryTypeIndex = ht2 };
                VkDeviceMemory imem;
                VkResult ia = vkAllocateMemory(dev, &iai, NULL, &imem);
                printf("  %-40s %s (type %u heap %u: %s)\n",
                       "vkAllocateMemory (imported host ptr)", res(ia),
                       ht2, mp.memoryTypes[ht2].heapIndex, pf2);
                if (ia == VK_SUCCESS) {
                    /* A buffer bound to it is the proof that the GPU can
                     * address those very pages. */
                    VkBufferCreateInfo ib = {
                        .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
                        .size = 1u << 20,
                        .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT |
                                 VK_BUFFER_USAGE_TRANSFER_SRC_BIT,
                        .sharingMode = VK_SHARING_MODE_EXCLUSIVE };
                    VkBuffer ibuf;
                    if (vkCreateBuffer(dev, &ib, NULL, &ibuf) == VK_SUCCESS) {
                        printf("    %-38s %s\n", "vkBindBufferMemory",
                               res(vkBindBufferMemory(dev, ibuf, imem, 0)));
                        vkDestroyBuffer(dev, ibuf, NULL);
                    }
                    vkFreeMemory(dev, imem, NULL);
                }
            }
            free(host);
        }
    }

    vkDestroyDevice(dev, NULL);
    vkDestroyInstance(inst, NULL);
    return 0;
}
