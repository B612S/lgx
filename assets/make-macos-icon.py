#!/usr/bin/env python3
"""Generate the Liquid Glass LGX Server icon layers and preview.

The .icon package is compiled with the open-source actool (or Xcode's actool)
into Assets.car + AppIcon.icns; macOS 26/27 renders the glass groups and the
light/dark/tinted appearances.
"""
import json
import math
import os
from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw, ImageFilter

OUT = Path(__file__).resolve().parent / "AppIcon.icon"
ASSETS = OUT / "Assets"
S = 2048  # render at 2x, downsample to 1024
SCALE = S / 1024.0
FINAL = 1024


def sc(v):
    return v * SCALE


def color(hex_or_tuple, a=255):
    if isinstance(hex_or_tuple, str):
        h = hex_or_tuple.lstrip("#")
        return (int(h[0:2], 16), int(h[2:4], 16), int(h[4:6], 16), a)
    return (*hex_or_tuple, a)


def gradient(size, box, c0, c1, p0=(0, 0), p1=(0, 1)):
    """Linear gradient inside box; p0/p1 are normalized within the box."""
    x0, y0, x1, y1 = [sc(v) for v in box]
    w, h = max(1, int(x1 - x0)), max(1, int(y1 - y0))
    ys, xs = np.mgrid[0:h, 0:w].astype(np.float32)
    dx = (p1[0] - p0[0]) * w
    dy = (p1[1] - p0[1]) * h
    denom = dx * dx + dy * dy
    if denom == 0:
        t = np.zeros((h, w), np.float32)
    else:
        t = ((xs - p0[0] * w) * dx + (ys - p0[1] * h) * dy) / denom
    t = np.clip(t, 0, 1)[..., None]
    c0a = np.array(c0, np.float32)
    c1a = np.array(c1, np.float32)
    arr = (c0a + (c1a - c0a) * t).astype(np.uint8)
    return Image.fromarray(arr, "RGBA")


def radial(size):
    ys, xs = np.mgrid[0:size, 0:size].astype(np.float32)
    return xs, ys


def radial_layer(size, center, radius, rgb, alpha, power=2.0):
    xs, ys = radial(size)
    d = np.sqrt((xs - center[0]) ** 2 + (ys - center[1]) ** 2) / radius
    a = np.clip(1.0 - d, 0, 1) ** power * alpha
    arr = np.zeros((size, size, 4), np.uint8)
    arr[..., 0] = rgb[0]
    arr[..., 1] = rgb[1]
    arr[..., 2] = rgb[2]
    arr[..., 3] = (a * 255).astype(np.uint8)
    return Image.fromarray(arr, "RGBA")


def rounded_mask(size, box, radius):
    m = Image.new("L", (size, size), 0)
    d = ImageDraw.Draw(m)
    d.rounded_rectangle([sc(v) for v in box], radius=sc(radius), fill=255)
    return m


def union_mask(size, shapes):
    m = Image.new("L", (size, size), 0)
    d = ImageDraw.Draw(m)
    for box, radius in shapes:
        d.rounded_rectangle([sc(v) for v in box], radius=sc(radius), fill=255)
    return m


def mask_to_rgba(mask):
    out = Image.new("RGBA", mask.size, (0, 0, 0, 0))
    out.putalpha(mask)
    return out


def colorize(mask, grad):
    out = grad.convert("RGBA").copy()
    a = mask
    out.putalpha(Image.composite(a, Image.new("L", mask.size, 0), out.getchannel("A")))
    return out


def save(img, name):
    img.resize((FINAL, FINAL), Image.LANCZOS).save(ASSETS / name)


