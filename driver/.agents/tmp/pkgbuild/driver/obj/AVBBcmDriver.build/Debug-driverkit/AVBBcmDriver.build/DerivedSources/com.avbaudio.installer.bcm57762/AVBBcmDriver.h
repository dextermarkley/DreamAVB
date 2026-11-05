/* iig(DriverKit-440) generated from AVBBcmDriver.iig */

/* AVBBcmDriver.iig:1-13 */
// AVBBcmDriver.iig - PCIDriverKit service for the Broadcom BCM57762 in the
// Apple Thunderbolt Gigabit Ethernet adapter (PCI 14e4:1682, subsystem
// 106b:00f6). Phase 0: EAV reference clock, RX/TX PTP timestamp latches and a
// minimal one-ring polled datapath, all driven through AVBBcmUserClient.

#ifndef AVBBcmDriver_h
#define AVBBcmDriver_h

#include <Availability.h>
#include <DriverKit/IOService.h>  /* .iig include */
#include <DriverKit/IOUserClient.h>  /* .iig include */
#include <DriverKit/IOTimerDispatchSource.h>  /* .iig include */

/* source class AVBBcmDriver AVBBcmDriver.iig:14-39 */

#if __DOCUMENTATION__
#define KERNEL IIG_KERNEL

class AVBBcmDriver : public IOService
{
public:
    virtual bool
    init() override;

    virtual void
    free() override;

    virtual kern_return_t
    Start(IOService * provider) override;

    virtual kern_return_t
    Stop(IOService * provider) override;

    virtual kern_return_t
    NewUserClient(uint32_t type, IOUserClient ** userClient) override;

    // Entry point for AVBBcmUserClient. Always called on this driver's
    // default queue, so it is serialized with TimerFired and Start/Stop.
    kern_return_t
    HandleUserMethod(uint64_t selector, IOUserClientMethodArguments * arguments) LOCALONLY;

    // 2 ms poll timer: harvests the RX return ring and the pending TX timestamp.
    virtual void
    TimerFired(OSAction * action, uint64_t time) TYPE(IOTimerDispatchSource::TimerOccurred);
};

#undef KERNEL
#else /* __DOCUMENTATION__ */

/* generated class AVBBcmDriver AVBBcmDriver.iig:14-39 */

#define AVBBcmDriver_TimerFired_ID            0x13bc66579159aa1eULL

#define AVBBcmDriver_Start_Args \
        IOService * provider

#define AVBBcmDriver_Stop_Args \
        IOService * provider

#define AVBBcmDriver_NewUserClient_Args \
        uint32_t type, \
        IOUserClient ** userClient

#define AVBBcmDriver_TimerFired_Args \
        OSAction * action, \
        uint64_t time

#define AVBBcmDriver_Methods \
\
public:\
\
    virtual kern_return_t\
    Dispatch(const IORPC rpc) APPLE_KEXT_OVERRIDE;\
\
    static kern_return_t\
    _Dispatch(AVBBcmDriver * self, const IORPC rpc);\
\
    kern_return_t\
    HandleUserMethod(\
        uint64_t selector,\
        IOUserClientMethodArguments * arguments);\
\
    kern_return_t\
    CreateActionTimerFired(size_t referenceSize, OSAction ** action);\
\
\
protected:\
    /* _Impl methods */\
\
    kern_return_t\
    Start_Impl(IOService_Start_Args);\
\
    kern_return_t\
    Stop_Impl(IOService_Stop_Args);\
\
    kern_return_t\
    NewUserClient_Impl(IOService_NewUserClient_Args);\
\
    void\
    TimerFired_Impl(AVBBcmDriver_TimerFired_Args);\
\
\
public:\
    /* _Invoke methods */\
\


#define AVBBcmDriver_KernelMethods \
\
protected:\
    /* _Impl methods */\
\


#define AVBBcmDriver_VirtualMethods \
\
public:\
\
    virtual bool\
    init(\
) APPLE_KEXT_OVERRIDE;\
\
    virtual void\
    free(\
) APPLE_KEXT_OVERRIDE;\
\


#if !KERNEL

extern OSMetaClass          * gAVBBcmDriverMetaClass;
extern const OSClassLoadInformation AVBBcmDriver_Class;

class AVBBcmDriverMetaClass : public OSMetaClass
{
public:
    virtual kern_return_t
    New(OSObject * instance) override;
    virtual kern_return_t
    Dispatch(const IORPC rpc) override;
};

#endif /* !KERNEL */

#if !KERNEL

class  AVBBcmDriverInterface : public OSInterface
{
public:
};

