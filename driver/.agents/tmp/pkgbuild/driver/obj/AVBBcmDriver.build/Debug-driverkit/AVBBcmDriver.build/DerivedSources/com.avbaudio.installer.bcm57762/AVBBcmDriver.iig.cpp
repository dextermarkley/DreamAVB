/* iig(DriverKit-440 May 22 2026 10:59:06) generated from AVBBcmDriver.iig */

#undef	IIG_IMPLEMENTATION
#define	IIG_IMPLEMENTATION 	AVBBcmDriver.iig

#if KERNEL
#include <libkern/c++/OSString.h>
#else
#include <DriverKit/DriverKit.h>
#endif /* KERNEL */
#include <DriverKit/IOReturn.h>
#include "AVBBcmDriver.h"


#if __has_builtin(__builtin_load_member_function_pointer)
#define SimpleMemberFunctionCast(cfnty, self, func) (cfnty)__builtin_load_member_function_pointer(self, func)
#else
#define SimpleMemberFunctionCast(cfnty, self, func) ({ union { typeof(func) memfun; cfnty cfun; } pair; pair.memfun = func; pair.cfun; })
#endif


#if !KERNEL
extern OSMetaClass * gOSContainerMetaClass;
extern OSMetaClass * gOSDataMetaClass;
extern OSMetaClass * gOSNumberMetaClass;
extern OSMetaClass * gOSBooleanMetaClass;
extern OSMetaClass * gOSDictionaryMetaClass;
extern OSMetaClass * gOSArrayMetaClass;
extern OSMetaClass * gOSSetMetaClass;
extern OSMetaClass * gOSOrderedSetMetaClass;
extern OSMetaClass * gIOServiceStateNotificationDispatchSourceMetaClass;
extern OSMetaClass * gOSStringMetaClass;
extern OSMetaClass * gIOMemoryMapMetaClass;
extern OSMetaClass * gOSAction_IOUserClient_KernelCompletionMetaClass;
extern OSMetaClass * gOSAction_AVBBcmDriver_TimerFiredMetaClass;
#endif /* !KERNEL */

#if !KERNEL

#define AVBBcmDriver_QueueNames  ""

#define AVBBcmDriver_MethodNames  ""

#define AVBBcmDriverMetaClass_MethodNames  ""

struct OSClassDescription_AVBBcmDriver_t
{
    OSClassDescription base;
    uint64_t           methodOptions[2 * 0];
    uint64_t           metaMethodOptions[2 * 0];
    char               queueNames[sizeof(AVBBcmDriver_QueueNames)];
    char               methodNames[sizeof(AVBBcmDriver_MethodNames)];
    char               metaMethodNames[sizeof(AVBBcmDriverMetaClass_MethodNames)];
};

const struct OSClassDescription_AVBBcmDriver_t
OSClassDescription_AVBBcmDriver =
{
    .base =
    {
        .descriptionSize         = sizeof(OSClassDescription_AVBBcmDriver_t),
        .name                    = "AVBBcmDriver",
        .superName               = "IOService",
        .methodOptionsSize       = 2 * sizeof(uint64_t) * 0,
        .methodOptionsOffset     = __builtin_offsetof(struct OSClassDescription_AVBBcmDriver_t, methodOptions),
        .metaMethodOptionsSize   = 2 * sizeof(uint64_t) * 0,
        .metaMethodOptionsOffset = __builtin_offsetof(struct OSClassDescription_AVBBcmDriver_t, metaMethodOptions),
        .queueNamesSize       = sizeof(AVBBcmDriver_QueueNames),
        .queueNamesOffset     = __builtin_offsetof(struct OSClassDescription_AVBBcmDriver_t, queueNames),
        .methodNamesSize         = sizeof(AVBBcmDriver_MethodNames),
        .methodNamesOffset       = __builtin_offsetof(struct OSClassDescription_AVBBcmDriver_t, methodNames),
        .metaMethodNamesSize     = sizeof(AVBBcmDriverMetaClass_MethodNames),
        .metaMethodNamesOffset   = __builtin_offsetof(struct OSClassDescription_AVBBcmDriver_t, metaMethodNames),
        .flags                   = 0*kOSClassCanRemote,
        .resv1                   = {0},
    },
    .methodOptions =
    {
    },
    .metaMethodOptions =
    {
    },
    .queueNames      = AVBBcmDriver_QueueNames,
    .methodNames     = AVBBcmDriver_MethodNames,
    .metaMethodNames = AVBBcmDriverMetaClass_MethodNames,
};

