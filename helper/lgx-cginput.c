// lgx-cginput: evdev codes from the lgx client -> CGEvent, no kernel driver.
//
// C on purpose: input injection is a plain C API, and the Swift variant tripped
// over the Swift runtime (SIGTRAP before the first line). One AppKit call in
// lgx-frontmost.m names the frontmost application, which function keys need
// for direct delivery. Mirrors the shape of helper/lgx-uinput.c on Linux.
//
// One command per line on stdin:
//   key <evdev> <0|1>      key down/up
//   btn <evdev> <0|1>      mouse button (0x110 left, 0x111 right, 0x112 middle,
//                          0x113 back, 0x114 forward; back/forward use Mac
//                          Mouse Fix's universal mapping: a navigation swipe
//                          or menu shortcut where raw buttons are ignored)
//   motion <x> <y>         absolute pointer, both 0..65535
//   wheel <axis> <steps>   axis 0 = vertical, 1 = horizontal; one step is a
//                          wheel notch and is replayed as a short pixel
//                          animation so the guest scrolls smoothly
//                          (LGX_SMOOTH_SCROLL=0 disables it, LGX_WHEEL_PIXELS
//                          sets how far a notch travels, default 48, and
//                          LGX_WHEEL_INVERT=1 flips the direction)
//
// Requires the Accessibility TCC grant for the responsible process.
#include <ApplicationServices/ApplicationServices.h>
#include <CoreGraphics/CoreGraphics.h>
#include <CoreVideo/CoreVideo.h>
#include <IOKit/hidsystem/ev_keymap.h>
#include <pthread.h>
#include <time.h>
#include <stdarg.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
    unsigned short evdev;
    CGKeyCode code;
} KeyMap;

static const KeyMap KEYMAP[] = {
    // Linux evdev code -> macOS virtual keycode (US ANSI layout).
    {1, 0x35},   // Esc
    {2, 0x12},   {3, 0x13},   {4, 0x14},   {5, 0x15},   {6, 0x17},
    {7, 0x16},   {8, 0x1A},   {9, 0x1C},   {10, 0x19},  {11, 0x1D},
    {12, 0x1B},  // -
    {13, 0x18},  // =
    {14, 0x33},  // Backspace
    {15, 0x30},  // Tab
    {16, 0x0C},  {17, 0x0D},  {18, 0x0E},  {19, 0x0F},  {20, 0x11},
    {21, 0x10},  {22, 0x20},  {23, 0x22},  {24, 0x1F},  {25, 0x23},
    {26, 0x21},  // [
    {27, 0x1E},  // ]
    {28, 0x24},  // Return
    {29, 0x3B},  // Left Control
    {30, 0x00},  {31, 0x01},  {32, 0x02},  {33, 0x03},  {34, 0x05},
    {35, 0x04},  {36, 0x26},  {37, 0x28},  {38, 0x25},
    {39, 0x29},  // ;
    {40, 0x27},  // '
    {41, 0x32},  // `
    {42, 0x38},  // Left Shift
    {43, 0x2A},  // backslash
    {44, 0x06},  {45, 0x07},  {46, 0x08},  {47, 0x09},  {48, 0x0B},
    {49, 0x2D},  {50, 0x2E},
    {51, 0x2B},  // ,
    {52, 0x2F},  // .
    {53, 0x2C},  // /
    {54, 0x3C},  // Right Shift
    {55, 0x43},  // Keypad *
    {56, 0x3A},  // Left Option
    {57, 0x31},  // Space
    {58, 0x39},  // Caps Lock
    {59, 0x7A},  {60, 0x78},  {61, 0x63},  {62, 0x76},  {63, 0x60},
    {64, 0x61},  {65, 0x62},  {66, 0x64},  {67, 0x65},  {68, 0x6D},
    {69, 0x47},  // Num Lock / Clear
    {71, 0x59},  {72, 0x5B},  {73, 0x5C},  {74, 0x4E},  {75, 0x56},
    {76, 0x57},  {77, 0x58},  {78, 0x45},  {79, 0x53},  {80, 0x54},
    {81, 0x55},  {82, 0x52},  {83, 0x41},
    {87, 0x67},  {88, 0x6F},
    {96, 0x4C},  // Keypad Enter
    {98, 0x4B},  // Keypad /
    {100, 0x3D}, // Right Option
    {102, 0x73}, // Home
    {103, 0x7E}, {104, 0x74}, {105, 0x7B}, {106, 0x7C}, {107, 0x77},
    {108, 0x7D}, {109, 0x79}, {110, 0x72}, {111, 0x75},
    {97, 0x3E},  // Right Control
    {125, 0x37}, // Left Command
    {126, 0x36}, // Right Command
};
static const size_t KEYMAP_LEN = sizeof(KEYMAP) / sizeof(KEYMAP[0]);

