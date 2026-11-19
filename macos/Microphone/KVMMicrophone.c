// KVM Microphone: a Core Audio driver (AudioServerPlugIn) with two devices sharing one
// ring buffer and one clock.
//   "KVM Microphone"       a visible input device, for calls on this Mac;
//   "KVM Microphone Feed"  a hidden output device: the KVM app plays the controlling
//                          device's microphone into it.
// What is written to the feed at sample time t is read by the input at the same t; the
// HAL schedules output ahead of input, so the input hears the feed about two IO cycles
// later. Reads clear what they took, so the input is silent once the feed stops.
// Installed in /Library/Audio/Plug-Ins/HAL by the app (see Microphone.swift); built by
// macos/build.sh. 48 kHz, 2 channels of 32-bit float, interleaved.

#include <CoreAudio/AudioServerPlugIn.h>
#include <mach/mach_time.h>
#include <os/lock.h>
#include <stdatomic.h>
#include <string.h>

#pragma mark Objects and format

enum {
    kObjectID_PlugIn = kAudioObjectPlugInObject,
    kObjectID_Input = 2,
    kObjectID_InputStream = 3,
    kObjectID_Feed = 4,
    kObjectID_FeedStream = 5,
};

#define kSampleRate 48000.0
#define kChannels 2
#define kBytesPerFrame (kChannels * sizeof(Float32))
#define kRingFrames 16384 // a power of two; also the zero time stamp period
#define kManufacturer CFSTR("KVM")
#define kInputName CFSTR("KVM Microphone")
#define kFeedName CFSTR("KVM Microphone Feed")
#define kInputUID CFSTR("KVMMicrophone_UID")
#define kFeedUID CFSTR("KVMMicrophoneFeed_UID") // the app looks the feed up by this
#define kModelUID CFSTR("KVMMicrophone_ModelUID")

static AudioServerPlugInHostRef gHost = NULL;
static _Atomic UInt32 gRefCount = 0;

// Shared by both devices: the clock runs while either does IO.
static os_unfair_lock gLock = OS_UNFAIR_LOCK_INIT;
static UInt32 gIOClients = 0;
static UInt32 gRunning[2] = {0, 0}; // per device: input, feed
static Float64 gHostTicksPerFrame = 0;
static UInt64 gAnchorHostTime = 0;
static UInt64 gTimeStampCount = 0;
static Float32 gRing[kRingFrames * kChannels];

static bool IsDevice(AudioObjectID id) { return id == kObjectID_Input || id == kObjectID_Feed; }
static bool IsStream(AudioObjectID id) { return id == kObjectID_InputStream || id == kObjectID_FeedStream; }
static bool IsInput(AudioObjectID id) { return id == kObjectID_Input || id == kObjectID_InputStream; }
static AudioObjectID StreamOf(AudioObjectID device) { return device == kObjectID_Input ? kObjectID_InputStream : kObjectID_FeedStream; }
static AudioObjectID DeviceOf(AudioObjectID stream) { return stream == kObjectID_InputStream ? kObjectID_Input : kObjectID_Feed; }

static AudioStreamBasicDescription Format(void) {
    AudioStreamBasicDescription f = {0};
    f.mSampleRate = kSampleRate;
    f.mFormatID = kAudioFormatLinearPCM;
    f.mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagsNativeEndian | kAudioFormatFlagIsPacked;
    f.mBytesPerPacket = kBytesPerFrame;
    f.mFramesPerPacket = 1;
    f.mBytesPerFrame = kBytesPerFrame;
    f.mChannelsPerFrame = kChannels;
    f.mBitsPerChannel = 32;
    return f;
}

#pragma mark Interface

