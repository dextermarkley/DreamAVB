// SPDX-License-Identifier: MIT OR Apache-2.0

// AVBBcmUserClient.cpp - validates argument counts with an
// IOUserClientMethodDispatch table, then forwards every call to
// AVBBcmDriver::HandleUserMethod. SendFrame/SendFrames go to the driver's TX
// queue so a talker batch never waits behind the 1 ms RX harvest; every other
// selector runs on the driver's default queue with the timer. The driver's
// txLock serialises the state the two queues share.

#include <os/log.h>

#include <DriverKit/IOLib.h>
#include <DriverKit/IOUserServer.h>
#include <DriverKit/IODispatchQueue.h>
#include <DriverKit/IOUserClient.h>
#include <DriverKit/IOMemoryDescriptor.h>

#include "AVBBcmUserClient.h"
#include "AVBBcmDriver.h"
#include "../AVBShared.h"

#define LOG(fmt, ...) os_log(OS_LOG_DEFAULT, "AVBBcm: uc: " fmt, ##__VA_ARGS__)

struct AVBBcmUserClient_IVars {
    AVBBcmDriver * driver;
    IODispatchQueue * driverQueue;
    IODispatchQueue * txQueue;
    bool ringsUsed;
};

namespace {

struct ForwardCtx {
    AVBBcmDriver * driver;
    AVBBcmUserClient * client;
    uint64_t selector;
    IOUserClientMethodArguments * args;
    kern_return_t ret;
};

void runOnDriverQueue(void * context)
{
    auto * c = static_cast<ForwardCtx *>(context);
    c->ret = c->driver->HandleUserMethod(c->client, c->selector, c->args);
}

struct MemoryCtx {
    AVBBcmDriver * driver;
    AVBBcmUserClient * client;
    uint64_t type;
    uint64_t * options;
    IOMemoryDescriptor ** memory;
    kern_return_t ret;
};

void copyMemoryOnDriverQueue(void * context)
{
    auto * c = static_cast<MemoryCtx *>(context);
    c->ret = c->driver->CopyRingMemory(c->client, c->type, c->options, c->memory);
}

struct GoneCtx {
    AVBBcmDriver * driver;
    AVBBcmUserClient * client;
};

void clientGoneOnDriverQueue(void * context)
{
    auto * c = static_cast<GoneCtx *>(context);
    c->driver->ClientGone(c->client);
}

void runSync(IODispatchQueue * q, void * ctx, IODispatchFunction fn)
{
    if (q->OnQueue()) {
        fn(ctx);
    } else {
        q->DispatchSync_f(ctx, fn);
    }
}

kern_return_t dispatchToDriver(OSObject * target, void * reference, IOUserClientMethodArguments * args)
{
    (void)reference;
    auto * self = OSDynamicCast(AVBBcmUserClient, target);
    if (self == nullptr || args == nullptr) {
        return kIOReturnBadArgument;
    }
    return self->Forward(args);
}

constexpr uint32_t kVar = kIOUserClientVariableStructureSize;

// {function, checkCompletionExists, scalarIn, structIn, scalarOut, structOut}
// per selector in AVBShared.h. Variable-size structures are range-checked
// again inside the driver.
const IOUserClientMethodDispatch kDispatch[kAvbSelBaseCount] = {
    /* 0 GetInfo          */ {dispatchToDriver, 0, 0, 0, 0, kVar},
    /* 1 ReadClock        */ {dispatchToDriver, 0, 1, 0, 0, kVar},
    /* 2 SetClock         */ {dispatchToDriver, 0, 1, 0, 1, 0},
    /* 3 AdjFreq          */ {dispatchToDriver, 0, 1, 0, 1, 0},
    /* 4 InitDatapath     */ {dispatchToDriver, 0, 1, 0, 1, 0},
    /* 5 PollRx           */ {dispatchToDriver, 0, 2, 0, 0, kVar},
    /* 6 SendFrame        */ {dispatchToDriver, 0, 1, kVar, 2, 0},
    /* 7 GetTxTimestamp   */ {dispatchToDriver, 0, 1, 0, 3, 0},
    /* 8 ReadReg32        */ {dispatchToDriver, 0, 1, 0, 1, 0},
    /* 9 ShutdownDatapath */ {dispatchToDriver, 0, 0, 0, 0, 0},
};
static_assert(sizeof(kDispatch) / sizeof(kDispatch[0]) == kAvbSelBaseCount, "BCM dispatch covers the base selectors");

// BCM-only RX-CPU bring-up selectors (16/17), outside the shared base range.
const IOUserClientMethodDispatch kDispatchLoadCoreImage =
    /* 16 LoadCoreImage: scalarIn load_addr, structIn image, scalarOut halted+mode */
    {dispatchToDriver, 0, 1, kVar, 2, 0};
const IOUserClientMethodDispatch kDispatchReadCoreMem =
    /* 17 ReadCoreMem: scalarIn virt_addr, scalarOut word */
    {dispatchToDriver, 0, 1, 0, 1, 0};
// ABI v3 shared rings and the selectors that go with them (same shapes as the
// I210 dispatch table).
const IOUserClientMethodDispatch kDispatchGetCounters =
    /* 11 GetCounters     */ {dispatchToDriver, 0, 0, 0, 0, kVar};
const IOUserClientMethodDispatch kDispatchSetStreamFilter =
    /* 12 SetStreamFilter */ {dispatchToDriver, 0, 0, kVar, 0, 0};
const IOUserClientMethodDispatch kDispatchRingAttach =
    /* 13 RingAttach      */ {dispatchToDriver, 0, 1, 0, 3, 0};
const IOUserClientMethodDispatch kDispatchRingDetach =
    /* 14 RingDetach      */ {dispatchToDriver, 0, 0, 0, 0, 0};
const IOUserClientMethodDispatch kDispatchSendFrames =
    /* 15 SendFrames      */ {dispatchToDriver, 0, 0, kVar, 2, 0};
const IOUserClientMethodDispatch kDispatchReadSram =
    /* 18 ReadSram: scalarIn nic_sram_addr, scalarOut word */
    {dispatchToDriver, 0, 1, 0, 1, 0};
const IOUserClientMethodDispatch kDispatchReadPhy =
    /* 19 ReadPhy: scalarIn clause-22 register, scalarOut value */
    {dispatchToDriver, 0, 1, 0, 1, 0};

}  // namespace