// Consumer/media keys are not keyboard keycodes on macOS: they are
// NX_SYSDEFINED events with an NX_KEYTYPE_* code. Keep the Linux evdev codes
// the client sends separate from KEYMAP.
typedef struct {
    unsigned short evdev;
    int nx;
} MediaMap;

static const MediaMap MEDIA_MAP[] = {
    {113, NX_KEYTYPE_MUTE},
    {114, NX_KEYTYPE_SOUND_DOWN},
    {115, NX_KEYTYPE_SOUND_UP},
    {161, NX_KEYTYPE_EJECT},
    {163, NX_KEYTYPE_NEXT},
    {164, NX_KEYTYPE_PLAY},       // play/pause toggles
    {165, NX_KEYTYPE_PREVIOUS},
    {168, NX_KEYTYPE_REWIND},
    {207, NX_KEYTYPE_PLAY},
    {208, NX_KEYTYPE_FAST},
    {224, NX_KEYTYPE_BRIGHTNESS_DOWN},
    {225, NX_KEYTYPE_BRIGHTNESS_UP},
    {228, NX_KEYTYPE_ILLUMINATION_TOGGLE},
    {229, NX_KEYTYPE_ILLUMINATION_DOWN},
    {230, NX_KEYTYPE_ILLUMINATION_UP},
};
static const size_t MEDIA_MAP_LEN = sizeof(MEDIA_MAP) / sizeof(MEDIA_MAP[0]);

static int media_key(unsigned short evdev) {
    for (size_t i = 0; i < MEDIA_MAP_LEN; i++) {
        if (MEDIA_MAP[i].evdev == evdev) return MEDIA_MAP[i].nx;
    }
    return -1;
}

// Implemented in lgx-frontmost.m (AppKit). Used for function keys (see
// post_key()) and for the app-under-pointer back/forward choice.
extern pid_t lgx_frontmost_pid(void);
extern const char *lgx_app_under_pointer_bundle_id(void);

// Implemented in lgx-media.m (AppKit). Media keys are NX_SYSDEFINED events,
// not keyboard keycodes.
extern void lgx_post_media_key(int key, int down);

static FILE *dbg = NULL;

static void logf_(const char *fmt, ...) {
    if (!dbg) dbg = fopen("/tmp/lgx-cginput.log", "a");
    if (!dbg) return;
    va_list ap;
    va_start(ap, fmt);
    vfprintf(dbg, fmt, ap);
    va_end(ap);
    fputc('\n', dbg);
    fflush(dbg);
}


static CGKeyCode map_key(unsigned short evdev, int *found) {
    for (size_t i = 0; i < KEYMAP_LEN; i++) {
        if (KEYMAP[i].evdev == evdev) {
            *found = 1;
            return KEYMAP[i].code;
        }
    }
    *found = 0;
    return 0;
}

// Synthetic key events do not reliably update the system's modifier state, so
// the helper tracks Shift/Control/Option/Command itself and stamps every key
// event with the current flags. Without this Command+C, Control+X and friends
// arrive as plain key presses.
static CGEventFlags g_flags = 0;

static CGEventFlags modifier_flag(CGKeyCode code) {
    switch (code) {
        case 0x38: case 0x3C: return kCGEventFlagMaskShift;
        case 0x3B: case 0x3E: return kCGEventFlagMaskControl;
        case 0x3A: case 0x3D: return kCGEventFlagMaskAlternate;
        case 0x37: case 0x36: return kCGEventFlagMaskCommand;
        default: return 0;
    }
}