static HRESULT QueryInterface(void *driver, REFIID iid, LPVOID *out);
static ULONG AddRef(void *driver);
static ULONG Release(void *driver);
static OSStatus Initialize(AudioServerPlugInDriverRef driver, AudioServerPlugInHostRef host);
static OSStatus CreateDevice(AudioServerPlugInDriverRef d, CFDictionaryRef desc, const AudioServerPlugInClientInfo *client, AudioObjectID *out);
static OSStatus DestroyDevice(AudioServerPlugInDriverRef d, AudioObjectID device);
static OSStatus AddDeviceClient(AudioServerPlugInDriverRef d, AudioObjectID device, const AudioServerPlugInClientInfo *client);
static OSStatus RemoveDeviceClient(AudioServerPlugInDriverRef d, AudioObjectID device, const AudioServerPlugInClientInfo *client);
static OSStatus PerformDeviceConfigurationChange(AudioServerPlugInDriverRef d, AudioObjectID device, UInt64 action, void *info);
static OSStatus AbortDeviceConfigurationChange(AudioServerPlugInDriverRef d, AudioObjectID device, UInt64 action, void *info);
static Boolean HasProperty(AudioServerPlugInDriverRef d, AudioObjectID obj, pid_t pid, const AudioObjectPropertyAddress *addr);
static OSStatus IsPropertySettable(AudioServerPlugInDriverRef d, AudioObjectID obj, pid_t pid, const AudioObjectPropertyAddress *addr, Boolean *out);
static OSStatus GetPropertyDataSize(AudioServerPlugInDriverRef d, AudioObjectID obj, pid_t pid, const AudioObjectPropertyAddress *addr, UInt32 qualSize, const void *qual, UInt32 *out);
static OSStatus GetPropertyData(AudioServerPlugInDriverRef d, AudioObjectID obj, pid_t pid, const AudioObjectPropertyAddress *addr, UInt32 qualSize, const void *qual, UInt32 inSize, UInt32 *outSize, void *out);
static OSStatus SetPropertyData(AudioServerPlugInDriverRef d, AudioObjectID obj, pid_t pid, const AudioObjectPropertyAddress *addr, UInt32 qualSize, const void *qual, UInt32 inSize, const void *data);
static OSStatus StartIO(AudioServerPlugInDriverRef d, AudioObjectID device, UInt32 client);
static OSStatus StopIO(AudioServerPlugInDriverRef d, AudioObjectID device, UInt32 client);
static OSStatus GetZeroTimeStamp(AudioServerPlugInDriverRef d, AudioObjectID device, UInt32 client, Float64 *sampleTime, UInt64 *hostTime, UInt64 *seed);
static OSStatus WillDoIOOperation(AudioServerPlugInDriverRef d, AudioObjectID device, UInt32 client, UInt32 op, Boolean *will, Boolean *inPlace);
static OSStatus BeginIOOperation(AudioServerPlugInDriverRef d, AudioObjectID device, UInt32 client, UInt32 op, UInt32 frames, const AudioServerPlugInIOCycleInfo *cycle);
static OSStatus DoIOOperation(AudioServerPlugInDriverRef d, AudioObjectID device, AudioObjectID stream, UInt32 client, UInt32 op, UInt32 frames, const AudioServerPlugInIOCycleInfo *cycle, void *main, void *secondary);
static OSStatus EndIOOperation(AudioServerPlugInDriverRef d, AudioObjectID device, UInt32 client, UInt32 op, UInt32 frames, const AudioServerPlugInIOCycleInfo *cycle);

static AudioServerPlugInDriverInterface gInterface = {
    NULL, QueryInterface, AddRef, Release, Initialize, CreateDevice, DestroyDevice,
    AddDeviceClient, RemoveDeviceClient, PerformDeviceConfigurationChange, AbortDeviceConfigurationChange,
    HasProperty, IsPropertySettable, GetPropertyDataSize, GetPropertyData, SetPropertyData,
    StartIO, StopIO, GetZeroTimeStamp, WillDoIOOperation, BeginIOOperation, DoIOOperation, EndIOOperation,
};
static AudioServerPlugInDriverInterface *gInterfacePtr = &gInterface;
static AudioServerPlugInDriverRef gDriver = &gInterfacePtr;