bool
AVBBcmUserClient::init()
{
    if (!super::init()) {
        return false;
    }
    ivars = IONewZero(AVBBcmUserClient_IVars, 1);
    return ivars != nullptr;
}

void
AVBBcmUserClient::free()
{
    if (ivars != nullptr) {
        OSSafeReleaseNULL(ivars->driverQueue);
        OSSafeReleaseNULL(ivars->txQueue);
        OSSafeReleaseNULL(ivars->driver);
    }
    IOSafeDeleteNULL(ivars, AVBBcmUserClient_IVars, 1);
    super::free();
}

kern_return_t
IMPL(AVBBcmUserClient, Start)
{
    kern_return_t ret = Start(provider, SUPERDISPATCH);
    if (ret != kIOReturnSuccess) {
        return ret;
    }
    auto * driver = OSDynamicCast(AVBBcmDriver, provider);
    if (driver == nullptr) {
        LOG("provider is not AVBBcmDriver");
        return kIOReturnNoDevice;
    }
    driver->retain();
    ivars->driver = driver;
    ret = driver->CopyDispatchQueue(kIOServiceDefaultQueueName, &ivars->driverQueue);
    if (ret != kIOReturnSuccess || ivars->driverQueue == nullptr) {
        LOG("CopyDispatchQueue failed 0x%x", ret);
        return ret != kIOReturnSuccess ? ret : kIOReturnError;
    }
    ivars->txQueue = driver->CopyTxQueue();
    if (ivars->txQueue == nullptr) {
        LOG("driver has no TX queue");
        return kIOReturnError;
    }
    LOG("opened");
    return kIOReturnSuccess;
}