// F1-F12 carry the kCGEventFlagMaskSecondaryFn flag, but posting them at the
// HID tap still lets macOS consume some of them as system shortcuts: F11 opens
// Show Desktop and never reaches the focused app. Post function keys directly
// to the frontmost PID instead. Other keys stay on the HID tap so shortcuts
// such as Cmd+Tab and Mission Control's Ctrl+Left/Right keep working.
static int is_function_key(CGKeyCode code) {
    switch (code) {
        case 0x7A: case 0x78: case 0x63: case 0x76: case 0x60: case 0x61:
        case 0x62: case 0x64: case 0x65: case 0x6D: case 0x67: case 0x6F:
            return 1;
        default:
            return 0;
    }
}

static void post_key(unsigned short evdev, int down) {
    int nx = media_key(evdev);
    if (nx >= 0) {
        lgx_post_media_key(nx, down != 0);
        return;
    }
    int found = 0;
    CGKeyCode code = map_key(evdev, &found);
    if (!found) return;
    CGEventFlags flag = modifier_flag(code);
    if (flag) {
        if (down) {
            g_flags |= flag;
        } else {
            g_flags &= ~flag;
        }
    }
    CGEventSourceRef src = CGEventSourceCreate(kCGEventSourceStateHIDSystemState);
    CGEventRef ev = CGEventCreateKeyboardEvent(src, code, down != 0);
    if (ev) {
        // OR the flags instead of replacing them: the event created for arrow
        // keys already carries kCGEventFlagMaskNumericPad (and function keys
        // carry their own), and overwriting it makes Mission Control's
        // Ctrl+Left/Right stop matching.
        CGEventSetFlags(ev, CGEventGetFlags(ev) | g_flags);
        pid_t pid = is_function_key(code) ? lgx_frontmost_pid() : 0;
        if (pid > 0) {
            CGEventPostToPid(pid, ev);
        } else {
            CGEventPost(kCGHIDEventTap, ev);
        }
        CFRelease(ev);
    }
    if (src) CFRelease(src);
}

// Drag state: while a button is held, motion must be posted as *Dragged,
// otherwise macOS moves the pointer but leaves the grabbed window behind.
static int dragging = 0;   // 0 none, 1 left, 2 right, 3 other

// The system cursor can lag behind a burst of warps, so button events use the
// last position the client sent. The click then lands where the user sees the
// pointer even when a warp is throttled.
static int g_have_motion = 0;
static CGPoint g_motion_point = {0, 0};

// The display selected by the server's --display option. ScreenCaptureKit
// reports its CGDirectDisplayID; mapping motion against the main display
// would move and click on the wrong monitor when another one is streamed.
static CGDirectDisplayID g_display_id = 0;

// Synthetic mouse events carry their own click count: macOS does not derive
// double/triple clicks for injected events, and always sending 1 makes every
// press a single click. Track each button separately.
typedef struct {
    double last_down;
    double down_posted;
    CGPoint point;
    int count;
} ClickState;

static ClickState g_clicks[5];
// The button number used for OtherMouseDragged; extra mouse buttons must keep
// their own number instead of always dragging as the middle button.
static CGMouseButton g_drag_button = kCGMouseButtonLeft;

static double now_sec(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec / 1e9;
}

// Undocumented CGEvent fields and IOHID constants for a trackpad navigation
// swipe. Mac Mouse Fix and SensibleSideButtons use the same values: field 55 is
// NSEventTypeGesture, 110 the IOHID event type, 132 its phase and 115 the swipe
// direction. Mac Mouse Fix posts them at the session tap, not the HID tap.
// The event sequence is adapted from Mac Mouse Fix (MMF License); see
// THIRD_PARTY_NOTICES.md. Mac Mouse Fix credits SensibleSideButtons and
// Calftrail Touch for the original reverse engineering.
enum {
    kLGXFieldEventType = 55,
    kLGXFieldHIDEventType = 110,
    kLGXFieldHIDPhase = 132,
    kLGXFieldHIDSwipe = 115,
};

// ANSI keycodes for the menu-shortcut fallback. Mac Mouse Fix additionally
// emulates Apple's automatic key-equivalent localization; this helper uses the
// ANSI positions, which match the ABC layout used by default.
enum {
    kLGXKeyLeftBracket = 0x21,  // [
    kLGXKeyRightBracket = 0x1E, // ]
    kLGXKeyLeftArrow = 0x7B,
    kLGXKeyRightArrow = 0x7C,
};

typedef enum {
    SIDE_RAW = 0,
    SIDE_SWIPE,
    SIDE_CMD_BRACKET,
    SIDE_CMD_ARROW,
    SIDE_OPT_CMD_BRACKET,
} SideAction;