OSMetaClass * gAVBBcmDriverMetaClass;

static kern_return_t
AVBBcmDriver_New(OSMetaClass * instance);

const OSClassLoadInformation
AVBBcmDriver_Class = 
{
    .description       = &OSClassDescription_AVBBcmDriver.base,
    .metaPointer       = &gAVBBcmDriverMetaClass,
    .version           = 1,
    .instanceSize      = sizeof(AVBBcmDriver),

    .resv2             = {0},

    .New               = &AVBBcmDriver_New,
    .resv3             = {0},

};

extern const void * const
gAVBBcmDriver_Declaration;
const void * const
gAVBBcmDriver_Declaration
__attribute__((used,visibility("hidden"),section("__DATA_CONST,__osclassinfo,regular,no_dead_strip"),no_sanitize("address")))
    = &AVBBcmDriver_Class;

static kern_return_t
AVBBcmDriver_New(OSMetaClass * instance)
{
    if (!new(instance) AVBBcmDriverMetaClass) return (kIOReturnNoMemory);
    return (kIOReturnSuccess);
}

kern_return_t
AVBBcmDriverMetaClass::New(OSObject * instance)
{
    if (!new(instance) AVBBcmDriver) return (kIOReturnNoMemory);
    return (kIOReturnSuccess);
}

#endif /* !KERNEL */

#ifdef KERNEL
#define MESSAGE_CONTENT(__field) (messageContent->__field)
#else /* KERNEL */
#define MESSAGE_CONTENT(__field) (message->content.__field)
#endif /* KERNEL */

kern_return_t
AVBBcmDriver::Dispatch(const IORPC rpc)
{
    return _Dispatch(this, rpc);
}

kern_return_t
AVBBcmDriver::_Dispatch(AVBBcmDriver * self, const IORPC rpc)
{
    kern_return_t ret = kIOReturnUnsupported;
#ifdef KERNEL
    IORPCMessage * msg = rpc.kernelContent;
#else /* KERNEL */
    IORPCMessage * msg = IORPCMessageFromMach(rpc.message, false);
#endif /* KERNEL */

    switch (msg->msgid)
    {
        case IOService_Start_ID:
        {
            ret = IOService::Start_Invoke(rpc, self, SimpleMemberFunctionCast(IOService::Start_Handler, *self, &AVBBcmDriver::Start_Impl));
            break;
        }
        case IOService_Stop_ID:
        {
            ret = IOService::Stop_Invoke(rpc, self, SimpleMemberFunctionCast(IOService::Stop_Handler, *self, &AVBBcmDriver::Stop_Impl));
            break;
        }
        case IOService_NewUserClient_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOService::NewUserClient_Invoke(rpc, self, SimpleMemberFunctionCast(IOService::NewUserClient_Handler, *self, &AVBBcmDriver::NewUserClient_Impl));
            break;
        }
        case AVBBcmDriver_TimerFired_ID:
#if !KERNEL
        if (self->IsRemote())
        {
            ret = self->OSMetaClassBase::Dispatch(rpc);
            break;
        }
        else
#endif /* !KERNEL */
        {
            ret = IOTimerDispatchSource::TimerOccurred_Invoke(rpc, self, SimpleMemberFunctionCast(IOTimerDispatchSource::TimerOccurred_Handler, *self, &AVBBcmDriver::TimerFired_Impl), OSTypeID(OSAction_AVBBcmDriver_TimerFired));
            break;
        }

        default:
            ret = IOService::_Dispatch(self, rpc);
            break;
    }

    return (ret);
}

