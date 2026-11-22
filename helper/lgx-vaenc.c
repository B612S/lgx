/* lgx-vaenc: VAAPI HEVC encoder that hands out its NV12 surfaces as dma-bufs
 * so the caller can render into them with GL (zero copy).
 *
 * Usage: lgx-vaenc SOCKET WIDTH HEIGHT FPS BITRATE [import]
 *
 * Set LGX_LOW_POWER=0 to disable Intel VDENC low-power mode.
 *
 * Socket protocol (AF_UNIX, caller connects to argv[1]):
 *   helper -> caller: u32 nframes
 *                     per frame: u64 modifier, u32 width, u32 height,
 *                                u32 fourcc, u32 nplanes,
 *                                per plane: u32 offset, u32 pitch, one fd
 *   caller -> helper: u8 cmd
 *                     'E' + u32 index  -> encodes that surface
 *                     'K'              -> force the next frame to be an IDR
 *                     'Q'              -> quit
 *   helper -> caller: u32 len + bitstream bytes (answer to 'E')
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <sys/un.h>
#include <unistd.h>

#include <libavcodec/avcodec.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_vaapi.h>
#include <libavutil/hwcontext_drm.h>
#include <libavutil/opt.h>
#include <libavutil/pixfmt.h>
#include <va/va.h>
#include <va/va_drmcommon.h>
#include <va/va_vpp.h>

#ifndef DRM_FORMAT_ARGB8888
#define DRM_FORMAT_ARGB8888 0x34325241
#endif
#ifndef DRM_FORMAT_XRGB8888
#define DRM_FORMAT_XRGB8888 0x34325258
#endif

#define MAX_FRAMES 4

static int use_vpp;
static VAConfigID vpp_cfg;
static VAContextID vpp_ctx;
static VASurfaceID vpp_out;
struct VppInput {
    uint32_t id;
    VASurfaceID surf;
    int w, h, stride;
    uint32_t fourcc;
    uint64_t modifier;
    int offset;
};
#define MAX_VPP_INPUTS 8
static struct VppInput vpp_inputs[MAX_VPP_INPUTS];
static int vpp_ninputs;
/// Milliseconds are too fine for a failure that repeats every frame; one line
/// per second is enough to diagnose without flooding the journal.
static time_t last_import_error;

static int vpp_init(VADisplay dpy, VASurfaceID out, int w, int h) {
    VAConfigAttrib ca = { VAConfigAttribRTFormat, VA_RT_FORMAT_YUV420 };
    VAStatus s = vaCreateConfig(dpy, VAProfileNone, VAEntrypointVideoProc, &ca, 1, &vpp_cfg);
    if (s != VA_STATUS_SUCCESS) {
        fprintf(stderr, "vaenc: vpp config failed: %s\n", vaErrorStr(s));
        return -1;
    }
    s = vaCreateContext(dpy, vpp_cfg, w, h, 0, &out, 1, &vpp_ctx);
    if (s != VA_STATUS_SUCCESS) {
        fprintf(stderr, "vaenc: vpp context failed: %s\n", vaErrorStr(s));
        return -1;
    }
    vpp_out = out;
    return 0;
}

static void vpp_drop_input(VADisplay dpy, int i) {
    vaDestroySurfaces(dpy, &vpp_inputs[i].surf, 1);
    vpp_ninputs--;
    memmove(&vpp_inputs[i], &vpp_inputs[i + 1],
            (size_t)(vpp_ninputs - i) * sizeof(vpp_inputs[0]));
}

static VASurfaceID vpp_import(VADisplay dpy, uint32_t id, int w, int h, int stride,
                              uint32_t fourcc, uint64_t modifier, int offset, int fd) {
    for (int i = 0; i < vpp_ninputs; i++) {
        if (vpp_inputs[i].id == id && vpp_inputs[i].w == w && vpp_inputs[i].h == h &&
            vpp_inputs[i].stride == stride && vpp_inputs[i].fourcc == fourcc &&
            vpp_inputs[i].modifier == modifier && vpp_inputs[i].offset == offset) {
            close(fd);
            // LRU: the most recently used entry moves to the end.
            struct VppInput hit = vpp_inputs[i];
            memmove(&vpp_inputs[i], &vpp_inputs[i + 1],
                    (size_t)(vpp_ninputs - i - 1) * sizeof(vpp_inputs[0]));
            vpp_inputs[vpp_ninputs - 1] = hit;
            return hit.surf;
        }
        if (vpp_inputs[i].id == id) {
            vpp_drop_input(dpy, i);
            break;
        }
    }
    VADRMPRIMESurfaceDescriptor desc;
    memset(&desc, 0, sizeof(desc));
    desc.fourcc = fourcc;
    desc.width = (uint32_t)w;
    desc.height = (uint32_t)h;
    desc.num_objects = 1;
    desc.objects[0].fd = fd;
    desc.objects[0].size = (uint64_t)offset + (uint64_t)stride * (uint64_t)h;
    desc.objects[0].drm_format_modifier = modifier;
    desc.num_layers = 1;
    desc.layers[0].drm_format = fourcc;
    desc.layers[0].num_planes = 1;
    desc.layers[0].object_index[0] = 0;
    desc.layers[0].offset[0] = (uint32_t)offset;
    desc.layers[0].pitch[0] = (uint32_t)stride;

    VASurfaceAttrib attrs[3];
    memset(attrs, 0, sizeof(attrs));
    attrs[0].type = VASurfaceAttribMemoryType;
    attrs[0].flags = VA_SURFACE_ATTRIB_SETTABLE;
    attrs[0].value.type = VAGenericValueTypeInteger;
    attrs[0].value.value.i = VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2;
    attrs[1].type = VASurfaceAttribExternalBufferDescriptor;
    attrs[1].flags = VA_SURFACE_ATTRIB_SETTABLE;
    attrs[1].value.type = VAGenericValueTypePointer;
    attrs[1].value.value.p = &desc;
    attrs[2].type = VASurfaceAttribPixelFormat;
    attrs[2].flags = VA_SURFACE_ATTRIB_SETTABLE;
    attrs[2].value.type = VAGenericValueTypeInteger;
    attrs[2].value.value.i =
        fourcc == DRM_FORMAT_ARGB8888 ? VA_FOURCC_ARGB :
        fourcc == DRM_FORMAT_XRGB8888 ? VA_FOURCC_BGRX : VA_FOURCC_ARGB;

    VASurfaceID surf = VA_INVALID_SURFACE;
    VAStatus s = vaCreateSurfaces(dpy, VA_RT_FORMAT_RGB32, (unsigned)w, (unsigned)h, &surf, 1, attrs, 3);
    close(fd);
    if (s != VA_STATUS_SUCCESS) {
        time_t now = time(NULL);
        if (now != last_import_error) {
            last_import_error = now;
            fprintf(stderr, "vaenc: vpp rgb surface failed: %s\n", vaErrorStr(s));
        }
        return VA_INVALID_SURFACE;
    }
    if (vpp_ninputs == MAX_VPP_INPUTS) {
        // Evict the least recently used surface instead of leaking the new
        // one, which is what happened once the fixed table filled up.
        vpp_drop_input(dpy, 0);
    }
    vpp_inputs[vpp_ninputs].id = id;
    vpp_inputs[vpp_ninputs].surf = surf;
    vpp_inputs[vpp_ninputs].w = w;
    vpp_inputs[vpp_ninputs].h = h;
    vpp_inputs[vpp_ninputs].stride = stride;
    vpp_inputs[vpp_ninputs].fourcc = fourcc;
    vpp_inputs[vpp_ninputs].modifier = modifier;
    vpp_inputs[vpp_ninputs].offset = offset;
    vpp_ninputs++;
    return surf;
}

static int vpp_convert(VADisplay dpy, VASurfaceID in) {
    VAProcPipelineParameterBuffer p;
    memset(&p, 0, sizeof(p));
    p.surface = in;
    p.surface_color_standard = VAProcColorStandardExplicit;
    p.output_color_standard = VAProcColorStandardExplicit;
    p.input_color_properties.color_range = VA_SOURCE_RANGE_FULL;
    p.input_color_properties.colour_primaries = 1;
    p.input_color_properties.transfer_characteristics = 1;
    p.input_color_properties.matrix_coefficients = 1;
    p.output_color_properties.color_range = VA_SOURCE_RANGE_FULL;
    p.output_color_properties.colour_primaries = 1;
    p.output_color_properties.transfer_characteristics = 1;
    p.output_color_properties.matrix_coefficients = 1;
    VABufferID pb;
    VAStatus s = vaCreateBuffer(dpy, vpp_ctx, VAProcPipelineParameterBufferType,
                                sizeof(p), 1, &p, &pb);
    if (s != VA_STATUS_SUCCESS) return -1;
    s = vaBeginPicture(dpy, vpp_ctx, vpp_out);
    if (s == VA_STATUS_SUCCESS) s = vaRenderPicture(dpy, vpp_ctx, &pb, 1);
    if (s == VA_STATUS_SUCCESS) s = vaEndPicture(dpy, vpp_ctx);
    vaDestroyBuffer(dpy, pb);
    if (s != VA_STATUS_SUCCESS) {
        fprintf(stderr, "vaenc: vpp render failed: %s\n", vaErrorStr(s));
        return -1;
    }
    s = vaSyncSurface(dpy, vpp_out);
    if (s != VA_STATUS_SUCCESS) {
        static time_t last_sync_error;
        time_t now = time(NULL);
        if (now != last_sync_error) {
            last_sync_error = now;
            fprintf(stderr, "vaenc: vpp sync failed: %s\n", vaErrorStr(s));
        }
        return -1;
    }
    return 0;
}

static AVBufferRef *hw_dev, *frames_ref;
static AVCodecContext *enc;
static AVFrame *frames[MAX_FRAMES];
static int nframes;

static int export_frame(AVFrame *f, VADRMPRIMESurfaceDescriptor *desc) {
    AVHWDeviceContext *dev = (AVHWDeviceContext *)hw_dev->data;
    AVVAAPIDeviceContext *va = dev->hwctx;
    VASurfaceID id = (VASurfaceID)(uintptr_t)f->data[3];
    VAStatus st = vaExportSurfaceHandle(va->display, id,
        VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
        VA_EXPORT_SURFACE_READ_WRITE | VA_EXPORT_SURFACE_SEPARATE_LAYERS, desc);
    return st == VA_STATUS_SUCCESS ? 0 : -1;
}

static int send_all(int fd, const void *buf, size_t len) {
    const uint8_t *p = buf;
    while (len) {
        ssize_t n = send(fd, p, len, 0);
        if (n <= 0) return -1;
        p += n;
        len -= (size_t)n;
    }
    return 0;
}

static int send_with_fds(int fd, const void *hdr, size_t hdrlen, int *fds, int nfds) {
    struct msghdr msg = {0};
    struct iovec iov = { .iov_base = (void *)hdr, .iov_len = hdrlen };
    char ctrl[CMSG_SPACE(sizeof(int) * 4)];
    memset(ctrl, 0, sizeof(ctrl));
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    if (nfds > 0) {
        msg.msg_control = ctrl;
        msg.msg_controllen = CMSG_SPACE(sizeof(int) * nfds);
        struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
        c->cmsg_level = SOL_SOCKET;
        c->cmsg_type = SCM_RIGHTS;
        c->cmsg_len = CMSG_LEN(sizeof(int) * nfds);
        memcpy(CMSG_DATA(c), fds, sizeof(int) * nfds);
    }
    return sendmsg(fd, &msg, 0) < 0 ? -1 : 0;
}

static int g_w = 1920, g_h = 1080, g_fps = 160;
static int force_idr;

int main(int argc, char **argv) {
    if (argc < 6) {
        fprintf(stderr, "usage: %s socket width height fps bitrate [import]\n", argv[0]);
        return 1;
    }
    g_w = atoi(argv[2]) & ~1;
    g_h = atoi(argv[3]) & ~1;
    g_fps = atoi(argv[4]);
    long bitrate = atol(argv[5]);
    if (g_w < 16 || g_h < 16 || g_fps < 1 || bitrate < 100000) {
        fprintf(stderr, "vaenc: bad mode %dx%d@%d, %ld bit/s\n", g_w, g_h, g_fps, bitrate);
        return 1;
    }
    int import_mode = (argc > 6 && strcmp(argv[6], "import") == 0);

    const char *drm_node = getenv("LGX_DRM_NODE");
    if (!drm_node || !*drm_node) drm_node = "/dev/dri/renderD128";
    if (av_hwdevice_ctx_create(&hw_dev, AV_HWDEVICE_TYPE_VAAPI, drm_node, NULL, 0) < 0) {
        fprintf(stderr, "vaenc: hwdevice failed\n");
        return 1;
    }
    frames_ref = av_hwframe_ctx_alloc(hw_dev);
    AVHWFramesContext *fc = (AVHWFramesContext *)frames_ref->data;
    fc->format = AV_PIX_FMT_VAAPI;
    fc->sw_format = AV_PIX_FMT_NV12;
    fc->width = g_w;
    fc->height = g_h;
    fc->initial_pool_size = MAX_FRAMES;
    if (av_hwframe_ctx_init(frames_ref) < 0) { fprintf(stderr, "vaenc: frames ctx failed\n"); return 1; }

    const AVCodec *codec = avcodec_find_encoder_by_name("hevc_vaapi");
    if (!codec) { fprintf(stderr, "vaenc: no hevc_vaapi\n"); return 1; }
    enc = avcodec_alloc_context3(codec);
    enc->pix_fmt = AV_PIX_FMT_VAAPI;
    enc->profile = AV_PROFILE_HEVC_MAIN;
    enc->width = g_w;
    enc->height = g_h;
    enc->time_base = (AVRational){1, g_fps};
    enc->framerate = (AVRational){g_fps, 1};
    enc->bit_rate = bitrate;
    enc->gop_size = g_fps;
    enc->max_b_frames = 0;
    enc->flags |= AV_CODEC_FLAG_LOW_DELAY;
    /* async_depth 2 is the default and adds one frame of latency; CBR keeps
     * the link rate (and therefore the packet pacing) steady. */
    int async_depth = 1;
    const char *ad = getenv("LGX_ASYNC_DEPTH");
    if (ad && atoi(ad) >= 1) async_depth = atoi(ad);
    av_opt_set_int(enc->priv_data, "async_depth", async_depth, 0);
    /* VBR (3) spends bits only on the content that changed, while CBR pads a
     * static desktop up to the target rate (measured 4.7 MiB/s vs 1.2 MiB/s
     * at 1080p160). rc_max_rate keeps motion at the configured ceiling.
     * LGX_RC_MODE overrides the mode: 0 auto, 1 CQP, 2 CBR, 3 VBR, 4 ICQ. */
    int rc_mode = 3;
    const char *rm = getenv("LGX_RC_MODE");
    if (rm && atoi(rm) >= 0 && atoi(rm) <= 4) rc_mode = atoi(rm);
    av_opt_set_int(enc->priv_data, "rc_mode", rc_mode, 0);
    if (rc_mode == 1) {
        /* CQP: a fixed quantiser keeps the keyframes small and the quality
         * uniform; LGX_QP overrides it (FFmpeg wants FF_QP2LAMBDA units). */
        int qp = 24;
        const char *q = getenv("LGX_QP");
        if (q && atoi(q) > 0 && atoi(q) < 52) qp = atoi(q);
        /* iHD takes the constant quantiser from global_quality, raw (not
         * FF_QP2LAMBDA-scaled: multiplying it clamped to QP 51 and crushed
         * the picture). qmin/qmax are set to match, but VDENC ignores them,
         * so an adaptive QP has to go through global_quality. */
        enc->global_quality = qp;
        enc->qmin = qp;
        enc->qmax = qp;
    }
    if (rc_mode >= 3) {
        enc->rc_max_rate = bitrate;
        /* A 1 s buffer lets the first IDR spend the whole budget: measured
         * 668 KiB for 1080p, which is 130 ms of the link in one burst. An
         * eighth of a second keeps a keyframe inside one or two frames of
         * budget without starving motion. */
        enc->rc_buffer_size = bitrate / 8;
    }
    /* VDENC (low_power) is the fast path on Gen12+; it is a little less
     * flexible than the default encoder but much cheaper per frame. */
    int low_power = 1;
    const char *lp = getenv("LGX_LOW_POWER");
    if (lp && atoi(lp) == 0) low_power = 0;
    if (av_opt_set_int(enc->priv_data, "low_power", low_power, 0) < 0) {
        fprintf(stderr, "vaenc: low_power option unavailable\n");
    }
    enc->color_range = AVCOL_RANGE_JPEG;
    enc->colorspace = AVCOL_SPC_BT709;
    enc->color_primaries = AVCOL_PRI_BT709;
    enc->color_trc = AVCOL_TRC_BT709;
    enc->hw_frames_ctx = av_buffer_ref(frames_ref);
    if (avcodec_open2(enc, codec, NULL) < 0) {
        /* VDENC (low_power) exists only on Intel Gen12+. Mesa's radeonsi
         * rejects the option, and older Intel drivers have no low-power
         * entry point at all; retry once with the regular encoder. */
        if (low_power) {
            fprintf(stderr, "vaenc: low_power open failed; retrying without VDENC\n");
            av_opt_set_int(enc->priv_data, "low_power", 0, 0);
            if (avcodec_open2(enc, codec, NULL) < 0) {
                fprintf(stderr, "vaenc: open failed\n");
                return 1;
            }
        } else {
            fprintf(stderr, "vaenc: open failed\n");
            return 1;
        }
    }

    for (int i = 0; i < MAX_FRAMES && !import_mode; i++) {
        frames[i] = av_frame_alloc();
        frames[i]->format = AV_PIX_FMT_VAAPI;
        frames[i]->width = g_w;
        frames[i]->height = g_h;
        if (av_hwframe_get_buffer(frames_ref, frames[i], 0) < 0) {
            fprintf(stderr, "vaenc: get_buffer failed\n");
            return 1;
        }
    }
    nframes = import_mode ? 0 : MAX_FRAMES;

    use_vpp = getenv("LGX_VPP") != NULL && !import_mode;
    if (use_vpp) {
        AVHWDeviceContext *hdev = (AVHWDeviceContext *)hw_dev->data;
        AVVAAPIDeviceContext *vactx = hdev->hwctx;
        if (vpp_init(vactx->display, (VASurfaceID)(uintptr_t)frames[0]->data[3], g_w, g_h) < 0) {
            use_vpp = 0;
        } else {
            fprintf(stderr, "vaenc: VPP RGB->NV12 enabled\n");
        }
    }

    int lsock = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un addr = {0};
    addr.sun_family = AF_UNIX;
    snprintf(addr.sun_path, sizeof(addr.sun_path), "%s", argv[1]);
    unlink(argv[1]);
    if (bind(lsock, (struct sockaddr *)&addr, sizeof(addr)) < 0 || listen(lsock, 1) < 0) {
        perror("bind/listen");
        return 1;
    }
    int cfd = accept(lsock, NULL, NULL);
    if (cfd < 0) { perror("accept"); return 1; }

    uint32_t n = nframes;
    if (send_all(cfd, &n, 4) < 0) return 1;
    for (int i = 0; i < nframes; i++) {
        VADRMPRIMESurfaceDescriptor desc;
        if (export_frame(frames[i], &desc) < 0) { fprintf(stderr, "vaenc: export failed\n"); return 1; }
        uint32_t hdr[4] = { desc.width, desc.height, desc.fourcc, desc.num_layers };
        uint64_t modifier = desc.objects[0].drm_format_modifier;
        if (send_all(cfd, &modifier, 8) < 0) return 1;
        if (send_all(cfd, hdr, sizeof(hdr)) < 0) return 1;
        int fds[4];
        uint32_t plane[2] = { 0, 0 };
        uint32_t meta[4] = { 0, 0, 0, 0 }; /* offset0, pitch0, offset1, pitch1 */
        int nf = 0;
        for (unsigned l = 0; l < desc.num_layers && l < 2; l++) {
            for (unsigned p = 0; p < desc.layers[l].num_planes && p < 1; p++) {
                uint32_t obj = desc.layers[l].object_index[p];
                fds[nf] = desc.objects[obj].fd;
                meta[nf * 2] = desc.layers[l].offset[p];
                meta[nf * 2 + 1] = desc.layers[l].pitch[p];
                nf++;
            }
        }
        plane[0] = (uint32_t)nf;
        if (send_with_fds(cfd, meta, sizeof(uint32_t) * 4, fds, nf) < 0) {
            fprintf(stderr, "vaenc: send fds failed: %s\n", strerror(errno));
            return 1;
        }
        /* tell the caller how many planes followed */
        if (send_all(cfd, plane, 4) < 0) return 1;
    }

    for (;;) {
        uint8_t cmd;
        if (recv(cfd, &cmd, 1, MSG_WAITALL) != 1) break;
        if (cmd == 'Q') break;

        if (cmd == 'K') {
            force_idr = 1;
            continue;
        }

        if (cmd == 'I') {
            /* external NV12 dma-buf: meta (offset0, pitch0, offset1, pitch1) + fd */
            uint32_t meta[4];
            int fds[2] = {-1, -1};
            struct msghdr m = {0};
            struct iovec iov = { .iov_base = meta, .iov_len = sizeof(meta) };
            char ctrl[CMSG_SPACE(sizeof(int))];
            m.msg_iov = &iov;
            m.msg_iovlen = 1;
            m.msg_control = ctrl;
            m.msg_controllen = sizeof(ctrl);
            if (recvmsg(cfd, &m, 0) <= 0) break;
            struct cmsghdr *cm = CMSG_FIRSTHDR(&m);
            if (!cm) break;
            memcpy(fds, CMSG_DATA(cm), sizeof(int) * 2);

            /* Map our dma-buf into VAAPI through libavcodec's own hwframe path. */
            AVDRMFrameDescriptor *d = av_mallocz(sizeof(*d));
            d->nb_objects = 1;
            d->objects[0].fd = fds[0];
            d->objects[0].size = meta[2] + meta[3];
            d->objects[0].format_modifier = 0;
            d->nb_layers = 1;
            d->layers[0].format = 0x3231564e; /* DRM_FORMAT_NV12 */
            d->layers[0].nb_planes = 2;
            d->layers[0].planes[0].object_index = 0;
            d->layers[0].planes[0].offset = 0;
            d->layers[0].planes[0].pitch = meta[0];
            d->layers[0].planes[1].object_index = 0;
            d->layers[0].planes[1].offset = meta[2];
            d->layers[0].planes[1].pitch = meta[1];

            AVFrame *drm = av_frame_alloc();
            drm->format = AV_PIX_FMT_DRM_PRIME;
            drm->width = g_w;
            drm->height = g_h;
            drm->data[0] = (uint8_t *)d;
            drm->buf[0] = av_buffer_create((uint8_t *)d, sizeof(*d), NULL, NULL, 0);

            frames[0] = av_frame_alloc();
            frames[0]->format = AV_PIX_FMT_VAAPI;
            frames[0]->width = g_w;
            frames[0]->height = g_h;
            frames[0]->hw_frames_ctx = av_buffer_ref(frames_ref);
            int mr = av_hwframe_map(frames[0], drm, AV_HWFRAME_MAP_READ | AV_HWFRAME_MAP_WRITE);
            char mb[128];
            av_strerror(mr, mb, sizeof(mb));
            fprintf(stderr, "vaenc: hwframe_map -> %d (%s)\n", mr, mb);
            if (mr < 0) { uint32_t zero = 0; send_all(cfd, &zero, 4); continue; }
            nframes = 1;
            uint32_t ok = 1;
            send_all(cfd, &ok, 4);
            continue;
        }

        uint32_t idx = 0;
        if (cmd == 'R') {
            uint32_t meta[8] = {0};
            int fds[1] = {-1};
            struct msghdr m = {0};
            struct iovec iov = { .iov_base = meta, .iov_len = sizeof(meta) };
            char ctrl[CMSG_SPACE(sizeof(int))];
            m.msg_iov = &iov;
            m.msg_iovlen = 1;
            m.msg_control = ctrl;
            m.msg_controllen = sizeof(ctrl);
            if (recvmsg(cfd, &m, 0) <= 0) break;
            struct cmsghdr *cm = CMSG_FIRSTHDR(&m);
            if (!cm) break;
            memcpy(fds, CMSG_DATA(cm), sizeof(int));
            if (!use_vpp) { close(fds[0]); uint32_t zero = 0; send_all(cfd, &zero, 4); continue; }
            AVHWDeviceContext *hdev = (AVHWDeviceContext *)hw_dev->data;
            AVVAAPIDeviceContext *vactx = hdev->hwctx;
            uint64_t mod = (uint64_t)meta[5] | ((uint64_t)meta[6] << 32);
            VASurfaceID in = vpp_import(vactx->display, meta[4], (int)meta[0], (int)meta[1],
                                        (int)meta[2], meta[3], mod, (int)meta[7], fds[0]);
            if (in == VA_INVALID_SURFACE) {
                static time_t last_import_error2;
                time_t now = time(NULL);
                if (now != last_import_error2) {
                    last_import_error2 = now;
                    fprintf(stderr,
                            "vaenc: import failed: %dx%d stride=%d fourcc=%x mod=%llx off=%d\n",
                            (int)meta[0], (int)meta[1], (int)meta[2], meta[3],
                            (unsigned long long)mod, (int)meta[7]);
                }
            }
            if (in == VA_INVALID_SURFACE || vpp_convert(vactx->display, in) < 0) {
                uint32_t zero = 0;
                send_all(cfd, &zero, 4);
                continue;
            }
            {
                static time_t last_ok;
                time_t now = time(NULL);
                if (now != last_ok) {
                    last_ok = now;
                    fprintf(stderr,
                            "vaenc: vpp ok %dx%d stride=%d fourcc=%x mod=%llx off=%d\n",
                            (int)meta[0], (int)meta[1], (int)meta[2], meta[3],
                            (unsigned long long)mod, (int)meta[7]);
                }
            }
        } else if (cmd == 'F') {
            uint32_t len;
            if (recv(cfd, &idx, 4, MSG_WAITALL) != 4) break;
            if (recv(cfd, &len, 4, MSG_WAITALL) != 4) break;
            uint8_t *data = malloc(len);
            size_t got = 0;
            while (got < len) {
                ssize_t n = recv(cfd, data + got, len - got, 0);
                if (n <= 0) { free(data); goto done; }
                got += (size_t)n;
            }
    
            if (idx < (uint32_t)nframes) {
                AVHWDeviceContext *dev = (AVHWDeviceContext *)hw_dev->data;
                AVVAAPIDeviceContext *va = dev->hwctx;
                VASurfaceID sid = (VASurfaceID)(uintptr_t)frames[idx]->data[3];
                VAImage img;
                VAStatus dst = vaDeriveImage(va->display, sid, &img);

                if (dst == VA_STATUS_SUCCESS) {
                    void *ptr = NULL;
                    if (vaMapBuffer(va->display, img.buf, &ptr) == VA_STATUS_SUCCESS) {
                        uint8_t *dst = ptr;
                        int w = enc->width, h = enc->height;
                        for (int y = 0; y < h; y++)
                            memcpy(dst + img.offsets[0] + (size_t)y * img.pitches[0],
                                   data + (size_t)y * w, w);
                        for (int y = 0; y < h / 2; y++)
                            memcpy(dst + img.offsets[1] + (size_t)y * img.pitches[1],
                                   data + (size_t)w * h + (size_t)y * w, w);
                        vaUnmapBuffer(va->display, img.buf);
                    }
                    vaDestroyImage(va->display, img.image_id);
                }
            }
            free(data);
        } else if (cmd == 'E') {
            if (recv(cfd, &idx, 4, MSG_WAITALL) != 4) break;
        } else {
            continue;
        }

        if (idx >= (uint32_t)nframes) {
            uint32_t zero = 0;
            send_all(cfd, &zero, 4);
            continue;
        }
        AVFrame *f = frames[idx];
        f->pts = enc->frame_num;
        if (force_idr) {
            f->pict_type = AV_PICTURE_TYPE_I;
            f->flags |= AV_FRAME_FLAG_KEY;
            force_idr = 0;
        } else {
            /* The imported surface is reused for every frame; leaving I/key
             * set would turn the whole stream into a sequence of IDRs. */
            f->pict_type = AV_PICTURE_TYPE_NONE;
            f->flags &= ~AV_FRAME_FLAG_KEY;
        }
        int sfr = avcodec_send_frame(enc, f);
        if (sfr < 0) {
            char eb[128];
            av_strerror(sfr, eb, sizeof(eb));
            fprintf(stderr, "vaenc: send_frame failed: %s\n", eb);
            uint32_t zero = 0;
            send_all(cfd, &zero, 4);
            continue;
        }
        /* With async_depth > 1 the encoder may return more than one packet
         * for a single sent frame. Send the count first; the Rust side reads
         * exactly that many packets and broadcasts each access unit. */
        AVPacket *pkt = av_packet_alloc();
        AVPacket *out[16];
        int np = 0;
        int rc = avcodec_receive_packet(enc, pkt);
        while (rc == 0) {
            if (np < 16) {
                out[np] = av_packet_alloc();
                av_packet_move_ref(out[np], pkt);
                np++;
            } else {
                av_packet_unref(pkt);
            }
            rc = avcodec_receive_packet(enc, pkt);
        }
        av_packet_free(&pkt);
        uint32_t count = (uint32_t)np;
        {
            static time_t last_np;
            time_t now = time(NULL);
            if (now != last_np) {
                last_np = now;
                fprintf(stderr, "vaenc: packet count %d\n", np);
            }
        }
        if (send_all(cfd, &count, 4) < 0) goto done;
        for (int i = 0; i < np; i++) {
            uint32_t len = out[i]->size;
            if (send_all(cfd, &len, 4) < 0 || send_all(cfd, out[i]->data, out[i]->size) < 0) {
                for (int j = i; j < np; j++) av_packet_free(&out[j]);
                goto done;
            }
            av_packet_free(&out[i]);
        }
    }
done:
    close(cfd);
    close(lsock);
    unlink(argv[1]);
    return 0;
}