// The factory named in Info.plist.
void *KVMMicrophone_Create(CFAllocatorRef allocator, CFUUIDRef type);
void *KVMMicrophone_Create(CFAllocatorRef allocator, CFUUIDRef type) {
    (void)allocator;
    return CFEqual(type, kAudioServerPlugInTypeUUID) ? gDriver : NULL;
}

static HRESULT QueryInterface(void *driver, REFIID iid, LPVOID *out) {
    if (driver != gDriver || out == NULL) return kAudioHardwareBadObjectError;
    CFUUIDRef requested = CFUUIDCreateFromUUIDBytes(NULL, iid);
    if (requested == NULL) return kAudioHardwareIllegalOperationError;
    bool ok = CFEqual(requested, IUnknownUUID) || CFEqual(requested, kAudioServerPlugInDriverInterfaceUUID);
    CFRelease(requested);
    if (!ok) return E_NOINTERFACE;
    atomic_fetch_add(&gRefCount, 1);
    *out = gDriver;
    return S_OK;
}

static ULONG AddRef(void *driver) {
    if (driver != gDriver) return 0;
    return atomic_fetch_add(&gRefCount, 1) + 1;
}

static ULONG Release(void *driver) {
    if (driver != gDriver) return 0;
    UInt32 count = atomic_load(&gRefCount);
    if (count > 0) count = atomic_fetch_sub(&gRefCount, 1) - 1;
    return count;
}

static OSStatus Initialize(AudioServerPlugInDriverRef driver, AudioServerPlugInHostRef host) {
    if (driver != gDriver) return kAudioHardwareBadObjectError;
    gHost = host;
    mach_timebase_info_data_t timebase;
    mach_timebase_info(&timebase);
    Float64 hostClockFrequency = (Float64)timebase.denom / (Float64)timebase.numer * 1000000000.0;
    gHostTicksPerFrame = hostClockFrequency / kSampleRate;
    return 0;
}

static OSStatus CreateDevice(AudioServerPlugInDriverRef d, CFDictionaryRef desc, const AudioServerPlugInClientInfo *client, AudioObjectID *out) {
    (void)d; (void)desc; (void)client; (void)out;
    return kAudioHardwareUnsupportedOperationError;
}

static OSStatus DestroyDevice(AudioServerPlugInDriverRef d, AudioObjectID device) {
    (void)d; (void)device;
    return kAudioHardwareUnsupportedOperationError;
}

static OSStatus AddDeviceClient(AudioServerPlugInDriverRef d, AudioObjectID device, const AudioServerPlugInClientInfo *client) {
    (void)client;
    return d == gDriver && IsDevice(device) ? 0 : kAudioHardwareBadObjectError;
}

static OSStatus RemoveDeviceClient(AudioServerPlugInDriverRef d, AudioObjectID device, const AudioServerPlugInClientInfo *client) {
    (void)client;
    return d == gDriver && IsDevice(device) ? 0 : kAudioHardwareBadObjectError;
}

static OSStatus PerformDeviceConfigurationChange(AudioServerPlugInDriverRef d, AudioObjectID device, UInt64 action, void *info) {
    (void)d; (void)device; (void)action; (void)info;
    return 0;
}

static OSStatus AbortDeviceConfigurationChange(AudioServerPlugInDriverRef d, AudioObjectID device, UInt64 action, void *info) {
    (void)d; (void)device; (void)action; (void)info;
    return 0;
}

#pragma mark Properties

// Writes one value of type T into the caller's buffer, checking its size.
#define RETURN_VALUE(T, value) do { \
        if (inSize < sizeof(T)) return kAudioHardwareBadPropertySizeError; \
        *(T *)out = (value); *outSize = sizeof(T); return 0; \
    } while (0)