kern_return_t
IMPL(AVBBcmUserClient, Stop)
{
    // The driver releases the rings (and takes the datapath down) if this
    // connection owned them.
    if (ivars != nullptr && ivars->driver != nullptr && ivars->driverQueue != nullptr) {
        GoneCtx ctx{ivars->driver, this};
        runSync(ivars->driverQueue, &ctx, clientGoneOnDriverQueue);
    }
    LOG("closed");
    return Stop(provider, SUPERDISPATCH);
}

kern_return_t
IMPL(AVBBcmUserClient, CopyClientMemoryForType)
{
    if (ivars == nullptr || ivars->driver == nullptr || ivars->driverQueue == nullptr || options == nullptr ||
        memory == nullptr) {
        return kIOReturnNotReady;
    }
    MemoryCtx ctx{ivars->driver, this, type, options, memory, kIOReturnError};
    runSync(ivars->driverQueue, &ctx, copyMemoryOnDriverQueue);
    return ctx.ret;
}

void
AVBBcmUserClient::MarkRingsUsed()
{
    if (ivars != nullptr) {
        ivars->ringsUsed = true;
    }
}

bool
AVBBcmUserClient::RingsUsed()
{
    return ivars != nullptr && ivars->ringsUsed;
}

kern_return_t
AVBBcmUserClient::ExternalMethod(uint64_t selector,
                                 IOUserClientMethodArguments * arguments,
                                 const IOUserClientMethodDispatch * dispatch,
                                 OSObject * target,
                                 void * reference)
{
    (void)dispatch;
    (void)target;
    (void)reference;
    if (selector == kAvbSelLoadCoreImage) {
        return super::ExternalMethod(selector, arguments, &kDispatchLoadCoreImage, this, nullptr);
    }
    if (selector == kAvbSelReadCoreMem) {
        return super::ExternalMethod(selector, arguments, &kDispatchReadCoreMem, this, nullptr);
    }
    if (selector == kAvbSelReadSram) {
        return super::ExternalMethod(selector, arguments, &kDispatchReadSram, this, nullptr);
    }
    if (selector == kAvbSelReadPhy) {
        return super::ExternalMethod(selector, arguments, &kDispatchReadPhy, this, nullptr);
    }
    if (selector == kAvbSelGetCounters) {
        return super::ExternalMethod(selector, arguments, &kDispatchGetCounters, this, nullptr);
    }
    if (selector == kAvbSelSetStreamFilter) {
        return super::ExternalMethod(selector, arguments, &kDispatchSetStreamFilter, this, nullptr);
    }
    if (selector == kAvbSelRingAttach) {
        return super::ExternalMethod(selector, arguments, &kDispatchRingAttach, this, nullptr);
    }
    if (selector == kAvbSelRingDetach) {
        return super::ExternalMethod(selector, arguments, &kDispatchRingDetach, this, nullptr);
    }
    if (selector == kAvbSelSendFrames) {
        return super::ExternalMethod(selector, arguments, &kDispatchSendFrames, this, nullptr);
    }
    if (selector >= kAvbSelBaseCount) {
        // 10 StepClock is I210-only: the BCM clock cannot be stepped, so the
        // daemon disciplines its offset in software.
        return kIOReturnUnsupported;
    }
    return super::ExternalMethod(selector, arguments, &kDispatch[selector], this, nullptr);
}

kern_return_t
AVBBcmUserClient::Forward(IOUserClientMethodArguments * arguments)
{
    if (ivars == nullptr || ivars->driver == nullptr || ivars->driverQueue == nullptr) {
        return kIOReturnNotReady;
    }
    ForwardCtx ctx{ivars->driver, this, arguments->selector, arguments, kIOReturnError};
    const bool tx = arguments->selector == kAvbSelSendFrame || arguments->selector == kAvbSelSendFrames;
    IODispatchQueue * q = tx && ivars->txQueue != nullptr ? ivars->txQueue : ivars->driverQueue;
    runSync(q, &ctx, runOnDriverQueue);
    return ctx.ret;
}