static int starts_with(const char *s, const char *prefix) {
    return strncmp(s, prefix, strlen(prefix)) == 0;
}

// The same decision tree as Mac Mouse Fix's "Universal Back and Forward":
// the app under the pointer picks the method, because a navigation swipe acts
// on the window under the pointer.
//   - navigation swipe: the native back/forward gesture (Safari, Finder and
//     the other Apple apps that implement it)
//   - menu shortcuts: apps that expose back/forward only as a menu item
//   - raw button 3/4: everything else (Chrome, Firefox, editors), so those
//     apps keep their own handling
static SideAction side_action(void) {
    const char *id = lgx_app_under_pointer_bundle_id();
    if (!id || !id[0]) return SIDE_RAW;

    if (starts_with(id, "com.operasoftware.Opera")) return SIDE_SWIPE;
    if (starts_with(id, "com.binarynights.ForkLift")) return SIDE_SWIPE;

    if (starts_with(id, "org.zotero.zotero")) return SIDE_CMD_BRACKET;
    if (starts_with(id, "com.apple.systempreferences")) return SIDE_CMD_BRACKET;
    if (starts_with(id, "com.apple.AppStore")) return SIDE_CMD_BRACKET;
    if (starts_with(id, "com.adobe.Acrobat.Pro")) return SIDE_CMD_ARROW;
    if (starts_with(id, "dev.warp.Warp")) return SIDE_CMD_BRACKET;

    if (starts_with(id, "com.apple.Music")) return SIDE_CMD_BRACKET;
    if (starts_with(id, "com.apple.iCal")) return SIDE_CMD_ARROW;
    if (starts_with(id, "com.apple.AddressBook")) return SIDE_CMD_BRACKET;
    if (starts_with(id, "com.apple.Notes")) return SIDE_OPT_CMD_BRACKET;
    if (starts_with(id, "com.apple.freeform")) return SIDE_OPT_CMD_BRACKET;
    if (starts_with(id, "com.apple.TV")) return SIDE_CMD_BRACKET;
    if (starts_with(id, "com.apple.iBooksX")) return SIDE_CMD_BRACKET;
    if (starts_with(id, "com.apple.Preview")) return SIDE_CMD_BRACKET;

    if (starts_with(id, "com.apple.")) return SIDE_SWIPE;
    return SIDE_RAW;
}

static void post_navigation_swipe(int back) {
    CGEventRef ev = CGEventCreate(NULL);
    if (!ev) return;
    CGEventSetIntegerValueField(ev, kLGXFieldEventType, 29);     // NSEventTypeGesture
    CGEventSetIntegerValueField(ev, kLGXFieldHIDEventType, 16);  // kIOHIDEventTypeNavigationSwipe
    CGEventSetIntegerValueField(ev, kLGXFieldHIDPhase, 1);       // kIOHIDEventPhaseBegan
    CGEventSetIntegerValueField(ev, kLGXFieldHIDSwipe, back ? 4 : 8); // kIOHIDSwipeLeft/Right
    CGEventPost(kCGSessionEventTap, ev);
    CGEventSetIntegerValueField(ev, kLGXFieldHIDSwipe, 0);       // kIOHIDSwipeNone
    CGEventSetIntegerValueField(ev, kLGXFieldHIDPhase, 4);       // kIOHIDEventPhaseEnded
    CGEventPost(kCGSessionEventTap, ev);
    CFRelease(ev);
}

// A complete modifier+key chord for the apps that only expose back/forward as
// a menu shortcut. The final plain event clears any modifier state the
// WindowServer may have latched from the synthetic chord.
static void post_shortcut(CGKeyCode key, CGEventFlags flags) {
    CGEventRef down = CGEventCreateKeyboardEvent(NULL, key, true);
    CGEventRef up = CGEventCreateKeyboardEvent(NULL, key, false);
    if (down) CGEventSetFlags(down, flags);
    if (up) CGEventSetFlags(up, flags);
    if (down) {
        CGEventPost(kCGHIDEventTap, down);
        CFRelease(down);
    }
    if (up) {
        CGEventPost(kCGHIDEventTap, up);
        CFRelease(up);
    }
    CGEventRef restore = CGEventCreate(NULL);
    if (restore) {
        CGEventPost(kCGHIDEventTap, restore);
        CFRelease(restore);
    }
}