// Copies up to `count` object IDs that fit in the caller's buffer.
static OSStatus ReturnIDs(const AudioObjectID *ids, UInt32 count, UInt32 inSize, UInt32 *outSize, void *out) {
    UInt32 n = inSize / sizeof(AudioObjectID);
    if (n > count) n = count;
    memcpy(out, ids, n * sizeof(AudioObjectID));
    *outSize = n * sizeof(AudioObjectID);
    return 0;
}

static Boolean HasProperty(AudioServerPlugInDriverRef d, AudioObjectID obj, pid_t pid, const AudioObjectPropertyAddress *addr) {
    UInt32 size = 0;
    return d == gDriver && addr != NULL && GetPropertyDataSize(d, obj, pid, addr, 0, NULL, &size) == 0;
}

static OSStatus IsPropertySettable(AudioServerPlugInDriverRef d, AudioObjectID obj, pid_t pid, const AudioObjectPropertyAddress *addr, Boolean *out) {
    if (!HasProperty(d, obj, pid, addr) || out == NULL) return kAudioHardwareUnknownPropertyError;
    *out = false; // one fixed format and rate: nothing to change
    return 0;
}

static OSStatus GetPropertyDataSize(AudioServerPlugInDriverRef d, AudioObjectID obj, pid_t pid, const AudioObjectPropertyAddress *addr, UInt32 qualSize, const void *qual, UInt32 *out) {
    (void)pid; (void)qualSize; (void)qual;
    if (d != gDriver || addr == NULL || out == NULL) return kAudioHardwareBadObjectError;
    AudioObjectPropertySelector s = addr->mSelector;
    if (obj == kObjectID_PlugIn) {
        switch (s) {
            case kAudioObjectPropertyBaseClass:
            case kAudioObjectPropertyClass: *out = sizeof(AudioClassID); return 0;
            case kAudioObjectPropertyOwner: *out = sizeof(AudioObjectID); return 0;
            case kAudioObjectPropertyManufacturer:
            case kAudioPlugInPropertyResourceBundle: *out = sizeof(CFStringRef); return 0;
            case kAudioObjectPropertyOwnedObjects:
            case kAudioPlugInPropertyDeviceList: *out = 2 * sizeof(AudioObjectID); return 0;
            case kAudioPlugInPropertyTranslateUIDToDevice: *out = sizeof(AudioObjectID); return 0;
            default: return kAudioHardwareUnknownPropertyError;
        }
    }
    if (IsDevice(obj)) {
        bool input = IsInput(obj);
        switch (s) {
            case kAudioObjectPropertyBaseClass:
            case kAudioObjectPropertyClass: *out = sizeof(AudioClassID); return 0;
            case kAudioObjectPropertyOwner: *out = sizeof(AudioObjectID); return 0;
            case kAudioObjectPropertyName:
            case kAudioObjectPropertyManufacturer:
            case kAudioDevicePropertyDeviceUID:
            case kAudioDevicePropertyModelUID: *out = sizeof(CFStringRef); return 0;
            case kAudioObjectPropertyOwnedObjects:
            case kAudioDevicePropertyStreams: {
                bool matches = addr->mScope == kAudioObjectPropertyScopeGlobal
                    || addr->mScope == (input ? kAudioObjectPropertyScopeInput : kAudioObjectPropertyScopeOutput);
                *out = matches ? sizeof(AudioObjectID) : 0;
                return 0;
            }
            case kAudioObjectPropertyControlList: *out = 0; return 0;
            case kAudioDevicePropertyTransportType:
            case kAudioDevicePropertyClockDomain:
            case kAudioDevicePropertyDeviceIsAlive:
            case kAudioDevicePropertyDeviceIsRunning:
            case kAudioDevicePropertyDeviceCanBeDefaultDevice:
            case kAudioDevicePropertyDeviceCanBeDefaultSystemDevice:
            case kAudioDevicePropertyLatency:
            case kAudioDevicePropertySafetyOffset:
            case kAudioDevicePropertyIsHidden:
            case kAudioDevicePropertyZeroTimeStampPeriod:
            case kAudioDevicePropertyClockIsStable: *out = sizeof(UInt32); return 0;
            case kAudioDevicePropertyRelatedDevices: *out = sizeof(AudioObjectID); return 0;
            case kAudioDevicePropertyNominalSampleRate: *out = sizeof(Float64); return 0;
            case kAudioDevicePropertyAvailableNominalSampleRates: *out = sizeof(AudioValueRange); return 0;
            case kAudioDevicePropertyPreferredChannelsForStereo: *out = 2 * sizeof(UInt32); return 0;
            default: return kAudioHardwareUnknownPropertyError;
        }
    }
    if (IsStream(obj)) {
        switch (s) {
            case kAudioObjectPropertyBaseClass:
            case kAudioObjectPropertyClass: *out = sizeof(AudioClassID); return 0;
            case kAudioObjectPropertyOwner: *out = sizeof(AudioObjectID); return 0;
            case kAudioObjectPropertyOwnedObjects: *out = 0; return 0;
            case kAudioStreamPropertyIsActive:
            case kAudioStreamPropertyDirection:
            case kAudioStreamPropertyTerminalType:
            case kAudioStreamPropertyStartingChannel:
            case kAudioStreamPropertyLatency: *out = sizeof(UInt32); return 0;
            case kAudioStreamPropertyVirtualFormat:
            case kAudioStreamPropertyPhysicalFormat: *out = sizeof(AudioStreamBasicDescription); return 0;
            case kAudioStreamPropertyAvailableVirtualFormats:
            case kAudioStreamPropertyAvailablePhysicalFormats: *out = sizeof(AudioStreamRangedDescription); return 0;
            default: return kAudioHardwareUnknownPropertyError;
        }
    }
    return kAudioHardwareBadObjectError;
}

