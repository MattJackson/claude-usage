#!/usr/bin/env python3
"""Turn a raw, full-screen capture into a website-ready hero image.

Capture-quality standards this enforces (see packaging/screenshots/README.md
"Capture quality standards" for the human-readable version of the same
rules):

  1. Desktop context: the raw capture passed in should already be a
     full-screen (or near-full-screen) shot, not a tight crop to the menu's
     bounding box, so there's real desktop chrome/wallpaper visible around
     the menu bar / taskbar / panel. This script crops DOWN from that full
     shot rather than trying to crop UP from a tight one.
  2. Consistent 1600x900 output, always. If the source crop region is
     smaller than 1600x900 (very small/old display), the crop is padded
     with `--bg` rather than upscaled.
  3. Uniform menu position across OSes: the caller passes fractional anchor
     coordinates (0.0-1.0 of the source image's width/height) for where the
     menu/tray should land in the final frame; using fractions rather than
     literal pixel coordinates keeps behavior stable across the very
     different native resolutions GitHub-hosted macOS/Linux/Windows runners
     boot with.

Usage:
  postprocess.py <src.png> <out.png> --anchor-x 0.75 --anchor-y 0.11
      [--crop-width-frac 0.6] [--bg 30,30,30]
"""
from __future__ import annotations

import argparse
import sys

try:
    from PIL import Image
except ImportError as e:  # pragma: no cover - environment problem, not a bug
    print(
        "error: Pillow is required (pip install pillow) to post-process "
        f"screenshots: {e}",
        file=sys.stderr,
    )
    sys.exit(1)

TARGET_W = 1600
TARGET_H = 900
ASPECT = TARGET_W / TARGET_H  # 16:9


def parse_bg(spec: str) -> tuple[int, int, int]:
    parts = [int(p) for p in spec.split(",")]
    if len(parts) != 3:
        raise ValueError(f"--bg must be 'R,G,B', got {spec!r}")
    return parts[0], parts[1], parts[2]


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("src")
    parser.add_argument("out")
    parser.add_argument("--anchor-x", type=float, required=True, help="0.0-1.0 fraction of source width")
    parser.add_argument("--anchor-y", type=float, required=True, help="0.0-1.0 fraction of source height")
    parser.add_argument(
        "--crop-width-frac",
        type=float,
        default=0.6,
        help="fraction of source width to include in the 16:9 crop before resizing to 1600x900",
    )
    parser.add_argument("--bg", default="30,30,30", help="R,G,B pad color if the source is smaller than the crop")
    args = parser.parse_args(argv)

    bg = parse_bg(args.bg)
    im = Image.open(args.src).convert("RGB")
    src_w, src_h = im.size

    crop_w = max(int(src_w * args.crop_width_frac), 1)
    crop_h = int(crop_w / ASPECT)

    anchor_px = src_w * args.anchor_x
    anchor_py = src_h * args.anchor_y

    left = int(anchor_px - crop_w / 2)
    top = int(anchor_py - crop_h / 2)

    # Clamp the crop box fully inside the source image bounds rather than
    # letting the anchor push it off-frame (e.g. a menu anchored near the
    # right/bottom edge shouldn't produce a half-black crop from spilling
    # past the source's actual width/height).
    left = max(0, min(left, max(src_w - crop_w, 0)))
    top = max(0, min(top, max(src_h - crop_h, 0)))
    right = left + crop_w
    bottom = top + crop_h

    if crop_w <= src_w and crop_h <= src_h:
        cropped = im.crop((left, top, right, bottom))
    else:
        # Source is smaller than the requested crop (unusually small
        # display) — pad onto a canvas instead of upscaling, per capture
        # quality standard #2.
        canvas = Image.new("RGB", (crop_w, crop_h), bg)
        paste_x = max((crop_w - src_w) // 2, 0)
        paste_y = max((crop_h - src_h) // 2, 0)
        canvas.paste(im, (paste_x, paste_y))
        cropped = canvas

    final = cropped.resize((TARGET_W, TARGET_H), Image.LANCZOS)
    final.save(args.out)
    print(f"wrote {args.out} ({TARGET_W}x{TARGET_H}, cropped from {src_w}x{src_h} source)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
