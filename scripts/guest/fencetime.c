// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
/*
 * fencetime -- how long does an EMPTY vkQueueSubmit take to signal its fence?
 *
 *   gcc -O2 -o fencetime fencetime.c -lvulkan
 *
 * The reader for OPEN-QUESTIONS nr 10 (resolved 2026-08-15). Before the
 * event back-channel existed this measured host native 0.03 ms against
 * guest 10.10 ms, ten times identical to the hundredth -- not GPU work,
 * a 10 ms TIMEOUT POLL: the RM event that would wake the wait was
 * registered host-side and never delivered. Every fence, semaphore and
 * present wait paid it, a compositor several times per frame, which was
 * the 60-100 ms felt as lag. With the second virtqueue it reads 0.12 ms.
 * Green means: well under 1 ms.
 */
#include <stdio.h>
#include <time.h>
#include <vulkan/vulkan.h>
static double now(){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec*1e3+t.tv_nsec/1e6;}
int main(){
 VkInstance in; VkInstanceCreateInfo ic={VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO}; vkCreateInstance(&ic,0,&in);
 VkPhysicalDevice p[4]; uint32_t n=4; vkEnumeratePhysicalDevices(in,&n,p);
 float pr=1; VkDeviceQueueCreateInfo q={VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,0,0,0,1,&pr};
 VkDeviceCreateInfo dc={VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,0,0,1,&q}; VkDevice d; vkCreateDevice(p[0],&dc,0,&d);
 VkQueue Q; vkGetDeviceQueue(d,0,0,&Q); VkFence f; VkFenceCreateInfo fc={VK_STRUCTURE_TYPE_FENCE_CREATE_INFO}; vkCreateFence(d,&fc,0,&f);
 for(int i=0;i<10;i++){ double t0=now(); vkQueueSubmit(Q,0,0,f); vkWaitForFences(d,1,&f,1,~0ull); vkResetFences(d,1,&f); printf("%.2f ",now()-t0);} printf(" ms  (empty submit to fence, 10x)\n"); return 0;}
