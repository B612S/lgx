// lgx-frontmost: application identity shims for lgx-cginput.
//
// Function keys must be posted with CGEventPostToPid: posting F11 at the HID
// tap lets macOS consume it as the Show Desktop shortcut, so the focused app
// never sees the key. The Accessibility API's
// kAXFocusedApplicationAttribute returns kAXErrorCannotComplete from this
// launchd helper, so AppKit's NSWorkspace is the reliable source.
//
// Back/forward side buttons pick the application under the pointer, like Mac
// Mouse Fix's Universal Back and Forward: a navigation swipe acts on the
// window under the pointer, not on whichever app happens to be frontmost.
#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#include <stdio.h>
#include <sys/types.h>

pid_t lgx_frontmost_pid(void) {
    NSRunningApplication *front = [[NSWorkspace sharedWorkspace] frontmostApplication];
    return front ? front.processIdentifier : 0;
}

const char *lgx_app_under_pointer_bundle_id(void) {
    static char buf[256];
    buf[0] = 0;

    // CGEvent locations and CGWindowBounds share the Quartz screen space
    // (origin top-left). Do not use NSEvent.mouseLocation here: in a plain
    // helper it can be stale, while the client keeps warping the cursor.
    CGEventRef event = CGEventCreate(NULL);
    CGPoint point = event ? CGEventGetLocation(event) : CGPointZero;
    if (event) CFRelease(event);

    NSRunningApplication *app = nil;
    NSRunningApplication *fallback = nil;
    CFArrayRef windows = CGWindowListCopyWindowInfo(
        kCGWindowListOptionOnScreenOnly, kCGNullWindowID);
    if (windows) {
        for (CFIndex i = 0; i < CFArrayGetCount(windows); i++) {
            CFDictionaryRef info = (CFDictionaryRef)CFArrayGetValueAtIndex(windows, i);

            CFDictionaryRef boundsDict =
                (CFDictionaryRef)CFDictionaryGetValue(info, kCGWindowBounds);
            CGRect bounds;
            if (!boundsDict || !CGRectMakeWithDictionaryRepresentation(boundsDict, &bounds)) {
                continue;
            }
            if (!CGRectContainsPoint(bounds, point)) continue;

            double alpha = 1.0;
            CFNumberRef alphaRef = (CFNumberRef)CFDictionaryGetValue(info, kCGWindowAlpha);
            if (alphaRef) CFNumberGetValue(alphaRef, kCFNumberDoubleType, &alpha);
            if (alpha <= 0.0) continue;

            int layer = 0;
            CFNumberRef layerRef = (CFNumberRef)CFDictionaryGetValue(info, kCGWindowLayer);
            if (layerRef) CFNumberGetValue(layerRef, kCFNumberIntType, &layer);

            CFNumberRef owner = (CFNumberRef)CFDictionaryGetValue(info, kCGWindowOwnerPID);
            int pid = 0;
            if (!owner || !CFNumberGetValue(owner, kCFNumberIntType, &pid) || pid <= 0) {
                continue;
            }
            NSRunningApplication *candidate =
                [NSRunningApplication runningApplicationWithProcessIdentifier:pid];
            if (!candidate) continue;

            // Normal app windows win over menu bar, Dock and desktop layers;
            // those full-screen overlays otherwise shadow the app window.
            if (layer == 0) {
                app = candidate;
                break;
            }
            if (!fallback) fallback = candidate;
        }
        CFRelease(windows);
    }

    if (!app) app = fallback;
    if (!app) app = [[NSWorkspace sharedWorkspace] frontmostApplication];
    NSString *identifier = app.bundleIdentifier;
    snprintf(buf, sizeof buf, "%s", identifier ? identifier.UTF8String : "");
    return buf;
}
