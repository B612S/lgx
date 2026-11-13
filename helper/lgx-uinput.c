/* lgx-uinput: kernel-level virtual keyboard + mouse (uinput) for the guest.
 *
 * Works like Sunshine's input: a real input device, so every key and every
 * compositor shortcut works regardless of Wayland protocol quirks. Runs as
 * root; the server talks to it over a unix socket.
 *
 * Wire format (little endian):
 *   'K' u16 code u8 down     key event (evdev key code)
 *   'B' u16 code u8 down     mouse button
 *   'M' i32 dx i32 dy        relative motion
 *   'W' i32 steps            wheel
 *   'Q'                      quit
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/uinput.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

static int emit(int fd, int type, int code, int32_t value) {
    struct input_event ev = {0};
    ev.type = (uint16_t)type;
    ev.code = (uint16_t)code;
    ev.value = value;
    return write(fd, &ev, sizeof(ev)) == sizeof(ev) ? 0 : -1;
}

static int sync_event(int fd) {
    return emit(fd, EV_SYN, SYN_REPORT, 0);
}

static int setup_keyboard(void) {
    int fd = open("/dev/uinput", O_WRONLY | O_NONBLOCK);
    if (fd < 0) { perror("open /dev/uinput"); return -1; }

    ioctl(fd, UI_SET_EVBIT, EV_KEY);
    ioctl(fd, UI_SET_EVBIT, EV_SYN);
    ioctl(fd, UI_SET_EVBIT, EV_LED);
    for (int k = 1; k <= 248; k++) {
        if (ioctl(fd, UI_SET_KEYBIT, k) < 0) continue;
    }
    /* reserved/unused ranges would make the device look odd, but enabling the
     * whole range keeps every keyboard shortcut available */
    for (int k = 0x160; k <= 0x16f; k++) ioctl(fd, UI_SET_KEYBIT, k);

    struct uinput_setup usetup;
    memset(&usetup, 0, sizeof(usetup));
    usetup.id.bustype = BUS_USB;
    usetup.id.vendor = 0x1234;
    usetup.id.product = 0x0001;
    strcpy(usetup.name, "lgx virtual keyboard");
    if (ioctl(fd, UI_DEV_SETUP, &usetup) < 0) { perror("UI_DEV_SETUP"); close(fd); return -1; }
    if (ioctl(fd, UI_DEV_CREATE) < 0) { perror("UI_DEV_CREATE"); close(fd); return -1; }
    return fd;
}

#define MAX_PADS 4

static const int gamepad_buttons[25] = {
    BTN_SOUTH, BTN_EAST, BTN_NORTH, BTN_WEST,
    BTN_SELECT, BTN_MODE, BTN_START, BTN_THUMBL, BTN_THUMBR,
    BTN_TL, BTN_TR, BTN_DPAD_UP, BTN_DPAD_DOWN, BTN_DPAD_LEFT, BTN_DPAD_RIGHT,
    BTN_TRIGGER_HAPPY1, BTN_TRIGGER_HAPPY2, BTN_TRIGGER_HAPPY3,
    BTN_TRIGGER_HAPPY4, BTN_TRIGGER_HAPPY5, BTN_TRIGGER_HAPPY6,
    BTN_TRIGGER_HAPPY7, BTN_TRIGGER_HAPPY8, BTN_TRIGGER_HAPPY9,
    BTN_TRIGGER_HAPPY10,
};

static int setup_gamepad(int idx) {
    int fd = open("/dev/uinput", O_WRONLY | O_NONBLOCK);
    if (fd < 0) { perror("open /dev/uinput gamepad"); return -1; }

    ioctl(fd, UI_SET_EVBIT, EV_KEY);
    ioctl(fd, UI_SET_EVBIT, EV_ABS);
    ioctl(fd, UI_SET_EVBIT, EV_SYN);
    for (unsigned i = 0; i < sizeof(gamepad_buttons) / sizeof(gamepad_buttons[0]); i++) {
        ioctl(fd, UI_SET_KEYBIT, gamepad_buttons[i]);
    }
    const int axes[] = { ABS_X, ABS_Y, ABS_RX, ABS_RY, ABS_Z, ABS_RZ, ABS_HAT0X, ABS_HAT0Y };
    for (unsigned i = 0; i < sizeof(axes) / sizeof(axes[0]); i++) {
        ioctl(fd, UI_SET_ABSBIT, axes[i]);
    }

    struct uinput_setup usetup;
    memset(&usetup, 0, sizeof(usetup));
    usetup.id.bustype = BUS_USB;
    usetup.id.vendor = 0x1234;
    usetup.id.product = (uint16_t)(0x0003 + idx);
    snprintf(usetup.name, sizeof(usetup.name), "lgx virtual gamepad %d", idx + 1);
    if (ioctl(fd, UI_DEV_SETUP, &usetup) < 0) { perror("UI_DEV_SETUP gamepad"); close(fd); return -1; }

    for (unsigned i = 0; i < sizeof(axes) / sizeof(axes[0]); i++) {
        struct uinput_abs_setup a = {0};
        a.code = (uint16_t)axes[i];
        if (axes[i] == ABS_HAT0X || axes[i] == ABS_HAT0Y) {
            a.absinfo.minimum = -1;
            a.absinfo.maximum = 1;
        } else if (axes[i] == ABS_Z || axes[i] == ABS_RZ) {
            a.absinfo.minimum = 0;
            a.absinfo.maximum = 32767;
        } else {
            a.absinfo.minimum = -32768;
            a.absinfo.maximum = 32767;
        }
        ioctl(fd, UI_ABS_SETUP, &a);
    }
    if (ioctl(fd, UI_DEV_CREATE) < 0) { perror("UI_DEV_CREATE gamepad"); close(fd); return -1; }
    fprintf(stderr, "uinput: gamepad %d created (fd %d)\n", idx + 1, fd);
    return fd;
}