static void post_side_action(SideAction action, int back) {
    switch (action) {
        case SIDE_SWIPE:
            logf_("  -> navigation swipe %s", back ? "back" : "forward");
            post_navigation_swipe(back);
            break;
        case SIDE_CMD_BRACKET:
            logf_("  -> shortcut Cmd+%s", back ? "[" : "]");
            post_shortcut(back ? kLGXKeyLeftBracket : kLGXKeyRightBracket,
                          kCGEventFlagMaskCommand);
            break;
        case SIDE_CMD_ARROW:
            logf_("  -> shortcut Cmd+%s", back ? "Left" : "Right");
            post_shortcut(back ? kLGXKeyLeftArrow : kLGXKeyRightArrow,
                          kCGEventFlagMaskCommand);
            break;
        case SIDE_OPT_CMD_BRACKET:
            logf_("  -> shortcut Opt+Cmd+%s", back ? "[" : "]");
            post_shortcut(back ? kLGXKeyLeftBracket : kLGXKeyRightBracket,
                          kCGEventFlagMaskAlternate | kCGEventFlagMaskCommand);
            break;
        case SIDE_RAW:
            break;
    }
}

// A side-button down that became a swipe or shortcut has no raw mouse-up:
// swallow it even if the application under the pointer changed in between.
static SideAction g_side_action[2];

static void post_button(unsigned short evdev, int down) {
    if (evdev == 0x113 || evdev == 0x114) {
        int slot = evdev == 0x113 ? 0 : 1;
        if (down) {
            SideAction action = side_action();
            g_side_action[slot] = action;
            if (action != SIDE_RAW) {
                post_side_action(action, slot == 0);
                return;
            }
        } else {
            SideAction action = g_side_action[slot];
            g_side_action[slot] = SIDE_RAW;
            if (action != SIDE_RAW) return;
        }
    }
    CGMouseButton button;
    CGEventType type;
    switch (evdev) {
        case 0x110: button = kCGMouseButtonLeft; type = down ? kCGEventLeftMouseDown : kCGEventLeftMouseUp; break;
        case 0x111: button = kCGMouseButtonRight; type = down ? kCGEventRightMouseDown : kCGEventRightMouseUp; break;
        case 0x112: button = kCGMouseButtonCenter; type = down ? kCGEventOtherMouseDown : kCGEventOtherMouseUp; break;
        // X1 (BTN_SIDE) and X2 (BTN_EXTRA) are the browser back/forward
        // buttons. macOS numbers them 3 and 4.
        case 0x113: button = (CGMouseButton)3; type = down ? kCGEventOtherMouseDown : kCGEventOtherMouseUp; break;
        case 0x114: button = (CGMouseButton)4; type = down ? kCGEventOtherMouseDown : kCGEventOtherMouseUp; break;
        default: return;
    }
    CGPoint loc;
    if (g_have_motion) {
        loc = g_motion_point;
    } else {
        CGEventRef cur = CGEventCreate(NULL);
        loc = cur ? CGEventGetLocation(cur) : CGPointZero;
        if (cur) CFRelease(cur);
    }

    int idx = (int)button;
    if (idx < 0 || idx > 4) idx = 0;
    ClickState *cs = &g_clicks[idx];
    double now = now_sec();
    if (down) {
        double dx = loc.x - cs->point.x;
        double dy = loc.y - cs->point.y;
        if (cs->last_down > 0 && now - cs->last_down <= 0.5
            && dx * dx + dy * dy <= 16.0) {
            cs->count = cs->count < 3 ? cs->count + 1 : 1;
        } else {
            cs->count = 1;
        }
        cs->last_down = now;
        cs->point = loc;
    }
    int click_state = cs->count > 0 ? cs->count : 1;

    CGEventSourceRef src = CGEventSourceCreate(kCGEventSourceStateHIDSystemState);
    CGEventRef ev = CGEventCreateMouseEvent(src, type, loc, button);
    if (ev) {
        // Synthetic clicks are only honoured as real ones when the button number
        // and click state are explicit; without them some apps treat them as
        // secondary/ignored.
        CGEventSetIntegerValueField(ev, kCGMouseEventButtonNumber, (int64_t)button);
        CGEventSetIntegerValueField(ev, kCGMouseEventClickState, click_state);
        // A stuck modifier (typically Control) turns a left click into a right
        // click on macOS; synthetic mouse events must not inherit stale flags.
        CGEventSetFlags(ev, 0);
    }
    logf_("  -> post type=%d button=%d ev=%p down=%d click=%d loc=%.1f,%.1f flags=0x%llx",
          (int)type, (int)button, (void *)ev, down, click_state, loc.x, loc.y,
          (unsigned long long)CGEventSourceFlagsState(kCGEventSourceStateHIDSystemState));
    if (ev) {
        if (down) {
            // Put the system cursor on the click point; the button event
            // itself is explicit, so a throttled warp cannot move the click.
            CGWarpMouseCursorPosition(loc);
            CGAssociateMouseAndMouseCursorPosition(true);
        } else if (cs->down_posted > 0) {
            // The window server can drop a mouseUp posted back-to-back with
            // its mouseDown; keep the pair at least 30 ms apart.
            double dt = now_sec() - cs->down_posted;
            if (dt < 0.03) {
                usleep((useconds_t)((0.03 - dt) * 1000000.0));
            }
        }
        CGEventPost(kCGHIDEventTap, ev);
        if (down) {
            cs->down_posted = now_sec();
        } else {
            cs->down_posted = 0;
        }
        CFRelease(ev);
    }
    if (src) CFRelease(src);
    if (down) {
        dragging = button == kCGMouseButtonLeft ? 1 : button == kCGMouseButtonRight ? 2 : 3;
        g_drag_button = button;
    } else {
        dragging = 0;
    }
}