static OSStatus GetPropertyData(AudioServerPlugInDriverRef d, AudioObjectID obj, pid_t pid, const AudioObjectPropertyAddress *addr, UInt32 qualSize, const void *qual, UInt32 inSize, UInt32 *outSize, void *out) {
    (void)pid;
    if (d != gDriver || addr == NULL || outSize == NULL || out == NULL) return kAudioHardwareBadObjectError;
    AudioObjectPropertySelector s = addr->mSelector;

    if (obj == kObjectID_PlugIn) {
        switch (s) {
            case kAudioObjectPropertyBaseClass: RETURN_VALUE(AudioClassID, kAudioObjectClassID);
            case kAudioObjectPropertyClass: RETURN_VALUE(AudioClassID, kAudioPlugInClassID);
            case kAudioObjectPropertyOwner: RETURN_VALUE(AudioObjectID, kAudioObjectUnknown);
            case kAudioObjectPropertyManufacturer: RETURN_VALUE(CFStringRef, kManufacturer);
            case kAudioPlugInPropertyResourceBundle: RETURN_VALUE(CFStringRef, CFSTR(""));
            case kAudioObjectPropertyOwnedObjects:
            case kAudioPlugInPropertyDeviceList: {
                AudioObjectID ids[] = {kObjectID_Input, kObjectID_Feed};
                return ReturnIDs(ids, 2, inSize, outSize, out);
            }
            case kAudioPlugInPropertyTranslateUIDToDevice: {
                if (qualSize < sizeof(CFStringRef) || qual == NULL) return kAudioHardwareBadPropertySizeError;
                CFStringRef uid = *(const CFStringRef *)qual;
                AudioObjectID found = kAudioObjectUnknown;
                if (uid != NULL && CFStringCompare(uid, kInputUID, 0) == kCFCompareEqualTo) found = kObjectID_Input;
                if (uid != NULL && CFStringCompare(uid, kFeedUID, 0) == kCFCompareEqualTo) found = kObjectID_Feed;
                RETURN_VALUE(AudioObjectID, found);
            }
            default: return kAudioHardwareUnknownPropertyError;
        }
    }

    if (IsDevice(obj)) {
        bool input = IsInput(obj);
        switch (s) {
            case kAudioObjectPropertyBaseClass: RETURN_VALUE(AudioClassID, kAudioObjectClassID);
            case kAudioObjectPropertyClass: RETURN_VALUE(AudioClassID, kAudioDeviceClassID);
            case kAudioObjectPropertyOwner: RETURN_VALUE(AudioObjectID, kObjectID_PlugIn);
            case kAudioObjectPropertyName: RETURN_VALUE(CFStringRef, input ? kInputName : kFeedName);
            case kAudioObjectPropertyManufacturer: RETURN_VALUE(CFStringRef, kManufacturer);
            case kAudioDevicePropertyDeviceUID: RETURN_VALUE(CFStringRef, input ? kInputUID : kFeedUID);
            case kAudioDevicePropertyModelUID: RETURN_VALUE(CFStringRef, kModelUID);
            case kAudioObjectPropertyOwnedObjects:
            case kAudioDevicePropertyStreams: {
                bool matches = addr->mScope == kAudioObjectPropertyScopeGlobal
                    || addr->mScope == (input ? kAudioObjectPropertyScopeInput : kAudioObjectPropertyScopeOutput);
                AudioObjectID ids[] = {StreamOf(obj)};
                return ReturnIDs(ids, matches ? 1 : 0, inSize, outSize, out);
            }
            case kAudioObjectPropertyControlList: *outSize = 0; return 0;
            case kAudioDevicePropertyTransportType: RETURN_VALUE(UInt32, kAudioDeviceTransportTypeVirtual);
            case kAudioDevicePropertyRelatedDevices: RETURN_VALUE(AudioObjectID, obj);
            case kAudioDevicePropertyClockDomain: RETURN_VALUE(UInt32, 0);
            case kAudioDevicePropertyDeviceIsAlive: RETURN_VALUE(UInt32, 1);
            case kAudioDevicePropertyDeviceIsRunning: {
                os_unfair_lock_lock(&gLock);
                UInt32 running = gRunning[input ? 0 : 1] > 0;
                os_unfair_lock_unlock(&gLock);
                RETURN_VALUE(UInt32, running);
            }
            // The input may be picked as the default microphone; the feed is only for KVM.
            case kAudioDevicePropertyDeviceCanBeDefaultDevice: RETURN_VALUE(UInt32, input ? 1 : 0);
            case kAudioDevicePropertyDeviceCanBeDefaultSystemDevice: RETURN_VALUE(UInt32, 0);
            case kAudioDevicePropertyLatency: RETURN_VALUE(UInt32, 0);
            case kAudioDevicePropertySafetyOffset: RETURN_VALUE(UInt32, 0);
            case kAudioDevicePropertyIsHidden: RETURN_VALUE(UInt32, input ? 0 : 1);
            case kAudioDevicePropertyZeroTimeStampPeriod: RETURN_VALUE(UInt32, kRingFrames);
            case kAudioDevicePropertyClockIsStable: RETURN_VALUE(UInt32, 1);
            case kAudioDevicePropertyNominalSampleRate: RETURN_VALUE(Float64, kSampleRate);
            case kAudioDevicePropertyAvailableNominalSampleRates: {
                AudioValueRange range = {kSampleRate, kSampleRate};
                RETURN_VALUE(AudioValueRange, range);
            }
            case kAudioDevicePropertyPreferredChannelsForStereo: {
                if (inSize < 2 * sizeof(UInt32)) return kAudioHardwareBadPropertySizeError;
                ((UInt32 *)out)[0] = 1;
                ((UInt32 *)out)[1] = 2;
                *outSize = 2 * sizeof(UInt32);
                return 0;
            }
            default: return kAudioHardwareUnknownPropertyError;
        }
    }

    if (IsStream(obj)) {
        bool input = IsInput(obj);
        switch (s) {
            case kAudioObjectPropertyBaseClass: RETURN_VALUE(AudioClassID, kAudioObjectClassID);
            case kAudioObjectPropertyClass: RETURN_VALUE(AudioClassID, kAudioStreamClassID);
            case kAudioObjectPropertyOwner: RETURN_VALUE(AudioObjectID, DeviceOf(obj));
            case kAudioObjectPropertyOwnedObjects: *outSize = 0; return 0;
            case kAudioStreamPropertyIsActive: RETURN_VALUE(UInt32, 1);
            case kAudioStreamPropertyDirection: RETURN_VALUE(UInt32, input ? 1 : 0);
            case kAudioStreamPropertyTerminalType:
                RETURN_VALUE(UInt32, input ? kAudioStreamTerminalTypeMicrophone : kAudioStreamTerminalTypeSpeaker);
            case kAudioStreamPropertyStartingChannel: RETURN_VALUE(UInt32, 1);
            case kAudioStreamPropertyLatency: RETURN_VALUE(UInt32, 0);
            case kAudioStreamPropertyVirtualFormat:
            case kAudioStreamPropertyPhysicalFormat: RETURN_VALUE(AudioStreamBasicDescription, Format());
            case kAudioStreamPropertyAvailableVirtualFormats:
            case kAudioStreamPropertyAvailablePhysicalFormats: {
                AudioStreamRangedDescription ranged;
                ranged.mFormat = Format();
                ranged.mSampleRateRange.mMinimum = kSampleRate;
                ranged.mSampleRateRange.mMaximum = kSampleRate;
                RETURN_VALUE(AudioStreamRangedDescription, ranged);
            }
            default: return kAudioHardwareUnknownPropertyError;
        }
    }
    return kAudioHardwareBadObjectError;
}

