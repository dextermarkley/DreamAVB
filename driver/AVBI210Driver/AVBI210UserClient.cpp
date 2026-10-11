// SPDX-License-Identifier: MIT OR Apache-2.0

// AVBI210UserClient.cpp - validates argument counts with an
// IOUserClientMethodDispatch table, then forwards every call to
// AVBI210Driver::HandleUserMethod on the driver's default queue. That keeps all
// hardware access serialized with the driver's 500 us poll timer without
// locks. CopyClientMemoryForType and Stop are forwarded the same way
// (CopyRingMemory, ClientGone).

#include <os/log.h>

#include <DriverKit/IOLib.h>
#include <DriverKit/IOUserServer.h>
#include <DriverKit/IODispatchQueue.h>
#include <DriverKit/IOUserClient.h>
#include <DriverKit/IOMemoryDescriptor.h>
#include <DriverKit/OSArray.h>
#include <DriverKit/OSDictionary.h>
#include <DriverKit/OSString.h>

#include "AVBI210UserClient.h"
#include "AVBI210Driver.h"
#include "../AVBShared.h"

#define LOG(fmt, ...) os_log(OS_LOG_DEFAULT, "AVBI210: uc: " fmt, ##__VA_ARGS__)

// Clients must carry this entitlement value (design §3.3). The dext's
// allow-any-userclient-access lets ad-hoc/development-signed clients open it;
// this check restores the scoping to our own tools.
static const char * const kUserClientAccessKey = "com.apple.developer.driverkit.userclient-access";
static const char * const kOurDextId = "com.avbaudio.installer.i210";

struct AVBI210UserClient_IVars {
    AVBI210Driver * driver;
    IODispatchQueue * driverQueue;
    bool ringsUsed;
};

namespace {

struct ForwardCtx {
    AVBI210Driver * driver;
    AVBI210UserClient * client;
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
    AVBI210Driver * driver;
    AVBI210UserClient * client;
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
    AVBI210Driver * driver;
    AVBI210UserClient * client;
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
    auto * self = OSDynamicCast(AVBI210UserClient, target);
    if (self == nullptr || args == nullptr) {
        return kIOReturnBadArgument;
    }
    return self->Forward(args);
}

constexpr uint32_t kVar = kIOUserClientVariableStructureSize;

// {function, checkCompletionExists, scalarIn, structIn, scalarOut, structOut}
// per selector in AVBShared.h (design §3.2). Variable-size structures are
// range-checked again inside the driver.
const IOUserClientMethodDispatch kDispatch[kAvbSelCount] = {
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
    /* 10 StepClock       */ {dispatchToDriver, 0, 1, 0, 3, 0},
    /* 11 GetCounters     */ {dispatchToDriver, 0, 0, 0, 0, kVar},
    /* 12 SetStreamFilter */ {dispatchToDriver, 0, 0, kVar, 0, 0},
    /* 13 RingAttach      */ {dispatchToDriver, 0, 1, 0, 3, 0},
    /* 14 RingDetach      */ {dispatchToDriver, 0, 0, 0, 0, 0},
};
static_assert(sizeof(kDispatch) / sizeof(kDispatch[0]) == kAvbSelCount, "I210 dispatch covers every selector");

// True iff the client's userclient-access entitlement array names this dext.
bool clientEntitled(OSDictionary * ents)
{
    if (ents == nullptr) {
        return false;
    }
    auto * arr = OSDynamicCast(OSArray, ents->getObject(kUserClientAccessKey));
    if (arr == nullptr) {
        return false;
    }
    for (uint32_t i = 0; i < arr->getCount(); i++) {
        auto * s = OSDynamicCast(OSString, arr->getObject(i));
        if (s != nullptr && s->isEqualTo(kOurDextId)) {
            return true;
        }
    }
    return false;
}

}  // namespace

bool
AVBI210UserClient::init()
{
    if (!super::init()) {
        return false;
    }
    ivars = IONewZero(AVBI210UserClient_IVars, 1);
    return ivars != nullptr;
}

void
AVBI210UserClient::free()
{
    if (ivars != nullptr) {
        OSSafeReleaseNULL(ivars->driverQueue);
        OSSafeReleaseNULL(ivars->driver);
    }
    IOSafeDeleteNULL(ivars, AVBI210UserClient_IVars, 1);
    super::free();
}

kern_return_t
IMPL(AVBI210UserClient, Start)
{
    kern_return_t ret = Start(provider, SUPERDISPATCH);
    if (ret != kIOReturnSuccess) {
        return ret;
    }
    OSDictionary * ents = nullptr;
    const kern_return_t eret = CopyClientEntitlements(&ents);
    const bool entitled = eret == kIOReturnSuccess && clientEntitled(ents);
    OSSafeReleaseNULL(ents);
    if (!entitled) {
        LOG("client lacks userclient-access entitlement");
        return kIOReturnNotPermitted;
    }
    auto * driver = OSDynamicCast(AVBI210Driver, provider);
    if (driver == nullptr) {
        LOG("provider is not AVBI210Driver");
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
IMPL(AVBI210UserClient, Stop)
{
    // The driver logs the outcome (rings released, or "closed (not owner)").
    if (ivars != nullptr && ivars->driver != nullptr && ivars->driverQueue != nullptr) {
        GoneCtx ctx{ivars->driver, this};
        runSync(ivars->driverQueue, &ctx, clientGoneOnDriverQueue);
    }
    return Stop(provider, SUPERDISPATCH);
}

kern_return_t
IMPL(AVBI210UserClient, CopyClientMemoryForType)
{
    if (ivars == nullptr || ivars->driver == nullptr || ivars->driverQueue == nullptr || options == nullptr ||
        memory == nullptr) {
        return kIOReturnNotReady;
    }
    MemoryCtx ctx{ivars->driver, this, type, options, memory, kIOReturnError};
    runSync(ivars->driverQueue, &ctx, copyMemoryOnDriverQueue);
    return ctx.ret;
}

kern_return_t
AVBI210UserClient::ExternalMethod(uint64_t selector,
                                 IOUserClientMethodArguments * arguments,
                                 const IOUserClientMethodDispatch * dispatch,
                                 OSObject * target,
                                 void * reference)
{
    (void)dispatch;
    (void)target;
    (void)reference;
    if (selector >= kAvbSelCount) {
        // 15..31 unassigned; 32..47 reserved for the talker path.
        return kIOReturnUnsupported;
    }
    return super::ExternalMethod(selector, arguments, &kDispatch[selector], this, nullptr);
}

kern_return_t
AVBI210UserClient::Forward(IOUserClientMethodArguments * arguments)
{
    if (ivars == nullptr || ivars->driver == nullptr || ivars->driverQueue == nullptr) {
        return kIOReturnNotReady;
    }
    ForwardCtx ctx{ivars->driver, this, arguments->selector, arguments, kIOReturnError};
    runSync(ivars->driverQueue, &ctx, runOnDriverQueue);
    return ctx.ret;
}

void
AVBI210UserClient::MarkRingsUsed()
{
    if (ivars != nullptr) {
        ivars->ringsUsed = true;
    }
}

bool
AVBI210UserClient::RingsUsed()
{
    return ivars != nullptr && ivars->ringsUsed;
}