static void post_motion(int nx, int ny) {
    CGDirectDisplayID display = g_display_id != 0 ? g_display_id : CGMainDisplayID();
    CGRect screen = CGDisplayBounds(display);
    CGPoint pt;
    pt.x = screen.origin.x + (CGFloat)nx / 65535.0 * screen.size.width;
    pt.y = screen.origin.y + (CGFloat)ny / 65535.0 * screen.size.height;
    g_motion_point = pt;
    g_have_motion = 1;
    CGWarpMouseCursorPosition(pt);
    CGAssociateMouseAndMouseCursorPosition(true);
    CGEventType moveType = kCGEventMouseMoved;
    CGMouseButton moveButton = kCGMouseButtonLeft;
    if (dragging == 1) {
        moveType = kCGEventLeftMouseDragged;
    } else if (dragging == 2) {
        moveType = kCGEventRightMouseDragged;
        moveButton = kCGMouseButtonRight;
    } else if (dragging == 3) {
        moveType = kCGEventOtherMouseDragged;
        moveButton = g_drag_button;
    }
    CGEventSourceRef src = CGEventSourceCreate(kCGEventSourceStateHIDSystemState);
    CGEventRef ev = CGEventCreateMouseEvent(src, moveType, pt, moveButton);
    if (ev) CGEventSetFlags(ev, 0);
    if (ev) {
        CGEventPost(kCGHIDEventTap, ev);
        CFRelease(ev);
    }
    if (src) CFRelease(src);
}

// A mouse wheel delivers one event per notch. A line-based event makes the
// guest jump, so a notch is expanded into an ease-out animation of pixel scroll
// events. The animation is driven by CVDisplayLink: a background nanosleep is
// coalesced by macOS to ~30 Hz, which made the scroll visibly stepped even on
// a 120 Hz display. One event per display frame is what a trackpad produces.
static pthread_mutex_t g_scroll_lock = PTHREAD_MUTEX_INITIALIZER;
static int g_pending_y = 0;   // pixels still to deliver, signed
static int g_pending_x = 0;
static int g_smooth_scroll = 1;
static int g_wheel_pixels = 48;   // one notch, in pixels
static int g_wheel_invert = 0;    // 1 = keep the natural-scroll direction

static void post_scroll(int32_t dy, int32_t dx, int phase) {
    CGEventSourceRef src = CGEventSourceCreate(kCGEventSourceStateHIDSystemState);
    CGEventRef ev = CGEventCreateScrollWheelEvent(src, kCGScrollEventUnitPixel, 2, dy, dx);
    if (ev) {
        // Without this the event is treated as a discrete wheel click and the
        // app scrolls in whole lines again.
        CGEventSetIntegerValueField(ev, kCGScrollWheelEventIsContinuous, 1);
        // A phase turns the stream into one gesture, so the app smooths the
        // events instead of treating every frame as a separate wheel click.
        CGEventSetIntegerValueField(ev, kCGScrollWheelEventScrollPhase, phase);
        CGEventPost(kCGHIDEventTap, ev);
        CFRelease(ev);
    }
    if (src) CFRelease(src);
}

