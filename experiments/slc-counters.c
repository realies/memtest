// Standalone, ordinary-user IOReport availability diagnostic. No cache-size inference.
// Build with clang -O2 -std=c11 -Wall -Wextra -Werror -fblocks,
// -framework CoreFoundation -framework IOKit slc-counters.c -o slc-counters.
// API declarations cross-checked against kennss/SiliconScope Sources/CIOReport and
// vladkens/macmon. IOReport is private and may change; missing/rejected data stays unavailable.
#include <CoreFoundation/CoreFoundation.h>
#include <IOKit/IOKitLib.h>
#include <dlfcn.h>
#include <errno.h>
#include <inttypes.h>
#include <limits.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

typedef void *Subscription;
typedef struct {
    CFMutableDictionaryRef (*IOReportCopyAllChannels)(uint64_t, uint64_t);
    CFMutableDictionaryRef (*IOReportCopyChannelsInGroup)(CFStringRef, CFStringRef, uint64_t, uint64_t, uint64_t);
    Subscription (*IOReportCreateSubscription)(void *, CFMutableDictionaryRef, CFMutableDictionaryRef *, uint64_t, CFTypeRef);
    CFDictionaryRef (*IOReportCreateSamples)(Subscription, CFMutableDictionaryRef, CFTypeRef);
    CFDictionaryRef (*IOReportCreateSamplesDelta)(CFDictionaryRef, CFDictionaryRef, CFTypeRef);
    CFStringRef (*IOReportChannelGetGroup)(CFDictionaryRef);
    CFStringRef (*IOReportChannelGetSubGroup)(CFDictionaryRef);
    CFStringRef (*IOReportChannelGetChannelName)(CFDictionaryRef);
    int (*IOReportChannelGetFormat)(CFDictionaryRef);
    int64_t (*IOReportSimpleGetIntegerValue)(CFDictionaryRef, int);
    CFStringRef (*IOReportChannelGetUnitLabel)(CFDictionaryRef);
    uint64_t (*IOReportChannelGetChannelID)(CFDictionaryRef);
} API;
static API api;

static void sanitize(char *text) {
    for (unsigned char *p = (unsigned char *)text; *p; ++p)
        if (*p < 32 || *p == 127) *p = ' ';
}

static void string_value(CFStringRef value, char *output, size_t size) {
    if (!value || !CFStringGetCString(value, output, (CFIndex)size, kCFStringEncodingUTF8))
        snprintf(output, size, "<unavailable>");
    sanitize(output);
}

static CFArrayRef channel_array(CFDictionaryRef dictionary) {
    if (!dictionary) return NULL;
    CFTypeRef value = CFDictionaryGetValue(dictionary, CFSTR("IOReportChannels"));
    if (!value || CFGetTypeID(value) != CFArrayGetTypeID()) return NULL;
    return (CFArrayRef)value;
}

static CFIndex channel_count(CFDictionaryRef dictionary) {
    CFArrayRef array = channel_array(dictionary);
    return array ? CFArrayGetCount(array) : 0;
}

static void increment(CFMutableDictionaryRef counts, CFStringRef key) {
    int64_t count = 0;
    CFNumberRef old = CFDictionaryGetValue(counts, key);
    if (old) CFNumberGetValue(old, kCFNumberSInt64Type, &count);
    ++count;
    CFNumberRef number = CFNumberCreate(NULL, kCFNumberSInt64Type, &count);
    if (number) {
        CFDictionarySetValue(counts, key, number);
        CFRelease(number);
    }
}

static void print_count(const void *key, const void *value, void *context) {
    char name[512];
    int64_t count = 0;
    string_value((CFStringRef)key, name, sizeof name);
    CFNumberGetValue((CFNumberRef)value, kCFNumberSInt64Type, &count);
    printf("%s\t%s\t%" PRId64 "\n", (const char *)context, name, count);
}