static OSStatus SetPropertyData(AudioServerPlugInDriverRef d, AudioObjectID obj, pid_t pid, const AudioObjectPropertyAddress *addr, UInt32 qualSize, const void *qual, UInt32 inSize, const void *data) {
    (void)obj; (void)pid; (void)qualSize; (void)qual; (void)inSize; (void)data;
    if (d != gDriver || addr == NULL) return kAudioHardwareBadObjectError;
    return kAudioHardwareUnsupportedOperationError;
}

#pragma mark IO

static OSStatus StartIO(AudioServerPlugInDriverRef d, AudioObjectID device, UInt32 client) {
    (void)client;
    if (d != gDriver || !IsDevice(device)) return kAudioHardwareBadObjectError;
    os_unfair_lock_lock(&gLock);
    if (gIOClients == 0) {
        gAnchorHostTime = mach_absolute_time();
        gTimeStampCount = 0;
        memset(gRing, 0, sizeof(gRing));
    }
    gIOClients++;
    gRunning[IsInput(device) ? 0 : 1]++;
    os_unfair_lock_unlock(&gLock);
    return 0;
}

static OSStatus StopIO(AudioServerPlugInDriverRef d, AudioObjectID device, UInt32 client) {
    (void)client;
    if (d != gDriver || !IsDevice(device)) return kAudioHardwareBadObjectError;
    os_unfair_lock_lock(&gLock);
    UInt32 *running = &gRunning[IsInput(device) ? 0 : 1];
    if (*running > 0) {
        (*running)--;
        gIOClients--;
    }
    os_unfair_lock_unlock(&gLock);
    return 0;
}

