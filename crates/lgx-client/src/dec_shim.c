/* lgx-dec: in-process HEVC decoder used by the client.
 *
 * The shim exists so the Rust side never has to mirror libavcodec's private
 * struct layouts: it is compiled against the headers that are actually
 * installed.  It offers exactly what the renderer needs:
 *
 *   - VAAPI decoding (hardware) when a DRM render node is available;
 *   - every decoded frame exported as dma-buf planes (zero copy) via
 *     av_hwframe_map(AV_PIX_FMT_DRM_PRIME);
 *   - a CPU NV12 fallback for software decoding or hosts where the export
 *     is not supported.
 *
 * All pixel data stays owned by an opaque frame handle that the caller
 * releases after the GPU has imported it.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <libavcodec/avcodec.h>
#include <libavutil/error.h>
#include <libavutil/frame.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_drm.h>
#include <libavutil/pixdesc.h>
#include <libavutil/pixfmt.h>

#define LGX_DRM_FORMAT_NV12 0x3231564e /* 'NV12' */
#define LGX_DRM_FORMAT_R8 0x20203852   /* 'R8  ' */
#define LGX_DRM_FORMAT_GR88 0x38385247 /* 'GR88' */
#define LGX_DRM_MOD_INVALID 0x00ffffffffffffffULL
#define LGX_MAX_PLANES AV_DRM_MAX_PLANES

typedef struct LgxFrameInfo {
    int kind; /* 1 = dma-buf planes, 2 = CPU NV12 */
    int width;
    int height;
    const uint8_t *y;
    const uint8_t *uv;
    int y_stride;
    int uv_stride;
    int nb_planes;
    int fds[LGX_MAX_PLANES];
    uint32_t offsets[LGX_MAX_PLANES];
    uint32_t pitches[LGX_MAX_PLANES];
    uint64_t modifier;
    uint32_t fourcc;
    /* 1 = a single NV12 layer whose two planes share one dma-buf object,
     * 2 = two layers (R8 luma, R8G8 chroma), each with its own plane (the
     * layers may still reference the same object). The Vulkan importer needs
     * this: one two-plane image or two one-plane images, respectively. */
    uint32_t layers;
    int64_t pts;
} LgxFrameInfo;

typedef struct LgxDec LgxDec;
void lgx_dec_close(LgxDec *d);
void lgx_dec_free_frame(void *handle);

/* backend: 0 = software, 1 = VAAPI, 2 = CUDA/NVDEC, 3 = Vulkan Video */
struct LgxDec {
    AVCodecContext *ctx;
    AVBufferRef *hw;
    AVPacket *pkt;
    int backend;
    int hw_active;
    int want_dmabuf;
    int dmabuf_failed;
    char err[256];
};

typedef struct LgxOwned {
    AVFrame *frame;
    AVFrame *va; /* kept so the frame can still be downloaded on demand */
    uint8_t *buf;
} LgxOwned;

static void set_err(LgxDec *d, const char *what, int code) {
    char msg[AV_ERROR_MAX_STRING_SIZE] = {0};
    av_strerror(code, msg, sizeof(msg));
    snprintf(d->err, sizeof(d->err), "%s: %s", what, msg);
}

static enum AVPixelFormat pick_format(AVCodecContext *ctx, const enum AVPixelFormat *fmts) {
    LgxDec *d = ctx->opaque;
    enum AVPixelFormat want = AV_PIX_FMT_NONE;
    if (d && d->backend == 1) want = AV_PIX_FMT_VAAPI;
    if (d && d->backend == 2) want = AV_PIX_FMT_CUDA;
    if (d && d->backend == 3) want = AV_PIX_FMT_VULKAN;
    if (want != AV_PIX_FMT_NONE) {
        for (const enum AVPixelFormat *p = fmts; *p != AV_PIX_FMT_NONE; p++) {
            if (*p == want) return want;
        }
    }
    return fmts[0];
}