static int setup_mouse(void) {
    int fd = open("/dev/uinput", O_WRONLY | O_NONBLOCK);
    if (fd < 0) { perror("open /dev/uinput"); return -1; }

    ioctl(fd, UI_SET_EVBIT, EV_KEY);
    ioctl(fd, UI_SET_EVBIT, EV_REL);
    ioctl(fd, UI_SET_EVBIT, EV_SYN);
    ioctl(fd, UI_SET_KEYBIT, BTN_LEFT);
    ioctl(fd, UI_SET_KEYBIT, BTN_RIGHT);
    ioctl(fd, UI_SET_KEYBIT, BTN_MIDDLE);
    ioctl(fd, UI_SET_KEYBIT, BTN_SIDE);
    ioctl(fd, UI_SET_KEYBIT, BTN_EXTRA);
    ioctl(fd, UI_SET_RELBIT, REL_X);
    ioctl(fd, UI_SET_RELBIT, REL_Y);
    ioctl(fd, UI_SET_RELBIT, REL_WHEEL);
    ioctl(fd, UI_SET_RELBIT, REL_HWHEEL);
    ioctl(fd, UI_SET_EVBIT, EV_ABS);
    ioctl(fd, UI_SET_ABSBIT, ABS_X);
    ioctl(fd, UI_SET_ABSBIT, ABS_Y);
    ioctl(fd, UI_SET_PROPBIT, INPUT_PROP_DIRECT);

    struct uinput_setup usetup;
    memset(&usetup, 0, sizeof(usetup));
    usetup.id.bustype = BUS_USB;
    usetup.id.vendor = 0x1234;
    usetup.id.product = 0x0002;
    strcpy(usetup.name, "lgx virtual mouse");
    if (ioctl(fd, UI_DEV_SETUP, &usetup) < 0) { perror("UI_DEV_SETUP mouse"); close(fd); return -1; }
    struct uinput_abs_setup absx = {0};
    absx.code = ABS_X;
    absx.absinfo.minimum = 0;
    absx.absinfo.maximum = 65535;
    ioctl(fd, UI_ABS_SETUP, &absx);
    struct uinput_abs_setup absy = {0};
    absy.code = ABS_Y;
    absy.absinfo.minimum = 0;
    absy.absinfo.maximum = 65535;
    ioctl(fd, UI_ABS_SETUP, &absy);
    if (ioctl(fd, UI_DEV_CREATE) < 0) { perror("UI_DEV_CREATE mouse"); close(fd); return -1; }
    return fd;
}

static int read_all(int fd, void *buf, size_t len) {
    size_t got = 0;
    while (got < len) {
        ssize_t n = recv(fd, (char *)buf + got, len - got, 0);
        if (n <= 0) return -1;
        got += (size_t)n;
    }
    return 0;
}

