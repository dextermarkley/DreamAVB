// SPDX-License-Identifier: MIT OR Apache-2.0

// AVBBcmUserClient.cpp - validates argument counts with an
// IOUserClientMethodDispatch table, then forwards every call to
// AVBBcmDriver::HandleUserMethod on the driver's default queue. That keeps all
// hardware access serialized with the driver's 2 ms poll timer without locks.

#include <os/log.h>

#include <DriverKit/IOLib.h>
#include <DriverKit/IOUserServer.h>
#include <DriverKit/IODispatchQueue.h>
#include <DriverKit/IOUserClient.h>

#include "AVBBcmUserClient.h"
#include "AVBBcmDriver.h"
#include "../AVBShared.h"

#define LOG(fmt, ...) os_log(OS_LOG_DEFAULT, "AVBBcm: uc: " fmt, ##__VA_ARGS__)

struct AVBBcmUserClient_IVars {
    AVBBcmDriver * driver;
    IODispatchQueue * driverQueue;
};

namespace {

struct ForwardCtx {
    AVBBcmDriver * driver;
    uint64_t selector;
    IOUserClientMethodArguments * args;
    kern_return_t ret;
};

void runOnDriverQueue(void * context)
{
    auto * c = static_cast<ForwardCtx *>(context);
    c->ret = c->driver->HandleUserMethod(c->selector, c->args);
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
    LOG("opened");
    return kIOReturnSuccess;
}

kern_return_t
IMPL(AVBBcmUserClient, Stop)
{
    LOG("closed");
    return Stop(provider, SUPERDISPATCH);
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
    if (selector >= kAvbSelBaseCount) {
        // 10..14 (shared rings, StepClock, counters) are I210-only.
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
    ForwardCtx ctx{ivars->driver, arguments->selector, arguments, kIOReturnError};
    if (ivars->driverQueue->OnQueue()) {
        runOnDriverQueue(&ctx);
    } else {
        ivars->driverQueue->DispatchSync_f(&ctx, runOnDriverQueue);
    }
    return ctx.ret;
}