LgxDec *lgx_dec_open(int backend, const char *device, int want_dmabuf, int threads, char *err, int errlen) {
    LgxDec *d = calloc(1, sizeof(*d));
    if (!d) {
        if (err) {
            snprintf(err, errlen, "out of memory");
        }
        return NULL;
    }
    av_log_set_level(AV_LOG_ERROR);

    const AVCodec *codec = avcodec_find_decoder(AV_CODEC_ID_HEVC);
    if (!codec) {
        snprintf(d->err, sizeof(d->err), "libavcodec has no HEVC decoder");
        goto fail;
    }
    d->ctx = avcodec_alloc_context3(codec);
    if (!d->ctx) {
        snprintf(d->err, sizeof(d->err), "avcodec_alloc_context3 failed");
        goto fail;
    }
    d->ctx->flags |= AV_CODEC_FLAG_LOW_DELAY;
    d->backend = backend;
    d->ctx->opaque = d;

    if (backend == 1 || backend == 2 || backend == 3) {
        enum AVHWDeviceType type = backend == 1 ? AV_HWDEVICE_TYPE_VAAPI
                                  : backend == 2 ? AV_HWDEVICE_TYPE_CUDA
                                                 : AV_HWDEVICE_TYPE_VULKAN;
        const char *hw_name = backend == 1 ? "VAAPI" : backend == 2 ? "CUDA" : "Vulkan";
        const char *dev = (device && device[0]) ? device : NULL;
        if (av_hwdevice_ctx_create(&d->hw, type, dev, NULL, 0) < 0) {
            snprintf(
                d->err,
                sizeof(d->err),
                "cannot open the %s device%s%s",
                hw_name,
                dev ? " " : "",
                dev ? dev : ""
            );
            goto fail;
        }
        d->hw_active = 1;
        d->ctx->hw_device_ctx = av_buffer_ref(d->hw);
        if (!d->ctx->hw_device_ctx) {
            snprintf(d->err, sizeof(d->err), "cannot reference the hardware device");
            goto fail;
        }
    }

    if (d->hw_active) {
        d->ctx->get_format = pick_format;
        d->ctx->thread_count = 1;
    } else {
        d->ctx->thread_count = threads > 0 ? threads : 0;
        d->ctx->thread_type = FF_THREAD_SLICE;
        d->ctx->flags2 |= AV_CODEC_FLAG2_FAST;
    }

    int r = avcodec_open2(d->ctx, codec, NULL);
    if (r < 0) {
        set_err(d, "avcodec_open2", r);
        goto fail;
    }
    d->pkt = av_packet_alloc();
    if (!d->pkt) {
        snprintf(d->err, sizeof(d->err), "av_packet_alloc failed");
        goto fail;
    }
    d->want_dmabuf = want_dmabuf && d->hw_active;
    return d;

fail:
    if (err) {
        snprintf(err, errlen, "%s", d->err);
    }
    lgx_dec_close(d);
    return NULL;
}

int lgx_dec_push(LgxDec *d, const uint8_t *data, int len, int64_t pts) {
    av_packet_unref(d->pkt);
    int r = av_new_packet(d->pkt, len);
    if (r < 0) {
        set_err(d, "av_new_packet", r);
        return -1;
    }
    memcpy(d->pkt->data, data, (size_t)len);
    d->pkt->pts = pts;
    d->pkt->dts = pts;
    r = avcodec_send_packet(d->ctx, d->pkt);
    av_packet_unref(d->pkt);
    if (r == AVERROR(EAGAIN)) {
        return 2; /* caller drains frames and retries */
    }
    if (r < 0) {
        set_err(d, "avcodec_send_packet", r);
        return -1;
    }
    return 1;
}

static LgxOwned *owned_new(AVFrame *f, AVFrame *va, uint8_t *buf) {
    LgxOwned *o = calloc(1, sizeof(*o));
    if (!o) {
        av_frame_free(&f);
        av_frame_free(&va);
        av_free(buf);
        return NULL;
    }
    o->frame = f;
    o->va = va;
    o->buf = buf;
    return o;
}

