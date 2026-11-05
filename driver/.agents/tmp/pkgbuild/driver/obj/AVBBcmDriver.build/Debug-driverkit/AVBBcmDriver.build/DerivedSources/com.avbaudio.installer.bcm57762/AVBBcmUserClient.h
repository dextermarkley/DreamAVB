/* iig(DriverKit-440) generated from AVBBcmUserClient.iig */

/* AVBBcmUserClient.iig:1-10 */
// AVBBcmUserClient.iig - user client for avb-probe. ExternalMethod-only in
// Phase 0 (selectors in AVBShared.h); CopyClientMemoryForType shared rings
// are reserved for Phase 1 (selectors 16..31).

#ifndef AVBBcmUserClient_h
#define AVBBcmUserClient_h

#include <Availability.h>
#include <DriverKit/IOUserClient.h>  /* .iig include */

/* source class AVBBcmUserClient AVBBcmUserClient.iig:11-35 */

#if __DOCUMENTATION__
#define KERNEL IIG_KERNEL

class AVBBcmUserClient : public IOUserClient
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
    ExternalMethod(uint64_t selector,
                   IOUserClientMethodArguments * arguments,
                   const IOUserClientMethodDispatch * dispatch,
                   OSObject * target,
                   void * reference) override;

    // Runs the call on the driver's default queue (see AVBBcmUserClient.cpp).
    kern_return_t
    Forward(IOUserClientMethodArguments * arguments) LOCALONLY;
};

#undef KERNEL
#else /* __DOCUMENTATION__ */

/* generated class AVBBcmUserClient AVBBcmUserClient.iig:11-35 */


#define AVBBcmUserClient_Start_Args \
        IOService * provider

#define AVBBcmUserClient_Stop_Args \
        IOService * provider

#define AVBBcmUserClient_Methods \
\
public:\
\
    virtual kern_return_t\
    Dispatch(const IORPC rpc) APPLE_KEXT_OVERRIDE;\
\
    static kern_return_t\
    _Dispatch(AVBBcmUserClient * self, const IORPC rpc);\
\
    kern_return_t\
    Forward(\
        IOUserClientMethodArguments * arguments);\
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
\
public:\
    /* _Invoke methods */\
\


#define AVBBcmUserClient_KernelMethods \
\
protected:\
    /* _Impl methods */\
\


#define AVBBcmUserClient_VirtualMethods \
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
    virtual kern_return_t\
    ExternalMethod(\
        uint64_t selector,\
        IOUserClientMethodArguments * arguments,\
        const IOUserClientMethodDispatch * dispatch,\
        OSObject * target,\
        void * reference) APPLE_KEXT_OVERRIDE;\
\


#if !KERNEL

extern OSMetaClass          * gAVBBcmUserClientMetaClass;
extern const OSClassLoadInformation AVBBcmUserClient_Class;

class AVBBcmUserClientMetaClass : public OSMetaClass
{
public:
    virtual kern_return_t
    New(OSObject * instance) override;
    virtual kern_return_t
    Dispatch(const IORPC rpc) override;
};

#endif /* !KERNEL */

#if !KERNEL

class  AVBBcmUserClientInterface : public OSInterface
{
public:
};

struct AVBBcmUserClient_IVars;
struct AVBBcmUserClient_LocalIVars;

class AVBBcmUserClient : public IOUserClient, public AVBBcmUserClientInterface
{
#if !KERNEL
    friend class AVBBcmUserClientMetaClass;
#endif /* !KERNEL */

#if !KERNEL
public:
#ifdef AVBBcmUserClient_DECLARE_IVARS
AVBBcmUserClient_DECLARE_IVARS
#else /* AVBBcmUserClient_DECLARE_IVARS */
    union
    {
        AVBBcmUserClient_IVars * ivars;
        AVBBcmUserClient_LocalIVars * lvars;
    };
#endif /* AVBBcmUserClient_DECLARE_IVARS */
#endif /* !KERNEL */

#if !KERNEL
    static OSMetaClass *
    sGetMetaClass() { return gAVBBcmUserClientMetaClass; };
#endif /* KERNEL */

    using super = IOUserClient;

#if !KERNEL
    AVBBcmUserClient_Methods
    AVBBcmUserClient_VirtualMethods
#endif /* !KERNEL */

};
#endif /* !KERNEL */


#endif /* !__DOCUMENTATION__ */

/* AVBBcmUserClient.iig:37- */

#endif /* AVBBcmUserClient_h */
