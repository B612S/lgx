// lgx-media: media/volume/brightness keys as NX_SYSDEFINED events.
//
// A CGEvent keyboard event with a media keycode does not reach the system
// media handler; macOS expects the auxiliary-control event produced by NSEvent
// with subtype 8. data1 packs the NX_KEYTYPE_* code and the down/up state;
// data2 is -1 for these buttons.
//
// Both events are required: without the up event macOS treats the key as held
// and volume ramps to the maximum. The media handler acts once per press.
#import <AppKit/AppKit.h>

void lgx_post_media_key(int key, int down) {
    NSEvent *event = [NSEvent otherEventWithType:NSEventTypeSystemDefined
                                        location:NSZeroPoint
                                   modifierFlags:down ? 0xa00 : 0xb00
                                       timestamp:0
                                    windowNumber:0
                                         context:nil
                                         subtype:8
                                           data1:(key << 16) | ((down ? 0xa : 0xb) << 8)
                                           data2:-1];
    if (!event) return;
    CGEventRef ev = [event CGEvent];
    if (!ev) return;
    CGEventPost(kCGHIDEventTap, ev);
}