#if KERNEL
kern_return_t
AVBBcmDriver::MetaClass::Dispatch(const IORPC rpc)
{
#else /* KERNEL */
kern_return_t
AVBBcmDriverMetaClass::Dispatch(const IORPC rpc)
{
#endif /* !KERNEL */

    kern_return_t ret = kIOReturnUnsupported;
#ifdef KERNEL
    IORPCMessage * msg = rpc.kernelContent;
#else /* KERNEL */
    IORPCMessage * msg = IORPCMessageFromMach(rpc.message, false);
#endif /* KERNEL */

    switch (msg->msgid)
    {

        default:
            ret = OSMetaClassBase::Dispatch(rpc);
            break;
    }

    return (ret);
}

kern_return_t
AVBBcmDriver::CreateActionTimerFired(size_t referenceSize, OSAction ** action)
{
    kern_return_t ret;

#if defined(IOKIT_ENABLE_SHARED_PTR)
    OSSharedPtr<OSString>
#else /* defined(IOKIT_ENABLE_SHARED_PTR) */
    OSString *
#endif /* !defined(IOKIT_ENABLE_SHARED_PTR) */
    typeName = OSString::withCString("OSAction_AVBBcmDriver_TimerFired");
    if (!typeName) {
        return kIOReturnNoMemory;
    }
    ret = OSAction_AVBBcmDriver_TimerFired::CreateWithTypeName(this,
                           AVBBcmDriver_TimerFired_ID,
                           IOTimerDispatchSource_TimerOccurred_ID,
                           referenceSize,
#if defined(IOKIT_ENABLE_SHARED_PTR)
                           typeName.get(),
#else /* defined(IOKIT_ENABLE_SHARED_PTR) */
                           typeName,
#endif /* !defined(IOKIT_ENABLE_SHARED_PTR) */
                           action);

#if !defined(IOKIT_ENABLE_SHARED_PTR)
    typeName->release();
#endif /* !defined(IOKIT_ENABLE_SHARED_PTR) */
    return (ret);
}

#if KERNEL
OSDefineMetaClassAndStructors(OSAction_AVBBcmDriver_TimerFired, OSAction);
#endif /* KERNEL */

#if !KERNEL

#define OSAction_AVBBcmDriver_TimerFired_QueueNames  ""

#define OSAction_AVBBcmDriver_TimerFired_MethodNames  ""

#define OSAction_AVBBcmDriver_TimerFiredMetaClass_MethodNames  ""

struct OSClassDescription_OSAction_AVBBcmDriver_TimerFired_t
{
    OSClassDescription base;
    uint64_t           methodOptions[2 * 0];
    uint64_t           metaMethodOptions[2 * 0];
    char               queueNames[sizeof(OSAction_AVBBcmDriver_TimerFired_QueueNames)];
    char               methodNames[sizeof(OSAction_AVBBcmDriver_TimerFired_MethodNames)];
    char               metaMethodNames[sizeof(OSAction_AVBBcmDriver_TimerFiredMetaClass_MethodNames)];
};

 __attribute__((availability(driverkit,introduced=20,message="Type-safe OSAction factory methods are available in DriverKit 20 and newer")))
const struct OSClassDescription_OSAction_AVBBcmDriver_TimerFired_t
OSClassDescription_OSAction_AVBBcmDriver_TimerFired =
{
    .base =
    {
        .descriptionSize         = sizeof(OSClassDescription_OSAction_AVBBcmDriver_TimerFired_t),
        .name                    = "OSAction_AVBBcmDriver_TimerFired",
        .superName               = "OSAction",
        .methodOptionsSize       = 2 * sizeof(uint64_t) * 0,
        .methodOptionsOffset     = __builtin_offsetof(struct OSClassDescription_OSAction_AVBBcmDriver_TimerFired_t, methodOptions),
        .metaMethodOptionsSize   = 2 * sizeof(uint64_t) * 0,
        .metaMethodOptionsOffset = __builtin_offsetof(struct OSClassDescription_OSAction_AVBBcmDriver_TimerFired_t, metaMethodOptions),
        .queueNamesSize       = sizeof(OSAction_AVBBcmDriver_TimerFired_QueueNames),
        .queueNamesOffset     = __builtin_offsetof(struct OSClassDescription_OSAction_AVBBcmDriver_TimerFired_t, queueNames),
        .methodNamesSize         = sizeof(OSAction_AVBBcmDriver_TimerFired_MethodNames),
        .methodNamesOffset       = __builtin_offsetof(struct OSClassDescription_OSAction_AVBBcmDriver_TimerFired_t, methodNames),
        .metaMethodNamesSize     = sizeof(OSAction_AVBBcmDriver_TimerFiredMetaClass_MethodNames),
        .metaMethodNamesOffset   = __builtin_offsetof(struct OSClassDescription_OSAction_AVBBcmDriver_TimerFired_t, metaMethodNames),
        .flags                   = 0*kOSClassCanRemote,
        .resv1                   = {0},
    },
    .methodOptions =
    {
    },
    .metaMethodOptions =
    {
    },
    .queueNames      = OSAction_AVBBcmDriver_TimerFired_QueueNames,
    .methodNames     = OSAction_AVBBcmDriver_TimerFired_MethodNames,
    .metaMethodNames = OSAction_AVBBcmDriver_TimerFiredMetaClass_MethodNames,
};

 __attribute__((availability(driverkit,introduced=20,message="Type-safe OSAction factory methods are available in DriverKit 20 and newer")))
OSMetaClass * gOSAction_AVBBcmDriver_TimerFiredMetaClass;

 __attribute__((availability(driverkit,introduced=20,message="Type-safe OSAction factory methods are available in DriverKit 20 and newer")))
static kern_return_t
OSAction_AVBBcmDriver_TimerFired_New(OSMetaClass * instance);

 __attribute__((availability(driverkit,introduced=20,message="Type-safe OSAction factory methods are available in DriverKit 20 and newer")))
const OSClassLoadInformation
OSAction_AVBBcmDriver_TimerFired_Class = 
{
    .description       = &OSClassDescription_OSAction_AVBBcmDriver_TimerFired.base,
    .metaPointer       = &gOSAction_AVBBcmDriver_TimerFiredMetaClass,
    .version           = 1,
    .instanceSize      = sizeof(OSAction_AVBBcmDriver_TimerFired),

    .resv2             = {0},

    .New               = &OSAction_AVBBcmDriver_TimerFired_New,
    .resv3             = {0},

};

 __attribute__((availability(driverkit,introduced=20,message="Type-safe OSAction factory methods are available in DriverKit 20 and newer")))
extern const void * const
gOSAction_AVBBcmDriver_TimerFired_Declaration;
 __attribute__((availability(driverkit,introduced=20,message="Type-safe OSAction factory methods are available in DriverKit 20 and newer")))
const void * const
gOSAction_AVBBcmDriver_TimerFired_Declaration
 __attribute__((availability(driverkit,introduced=20,message="Type-safe OSAction factory methods are available in DriverKit 20 and newer")))
__attribute__((used,visibility("hidden"),section("__DATA_CONST,__osclassinfo,regular,no_dead_strip"),no_sanitize("address")))
    = &OSAction_AVBBcmDriver_TimerFired_Class;

 __attribute__((availability(driverkit,introduced=20,message="Type-safe OSAction factory methods are available in DriverKit 20 and newer")))
static kern_return_t
OSAction_AVBBcmDriver_TimerFired_New(OSMetaClass * instance)
{
    if (!new(instance) OSAction_AVBBcmDriver_TimerFiredMetaClass) return (kIOReturnNoMemory);
    return (kIOReturnSuccess);
}

 __attribute__((availability(driverkit,introduced=20,message="Type-safe OSAction factory methods are available in DriverKit 20 and newer")))
kern_return_t
OSAction_AVBBcmDriver_TimerFiredMetaClass::New(OSObject * instance)
{
    if (!new(instance) OSAction_AVBBcmDriver_TimerFired) return (kIOReturnNoMemory);
    return (kIOReturnSuccess);
}

#endif /* !KERNEL */

#ifdef KERNEL
#define MESSAGE_CONTENT(__field) (messageContent->__field)
#else /* KERNEL */
#define MESSAGE_CONTENT(__field) (message->content.__field)
#endif /* KERNEL */

kern_return_t
OSAction_AVBBcmDriver_TimerFired::Dispatch(const IORPC rpc)
{
    return _Dispatch(this, rpc);
}

kern_return_t
OSAction_AVBBcmDriver_TimerFired::_Dispatch(OSAction_AVBBcmDriver_TimerFired * self, const IORPC rpc)
{
    kern_return_t ret = kIOReturnUnsupported;
#ifdef KERNEL
    IORPCMessage * msg = rpc.kernelContent;
#else /* KERNEL */
    IORPCMessage * msg = IORPCMessageFromMach(rpc.message, false);
#endif /* KERNEL */

    switch (msg->msgid)
    {

        default:
            ret = OSAction::_Dispatch(self, rpc);
            break;
    }

    return (ret);
}

#if KERNEL
kern_return_t
OSAction_AVBBcmDriver_TimerFired::MetaClass::Dispatch(const IORPC rpc)
{
#else /* KERNEL */
kern_return_t
OSAction_AVBBcmDriver_TimerFiredMetaClass::Dispatch(const IORPC rpc)
{
#endif /* !KERNEL */

    kern_return_t ret = kIOReturnUnsupported;
#ifdef KERNEL
    IORPCMessage * msg = rpc.kernelContent;
#else /* KERNEL */
    IORPCMessage * msg = IORPCMessageFromMach(rpc.message, false);
#endif /* KERNEL */

    switch (msg->msgid)
    {

        default:
            ret = OSMetaClassBase::Dispatch(rpc);
            break;
    }

    return (ret);
}



