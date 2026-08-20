// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//
// Vulkan raytracing initialisation, and nothing else: create an instance,
// find the NVIDIA physical device, create a logical device with
// VK_KHR_acceleration_structure enabled, allocate one device-address buffer,
// tear it down.
//
// WHY THIS IS ITS OWN PROBE. libnvidia-rtcore is dlopened by the driver at
// exactly one moment: right after the 4 GiB VA reservation, and only when a
// client enables the acceleration-structure extension. No enumerating client
// reaches it -- vulkaninfo does not, vkcube does not, glxgears does not. The
// day it was missing from the guest, CS2 said "Failed to initialize Vulkan"
// and twelve ENOENTs in strace were the only evidence, because a missing
// FILE produces no failing RM call (OPEN-QUESTIONS number 11). So the
// coverage matrix needs a workload that provably takes that branch, and
// this is the smallest one.
//
// It also matters as a mediation probe rather than a rendering one: the
// initialisation was where GET_ACTIVE_DEVICE_IDS and GET_P2P_CAPS_MATRIX
// leaked a host gpuId through array mediation the wire could not express.
//
// Deterministic: no window, no swapchain, no surface, no shaders, no timing.
// One buffer of a fixed size, one device address queried.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <vulkan/vulkan.h>

#define NVIDIA_PCI_VENDOR_ID 0x10de
#define BUFSZ (1u << 20)

// The extension set an acceleration structure actually needs. All four, not
// just the headline one: VK_KHR_acceleration_structure requires
// deferred_host_operations and buffer_device_address, and asking for the
// headline alone is a device-creation error rather than a driver branch.
static const char *WANT[] = {
    VK_KHR_ACCELERATION_STRUCTURE_EXTENSION_NAME,
    VK_KHR_DEFERRED_HOST_OPERATIONS_EXTENSION_NAME,
    VK_KHR_BUFFER_DEVICE_ADDRESS_EXTENSION_NAME,
    VK_EXT_DESCRIPTOR_INDEXING_EXTENSION_NAME,
};
#define NWANT ((int)(sizeof WANT / sizeof WANT[0]))

