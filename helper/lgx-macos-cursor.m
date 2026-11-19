// lgx-macos-cursor: send the current system cursor shape to the server.
//
// The client draws the pointer locally so it does not trail the video. That
// local cursor cannot know the guest's cursor shape, so a resize, move or wait
// cursor stayed an arrow. This helper polls NSCursor.currentSystemCursor in
// the GUI session, crops the transparent border and writes one RGBA frame on
// stdout whenever the shape changes.
//
// Frame: u32 big-endian length, then u32 LE width, height, hotspot_x,
// hotspot_y, followed by width*height RGBA pixels (top-down, straight alpha).
#import <AppKit/AppKit.h>
#include <arpa/inet.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

static uint32_t last_hash = 0;

static uint32_t hash_bytes(const uint8_t *p, size_t n) {
    uint32_t h = 2166136261u;
    for (size_t i = 0; i < n; i++) {
        h ^= p[i];
        h *= 16777619u;
    }
    return h;
}

static NSData *cursor_payload(void) {
    NSCursor *cursor = [NSCursor currentSystemCursor];
    if (!cursor) cursor = [NSCursor arrowCursor];
    NSImage *image = cursor.image;
    NSSize size = image.size;
    if (size.width < 1 || size.height < 1) return nil;

    NSRect proposed = NSMakeRect(0, 0, size.width, size.height);
    CGImageRef cg = [image CGImageForProposedRect:&proposed context:NULL hints:NULL];
    if (!cg) return nil;
    size_t w = CGImageGetWidth(cg);
    size_t h = CGImageGetHeight(cg);
    if (w < 1 || h < 1 || w > 256 || h > 256) return nil;

    size_t stride = w * 4;
    uint8_t *px = calloc(h, stride);
    if (!px) return nil;
    CGColorSpaceRef cs = CGColorSpaceCreateDeviceRGB();
    CGContextRef ctx = CGBitmapContextCreate(px, w, h, 8, stride, cs,
        kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big);
    CGColorSpaceRelease(cs);
    if (!ctx) {
        free(px);
        return nil;
    }
    CGContextDrawImage(ctx, CGRectMake(0, 0, w, h), cg);
    CGContextRelease(ctx);

    // CG blits premultiplied; SDL cursors want straight alpha.
    for (size_t i = 0; i < w * h; i++) {
        uint8_t *p = px + i * 4;
        unsigned a = p[3];
        if (a != 0 && a != 255) {
            unsigned r = ((unsigned)p[0] * 255 + a / 2) / a;
            unsigned g = ((unsigned)p[1] * 255 + a / 2) / a;
            unsigned b = ((unsigned)p[2] * 255 + a / 2) / a;
            p[0] = (uint8_t)(r > 255 ? 255 : r);
            p[1] = (uint8_t)(g > 255 ? 255 : g);
            p[2] = (uint8_t)(b > 255 ? 255 : b);
        }
    }

    int minx = (int)w, miny = (int)h, maxx = -1, maxy = -1;
    for (size_t y = 0; y < h; y++) {
        for (size_t x = 0; x < w; x++) {
            if (px[y * stride + x * 4 + 3] != 0) {
                if ((int)x < minx) minx = (int)x;
                if ((int)x > maxx) maxx = (int)x;
                if ((int)y < miny) miny = (int)y;
                if ((int)y > maxy) maxy = (int)y;
            }
        }
    }
    if (maxx < minx || maxy < miny) {
        free(px);
        return nil;
    }

    int x0 = minx > 0 ? minx - 1 : 0;
    int y0 = miny > 0 ? miny - 1 : 0;
    int x1 = maxx + 1 < (int)w ? maxx + 1 : (int)w - 1;
    int y1 = maxy + 1 < (int)h ? maxy + 1 : (int)h - 1;
    int cw = x1 - x0 + 1, ch = y1 - y0 + 1;

    CGFloat sx = size.width > 0 ? (CGFloat)w / size.width : 1;
    CGFloat sy = size.height > 0 ? (CGFloat)h / size.height : 1;
    int hx = (int)llround(cursor.hotSpot.x * sx) - x0;
    int hy = (int)llround(cursor.hotSpot.y * sy) - y0;
    if (hx < 0) hx = 0;
    if (hx >= cw) hx = cw - 1;
    if (hy < 0) hy = 0;
    if (hy >= ch) hy = ch - 1;

    NSMutableData *out = [NSMutableData dataWithCapacity:16 + (NSUInteger)cw * ch * 4];
    uint32_t header[4] = {(uint32_t)cw, (uint32_t)ch, (uint32_t)hx, (uint32_t)hy};
    [out appendBytes:header length:sizeof header];
    for (int y = y0; y <= y1; y++) {
        [out appendBytes:&px[y * stride + x0 * 4] length:(NSUInteger)cw * 4];
    }
    free(px);
    return out;
}

static void poll_cursor(void) {
    @autoreleasepool {
        NSData *payload = cursor_payload();
        if (!payload) return;
        uint32_t hash = hash_bytes(payload.bytes, payload.length);
        if (hash == last_hash) return;
        last_hash = hash;
        uint32_t len = htonl((uint32_t)payload.length);
        if (fwrite(&len, 1, 4, stdout) != 4) exit(1);
        if (fwrite(payload.bytes, 1, payload.length, stdout) != payload.length) exit(1);
        fflush(stdout);
    }
}

int main(void) {
    poll_cursor();
    NSTimer *timer = [NSTimer timerWithTimeInterval:0.05
                                            repeats:YES
                                              block:^(NSTimer *t) {
        (void)t;
        poll_cursor();
    }];
    [[NSRunLoop currentRunLoop] addTimer:timer forMode:NSRunLoopCommonModes];
    [[NSRunLoop currentRunLoop] run];
    return 0;
}