// One shared timeline: a time stamp every kRingFrames frames since the first StartIO.
static OSStatus GetZeroTimeStamp(AudioServerPlugInDriverRef d, AudioObjectID device, UInt32 client, Float64 *sampleTime, UInt64 *hostTime, UInt64 *seed) {
    (void)client;
    if (d != gDriver || !IsDevice(device)) return kAudioHardwareBadObjectError;
    os_unfair_lock_lock(&gLock);
    Float64 ticksPerRing = gHostTicksPerFrame * kRingFrames;
    UInt64 now = mach_absolute_time();
    UInt64 next = gAnchorHostTime + (UInt64)((Float64)(gTimeStampCount + 1) * ticksPerRing);
    if (next <= now) gTimeStampCount++;
    *sampleTime = (Float64)(gTimeStampCount * kRingFrames);
    *hostTime = gAnchorHostTime + (UInt64)((Float64)gTimeStampCount * ticksPerRing);
    *seed = 1;
    os_unfair_lock_unlock(&gLock);
    return 0;
}

static OSStatus WillDoIOOperation(AudioServerPlugInDriverRef d, AudioObjectID device, UInt32 client, UInt32 op, Boolean *will, Boolean *inPlace) {
    (void)client;
    if (d != gDriver || !IsDevice(device) || will == NULL || inPlace == NULL) return kAudioHardwareBadObjectError;
    *will = IsInput(device) ? op == kAudioServerPlugInIOOperationReadInput : op == kAudioServerPlugInIOOperationWriteMix;
    *inPlace = true;
    return 0;
}