static void post_wheel_lines(int axis, int steps) {
    int32_t dy = axis == 0 ? (int32_t)(-steps) : 0;
    int32_t dx = axis == 1 ? (int32_t)steps : 0;
    CGEventSourceRef src = CGEventSourceCreate(kCGEventSourceStateHIDSystemState);
    CGEventRef ev = CGEventCreateScrollWheelEvent(src, kCGScrollEventUnitLine, 2, dy, dx);
    if (ev) {
        CGEventPost(kCGHIDEventTap, ev);
        CFRelease(ev);
    }
    if (src) CFRelease(src);
}

// Take at most a quarter of what is left: the first steps are large and the
// tail is short, which is the ease-out a trackpad produces. A zero axis must
// stay zero, otherwise every vertical event also jitters horizontally.
static int take_slice(int pending) {
    if (pending == 0) return 0;
    int mag = pending < 0 ? -pending : pending;
    int step = mag / 4;
    if (step < 1) step = 1;
    if (step > 60) step = 60;
    return pending < 0 ? -step : step;
}

static int g_scrolling = 0;
static CVDisplayLinkRef g_display_link = NULL;
static int g_display_link_running = 0;
static pthread_mutex_t g_link_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_t g_watchdog;
static volatile int g_watchdog_run = 1;
static uint64_t g_last_wheel_ns = 0;

static uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

// CVDisplayLink wakes the process at the panel rate (120 Hz here). It is only
// needed while a smooth-scroll animation is running, so create it once and
// start/stop it on demand instead of leaving a 120 Hz timer alive forever.
static void display_link_start(void) {
    pthread_mutex_lock(&g_link_lock);
    if (!g_display_link_running && g_display_link) {
        if (CVDisplayLinkStart(g_display_link) == kCVReturnSuccess) {
            g_display_link_running = 1;
        }
    }
    pthread_mutex_unlock(&g_link_lock);
}

static void display_link_stop(void) {
    pthread_mutex_lock(&g_link_lock);
    if (g_display_link_running && g_display_link) {
        CVDisplayLinkStop(g_display_link);
        g_display_link_running = 0;
    }
    pthread_mutex_unlock(&g_link_lock);
}

static void *scroll_watchdog(void *arg) {
    (void)arg;
    while (g_watchdog_run) {
        usleep(250000);
        pthread_mutex_lock(&g_scroll_lock);
        int idle = g_pending_x == 0 && g_pending_y == 0 && !g_scrolling;
        uint64_t last = g_last_wheel_ns;
        pthread_mutex_unlock(&g_scroll_lock);
        if (idle && now_ns() - last > 750000000ull) {
            display_link_stop();
        }
    }
    return NULL;
}

// One animation step per display frame. CVDisplayLink runs at the panel rate
// even for a background helper, so a 120 Hz display gets 120 events/s.
static CVReturn scroll_frame(CVDisplayLinkRef link, const CVTimeStamp *now,
                             const CVTimeStamp *output, CVOptionFlags flags_in,
                             CVOptionFlags *flags_out, void *ctx) {
    (void)link; (void)now; (void)output; (void)flags_in; (void)flags_out; (void)ctx;
    pthread_mutex_lock(&g_scroll_lock);
    if (g_pending_y == 0 && g_pending_x == 0) {
        pthread_mutex_unlock(&g_scroll_lock);
        return kCVReturnSuccess;
    }
    int dy = take_slice(g_pending_y);
    int dx = take_slice(g_pending_x);
    g_pending_y -= dy;
    g_pending_x -= dx;
    int phase = g_scrolling ? kCGScrollPhaseChanged : kCGScrollPhaseBegan;
    g_scrolling = 1;
    int done = g_pending_y == 0 && g_pending_x == 0;
    pthread_mutex_unlock(&g_scroll_lock);

    post_scroll(dy, dx, phase);
    if (done) {
        post_scroll(0, 0, kCGScrollPhaseEnded);
        pthread_mutex_lock(&g_scroll_lock);
        if (g_pending_y == 0 && g_pending_x == 0) g_scrolling = 0;
        pthread_mutex_unlock(&g_scroll_lock);
    }
    return kCVReturnSuccess;
}