static int map_dmabuf(LgxDec *d, AVFrame *src, LgxFrameInfo *out, void **handle) {
    AVFrame *drm = av_frame_alloc();
    if (!drm) {
        return 0;
    }
    /* av_hwframe_map() only produces a DRM descriptor when the destination
     * asks for that format. With the default (unset) format the VAAPI
     * hwcontext maps the surface to CPU NV12 instead, which is the old
     * fallback behaviour and never a dma-buf. */
    drm->format = AV_PIX_FMT_DRM_PRIME;
    if (av_hwframe_map(drm, src, AV_HWFRAME_MAP_READ) < 0 || !drm->data[0]) {
        av_frame_free(&drm);
        return 0;
    }
    (void)d;
    AVDRMFrameDescriptor *desc = (AVDRMFrameDescriptor *)drm->data[0];
    int nv12 = 0;
    int layers = 0;
    if (desc->nb_layers == 1 && desc->nb_objects >= 1) {
        nv12 = desc->layers[0].format == LGX_DRM_FORMAT_NV12 && desc->layers[0].nb_planes == 2;
        layers = 1;
    } else if (desc->nb_layers == 2 && desc->nb_objects >= 1) {
        uint32_t f0 = desc->layers[0].format;
        uint32_t f1 = desc->layers[1].format;
        nv12 = desc->layers[0].nb_planes == 1 && desc->layers[1].nb_planes == 1 &&
               f0 == LGX_DRM_FORMAT_R8 && f1 == LGX_DRM_FORMAT_GR88;
        layers = 2;
    }
    if (!nv12 || desc->objects[0].format_modifier == LGX_DRM_MOD_INVALID) {
        av_frame_free(&drm);
        return 0;
    }
    int np = 0;
    for (int l = 0; l < desc->nb_layers && np < LGX_MAX_PLANES; l++) {
        for (int p = 0; p < desc->layers[l].nb_planes && np < LGX_MAX_PLANES; p++) {
            const AVDRMPlaneDescriptor *pl = &desc->layers[l].planes[p];
            int obj = pl->object_index;
            if (obj < 0 || obj >= desc->nb_objects) {
                continue;
            }
            out->fds[np] = desc->objects[obj].fd;
            out->offsets[np] = (uint32_t)pl->offset;
            out->pitches[np] = (uint32_t)pl->pitch;
            np++;
        }
    }
    if (np != 2) {
        av_frame_free(&drm);
        return 0;
    }
    out->kind = 1;
    out->fourcc = LGX_DRM_FORMAT_NV12;
    out->modifier = desc->objects[0].format_modifier;
    out->nb_planes = np;
    out->layers = (uint32_t)layers;
    LgxOwned *own = owned_new(drm, av_frame_clone(src), NULL);
    if (!own) {
        return -1;
    }
    if (!own->va) {
        lgx_dec_free_frame(own);
        return -1;
    }
    *handle = own;
    return 1;
}

static int to_cpu(LgxDec *d, AVFrame *f, LgxFrameInfo *out, void **handle) {
    AVFrame *sw = NULL;
    const AVPixFmtDescriptor *desc = av_pix_fmt_desc_get(f->format);
    if (desc && (desc->flags & AV_PIX_FMT_FLAG_HWACCEL)) {
        sw = av_frame_alloc();
        if (!sw) {
            return -1;
        }
        sw->format = AV_PIX_FMT_NV12;
        sw->width = f->width;
        sw->height = f->height;
        int r = av_hwframe_transfer_data(sw, f, 0);
        av_frame_free(&f);
        f = NULL;
        if (r < 0) {
            set_err(d, "av_hwframe_transfer_data", r);
            av_frame_free(&sw);
            return -1;
        }
    } else {
        sw = f;
    }

    out->kind = 2;
    out->width = sw->width;
    out->height = sw->height;
    out->y_stride = sw->linesize[0];
    out->uv_stride = sw->linesize[1];

    if (sw->format == AV_PIX_FMT_NV12) {
        out->y = sw->data[0];
        out->uv = sw->data[1];
        LgxOwned *own = owned_new(sw, NULL, NULL);
        if (!own) {
            return -1;
        }
        *handle = own;
        return 1;
    }

    if (sw->format == AV_PIX_FMT_YUV420P || sw->format == AV_PIX_FMT_YUVJ420P) {
        int w = sw->width;
        int h = sw->height;
        uint8_t *buf = av_mallocz((size_t)w * h * 3 / 2);
        if (!buf) {
            av_frame_free(&sw);
            return -1;
        }
        for (int y = 0; y < h; y++) {
            memcpy(buf + (size_t)y * w, sw->data[0] + (size_t)y * sw->linesize[0], w);
        }
        uint8_t *uv = buf + (size_t)w * h;
        for (int y = 0; y < h / 2; y++) {
            const uint8_t *u = sw->data[1] + (size_t)y * sw->linesize[1];
            const uint8_t *v = sw->data[2] + (size_t)y * sw->linesize[2];
            for (int x = 0; x < w / 2; x++) {
                uv[(size_t)y * w + x * 2] = u[x];
                uv[(size_t)y * w + x * 2 + 1] = v[x];
            }
        }
        LgxOwned *own = owned_new(sw, NULL, buf);
        if (!own) {
            return -1;
        }
        out->y = buf;
        out->uv = uv;
        out->y_stride = w;
        out->uv_stride = w;
        *handle = own;
        return 1;
    }

    set_err(d, "unsupported pixel format from decoder", AVERROR(ENOSYS));
    av_frame_free(&sw);
    return -1;
}