def main():
    ASSETS.mkdir(parents=True, exist_ok=True)
    size = S

    # ---- body / stand silhouette -------------------------------------------
    body_shapes = [
        ((170, 232, 854, 664), 70),
        ((470, 664, 554, 740), 18),
        ((366, 736, 658, 772), 18),
    ]
    body_mask = union_mask(size, body_shapes)

    # Body: cool cyan/blue glass, brighter at the top-left.
    body_grad = gradient(size, (0, 0, 1024, 1024), color("#9CF4FF", 232), color("#1B5EA8", 208), (0.15, 0.05), (0.85, 1.0))
    body = colorize(body_mask, body_grad)
    # Glass rim.
    rim = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    d = ImageDraw.Draw(rim)
    for box, radius in body_shapes:
        d.rounded_rectangle([sc(v) for v in box], radius=sc(radius), outline=(230, 255, 255, 150), width=max(2, int(sc(3))))
    body = Image.alpha_composite(body, rim)
    # A soft inner shadow at the bottom gives the slab thickness.
    inner = Image.new("L", (size, size), 0)
    ImageDraw.Draw(inner).rounded_rectangle([sc(170), sc(232), sc(854), sc(664)], radius=sc(70), fill=255)
    shadow_band = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    ImageDraw.Draw(shadow_band).rectangle([0, sc(560), size, sc(680)], fill=(0, 20, 40, 26))
    shadow_band = shadow_band.filter(ImageFilter.GaussianBlur(sc(18)))
    body = Image.alpha_composite(body, Image.composite(shadow_band, Image.new("RGBA", (size, size), (0, 0, 0, 0)), inner))
    save(body, "body.png")

    # ---- screen -------------------------------------------------------------
    screen_box = (202, 264, 822, 632)
    screen_mask = rounded_mask(size, screen_box, 48)
    screen_grad = gradient(size, (0, 0, 1024, 1024), color("#071521"), color("#0B2A3E"), (0.5, 0.0), (0.5, 1.0))
    screen = colorize(screen_mask, screen_grad)
    # Top reflection inside the screen.
    refl = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    ImageDraw.Draw(refl).ellipse([sc(202), sc(210), sc(822), sc(430)], fill=(120, 220, 255, 26))
    refl = refl.filter(ImageFilter.GaussianBlur(sc(30)))
    screen = Image.alpha_composite(screen, Image.composite(refl, Image.new("RGBA", (size, size), (0, 0, 0, 0)), screen_mask))
    # Inner border.
    inner_rim = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    ImageDraw.Draw(inner_rim).rounded_rectangle([sc(202), sc(264), sc(822), sc(632)], radius=sc(48), outline=(200, 245, 255, 55), width=max(2, int(sc(2))))
    screen = Image.alpha_composite(screen, inner_rim)
    save(screen, "screen.png")

    # ---- bolt ---------------------------------------------------------------
    bolt = [(560, 312), (430, 506), (500, 506), (466, 624), (606, 428), (530, 428)]
    bolt_mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(bolt_mask).polygon([(sc(x), sc(y)) for x, y in bolt], fill=255)
    bolt_grad = gradient(size, (0, 0, 1024, 1024), color("#FFF1B8"), color("#FF8A2A"), (0.5, 0.28), (0.5, 0.72))
    bolt_img = colorize(bolt_mask, bolt_grad)
    # Bright edge.
    edge = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    ImageDraw.Draw(edge).line([(sc(x), sc(y)) for x, y in bolt] + [(sc(bolt[0][0]), sc(bolt[0][1]))], fill=(255, 245, 210, 130), width=max(2, int(sc(3))), joint="curve")
    bolt_img = Image.alpha_composite(bolt_img, edge)
    save(bolt_img, "bolt.png")

    # Bolt glow.
    glow = Image.new("RGBA", (size, size), (255, 176, 60, 0))
    gm = bolt_mask.filter(ImageFilter.GaussianBlur(sc(38)))
    gm = gm.point(lambda v: int(v * 0.75))
    glow.putalpha(gm)
    save(glow, "bolt-glow.png")

    # ---- display highlight / specular --------------------------------------
    hl = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    d = ImageDraw.Draw(hl)
    # Diagonal sheen across the top-left bezel.
    poly = [(sc(120), sc(210)), (sc(610), sc(210)), (sc(330), sc(700)), (sc(120), sc(700))]
    d.polygon(poly, fill=(255, 255, 255, 16))
    hl = hl.filter(ImageFilter.GaussianBlur(sc(34)))
    hl = Image.composite(hl, Image.new("RGBA", (size, size), (0, 0, 0, 0)), body_mask)
    save(hl, "highlight.png")

    # ---- glow and shadow ----------------------------------------------------
    glow_bg = radial_layer(size, (sc(512), sc(560)), sc(520), (38, 214, 235), 0.55, 2.2)
    save(glow_bg, "glow.png")

    shadow_mask = body_mask.filter(ImageFilter.GaussianBlur(sc(46)))
    shadow = Image.new("RGBA", (size, size), (0, 8, 20, 0))
    shadow.putalpha(shadow_mask.point(lambda v: int(v * 0.62)))
    save(shadow, "shadow.png")

    # ---- tinted (monochrome) composite -------------------------------------
    t = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    tm = mask_to_rgba(body_mask)
    tm = Image.composite(Image.new("RGBA", (size, size), (255, 255, 255, 255)), t, body_mask)
    t = Image.alpha_composite(t, tm)
    # screen: dark translucent inset
    sm = Image.new("RGBA", (size, size), (10, 14, 18, 210))
    sm.putalpha(Image.composite(Image.new("L", (size, size), 210), Image.new("L", (size, size), 0), screen_mask))
    t = Image.alpha_composite(t, sm)
    # bolt: white
    bm = Image.new("RGBA", (size, size), (255, 255, 255, 255))
    bm.putalpha(bolt_mask)
    t = Image.alpha_composite(t, bm)
    # highlight: white sheen
    hm = hl.copy()
    # recolor highlight to white at same alpha
    white_hl = Image.new("RGBA", (size, size), (255, 255, 255, 255))
    white_hl.putalpha(hl.getchannel("A"))
    t = Image.alpha_composite(t, white_hl)
    save(t, "tinted.png")

    # ---- icon.json ----------------------------------------------------------
    def fill_gradient(c0, c1):
        return {"linear-gradient": [c0, c1], "orientation": {"start": {"x": 0.5, "y": 0.0}, "stop": {"x": 0.5, "y": 1.0}}}

    def group(name, image, glass=False, **extra):
        g = {
            "name": name,
            "layers": [{"name": name, "image-name": image, "glass": glass}],
            "shadow": {"kind": "none", "opacity": 0},
            "specular": False,
            "translucency": {"enabled": False, "value": 0},
        }
        g.update(extra)
        return g

    colored_hidden = [{"appearance": "tinted", "value": True}]
    tinted_visible = [{"appearance": "tinted", "value": False}]

    groups = [
        # Front to back.
        group("Highlight", "highlight.png", **{"hidden-specializations": colored_hidden}),
        group("Bolt", "bolt.png", **{"hidden-specializations": colored_hidden}),
        group("BoltGlow", "bolt-glow.png", **{"blend-mode": "screen", "hidden-specializations": colored_hidden}),
        group("Screen", "screen.png", **{"hidden-specializations": colored_hidden}),
        group(
            "Display",
            "body.png",
            glass=True,
            **{
                "lighting": "combined",
                "specular": True,
                "blur-material": 0.68,
                "translucency": {"enabled": True, "value": 0.36},
                "shadow": {"kind": "layer-color", "opacity": 0.35},
                "hidden-specializations": colored_hidden,
            },
        ),
        # Tinted: one flattened monochrome composition, in front of the shadow.
        group(
            "Tinted",
            "tinted.png",
            glass=True,
            **{
                "lighting": "combined",
                "specular": True,
                "blur-material": 0.55,
                "translucency": {"enabled": True, "value": 0.25},
                "shadow": {"kind": "none", "opacity": 0},
                "hidden": True,
                "hidden-specializations": tinted_visible,
            },
        ),
        group("Shadow", "shadow.png"),
        group("Glow", "glow.png", **{"blend-mode": "screen", "hidden-specializations": colored_hidden}),
    ]

    icon = {
        "fill": fill_gradient("srgb:0.02,0.06,0.10,1.00", "srgb:0.02,0.13,0.18,1.00"),
        "fill-specializations": [
            {"value": fill_gradient("srgb:0.02,0.06,0.10,1.00", "srgb:0.02,0.13,0.18,1.00")},
            {"appearance": "dark", "value": fill_gradient("srgb:0.00,0.02,0.05,1.00", "srgb:0.01,0.09,0.13,1.00")},
            {"appearance": "tinted", "value": {"solid": "srgb:0.10,0.12,0.14,1.00"}},
        ],
        "groups": groups,
        "supported-platforms": {"squares": ["macOS"]},
    }
    (OUT / "icon.json").write_text(json.dumps(icon, indent=2) + "\n")

    # ---- preview composite (approximate; native glass is richer) ----------
    bg = gradient(size, (0, 0, 1024, 1024), color("#050F19"), color("#0A2A38"))
    bg = Image.alpha_composite(bg, glow_bg)
    bg = Image.alpha_composite(bg, Image.open(ASSETS / "shadow.png").resize((size, size), Image.LANCZOS))
    for name in ["body.png", "screen.png", "bolt-glow.png", "bolt.png", "highlight.png"]:
        bg = Image.alpha_composite(bg, Image.open(ASSETS / name).resize((size, size), Image.LANCZOS))
    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, size, size], radius=sc(232), fill=255)
    out = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    out.paste(bg, (0, 0), mask)
    out.resize((FINAL, FINAL), Image.LANCZOS).save(OUT.parent / "lgx-icon-preview.png")
    print("wrote", OUT)


if __name__ == "__main__":
    main()
