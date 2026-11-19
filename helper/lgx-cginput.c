// lgx-cginput: evdev codes from the lgx client -> CGEvent, no kernel driver.
//
// C on purpose: input injection is a plain C API, and the Swift variant tripped
// over the Swift runtime (SIGTRAP before the first line). One AppKit call in
// lgx-frontmost.m names the frontmost application, which function keys need
// for direct delivery. Mirrors the shape of helper/lgx-uinput.c on Linux.
//
// One command per line on stdin:
//   key <evdev> <0|1>      key down/up
//   btn <evdev> <0|1>      mouse button (0x110 left, 0x111 right, 0x112 middle)
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
#include <pthread.h>
#include <stdarg.h>
#include <sys/types.h>
#include <time.h>
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

// Implemented in lgx-frontmost.m (AppKit). Used for function keys only; see
// post_key().
extern pid_t lgx_frontmost_pid(void);

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

static void post_button(unsigned short evdev, int down) {
    CGMouseButton button;
    CGEventType type;
    switch (evdev) {
        case 0x110: button = kCGMouseButtonLeft; type = down ? kCGEventLeftMouseDown : kCGEventLeftMouseUp; break;
        case 0x111: button = kCGMouseButtonRight; type = down ? kCGEventRightMouseDown : kCGEventRightMouseUp; break;
        case 0x112: button = kCGMouseButtonCenter; type = down ? kCGEventOtherMouseDown : kCGEventOtherMouseUp; break;
        default: return;
    }
    CGEventRef cur = CGEventCreate(NULL);
    CGPoint loc = cur ? CGEventGetLocation(cur) : CGPointZero;
    if (cur) CFRelease(cur);
    CGEventSourceRef src = CGEventSourceCreate(kCGEventSourceStateHIDSystemState);
    CGEventRef ev = CGEventCreateMouseEvent(src, type, loc, button);
    if (ev) {
        // Synthetic clicks are only honoured as real ones when the button number
        // and click state are explicit; without them some apps treat them as
        // secondary/ignored.
        CGEventSetIntegerValueField(ev, kCGMouseEventButtonNumber, (int64_t)button);
        CGEventSetIntegerValueField(ev, kCGMouseEventClickState, 1);
        // A stuck modifier (typically Control) turns a left click into a right
        // click on macOS; synthetic mouse events must not inherit stale flags.
        CGEventSetFlags(ev, 0);
    }
    logf_("  -> post type=%d button=%d ev=%p down=%d flags=0x%llx", (int)type, (int)button,
          (void *)ev, down, (unsigned long long)CGEventSourceFlagsState(kCGEventSourceStateHIDSystemState));
    if (ev) {
        CGEventPost(kCGHIDEventTap, ev);
        CFRelease(ev);
    }
    if (src) CFRelease(src);
    if (down) {
        dragging = button == kCGMouseButtonLeft ? 1 : button == kCGMouseButtonRight ? 2 : 3;
    } else {
        dragging = 0;
    }
}

static void post_motion(int nx, int ny) {
    CGRect screen = CGDisplayBounds(CGMainDisplayID());
    CGPoint pt;
    pt.x = screen.origin.x + (CGFloat)nx / 65535.0 * screen.size.width;
    pt.y = screen.origin.y + (CGFloat)ny / 65535.0 * screen.size.height;
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
        moveButton = kCGMouseButtonCenter;
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

// A mouse wheel delivers one event per notch, and posting one line-based event
// per notch makes the guest jump; a trackpad instead sends a stream of pixel
// events that every AppKit/Chromium app already scrolls smoothly. So a notch is
// expanded into a short ease-out animation of pixel events.
static pthread_mutex_t g_scroll_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t g_scroll_cond = PTHREAD_COND_INITIALIZER;
static int g_pending_y = 0;   // pixels still to deliver, signed
static int g_pending_x = 0;
static int g_scroll_stop = 0;
static int g_smooth_scroll = 1;
static int g_wheel_pixels = 48;   // one notch, in pixels
static int g_wheel_invert = 0;    // 1 = keep the natural-scroll direction

static void post_scroll(int32_t dy, int32_t dx) {
    CGEventSourceRef src = CGEventSourceCreate(kCGEventSourceStateHIDSystemState);
    CGEventRef ev = CGEventCreateScrollWheelEvent(src, kCGScrollEventUnitPixel, 2, dy, dx);
    if (ev) {
        // Without this the event is treated as a discrete wheel click and the
        // app scrolls in whole lines again.
        CGEventSetIntegerValueField(ev, kCGScrollWheelEventIsContinuous, 1);
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
// tail is short, which is the ease-out a trackpad produces.
static int take_slice(int pending) {
    int mag = pending < 0 ? -pending : pending;
    int step = mag / 4;
    if (step < 1) step = 1;
    if (step > 60) step = 60;
    return pending < 0 ? -step : step;
}

static void *scroll_worker(void *arg) {
    (void)arg;
    for (;;) {
        pthread_mutex_lock(&g_scroll_lock);
        while (g_pending_y == 0 && g_pending_x == 0 && !g_scroll_stop) {
            pthread_cond_wait(&g_scroll_cond, &g_scroll_lock);
        }
        if (g_scroll_stop) {
            pthread_mutex_unlock(&g_scroll_lock);
            return NULL;
        }
        int dy = take_slice(g_pending_y);
        int dx = take_slice(g_pending_x);
        g_pending_y -= dy;
        g_pending_x -= dx;
        pthread_mutex_unlock(&g_scroll_lock);

        post_scroll(dy, dx);
        // 8 ms per step: one notch is delivered in about 80 ms, which keeps a
        // spinning wheel from building a backlog.
        struct timespec ts = {0, 8L * 1000 * 1000};
        nanosleep(&ts, NULL);
    }
}

// Queue one wheel command. Several notches accumulate, so a fast spin scrolls
// faster instead of starting a new animation each time.
static void queue_wheel(int axis, int steps) {
    if (steps == 0) return;
    // The client sends the wheel notch as the desktop reported it; some mice and
    // drivers (and "natural scrolling" preferences) end up inverted on macOS.
    if (g_wheel_invert) steps = -steps;
    if (!g_smooth_scroll) {
        post_wheel_lines(axis, steps);
        return;
    }
    int px = steps * g_wheel_pixels;
    pthread_mutex_lock(&g_scroll_lock);
    if (axis == 0) {
        g_pending_y += -px;
    } else {
        g_pending_x += px;
    }
    pthread_cond_signal(&g_scroll_cond);
    pthread_mutex_unlock(&g_scroll_lock);
}

int main(void) {
    fprintf(stderr, "lgx-cginput: accessibility=%s pid=%d\n",
            AXIsProcessTrusted() ? "granted" : "MISSING", (int)getpid());

    const char *smooth = getenv("LGX_SMOOTH_SCROLL");
    if (smooth && smooth[0] == '0') g_smooth_scroll = 0;
    const char *pixels = getenv("LGX_WHEEL_PIXELS");
    if (pixels && atoi(pixels) > 0) g_wheel_pixels = atoi(pixels);
    const char *invert = getenv("LGX_WHEEL_INVERT");
    if (invert && invert[0] == '1') g_wheel_invert = 1;
    if (g_smooth_scroll) {
        pthread_t worker;
        if (pthread_create(&worker, NULL, scroll_worker, NULL) == 0) {
            pthread_detach(worker);
            fprintf(stderr, "lgx-cginput: smooth scroll, %d px per notch%s\n",
                    g_wheel_pixels, g_wheel_invert ? ", inverted" : "");
        } else {
            g_smooth_scroll = 0;
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
    return 0;
}