struct AVBBcmDriver_IVars;
struct AVBBcmDriver_LocalIVars;

class AVBBcmDriver : public IOService, public AVBBcmDriverInterface
{
#if !KERNEL
    friend class AVBBcmDriverMetaClass;
#endif /* !KERNEL */

#if !KERNEL
public:
#ifdef AVBBcmDriver_DECLARE_IVARS
AVBBcmDriver_DECLARE_IVARS
#else /* AVBBcmDriver_DECLARE_IVARS */
    union
    {
        AVBBcmDriver_IVars * ivars;
        AVBBcmDriver_LocalIVars * lvars;
    };
#endif /* AVBBcmDriver_DECLARE_IVARS */
#endif /* !KERNEL */

#if !KERNEL
    static OSMetaClass *
    sGetMetaClass() { return gAVBBcmDriverMetaClass; };
#endif /* KERNEL */

    using super = IOService;

#if !KERNEL
    AVBBcmDriver_Methods
    AVBBcmDriver_VirtualMethods
#endif /* !KERNEL */

};
#endif /* !KERNEL */


#define OSAction_AVBBcmDriver_TimerFired_Methods \
\
public:\
\
    virtual kern_return_t\
    Dispatch(const IORPC rpc) APPLE_KEXT_OVERRIDE;\
\
    static kern_return_t\
    _Dispatch(OSAction_AVBBcmDriver_TimerFired * self, const IORPC rpc);\
\
\
protected:\
    /* _Impl methods */\
\
\
public:\
    /* _Invoke methods */\
\


#define OSAction_AVBBcmDriver_TimerFired_KernelMethods \
\
protected:\
    /* _Impl methods */\
\


#define OSAction_AVBBcmDriver_TimerFired_VirtualMethods \
\
public:\
\


#if !KERNEL

extern OSMetaClass          * gOSAction_AVBBcmDriver_TimerFiredMetaClass;
extern const OSClassLoadInformation OSAction_AVBBcmDriver_TimerFired_Class;

class OSAction_AVBBcmDriver_TimerFiredMetaClass : public OSMetaClass
{
public:
    virtual kern_return_t
    New(OSObject * instance) override;
    virtual kern_return_t
    Dispatch(const IORPC rpc) override;
};

#endif /* !KERNEL */

class  __attribute__((availability(driverkit,introduced=20,message="Type-safe OSAction factory methods are available in DriverKit 20 and newer"))) OSAction_AVBBcmDriver_TimerFiredInterface : public OSInterface
{
public:
};

struct OSAction_AVBBcmDriver_TimerFired_IVars;
struct OSAction_AVBBcmDriver_TimerFired_LocalIVars;

class __attribute__((availability(driverkit,introduced=20,message="Type-safe OSAction factory methods are available in DriverKit 20 and newer"))) OSAction_AVBBcmDriver_TimerFired : public OSAction, public OSAction_AVBBcmDriver_TimerFiredInterface
{
#if KERNEL
    OSDeclareDefaultStructorsWithDispatch(OSAction_AVBBcmDriver_TimerFired);
#endif /* KERNEL */

#if !KERNEL
    friend class OSAction_AVBBcmDriver_TimerFiredMetaClass;
#endif /* !KERNEL */

public:
#ifdef OSAction_AVBBcmDriver_TimerFired_DECLARE_IVARS
OSAction_AVBBcmDriver_TimerFired_DECLARE_IVARS
#else /* OSAction_AVBBcmDriver_TimerFired_DECLARE_IVARS */
    union
    {
        OSAction_AVBBcmDriver_TimerFired_IVars * ivars;
        OSAction_AVBBcmDriver_TimerFired_LocalIVars * lvars;
    };
#endif /* OSAction_AVBBcmDriver_TimerFired_DECLARE_IVARS */
#if !KERNEL
    static OSMetaClass *
    sGetMetaClass() { return gOSAction_AVBBcmDriver_TimerFiredMetaClass; };
    virtual const OSMetaClass *
    getMetaClass() const APPLE_KEXT_OVERRIDE { return gOSAction_AVBBcmDriver_TimerFiredMetaClass; };
#endif /* KERNEL */

    using super = OSAction;

#if !KERNEL
    OSAction_AVBBcmDriver_TimerFired_Methods
#endif /* !KERNEL */

    OSAction_AVBBcmDriver_TimerFired_VirtualMethods
};

#endif /* !__DOCUMENTATION__ */

/* AVBBcmDriver.iig:41- */

#endif /* AVBBcmDriver_h */
