/* iig(DriverKit-440 May 22 2026 10:59:06) generated from AVBBcmUserClient.iig */

#undef	IIG_IMPLEMENTATION
#define	IIG_IMPLEMENTATION 	AVBBcmUserClient.iig

#if KERNEL
#include <libkern/c++/OSString.h>
#else
#include <DriverKit/DriverKit.h>
#endif /* KERNEL */
#include <DriverKit/IOReturn.h>
#include "AVBBcmUserClient.h"


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
extern OSMetaClass * gIODispatchQueueMetaClass;
extern OSMetaClass * gOSStringMetaClass;
extern OSMetaClass * gIOServiceStateNotificationDispatchSourceMetaClass;
extern OSMetaClass * gIOMemoryMapMetaClass;
extern OSMetaClass * gOSAction_IOUserClient_KernelCompletionMetaClass;
#endif /* !KERNEL */

#if !KERNEL

#define AVBBcmUserClient_QueueNames  "" \
    "\037IOUserClientQueueExternalMethod"

#define AVBBcmUserClient_MethodNames  "" \
    "\017_ExternalMethod"

#define AVBBcmUserClientMetaClass_MethodNames  ""

struct OSClassDescription_AVBBcmUserClient_t
{
    OSClassDescription base;
    uint64_t           methodOptions[2 * 1];
    uint64_t           metaMethodOptions[2 * 0];
    char               queueNames[sizeof(AVBBcmUserClient_QueueNames)];
    char               methodNames[sizeof(AVBBcmUserClient_MethodNames)];
    char               metaMethodNames[sizeof(AVBBcmUserClientMetaClass_MethodNames)];
};

const struct OSClassDescription_AVBBcmUserClient_t
OSClassDescription_AVBBcmUserClient =
{
    .base =
    {
        .descriptionSize         = sizeof(OSClassDescription_AVBBcmUserClient_t),
        .name                    = "AVBBcmUserClient",
        .superName               = "IOUserClient",
        .methodOptionsSize       = 2 * sizeof(uint64_t) * 1,
        .methodOptionsOffset     = __builtin_offsetof(struct OSClassDescription_AVBBcmUserClient_t, methodOptions),
        .metaMethodOptionsSize   = 2 * sizeof(uint64_t) * 0,
        .metaMethodOptionsOffset = __builtin_offsetof(struct OSClassDescription_AVBBcmUserClient_t, metaMethodOptions),
        .queueNamesSize       = sizeof(AVBBcmUserClient_QueueNames),
        .queueNamesOffset     = __builtin_offsetof(struct OSClassDescription_AVBBcmUserClient_t, queueNames),
        .methodNamesSize         = sizeof(AVBBcmUserClient_MethodNames),
        .methodNamesOffset       = __builtin_offsetof(struct OSClassDescription_AVBBcmUserClient_t, methodNames),
        .metaMethodNamesSize     = sizeof(AVBBcmUserClientMetaClass_MethodNames),
        .metaMethodNamesOffset   = __builtin_offsetof(struct OSClassDescription_AVBBcmUserClient_t, metaMethodNames),
        .flags                   = 0*kOSClassCanRemote,
        .resv1                   = {0},
    },
    .methodOptions =
    {
        IOUserClient__ExternalMethod_ID,
        0x0000000000000000,
    },
    .metaMethodOptions =
    {
    },
    .queueNames      = AVBBcmUserClient_QueueNames,
    .methodNames     = AVBBcmUserClient_MethodNames,
    .metaMethodNames = AVBBcmUserClientMetaClass_MethodNames,
};

OSMetaClass * gAVBBcmUserClientMetaClass;

static kern_return_t
AVBBcmUserClient_New(OSMetaClass * instance);

const OSClassLoadInformation
AVBBcmUserClient_Class = 
{
    .description       = &OSClassDescription_AVBBcmUserClient.base,
    .metaPointer       = &gAVBBcmUserClientMetaClass,
    .version           = 1,
    .instanceSize      = sizeof(AVBBcmUserClient),

    .resv2             = {0},

    .New               = &AVBBcmUserClient_New,
    .resv3             = {0},

};

extern const void * const
gAVBBcmUserClient_Declaration;
const void * const
gAVBBcmUserClient_Declaration
__attribute__((used,visibility("hidden"),section("__DATA_CONST,__osclassinfo,regular,no_dead_strip"),no_sanitize("address")))
    = &AVBBcmUserClient_Class;

static kern_return_t
AVBBcmUserClient_New(OSMetaClass * instance)
{
    if (!new(instance) AVBBcmUserClientMetaClass) return (kIOReturnNoMemory);
    return (kIOReturnSuccess);
}

kern_return_t
AVBBcmUserClientMetaClass::New(OSObject * instance)
{
    if (!new(instance) AVBBcmUserClient) return (kIOReturnNoMemory);
    return (kIOReturnSuccess);
}

#endif /* !KERNEL */

#ifdef KERNEL
#define MESSAGE_CONTENT(__field) (messageContent->__field)
#else /* KERNEL */
#define MESSAGE_CONTENT(__field) (message->content.__field)
#endif /* KERNEL */

kern_return_t
AVBBcmUserClient::Dispatch(const IORPC rpc)
{
    return _Dispatch(this, rpc);
}

kern_return_t
AVBBcmUserClient::_Dispatch(AVBBcmUserClient * self, const IORPC rpc)
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
            ret = IOService::Start_Invoke(rpc, self, SimpleMemberFunctionCast(IOService::Start_Handler, *self, &AVBBcmUserClient::Start_Impl));
            break;
        }
        case IOService_Stop_ID:
        {
            ret = IOService::Stop_Invoke(rpc, self, SimpleMemberFunctionCast(IOService::Stop_Handler, *self, &AVBBcmUserClient::Stop_Impl));
            break;
        }

        default:
            ret = IOUserClient::_Dispatch(self, rpc);
            break;
    }

    return (ret);
}

#if KERNEL
kern_return_t
AVBBcmUserClient::MetaClass::Dispatch(const IORPC rpc)
{
#else /* KERNEL */
kern_return_t
AVBBcmUserClientMetaClass::Dispatch(const IORPC rpc)
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