// Queue one wheel command. Several notches accumulate, so a fast spin scrolls
// faster instead of starting a new animation each time.
static void queue_wheel(int axis, int steps) {
    if (steps == 0) return;
    // The client sends the wheel notch as the desktop reported it; some mice and
    // drivers (and "natural scrolling" preferences) end up inverted on macOS.
    if (g_wheel_invert) steps = -steps;
    if (!g_smooth_scroll || !g_display_link) {
        post_wheel_lines(axis, steps);
        return;
    }
    int px = steps * g_wheel_pixels;
    display_link_start();
    if (!g_display_link_running) {
        post_wheel_lines(axis, steps);
        return;
    }
    pthread_mutex_lock(&g_scroll_lock);
    g_last_wheel_ns = now_ns();
    if (axis == 0) {
        g_pending_y += -px;
    } else {
        g_pending_x += px;
    }
    pthread_mutex_unlock(&g_scroll_lock);
}

int main(void) {
    fprintf(stderr, "lgx-cginput: accessibility=%s pid=%d\n",
            AXIsProcessTrusted() ? "granted" : "MISSING", (int)getpid());

    const char *display_env = getenv("LGX_DISPLAY_ID");
    if (display_env && display_env[0]) {
        unsigned long id = strtoul(display_env, NULL, 10);
        if (id != 0) g_display_id = (CGDirectDisplayID)id;
    }
    fprintf(stderr, "lgx-cginput: display id %u\n", (unsigned)g_display_id);

    const char *smooth = getenv("LGX_SMOOTH_SCROLL");
    if (smooth && smooth[0] == '0') g_smooth_scroll = 0;
    const char *pixels = getenv("LGX_WHEEL_PIXELS");
    if (pixels && atoi(pixels) > 0) g_wheel_pixels = atoi(pixels);
    const char *invert = getenv("LGX_WHEEL_INVERT");
    if (invert && invert[0] == '1') g_wheel_invert = 1;
    if (g_smooth_scroll) {
        if (CVDisplayLinkCreateWithActiveCGDisplays(&g_display_link) == kCVReturnSuccess
            && CVDisplayLinkSetOutputCallback(g_display_link, scroll_frame, NULL) == kCVReturnSuccess) {
            if (pthread_create(&g_watchdog, NULL, scroll_watchdog, NULL) == 0) {
                fprintf(stderr, "lgx-cginput: smooth scroll on demand, %d px per notch%s\n",
                        g_wheel_pixels, g_wheel_invert ? ", inverted" : "");
            } else {
                CVDisplayLinkRelease(g_display_link);
                g_display_link = NULL;
                g_smooth_scroll = 0;
                fprintf(stderr, "lgx-cginput: smooth scroll thread failed, using line events\n");
            }
        } else {
            if (g_display_link) {
                CVDisplayLinkRelease(g_display_link);
                g_display_link = NULL;
            }
            g_smooth_scroll = 0;
            fprintf(stderr, "lgx-cginput: smooth scroll unavailable, using line events\n");
        }
    }

    char line[256];
    while (fgets(line, sizeof line, stdin)) {
        char cmd[32];
        int a = 0, b = 0;
        if (sscanf(line, "%31s %d %d", cmd, &a, &b) < 2) continue;
        logf_("cmd=%s a=%d b=%d", cmd, a, b);
        if (strcmp(cmd, "key") == 0) {
            post_key((unsigned short)a, b);
        } else if (strcmp(cmd, "btn") == 0) {
            post_button((unsigned short)a, b);
        } else if (strcmp(cmd, "motion") == 0) {
            post_motion(a, b);
        } else if (strcmp(cmd, "wheel") == 0) {
            queue_wheel(a, b);
        }
    }
    g_watchdog_run = 0;
    if (g_smooth_scroll && g_display_link) {
        pthread_join(g_watchdog, NULL);
    }
    display_link_stop();
    if (g_display_link) {
        if (g_scrolling) {
            post_scroll(0, 0, kCGScrollPhaseEnded);
            g_scrolling = 0;
        }
        CVDisplayLinkRelease(g_display_link);
        g_display_link = NULL;
    }
    return 0;
}