int lgx_dec_pull(LgxDec *d, LgxFrameInfo *out, void **handle) {
    *handle = NULL;
    AVFrame *f = av_frame_alloc();
    if (!f) {
        return -1;
    }
    int r = avcodec_receive_frame(d->ctx, f);
    if (r == AVERROR(EAGAIN) || r == AVERROR_EOF) {
        av_frame_free(&f);
        return 0;
    }
    if (r < 0) {
        set_err(d, "avcodec_receive_frame", r);
        av_frame_free(&f);
        return -1;
    }
    memset(out, 0, sizeof(*out));
    out->width = f->width;
    out->height = f->height;
    out->pts = f->pts;

    if (d->want_dmabuf && !d->dmabuf_failed) {
        int mapped = map_dmabuf(d, f, out, handle);
        if (mapped > 0) {
            av_frame_free(&f);
            return 1;
        }
        if (mapped < 0) {
            av_frame_free(&f);
            return -1;
        }
        /* The driver can export hardware frames but not as dma-buf (or this
         * backend has no export path). Do not retry the export every frame. */
        d->dmabuf_failed = 1;
    }
    return to_cpu(d, f, out, handle) > 0 ? 2 : -1;
}

int lgx_dec_download_frame(void *handle, LgxFrameInfo *out, void **cpu_handle) {
    LgxOwned *o = handle;
    if (!o || !o->va) {
        return 0;
    }
    const AVPixFmtDescriptor *desc = av_pix_fmt_desc_get(o->va->format);
    if (!desc || !(desc->flags & AV_PIX_FMT_FLAG_HWACCEL)) {
        return 0;
    }
    AVFrame *sw = av_frame_alloc();
    if (!sw) {
        return 0;
    }
    sw->format = AV_PIX_FMT_NV12;
    sw->width = o->va->width;
    sw->height = o->va->height;
    if (av_hwframe_transfer_data(sw, o->va, 0) < 0) {
        av_frame_free(&sw);
        return 0;
    }
    LgxOwned *cpu = owned_new(sw, NULL, NULL);
    if (!cpu) {
        return 0;
    }
    memset(out, 0, sizeof(*out));
    out->kind = 2;
    out->width = sw->width;
    out->height = sw->height;
    out->y = sw->data[0];
    out->uv = sw->data[1];
    out->y_stride = sw->linesize[0];
    out->uv_stride = sw->linesize[1];
    *cpu_handle = cpu;
    return 1;
}

void lgx_dec_free_frame(void *handle) {
    LgxOwned *o = handle;
    if (!o) {
        return;
    }
    if (o->frame) {
        av_frame_free(&o->frame);
    }
    if (o->va) {
        av_frame_free(&o->va);
    }
    av_free(o->buf);
    free(o);
}

void lgx_dec_close(LgxDec *d) {
    if (!d) {
        return;
    }
    av_packet_free(&d->pkt);
    avcodec_free_context(&d->ctx);
    av_buffer_unref(&d->hw);
    free(d);
}

const char *lgx_dec_last_error(const LgxDec *d) {
    return d ? d->err : "decoder is not open";
}

int lgx_dec_is_hw(const LgxDec *d) {
    return d && d->hw_active;
}