int main(int argc, char **argv) {
    const char *path = argc > 1 ? argv[1] : "/run/lgx-input.sock";
    int kbd = setup_keyboard();
    int mouse = setup_mouse();
    if (kbd < 0 || mouse < 0) return 1;
    /* Gamepad devices are created lazily on the first connected snapshot so
     * the guest does not get four empty pads when one controller is used. */
    int pads[MAX_PADS];
    for (int i = 0; i < MAX_PADS; i++) pads[i] = -1;
    /* Capability marker: the server only sends gamepad packets to a helper
     * that understands them. Remove it again on exit. */
    int cap = open("/run/lgx-input.gamepad", O_CREAT | O_WRONLY | O_TRUNC, 0666);
    if (cap >= 0) { write(cap, "1", 1); close(cap); }
    uint32_t last_buttons[MAX_PADS] = {0};
    int16_t last_axes[MAX_PADS][6] = {{0}};
    int8_t last_hat[MAX_PADS][2] = {{0}};
    usleep(300000);

    unlink(path);
    int lsock = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un addr = {0};
    addr.sun_family = AF_UNIX;
    snprintf(addr.sun_path, sizeof(addr.sun_path), "%s", path);
    if (bind(lsock, (struct sockaddr *)&addr, sizeof(addr)) < 0) { perror("bind"); return 1; }
    chmod(path, 0666);
    listen(lsock, 4);

    for (;;) {
        int c = accept(lsock, NULL, NULL);
        if (c < 0) { if (errno == EINTR) continue; break; }
        fprintf(stderr, "uinput: client connected\n");
        for (;;) {
            char cmd;
            if (read_all(c, &cmd, 1) < 0) break;
            if (cmd == 'Q') {
                close(c);
                close(lsock);
                unlink(path);
                close(kbd);
                close(mouse);
                for (int i = 0; i < MAX_PADS; i++) if (pads[i] >= 0) close(pads[i]);
                unlink("/run/lgx-input.gamepad");
                return 0;
            }
            if (cmd == 'K' || cmd == 'B') {
                uint8_t rest[3];
                if (read_all(c, rest, 3) < 0) break;
                uint16_t code = (uint16_t)(rest[0] | (rest[1] << 8));
                int down = rest[2];
                int r = emit(cmd == 'K' ? kbd : mouse, EV_KEY, code, down);
                int r2 = sync_event(cmd == 'K' ? kbd : mouse);
                fprintf(stderr, "uinput: key code=%u down=%d write=%d sync=%d errno=%d\n", code, down, r, r2, errno);
            } else if (cmd == 'M') {
                int32_t d[2];
                if (read_all(c, d, sizeof(d)) < 0) break;
                emit(mouse, EV_REL, REL_X, d[0]);
                emit(mouse, EV_REL, REL_Y, d[1]);
                sync_event(mouse);
            } else if (cmd == 'A') {
                int32_t p[2];
                if (read_all(c, p, sizeof(p)) < 0) break;
                emit(mouse, EV_ABS, ABS_X, p[0]);
                emit(mouse, EV_ABS, ABS_Y, p[1]);
                sync_event(mouse);
            } else if (cmd == 'W') {
                int32_t steps;
                if (read_all(c, &steps, sizeof(steps)) < 0) break;
                emit(mouse, EV_REL, REL_WHEEL, steps);
                sync_event(mouse);
            } else if (cmd == 'G') {
                uint8_t g[20];
                if (read_all(c, g, sizeof(g)) < 0) break;
                int pi = g[0];
                if (pi < 0 || pi >= MAX_PADS) continue;
                int connected = g[1];
                if (connected && pads[pi] < 0) pads[pi] = setup_gamepad(pi);
                if (pads[pi] < 0) continue;
                uint32_t buttons = (uint32_t)g[2] | ((uint32_t)g[3] << 8) |
                                   ((uint32_t)g[4] << 16) | ((uint32_t)g[5] << 24);
                int16_t ax[6];
                for (int i = 0; i < 6; i++) ax[i] = (int16_t)(g[6 + i * 2] | (g[7 + i * 2] << 8));
                int8_t hx = (int8_t)g[18], hy = (int8_t)g[19];
                if (!connected) {
                    buttons = 0;
                    for (int i = 0; i < 6; i++) ax[i] = 0;
                    hx = hy = 0;
                }
                int dirty = 0;
                uint32_t changed = buttons ^ last_buttons[pi];
                for (unsigned i = 0; i < sizeof(gamepad_buttons) / sizeof(gamepad_buttons[0]); i++) {
                    if (changed & (1u << i)) {
                        emit(pads[pi], EV_KEY, gamepad_buttons[i], (buttons >> i) & 1);
                        dirty = 1;
                    }
                }
                last_buttons[pi] = buttons;
                const int axis_codes[6] = { ABS_X, ABS_Y, ABS_RX, ABS_RY, ABS_Z, ABS_RZ };
                for (int i = 0; i < 6; i++) {
                    if (last_axes[pi][i] != ax[i]) {
                        emit(pads[pi], EV_ABS, axis_codes[i], ax[i]);
                        last_axes[pi][i] = ax[i];
                        dirty = 1;
                    }
                }
                if (last_hat[pi][0] != hx) {
                    emit(pads[pi], EV_ABS, ABS_HAT0X, hx);
                    last_hat[pi][0] = hx;
                    dirty = 1;
                }
                if (last_hat[pi][1] != hy) {
                    emit(pads[pi], EV_ABS, ABS_HAT0Y, hy);
                    last_hat[pi][1] = hy;
                    dirty = 1;
                }
                if (dirty) sync_event(pads[pi]);
            }
        }
        close(c);
    }
    return 0;
}
