/* lgx-nvenc: NVENC HEVC encoder for the lgx server.
 *
 * The VAAPI helper (`lgx-vaenc`) owns its surfaces and hands them out as
 * dma-bufs; NVENC is the opposite: it takes an NV12 frame in system memory and
 * uploads it to the GPU itself. The server converts the captured RGB frame to
 * NV12 (VPP on VAAPI, Vulkan compute or CPU otherwise) and sends it here.
 *
 * Usage: lgx-nvenc SOCKET WIDTH HEIGHT FPS BITRATE
 *
 * Environment:
 *   LGX_NVENC_PRESET   NVENC preset name (default p1, the lowest-latency one)
 *   LGX_NVENC_TUNE     tuning info (default ull)
 *   LGX_ASYNC_DEPTH    accepted for symmetry with lgx-vaenc; NVENC has no
 *                      equivalent option and always runs with zerolatency
 *
 * Socket protocol (AF_UNIX, caller connects to argv[1]):
 *   helper -> caller: u32 nframes (always 0: no surfaces to hand out)
 *   caller -> helper: u8 cmd
 *                     'F' + u32 index + u32 len + NV12 bytes -> encode
 *                     'K'                                      -> next frame IDR
 *                     'Q'                                      -> quit
 *   helper -> caller: u32 count, then per packet: u32 len + Annex-B bytes
 */
#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include <libavcodec/avcodec.h>
#include <libavutil/opt.h>
#include <libavutil/pixfmt.h>

#define MAX_PACKETS 16

static int send_all(int fd, const void *buf, size_t len) {
    const uint8_t *p = buf;
    while (len) {
        ssize_t n = send(fd, p, len, MSG_NOSIGNAL);
        if (n <= 0) return -1;
        p += n;
        len -= (size_t)n;
    }
    return 0;
}

static int recv_all(int fd, void *buf, size_t len) {
    uint8_t *p = buf;
    while (len) {
        ssize_t n = recv(fd, p, len, 0);
        if (n <= 0) return -1;
        p += n;
        len -= (size_t)n;
    }
    return 0;
}

static const char *opt_str(const char *name, const char *fallback) {
    const char *v = getenv(name);
    return (v && *v) ? v : fallback;
}

static void set_opt(void *priv, const char *key, const char *value) {
    if (av_opt_set(priv, key, value, 0) < 0) {
        fprintf(stderr, "nvenc: option %s=%s rejected\n", key, value);
    }
}

static void set_int(void *priv, const char *key, int value) {
    if (av_opt_set_int(priv, key, value, 0) < 0) {
        fprintf(stderr, "nvenc: option %s=%d rejected\n", key, value);
    }
}

static int drain_packets(AVCodecContext *enc, int cfd, int *sent) {
    AVPacket *pkt = av_packet_alloc();
    AVPacket *out[MAX_PACKETS];
    int np = 0;
    if (!pkt) return -1;
    int rc = avcodec_receive_packet(enc, pkt);
    while (rc == 0) {
        if (np < MAX_PACKETS) {
            out[np] = av_packet_alloc();
            if (!out[np]) {
                av_packet_unref(pkt);
                break;
            }
            av_packet_move_ref(out[np], pkt);
            np++;
        } else {
            av_packet_unref(pkt);
        }
        rc = avcodec_receive_packet(enc, pkt);
    }
    av_packet_free(&pkt);
    uint32_t count = (uint32_t)np;
    int ret = 0;
    if (send_all(cfd, &count, 4) < 0) {
        ret = -1;
        goto done;
    }
    for (int i = 0; i < np; i++) {
        uint32_t len = (uint32_t)out[i]->size;
        if (send_all(cfd, &len, 4) < 0 || send_all(cfd, out[i]->data, out[i]->size) < 0) {
            ret = -1;
            for (int j = i; j < np; j++) av_packet_free(&out[j]);
            goto done;
        }
        *sent += 1;
        av_packet_free(&out[i]);
    }
done:
    return ret;
}

