// lgx-frontmost: the frontmost application's PID for lgx-cginput.
//
// Function keys must be posted with CGEventPostToPid: posting F11 at the HID
// tap lets macOS consume it as the Show Desktop shortcut, so the focused app
// never sees the key. The Accessibility API's
// kAXFocusedApplicationAttribute returns kAXErrorCannotComplete from this
// launchd helper, so AppKit's NSWorkspace is the reliable source.
#import <AppKit/AppKit.h>
#include <sys/types.h>

pid_t lgx_frontmost_pid(void) {
    NSRunningApplication *front = [[NSWorkspace sharedWorkspace] frontmostApplication];
    return front ? front.processIdentifier : 0;
}
