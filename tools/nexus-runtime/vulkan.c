/* Execute a real SPIR-V compute shader through the pinned upstream ICD. */
#include <vulkan/vulkan.h>
#include <vulkan/vk_icd.h>
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>

#define CHECK(x) do { VkResult result = (x); if (result != VK_SUCCESS) { fprintf(stderr, "vulkan-probe: FAIL %s: %d\n", #x, result); return 1; } } while (0)
#define INSTANCE_FN(name) PFN_##name name = (PFN_##name)get(instance, #name); if (!name) return 2
#define DEVICE_FN(name) PFN_##name name = (PFN_##name)vkGetDeviceProcAddr(device, #name); if (!name) return 3

/* SPIR-V 1.3, GLCompute, local size 1. A StorageBuffer uint is set to 42.
 * This is shader bytecode, not a host implementation of the dispatch. */
static const uint32_t shader[] = {
    0x07230203, 0x00010300, 0, 13, 0,
    (2u<<16)|17, 1,                          /* Shader capability */
    (3u<<16)|14, 0, 1,                      /* Logical GLSL450 */
    (5u<<16)|15, 5, 10, 0x6e69616d, 0,       /* main */
    (6u<<16)|16, 10, 17, 1, 1, 1,           /* LocalSize */
    (3u<<16)|71, 4, 2,                      /* struct Block */
    (5u<<16)|72, 4, 0, 35, 0,               /* member Offset 0 */
    (4u<<16)|71, 9, 34, 0,                  /* DescriptorSet 0 */
    (4u<<16)|71, 9, 33, 0,                  /* Binding 0 */
    (2u<<16)|19, 1,                         /* void */
    (3u<<16)|33, 2, 1,                      /* function type */
    (4u<<16)|21, 3, 32, 0,                  /* uint */
    (3u<<16)|30, 4, 3,                      /* struct uint */
    (4u<<16)|32, 5, 12, 4,                  /* ptr StorageBuffer struct */
    (4u<<16)|32, 6, 12, 3,                  /* ptr StorageBuffer uint */
    (4u<<16)|43, 3, 7, 0,
    (4u<<16)|43, 3, 8, 42,
    (4u<<16)|59, 5, 9, 12,
    (5u<<16)|54, 1, 10, 0, 2,
    (2u<<16)|248, 11,
    (5u<<16)|65, 6, 12, 9, 7,
    (3u<<16)|62, 12, 8,
    (1u<<16)|253,
    (1u<<16)|56
};

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    puts("vulkan-probe: loading upstream SwiftShader ICD");
    void *library = dlopen("/usr/lib/libvk_swiftshader.so", RTLD_NOW | RTLD_LOCAL);
    if (!library) { fprintf(stderr, "vulkan-probe: FAIL dlopen: %s\n", dlerror()); return 1; }
    PFN_vkNegotiateLoaderICDInterfaceVersion negotiate = (PFN_vkNegotiateLoaderICDInterfaceVersion)dlsym(library, "vk_icdNegotiateLoaderICDInterfaceVersion");
    uint32_t interfaceVersion = 7;
    if (!negotiate) return 2;
    CHECK(negotiate(&interfaceVersion));
    PFN_vkGetInstanceProcAddr get = (PFN_vkGetInstanceProcAddr)dlsym(library, "vk_icdGetInstanceProcAddr");
    if (!get) return 2;
    PFN_vkCreateInstance vkCreateInstance = (PFN_vkCreateInstance)get(VK_NULL_HANDLE, "vkCreateInstance");
    VkApplicationInfo app = {.sType=VK_STRUCTURE_TYPE_APPLICATION_INFO, .pApplicationName="Nexus Vulkan probe", .apiVersion=VK_API_VERSION_1_1};
    VkInstanceCreateInfo create = {.sType=VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo=&app};
    VkInstance instance;
    CHECK(vkCreateInstance(&create, NULL, &instance));
    INSTANCE_FN(vkDestroyInstance); INSTANCE_FN(vkEnumeratePhysicalDevices);
    INSTANCE_FN(vkGetPhysicalDeviceProperties); INSTANCE_FN(vkGetPhysicalDeviceQueueFamilyProperties);
    INSTANCE_FN(vkGetPhysicalDeviceMemoryProperties); INSTANCE_FN(vkCreateDevice); INSTANCE_FN(vkGetDeviceProcAddr);
    uint32_t count=1;
    VkPhysicalDevice physical;
    CHECK(vkEnumeratePhysicalDevices(instance, &count, &physical));
    if (count != 1) return 4;
    VkPhysicalDeviceProperties properties;
    vkGetPhysicalDeviceProperties(physical, &properties);
    printf("vulkan-probe: %s API %u.%u.%u\n", properties.deviceName, VK_VERSION_MAJOR(properties.apiVersion), VK_VERSION_MINOR(properties.apiVersion), VK_VERSION_PATCH(properties.apiVersion));
    count=0;
    vkGetPhysicalDeviceQueueFamilyProperties(physical, &count, NULL);
    VkQueueFamilyProperties families[16];
    if (!count || count > 16) return 5;
    vkGetPhysicalDeviceQueueFamilyProperties(physical, &count, families);
    uint32_t family=0;
    while (family < count && !(families[family].queueFlags & VK_QUEUE_COMPUTE_BIT)) family++;
    if (family == count) return 6;
    float priority=1;
    VkDeviceQueueCreateInfo queues = {.sType=VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueFamilyIndex=family, .queueCount=1, .pQueuePriorities=&priority};
    VkDeviceCreateInfo deviceInfo = {.sType=VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .queueCreateInfoCount=1, .pQueueCreateInfos=&queues};
    VkDevice device;
    CHECK(vkCreateDevice(physical, &deviceInfo, NULL, &device));
    DEVICE_FN(vkDestroyDevice); DEVICE_FN(vkGetDeviceQueue);
    DEVICE_FN(vkCreateBuffer); DEVICE_FN(vkDestroyBuffer); DEVICE_FN(vkGetBufferMemoryRequirements);
    DEVICE_FN(vkAllocateMemory); DEVICE_FN(vkFreeMemory); DEVICE_FN(vkBindBufferMemory);
    DEVICE_FN(vkMapMemory); DEVICE_FN(vkUnmapMemory);
    DEVICE_FN(vkCreateDescriptorSetLayout); DEVICE_FN(vkDestroyDescriptorSetLayout);
    DEVICE_FN(vkCreateDescriptorPool); DEVICE_FN(vkDestroyDescriptorPool);
    DEVICE_FN(vkAllocateDescriptorSets); DEVICE_FN(vkUpdateDescriptorSets);
    DEVICE_FN(vkCreatePipelineLayout); DEVICE_FN(vkDestroyPipelineLayout);
    DEVICE_FN(vkCreateShaderModule); DEVICE_FN(vkDestroyShaderModule);
    DEVICE_FN(vkCreateComputePipelines); DEVICE_FN(vkDestroyPipeline);
    DEVICE_FN(vkCreateCommandPool); DEVICE_FN(vkDestroyCommandPool);
    DEVICE_FN(vkAllocateCommandBuffers); DEVICE_FN(vkBeginCommandBuffer); DEVICE_FN(vkEndCommandBuffer);
    DEVICE_FN(vkCmdBindPipeline); DEVICE_FN(vkCmdBindDescriptorSets); DEVICE_FN(vkCmdDispatch); DEVICE_FN(vkCmdPipelineBarrier);
    DEVICE_FN(vkQueueSubmit); DEVICE_FN(vkQueueWaitIdle);
    VkQueue queue;
    vkGetDeviceQueue(device, family, 0, &queue);
    VkBufferCreateInfo bufferInfo = {.sType=VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size=256, .usage=VK_BUFFER_USAGE_STORAGE_BUFFER_BIT, .sharingMode=VK_SHARING_MODE_EXCLUSIVE};
    VkBuffer buffer;
    CHECK(vkCreateBuffer(device, &bufferInfo, NULL, &buffer));
    VkMemoryRequirements requirements;
    vkGetBufferMemoryRequirements(device, buffer, &requirements);
    VkPhysicalDeviceMemoryProperties memoryProperties;
    vkGetPhysicalDeviceMemoryProperties(physical, &memoryProperties);
    uint32_t memoryType=0;
    const VkMemoryPropertyFlags wanted = VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
    while (memoryType < memoryProperties.memoryTypeCount && (!(requirements.memoryTypeBits & (1u<<memoryType)) || (memoryProperties.memoryTypes[memoryType].propertyFlags & wanted) != wanted)) memoryType++;
    if (memoryType == memoryProperties.memoryTypeCount) return 7;
    VkMemoryAllocateInfo allocation = {.sType=VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize=requirements.size, .memoryTypeIndex=memoryType};
    VkDeviceMemory memory;
    CHECK(vkAllocateMemory(device, &allocation, NULL, &memory));
    CHECK(vkBindBufferMemory(device, buffer, memory, 0));
    uint32_t *mapped;
    CHECK(vkMapMemory(device, memory, 0, requirements.size, 0, (void**)&mapped));
    memset(mapped, 0xa5, 256);
    VkDescriptorSetLayoutBinding binding = {.binding=0, .descriptorType=VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .descriptorCount=1, .stageFlags=VK_SHADER_STAGE_COMPUTE_BIT};
    VkDescriptorSetLayoutCreateInfo layoutInfo = {.sType=VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO, .bindingCount=1, .pBindings=&binding};
    VkDescriptorSetLayout layout;
    CHECK(vkCreateDescriptorSetLayout(device, &layoutInfo, NULL, &layout));
    VkDescriptorPoolSize poolSize = {.type=VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .descriptorCount=1};
    VkDescriptorPoolCreateInfo poolInfo = {.sType=VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO, .maxSets=1, .poolSizeCount=1, .pPoolSizes=&poolSize};
    VkDescriptorPool pool;
    CHECK(vkCreateDescriptorPool(device, &poolInfo, NULL, &pool));
    VkDescriptorSetAllocateInfo setInfo = {.sType=VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO, .descriptorPool=pool, .descriptorSetCount=1, .pSetLayouts=&layout};
    VkDescriptorSet set;
    CHECK(vkAllocateDescriptorSets(device, &setInfo, &set));
    VkDescriptorBufferInfo descriptorBuffer = {.buffer=buffer, .offset=0, .range=256};
    VkWriteDescriptorSet write = {.sType=VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet=set, .dstBinding=0, .descriptorCount=1, .descriptorType=VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, .pBufferInfo=&descriptorBuffer};
    vkUpdateDescriptorSets(device, 1, &write, 0, NULL);
    VkPipelineLayoutCreateInfo pipelineLayoutInfo = {.sType=VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO, .setLayoutCount=1, .pSetLayouts=&layout};
    VkPipelineLayout pipelineLayout;
    CHECK(vkCreatePipelineLayout(device, &pipelineLayoutInfo, NULL, &pipelineLayout));
    VkShaderModuleCreateInfo moduleInfo = {.sType=VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, .codeSize=sizeof(shader), .pCode=shader};
    VkShaderModule module;
    CHECK(vkCreateShaderModule(device, &moduleInfo, NULL, &module));
    VkComputePipelineCreateInfo pipelineInfo = {.sType=VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO, .stage={.sType=VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage=VK_SHADER_STAGE_COMPUTE_BIT, .module=module, .pName="main"}, .layout=pipelineLayout};
    VkPipeline pipeline;
    puts("vulkan-probe: compiling SPIR-V through Reactor");
    CHECK(vkCreateComputePipelines(device, VK_NULL_HANDLE, 1, &pipelineInfo, NULL, &pipeline));
    VkCommandPoolCreateInfo commandPoolInfo = {.sType=VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .queueFamilyIndex=family};
    VkCommandPool commandPool;
    CHECK(vkCreateCommandPool(device, &commandPoolInfo, NULL, &commandPool));
    VkCommandBufferAllocateInfo commandInfo = {.sType=VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool=commandPool, .level=VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount=1};
    VkCommandBuffer command;
    CHECK(vkAllocateCommandBuffers(device, &commandInfo, &command));
    VkCommandBufferBeginInfo begin = {.sType=VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
    CHECK(vkBeginCommandBuffer(command, &begin));
    vkCmdBindPipeline(command, VK_PIPELINE_BIND_POINT_COMPUTE, pipeline);
    vkCmdBindDescriptorSets(command, VK_PIPELINE_BIND_POINT_COMPUTE, pipelineLayout, 0, 1, &set, 0, NULL);
    vkCmdDispatch(command, 1, 1, 1);
    VkMemoryBarrier barrier = {.sType=VK_STRUCTURE_TYPE_MEMORY_BARRIER, .srcAccessMask=VK_ACCESS_SHADER_WRITE_BIT, .dstAccessMask=VK_ACCESS_HOST_READ_BIT};
    vkCmdPipelineBarrier(command, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_PIPELINE_STAGE_HOST_BIT, 0, 1, &barrier, 0, NULL, 0, NULL);
    CHECK(vkEndCommandBuffer(command));
    VkSubmitInfo submit = {.sType=VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount=1, .pCommandBuffers=&command};
    CHECK(vkQueueSubmit(queue, 1, &submit, VK_NULL_HANDLE));
    CHECK(vkQueueWaitIdle(queue));
    if (mapped[0] != 42) { fprintf(stderr, "vulkan-probe: FAIL dispatch result %u\n", mapped[0]); return 8; }
    for (uint32_t i=1; i<64; i++) if (mapped[i] != 0xa5a5a5a5) return 9;
    vkDestroyCommandPool(device, commandPool, NULL);
    vkDestroyPipeline(device, pipeline, NULL);
    vkDestroyShaderModule(device, module, NULL);
    vkDestroyPipelineLayout(device, pipelineLayout, NULL);
    vkDestroyDescriptorPool(device, pool, NULL);
    vkDestroyDescriptorSetLayout(device, layout, NULL);
    vkUnmapMemory(device, memory);
    vkDestroyBuffer(device, buffer, NULL);
    vkFreeMemory(device, memory, NULL);
    vkDestroyDevice(device, NULL);
    vkDestroyInstance(instance, NULL);
    dlclose(library);
    puts("vulkan-probe: PASS real SPIR-V JIT dispatch and memory visibility");
    return 0;
}