static void inventory_groups(void) {
    if (!api.IOReportCopyAllChannels) {
        puts("status\tall-channels\tsymbol_unavailable");
        return;
    }
    CFMutableDictionaryRef channels = api.IOReportCopyAllChannels(0, 0);
    CFArrayRef array = channel_array(channels);
    if (!array) {
        puts("status\tall-channels\tunavailable_channel_array");
        if (channels) CFRelease(channels);
        return;
    }
    printf("status\tall-channels\tchannels\t%ld\n", (long)CFArrayGetCount(array));
    CFMutableDictionaryRef counts = CFDictionaryCreateMutable(NULL, 0,
        &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    if (counts) {
        for (CFIndex i = 0; i < CFArrayGetCount(array); ++i) {
            CFTypeRef item = CFArrayGetValueAtIndex(array, i);
            if (!item || CFGetTypeID(item) != CFDictionaryGetTypeID()) continue;
            CFStringRef group = api.IOReportChannelGetGroup((CFDictionaryRef)item);
            if (!group || CFGetTypeID(group) != CFStringGetTypeID()) group = CFSTR("<unavailable>");
            increment(counts, group);
        }
        CFDictionaryApplyFunction(counts, print_count, "group");
        CFRelease(counts);
    } else {
        puts("status\tall-channels\tallocation_failed");
    }
    CFRelease(channels);
}

static void inventory_services(void) {
    const char *classes[] = {"AppleMemCacheController", "AppleVirtualPlatformARMPE",
                            "AppleARMIODevice", "IOReportHub"};
    for (size_t i = 0; i < sizeof classes / sizeof *classes; ++i) {
        CFMutableDictionaryRef matching = IOServiceMatching(classes[i]);
        if (!matching) {
            printf("service\t%s\tmatching_unavailable\n", classes[i]);
            continue;
        }
        io_iterator_t iterator = IO_OBJECT_NULL;
        // IOServiceGetMatchingServices consumes the matching dictionary.
        kern_return_t result = IOServiceGetMatchingServices(kIOMainPortDefault, matching, &iterator);
        if (result != KERN_SUCCESS) {
            printf("service\t%s\tquery_failed\t0x%08" PRIx32 "\n", classes[i], (uint32_t)result);
            continue;
        }
        CFMutableDictionaryRef counts = CFDictionaryCreateMutable(NULL, 0,
            &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
        unsigned long count = 0;
        io_service_t service;
        while ((service = IOIteratorNext(iterator)) != IO_OBJECT_NULL) {
            ++count;
            io_name_t name;
            if (counts && IOObjectGetClass(service, name) == KERN_SUCCESS) {
                CFStringRef string = CFStringCreateWithCString(NULL, name, kCFStringEncodingUTF8);
                if (string) { increment(counts, string); CFRelease(string); }
            }
            IOObjectRelease(service);
        }
        IOObjectRelease(iterator);
        printf("service\t%s\tcount\t%lu\n", classes[i], count);
        if (counts) {
            printf("service-class-query\t%s\n", classes[i]);
            CFDictionaryApplyFunction(counts, print_count, "service-class");
            CFRelease(counts);
        }
    }
}

static void print_channels(const char *phase, CFDictionaryRef dictionary, bool read_values) {
    CFArrayRef array = channel_array(dictionary);
    if (!array) {
        printf("status\t%s\tunavailable_channel_array\n", phase);
        return;
    }
    for (CFIndex i = 0; i < CFArrayGetCount(array); ++i) {
        CFTypeRef item = CFArrayGetValueAtIndex(array, i);
        if (!item || CFGetTypeID(item) != CFDictionaryGetTypeID()) continue;
        CFDictionaryRef channel = (CFDictionaryRef)item;
        char group[256], subgroup[256], name[512], unit[128], identifier[64];
        string_value(api.IOReportChannelGetGroup(channel), group, sizeof group);
        string_value(api.IOReportChannelGetSubGroup(channel), subgroup, sizeof subgroup);
        string_value(api.IOReportChannelGetChannelName(channel), name, sizeof name);
        string_value(api.IOReportChannelGetUnitLabel ? api.IOReportChannelGetUnitLabel(channel) : NULL,
                     unit, sizeof unit);
        if (api.IOReportChannelGetChannelID)
            snprintf(identifier, sizeof identifier, "0x%016" PRIx64, api.IOReportChannelGetChannelID(channel));
        else
            snprintf(identifier, sizeof identifier, "<unavailable>");
        int format = api.IOReportChannelGetFormat(channel);
        printf("channel\t%s\t%s\t%s\t%s\t%s\t%s\t%d\t", phase, group, subgroup,
               name, identifier, unit, format);
        if (!read_values) {
            puts("not_sampled");
        } else if (format != 1) {
            puts("unsupported_format");
        } else {
            int64_t value = api.IOReportSimpleGetIntegerValue(channel, 0);
            if (value == INT64_MIN) puts("unavailable_sentinel");
            else if (value < 0) printf("negative_or_unavailable:%" PRId64 "\n", value);
            else printf("%" PRId64 "\n", value);
        }
    }
}

static void save_channels(const char *directory, const char *name, CFDictionaryRef channels) {
    // Whitelist channel metadata only; never export arbitrary registry/driver dictionaries.
    CFArrayRef input = channel_array(channels);
    if (!input) return;
    CFMutableArrayRef output = CFArrayCreateMutable(NULL, 0, &kCFTypeArrayCallBacks);
    for (CFIndex i = 0; i < CFArrayGetCount(input); ++i) {
        CFTypeRef item = CFArrayGetValueAtIndex(input, i);
        if (!item || CFGetTypeID(item) != CFDictionaryGetTypeID()) continue;
        CFDictionaryRef channel = (CFDictionaryRef)item;
        CFMutableDictionaryRef entry = CFDictionaryCreateMutable(NULL, 0,
            &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
        CFStringRef keys[] = {CFSTR("group"), CFSTR("subgroup"), CFSTR("channel"), CFSTR("unit")};
        CFStringRef values[] = {api.IOReportChannelGetGroup(channel), api.IOReportChannelGetSubGroup(channel),
            api.IOReportChannelGetChannelName(channel),
            api.IOReportChannelGetUnitLabel ? api.IOReportChannelGetUnitLabel(channel) : NULL};
        for (size_t j = 0; j < sizeof keys / sizeof *keys; ++j)
            if (values[j]) CFDictionarySetValue(entry, keys[j], values[j]);
        int format = api.IOReportChannelGetFormat(channel);
        CFNumberRef number = CFNumberCreate(NULL, kCFNumberIntType, &format);
        CFDictionarySetValue(entry, CFSTR("format"), number);
        CFRelease(number);
        if (api.IOReportChannelGetChannelID) {
            char identifier[32];
            snprintf(identifier, sizeof identifier, "0x%016" PRIx64, api.IOReportChannelGetChannelID(channel));
            CFStringRef string = CFStringCreateWithCString(NULL, identifier, kCFStringEncodingUTF8);
            CFDictionarySetValue(entry, CFSTR("id"), string);
            CFRelease(string);
        }
        CFArrayAppendValue(output, entry);
        CFRelease(entry);
    }
    CFErrorRef error = NULL;
    CFDataRef data = CFPropertyListCreateData(NULL, output, kCFPropertyListXMLFormat_v1_0, 0, &error);
    CFRelease(output);
    if (!data) {
        printf("status\t%s\tmetadata_serialization_failed\n", name);
        if (error) CFRelease(error);
        return;
    }
    char path[4096];
    int length = snprintf(path, sizeof path, "%s/%s.plist", directory, name);
    FILE *file = length > 0 && (size_t)length < sizeof path ? fopen(path, "wb") : NULL;
    if (!file) {
        printf("status\t%s\tmetadata_file_open_failed\n", name);
    } else {
        size_t size = (size_t)CFDataGetLength(data);
        bool failed = fwrite(CFDataGetBytePtr(data), 1, size, file) != size;
        if (fclose(file) != 0) failed = true;
        if (failed) printf("status\t%s\tmetadata_file_write_failed\n", name);
    }
    CFRelease(data);
}

static void attempt(const char *directory, const char *label, CFMutableDictionaryRef desired) {
    CFIndex requested = channel_count(desired);
    printf("status\t%s\trequested_channels\t%ld\n", label, (long)requested);
    if (!desired || !requested) {
        printf("status\t%s\tunavailable\n", label);
        return;
    }
    save_channels(directory, label, desired);
    print_channels(label, desired, false);
    CFMutableDictionaryRef subscribed = NULL;
    Subscription subscription = api.IOReportCreateSubscription(NULL, desired, &subscribed, 0, NULL);
    CFIndex retained = channel_count(subscribed);
    printf("status\t%s\tsubscription_%s\tretained_channels\t%ld\n", label,
           subscription ? "created" : "rejected", (long)retained);
    if (!subscription || !subscribed || !retained) {
        if (subscribed) CFRelease(subscribed);
        return;
    }
    if (retained != requested) printf("status\t%s\trequested_set_changed\n", label);
    char subscribed_label[128];
    snprintf(subscribed_label, sizeof subscribed_label, "%s-subscribed", label);
    save_channels(directory, subscribed_label, subscribed);
    print_channels("subscribed", subscribed, false);
    CFDictionaryRef first = api.IOReportCreateSamples(subscription, subscribed, NULL);
    struct timespec pause = {.tv_sec = 0, .tv_nsec = 200000000};
    while (nanosleep(&pause, &pause) != 0 && errno == EINTR) {}
    CFDictionaryRef second = api.IOReportCreateSamples(subscription, subscribed, NULL);
    CFDictionaryRef delta = first && second ? api.IOReportCreateSamplesDelta(first, second, NULL) : NULL;
    printf("status\t%s\tsamples_%s\n", label, delta ? "created" : "unavailable");
    if (first) print_channels("first", first, true);
    if (second) print_channels("second", second, true);
    if (delta) print_channels("delta", delta, true);
    if (delta) CFRelease(delta);
    if (first) CFRelease(first);
    if (second) CFRelease(second);
    CFRelease(subscribed);
    // IOReport's opaque subscription has no verified public destructor. This bounded
    // diagnostic keeps its few handles alive until process exit rather than guessing.
}

int main(int argc, char **argv) {
    if (argc > 2) { fprintf(stderr, "usage: %s [new-output-directory]\n", argv[0]); return 2; }
    char temporary[] = "/tmp/memtest-ioreport.XXXXXX";
    const char *directory = argc == 2 ? argv[1] : mkdtemp(temporary);
    if (!directory || (argc == 2 && mkdir(directory, 0700) != 0)) { perror("output directory"); return 1; }
    printf("IOReport availability diagnostic; uid=%lu\n", (unsigned long)getuid());
    printf("Output directory: %s\n", directory);
    puts("System-wide counter deltas only; no process attribution, cache-size inference or cache-hit percentage.");
    puts("Columns: channel / phase / group / subgroup / name / id / unit / format / raw_value_or_status");
    inventory_services();
    void *library = dlopen("/usr/lib/libIOReport.dylib", RTLD_NOW | RTLD_LOCAL);
    if (!library) { fprintf(stderr, "IOReport unavailable: %s\n", dlerror()); return 1; }
#define REQUIRED(symbol) do { \
    api.symbol = (__typeof__(api.symbol))dlsym(library, #symbol); \
    if (!api.symbol) { fprintf(stderr, "Required symbol unavailable: %s\n", #symbol); return 1; } \
} while (0)
    REQUIRED(IOReportCopyChannelsInGroup);
    REQUIRED(IOReportCreateSubscription);
    REQUIRED(IOReportCreateSamples);
    REQUIRED(IOReportCreateSamplesDelta);
    REQUIRED(IOReportChannelGetGroup);
    REQUIRED(IOReportChannelGetSubGroup);
    REQUIRED(IOReportChannelGetChannelName);
    REQUIRED(IOReportChannelGetFormat);
    REQUIRED(IOReportSimpleGetIntegerValue);
#undef REQUIRED
    api.IOReportCopyAllChannels = (__typeof__(api.IOReportCopyAllChannels))dlsym(library, "IOReportCopyAllChannels");
    api.IOReportChannelGetUnitLabel = (__typeof__(api.IOReportChannelGetUnitLabel))dlsym(library, "IOReportChannelGetUnitLabel");
    api.IOReportChannelGetChannelID = (__typeof__(api.IOReportChannelGetChannelID))dlsym(library, "IOReportChannelGetChannelID");
    inventory_groups();
    const struct { const char *label; CFStringRef group; CFStringRef subgroup; } queries[] = {
        {"amc-all", CFSTR("AMC Stats"), NULL},
        {"amc-dsid", CFSTR("AMC Stats"), CFSTR("DSID Group Stats")},
        {"amc-perf", CFSTR("AMC Stats"), CFSTR("Perf Counters")},
        {"energy-api-control", CFSTR("Energy Model"), NULL},
    };
    for (size_t i = 0; i < sizeof queries / sizeof *queries; ++i) {
        puts("");
        printf("query\t%s\n", queries[i].label);
        CFMutableDictionaryRef channels = api.IOReportCopyChannelsInGroup(queries[i].group, queries[i].subgroup, 0, 0, 0);
        attempt(directory, queries[i].label, channels);
        if (channels) CFRelease(channels);
    }
    puts("\nComplete. A visible legend, a successful subscription, and usable samples are separate findings.");
    return 0;
}