int main(void)
{
    VkApplicationInfo app = {
        .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName = "vkrt",
        .apiVersion = VK_API_VERSION_1_2,
    };
    VkInstanceCreateInfo ici = {
        .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        .pApplicationInfo = &app,
    };
    VkInstance inst;
    VkResult r = vkCreateInstance(&ici, NULL, &inst);
    if (r != VK_SUCCESS) {
        fprintf(stderr, "vkrt: vkCreateInstance -> %d\n", r);
        return 1;
    }

    uint32_t n = 0;
    vkEnumeratePhysicalDevices(inst, &n, NULL);
    if (n == 0) {
        fprintf(stderr, "vkrt: no Vulkan physical device\n");
        return 1;
    }
    if (n > 8)
        n = 8;
    VkPhysicalDevice devs[8];
    vkEnumeratePhysicalDevices(inst, &n, devs);

    VkPhysicalDevice pd = VK_NULL_HANDLE;
    VkPhysicalDeviceProperties props = { 0 };
    for (uint32_t i = 0; i < n; i++) {
        VkPhysicalDeviceProperties p;
        vkGetPhysicalDeviceProperties(devs[i], &p);
        if (p.vendorID == NVIDIA_PCI_VENDOR_ID) {
            pd = devs[i];
            props = p;
            break;
        }
    }
    if (!pd) {
        fprintf(stderr, "vkrt: %u device(s), none with vendorID 0x%x\n", n, NVIDIA_PCI_VENDOR_ID);
        return 1;
    }
    printf("VKDEVICE=%s\n", props.deviceName);

    // The extensions have to be OFFERED before they can be enabled. If the
    // driver does not offer them the answer is "this card cannot", which is
    // a different finding from "the library was missing" and must not be
    // reported as the same thing.
    uint32_t ne = 0;
    vkEnumerateDeviceExtensionProperties(pd, NULL, &ne, NULL);
    VkExtensionProperties *ext = calloc(ne ? ne : 1, sizeof *ext);
    vkEnumerateDeviceExtensionProperties(pd, NULL, &ne, ext);
    int missing = 0;
    for (int w = 0; w < NWANT; w++) {
        int found = 0;
        for (uint32_t i = 0; i < ne; i++)
            if (!strcmp(ext[i].extensionName, WANT[w]))
                found = 1;
        if (!found) {
            fprintf(stderr, "vkrt: device does not offer %s\n", WANT[w]);
            missing++;
        }
    }
    free(ext);
    if (missing) {
        printf("VKRT=not-offered\n");
        return 1;
    }

    uint32_t nq = 0;
    vkGetPhysicalDeviceQueueFamilyProperties(pd, &nq, NULL);
    if (nq == 0) {
        fprintf(stderr, "vkrt: no queue family\n");
        return 1;
    }
    float prio = 1.0f;
    VkDeviceQueueCreateInfo qci = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
        .queueFamilyIndex = 0,
        .queueCount = 1,
        .pQueuePriorities = &prio,
    };

    VkPhysicalDeviceAccelerationStructureFeaturesKHR accel = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ACCELERATION_STRUCTURE_FEATURES_KHR,
        .accelerationStructure = VK_TRUE,
    };
    VkPhysicalDeviceBufferDeviceAddressFeatures bda = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_BUFFER_DEVICE_ADDRESS_FEATURES,
        .pNext = &accel,
        .bufferDeviceAddress = VK_TRUE,
    };
    VkDeviceCreateInfo dci = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
        .pNext = &bda,
        .queueCreateInfoCount = 1,
        .pQueueCreateInfos = &qci,
        .enabledExtensionCount = NWANT,
        .ppEnabledExtensionNames = WANT,
    };
    VkDevice dev;
    r = vkCreateDevice(pd, &dci, NULL, &dev);
    if (r != VK_SUCCESS) {
        // This is the failure CS2 showed. It is worth its own line because
        // it is the one that a missing libnvidia-rtcore produces.
        fprintf(stderr, "vkrt: vkCreateDevice with acceleration structures -> %d\n", r);
        printf("VKRT=device-create-failed\n");
        return 1;
    }

    // A buffer with a device address: the allocation shape a BLAS build
    // needs, without building one. It is what turns "the device was created"
    // into "the device can hand out a GPU address", which is where the
    // 4 GiB VA reservation and the rtcore dlopen sit.
    VkBufferCreateInfo bci = {
        .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
        .size = BUFSZ,
        .usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT | VK_BUFFER_USAGE_SHADER_DEVICE_ADDRESS_BIT,
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
    };
    VkBuffer buf;
    if (vkCreateBuffer(dev, &bci, NULL, &buf) != VK_SUCCESS) {
        fprintf(stderr, "vkrt: vkCreateBuffer failed\n");
        return 1;
    }
    VkMemoryRequirements mr;
    vkGetBufferMemoryRequirements(dev, buf, &mr);
    VkPhysicalDeviceMemoryProperties mp;
    vkGetPhysicalDeviceMemoryProperties(pd, &mp);
    uint32_t type = UINT32_MAX;
    for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
        if ((mr.memoryTypeBits & (1u << i)) &&
            (mp.memoryTypes[i].propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)) {
            type = i;
            break;
        }
    if (type == UINT32_MAX) {
        fprintf(stderr, "vkrt: no device-local memory type for the buffer\n");
        return 1;
    }
    VkMemoryAllocateFlagsInfo afi = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_FLAGS_INFO,
        .flags = VK_MEMORY_ALLOCATE_DEVICE_ADDRESS_BIT,
    };
    VkMemoryAllocateInfo mai = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .pNext = &afi,
        .allocationSize = mr.size,
        .memoryTypeIndex = type,
    };
    VkDeviceMemory mem;
    if (vkAllocateMemory(dev, &mai, NULL, &mem) != VK_SUCCESS) {
        fprintf(stderr, "vkrt: vkAllocateMemory failed\n");
        return 1;
    }
    vkBindBufferMemory(dev, buf, mem, 0);

    VkBufferDeviceAddressInfo bdai = {
        .sType = VK_STRUCTURE_TYPE_BUFFER_DEVICE_ADDRESS_INFO,
        .buffer = buf,
    };
    VkDeviceAddress addr = vkGetBufferDeviceAddress(dev, &bdai);

    vkFreeMemory(dev, mem, NULL);
    vkDestroyBuffer(dev, buf, NULL);
    vkDestroyDevice(dev, NULL);
    vkDestroyInstance(inst, NULL);

    if (addr == 0) {
        fprintf(stderr, "vkrt: device address is 0\n");
        printf("VKRT=no-device-address\n");
        return 1;
    }
    // The address itself is not printed: it moves between runs and would
    // make an otherwise deterministic output look non-deterministic.
    printf("VKRT=ok\n");
    printf("vkrt ok\n");
    return 0;
}