int main(int argc, char **argv) {
    if (argc < 6) {
        fprintf(stderr, "usage: %s socket width height fps bitrate\n", argv[0]);
        return 1;
    }
    int w = atoi(argv[2]) & ~1;
    int h = atoi(argv[3]) & ~1;
    int fps = atoi(argv[4]);
    long bitrate = atol(argv[5]);
    if (w < 16 || h < 16 || fps < 1 || bitrate < 100000) {
        fprintf(stderr, "nvenc: bad mode %dx%d@%d, %ld bit/s\n", w, h, fps, bitrate);
        return 1;
    }

    const AVCodec *codec = avcodec_find_encoder_by_name("hevc_nvenc");
    if (!codec) {
        fprintf(stderr, "nvenc: this libavcodec has no hevc_nvenc encoder\n");
        return 1;
    }
    AVCodecContext *enc = avcodec_alloc_context3(codec);
    if (!enc) return 1;
    enc->pix_fmt = AV_PIX_FMT_NV12;
    enc->profile = AV_PROFILE_HEVC_MAIN;
    enc->width = w;
    enc->height = h;
    enc->time_base = (AVRational){1, fps};
    enc->framerate = (AVRational){fps, 1};
    enc->bit_rate = bitrate;
    enc->gop_size = fps;
    enc->max_b_frames = 0;
    enc->flags |= AV_CODEC_FLAG_LOW_DELAY;
    enc->color_range = AVCOL_RANGE_JPEG;
    enc->colorspace = AVCOL_SPC_BT709;
    enc->color_primaries = AVCOL_PRI_BT709;
    enc->color_trc = AVCOL_TRC_BT709;

    /* Hardware HEVC on a remote desktop needs the smallest possible queue:
     * no B-frames, no lookahead, CBR so the packet pacing matches the link
     * rate, and zerolatency (NVENC otherwise keeps a reorder delay). */
    void *priv = enc->priv_data;
    set_opt(priv, "preset", opt_str("LGX_NVENC_PRESET", "p1"));
    set_opt(priv, "tune", opt_str("LGX_NVENC_TUNE", "ull"));
    set_opt(priv, "rc", "cbr");
    set_int(priv, "zerolatency", 1);
    set_int(priv, "delay", 0);
    set_int(priv, "rc-lookahead", 0);
    set_int(priv, "b_ref_mode", 0);
    set_int(priv, "multipass", 0);
    /* `pict_type = I` alone becomes NV_ENC_PIC_FLAG_FORCEINTRA, which NVENC
     * encodes as a TRAIL_R slice, not an IDR. The client gates new sessions on
     * NAL 19/20, so a 'K' request must really produce an IDR. */
    set_int(priv, "forced-idr", 1);
    /* Bound the internal surface pool; the default is fine for latency but a
     * small pool also bounds memory on 4K streams. */
    set_int(priv, "surfaces", 4);

    if (avcodec_open2(enc, codec, NULL) < 0) {
        fprintf(stderr, "nvenc: avcodec_open2 failed (is an NVIDIA GPU with NVENC present?)\n");
        return 1;
    }

    AVFrame *frame = av_frame_alloc();
    if (!frame) return 1;
    frame->format = AV_PIX_FMT_NV12;
    frame->width = w;
    frame->height = h;
    if (av_frame_get_buffer(frame, 0) < 0) {
        fprintf(stderr, "nvenc: frame buffer allocation failed\n");
        return 1;
    }

    int lsock = socket(AF_UNIX, SOCK_STREAM, 0);
    if (lsock < 0) {
        perror("nvenc: socket");
        return 1;
    }
    struct sockaddr_un addr = {0};
    addr.sun_family = AF_UNIX;
    snprintf(addr.sun_path, sizeof(addr.sun_path), "%s", argv[1]);
    unlink(argv[1]);
    if (bind(lsock, (struct sockaddr *)&addr, sizeof(addr)) < 0 || listen(lsock, 1) < 0) {
        perror("nvenc: bind/listen");
        return 1;
    }
    int cfd = accept(lsock, NULL, NULL);
    if (cfd < 0) {
        perror("nvenc: accept");
        return 1;
    }

    uint32_t no_frames = 0;
    if (send_all(cfd, &no_frames, 4) < 0) return 1;
    fprintf(stderr, "nvenc: HEVC %dx%d@%d ready (%s/%s, CBR %ld Mbit/s)\n", w, h, fps,
            opt_str("LGX_NVENC_PRESET", "p1"), opt_str("LGX_NVENC_TUNE", "ull"),
            bitrate / 1000000);

    size_t frame_bytes = (size_t)w * (size_t)h * 3 / 2;
    uint8_t *buf = malloc(frame_bytes);
    if (!buf) return 1;
    int force_idr = 0;
    uint64_t pts = 0;
    int encoded = 0;
    int ret = 0;

    for (;;) {
        uint8_t cmd;
        if (recv_all(cfd, &cmd, 1) < 0) break;
        if (cmd == 'Q') break;
        if (cmd == 'K') {
            force_idr = 1;
            continue;
        }
        if (cmd != 'F') {
            /* An unknown command means the peer and helper disagree about the
             * protocol; consuming the wrong bytes would desynchronise the
             * stream, so close instead. */
            fprintf(stderr, "nvenc: unknown command %u\n", (unsigned)cmd);
            ret = 1;
            break;
        }

        uint32_t idx = 0;
        uint32_t len = 0;
        if (recv_all(cfd, &idx, 4) < 0 || recv_all(cfd, &len, 4) < 0) break;
        if (len != frame_bytes) {
            /* Keep the stream in sync: the payload still has to be consumed. */
            fprintf(stderr, "nvenc: bad frame size %u (want %zu)\n", len, frame_bytes);
            if (len > frame_bytes) {
                uint8_t *swallow = malloc(len - frame_bytes);
                if (swallow) {
                    recv_all(cfd, swallow, len - frame_bytes);
                    free(swallow);
                }
            }
            uint32_t zero = 0;
            send_all(cfd, &zero, 4);
            continue;
        }
        if (recv_all(cfd, buf, len) < 0) break;

        if (av_frame_make_writable(frame) < 0) {
            uint32_t zero = 0;
            send_all(cfd, &zero, 4);
            continue;
        }
        for (int y = 0; y < h; y++) {
            memcpy(frame->data[0] + (size_t)y * frame->linesize[0], buf + (size_t)y * w, (size_t)w);
        }
        for (int y = 0; y < h / 2; y++) {
            memcpy(frame->data[1] + (size_t)y * frame->linesize[1],
                   buf + (size_t)w * h + (size_t)y * w, (size_t)w);
        }
        frame->pts = (int64_t)pts++;
        if (force_idr) {
            frame->pict_type = AV_PICTURE_TYPE_I;
            frame->flags |= AV_FRAME_FLAG_KEY;
            force_idr = 0;
        } else {
            frame->pict_type = AV_PICTURE_TYPE_NONE;
            frame->flags &= ~AV_FRAME_FLAG_KEY;
        }
        int sfr = avcodec_send_frame(enc, frame);
        if (sfr < 0) {
            char eb[128] = {0};
            av_strerror(sfr, eb, sizeof(eb));
            fprintf(stderr, "nvenc: send_frame failed: %s\n", eb);
            uint32_t zero = 0;
            send_all(cfd, &zero, 4);
            continue;
        }
        if (drain_packets(enc, cfd, &encoded) < 0) {
            ret = 1;
            break;
        }
    }

    free(buf);
    av_frame_free(&frame);
    avcodec_free_context(&enc);
    close(cfd);
    close(lsock);
    unlink(argv[1]);
    fprintf(stderr, "nvenc: closed after %d packets\n", encoded);
    return ret;
}