static OSStatus BeginIOOperation(AudioServerPlugInDriverRef d, AudioObjectID device, UInt32 client, UInt32 op, UInt32 frames, const AudioServerPlugInIOCycleInfo *cycle) {
    (void)client; (void)op; (void)frames; (void)cycle;
    return d == gDriver && IsDevice(device) ? 0 : kAudioHardwareBadObjectError;
}

static OSStatus EndIOOperation(AudioServerPlugInDriverRef d, AudioObjectID device, UInt32 client, UInt32 op, UInt32 frames, const AudioServerPlugInIOCycleInfo *cycle) {
    (void)client; (void)op; (void)frames; (void)cycle;
    return d == gDriver && IsDevice(device) ? 0 : kAudioHardwareBadObjectError;
}

// The ring frame for a sample time; masking also wraps the negative times right after a start.
static Float32 *Slot(SInt64 sampleTime) {
    return &gRing[(UInt64)(sampleTime & (kRingFrames - 1)) * kChannels];
}

static OSStatus DoIOOperation(AudioServerPlugInDriverRef d, AudioObjectID device, AudioObjectID stream, UInt32 client, UInt32 op, UInt32 frames, const AudioServerPlugInIOCycleInfo *cycle, void *main, void *secondary) {
    (void)client; (void)secondary;
    if (d != gDriver || !IsDevice(device) || stream != StreamOf(device) || cycle == NULL || main == NULL) {
        return kAudioHardwareBadObjectError;
    }
    if (frames > kRingFrames) frames = kRingFrames;
    Float32 *buffer = main;
    os_unfair_lock_lock(&gLock);
    if (op == kAudioServerPlugInIOOperationWriteMix && device == kObjectID_Feed) {
        SInt64 start = (SInt64)cycle->mOutputTime.mSampleTime;
        for (UInt32 i = 0; i < frames; i++) {
            Float32 *slot = Slot(start + i);
            for (UInt32 c = 0; c < kChannels; c++) slot[c] = buffer[i * kChannels + c];
        }
    } else if (op == kAudioServerPlugInIOOperationReadInput && device == kObjectID_Input) {
        SInt64 start = (SInt64)cycle->mInputTime.mSampleTime;
        for (UInt32 i = 0; i < frames; i++) {
            Float32 *slot = Slot(start + i);
            for (UInt32 c = 0; c < kChannels; c++) {
                buffer[i * kChannels + c] = slot[c];
                slot[c] = 0; // taken: silence if the feed stops
            }
        }
    }
    os_unfair_lock_unlock(&gLock);
    return 0;
}
