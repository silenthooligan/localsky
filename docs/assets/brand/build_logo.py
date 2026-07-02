#!/usr/bin/env python3
"""LocalSky logomark / icon pipeline (v3, real vector trace).

Pipeline:
1. logomark-master.png  =  reference artwork cropped from the brand
                           reference jpg, transparent background.
2. Split master into TWO bitmaps by color:
     - ink_bmp  = pixels near #2a3139 (cloud outline + pin V)
     - blue_bmp = pixels near #1490dc (wifi + infinity + water drop)
3. Vector-trace each bitmap with pure-Python potrace, producing real
   cubic-Bezier SVG paths.
4. Compose those paths into:
     - logomark-art.svg          (transparent bg, ink + blue, canonical)
     - logomark-art-dark.svg     (teal recolor of ink for dark backdrops)
     - favicon.svg / favicon-light.svg / apple-touch-icon.svg
       (rounded-square backgrounds + the appropriate art layer)
     - icons/maskable-source.svg (safe-zone inset for Android adaptive)
5. Rasterize PNGs at the PWA-required sizes via Inkscape.

The v2 attempt embedded the master PNG as a base64 data URI inside the
SVG, which wasn't an honest mirror. v3 is real vector geometry.

Requires:
  pip install --user --break-system-packages potrace numpy pillow
  pacman/xbps/apt install inkscape    (for raster export)
"""

import subprocess
from pathlib import Path

import numpy as np
import potrace
from PIL import Image

# ─────────────────────────────────────────────────────────────────────
# Brand palette (extracted in v1 via PIL color histograms)
# ─────────────────────────────────────────────────────────────────────
INK_HEX  = "#2a3139"
BLUE_HEX = "#1490dc"
TEAL_HEX = "#149c92"
WHITE_HEX = "#ffffff"
NAVY_BG_HEX = "#0b1220"

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
PUBLIC = REPO / "public"
MASTER = HERE / "logomark-master.png"


# ─────────────────────────────────────────────────────────────────────
# Color-bucketed bitmaps from the master raster
# ─────────────────────────────────────────────────────────────────────
def _bucket_bitmap(master_arr: np.ndarray, target_rgb: tuple[int, int, int],
                   alpha_min: int = 80, max_distance: int = 90) -> np.ndarray:
    """Return a boolean bitmap of pixels within `max_distance` (cheap
    Manhattan in RGB) of `target_rgb` AND opaque enough."""
    R = master_arr[..., 0].astype(int)
    G = master_arr[..., 1].astype(int)
    B = master_arr[..., 2].astype(int)
    A = master_arr[..., 3]
    tR, tG, tB = target_rgb
    d = np.abs(R - tR) + np.abs(G - tG) + np.abs(B - tB)
    return (A > alpha_min) & (d < max_distance)


def _is_image_boundary(curve, W: int, H: int, slack: float = 2.0) -> bool:
    """Detect the bounding-box subpath potrace emits as the negative
    container around any traced region. Heuristic: every segment
    endpoint sits on the image perimeter (within `slack` px)."""
    points = [curve.start_point] + [s.end_point for s in curve]
    on_edge = sum(
        1 for p in points
        if p.x <= slack or p.x >= W - slack or p.y <= slack or p.y >= H - slack
    )
    return on_edge == len(points)


def _trace_bitmap_to_svg_d(bmp: np.ndarray, W: int, H: int) -> str:
    """Run potrace over the bool bitmap, drop image-boundary subpaths,
    and emit a single SVG path `d` attribute string."""
    bitmap = potrace.Bitmap(bmp)
    path = bitmap.trace(turdsize=4, alphamax=1.0, opttolerance=0.4)
    out_chunks: list[str] = []
    for curve in path:
        if _is_image_boundary(curve, W, H):
            continue
        sp = curve.start_point
        chunks = [f"M{sp.x:.1f},{sp.y:.1f}"]
        for seg in curve:
            ep = seg.end_point
            if seg.is_corner:
                c = seg.c
                chunks.append(f"L{c.x:.1f},{c.y:.1f}L{ep.x:.1f},{ep.y:.1f}")
            else:
                c1, c2 = seg.c1, seg.c2
                chunks.append(
                    f"C{c1.x:.1f},{c1.y:.1f} "
                    f"{c2.x:.1f},{c2.y:.1f} "
                    f"{ep.x:.1f},{ep.y:.1f}"
                )
        chunks.append("Z")
        out_chunks.append("".join(chunks))
    return " ".join(out_chunks)


# ─────────────────────────────────────────────────────────────────────
# SVG assembly
# ─────────────────────────────────────────────────────────────────────
def _svg_art(W: int, H: int, ink_d: str, blue_d: str, *, ink_fill: str) -> str:
    """Transparent-background SVG with the two-layer logomark.
    `ink_fill` swaps between INK (#2a3139, light variant) and TEAL
    (#149c92, dark variant); the blue layer stays blue either way."""
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" '
        f'role="img" aria-label="LocalSky">'
        f'<title>LocalSky</title>'
        f'<path fill="{ink_fill}" fill-rule="evenodd" d="{ink_d}"/>'
        f'<path fill="{BLUE_HEX}" fill-rule="evenodd" d="{blue_d}"/>'
        f'</svg>'
    )


def _svg_with_bg(W: int, H: int, ink_d: str, blue_d: str, *,
                 ink_fill: str, bg: str, bg_radius: int,
                 safe_margin: float = 0.0) -> str:
    """Same art but composited onto a rounded-square background. The
    art scales into a centered (1 - 2*safe_margin) box so the Android
    maskable variant can leave the platform a margin to crop."""
    art_size = W * (1 - 2 * safe_margin)
    art_offset = W * safe_margin
    # We re-use the trace coordinates directly; scaling happens via
    # an outer <g transform>.
    scale = art_size / W
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" '
        f'role="img" aria-label="LocalSky">'
        f'<title>LocalSky</title>'
        f'<rect width="{W}" height="{H}" rx="{bg_radius}" ry="{bg_radius}" fill="{bg}"/>'
        f'<g transform="translate({art_offset:.1f} {art_offset:.1f}) scale({scale:.4f})">'
        f'<path fill="{ink_fill}" fill-rule="evenodd" d="{ink_d}"/>'
        f'<path fill="{BLUE_HEX}" fill-rule="evenodd" d="{blue_d}"/>'
        f'</g></svg>'
    )


# ─────────────────────────────────────────────────────────────────────
# Raster export via Inkscape (already cached locally)
# ─────────────────────────────────────────────────────────────────────
def _rasterize(src_svg: Path, dest_png: Path, size: int):
    subprocess.run(
        [
            "inkscape",
            "--export-type=png",
            f"--export-filename={dest_png}",
            f"--export-width={size}",
            f"--export-height={size}",
            str(src_svg),
        ],
        check=False,
        capture_output=True,
    )


# ─────────────────────────────────────────────────────────────────────
def main():
    master = Image.open(MASTER).convert("RGBA")
    W, H = master.size
    arr = np.array(master)
    print(f"loaded master {W}x{H} from {MASTER.relative_to(REPO)}")

    ink_bmp  = _bucket_bitmap(arr, (0x2a, 0x31, 0x39))
    blue_bmp = _bucket_bitmap(arr, (0x14, 0x90, 0xdc))
    print(f"ink pixels: {int(ink_bmp.sum())}   blue pixels: {int(blue_bmp.sum())}")

    ink_d  = _trace_bitmap_to_svg_d(ink_bmp, W, H)
    blue_d = _trace_bitmap_to_svg_d(blue_bmp, W, H)
    print(f"traced d strings: ink={len(ink_d)} chars, blue={len(blue_d)} chars")

    # Canonical transparent-bg art tiles for README + docs.
    (HERE / "logomark-art.svg").write_text(
        _svg_art(W, H, ink_d, blue_d, ink_fill=INK_HEX)
    )
    (HERE / "logomark-art-dark.svg").write_text(
        _svg_art(W, H, ink_d, blue_d, ink_fill=TEAL_HEX)
    )

    # Favicons + apple-touch + maskable. All sourced from the same
    # traced d-strings; only the background + art_inset + ink color
    # change per variant.
    targets = [
        # (path, size, bg_radius, ink_fill, bg, safe_margin)
        (PUBLIC / "favicon.svg",       W, 70, TEAL_HEX, NAVY_BG_HEX, 0.06),
        (PUBLIC / "favicon-light.svg", W, 70, INK_HEX,  WHITE_HEX,   0.06),
        (PUBLIC / "apple-touch-icon.svg", W, 70, TEAL_HEX, NAVY_BG_HEX, 0.06),
        (PUBLIC / "icons" / "maskable-source.svg", W, 0, TEAL_HEX, NAVY_BG_HEX, 0.20),
    ]
    for path, _w, radius, ink_fill, bg, inset in targets:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(_svg_with_bg(
            W, H, ink_d, blue_d,
            ink_fill=ink_fill, bg=bg, bg_radius=radius, safe_margin=inset,
        ))
        print(f"  svg  {path.relative_to(REPO)}")

    # Raster PNGs the PWA manifest references. We render from the
    # composited SVGs (with backgrounds) so they look right against
    # the install splash / iOS home screen.
    rasters = [
        (PUBLIC / "favicon.svg",                  PUBLIC / "icons" / "icon-192.png", 192),
        (PUBLIC / "favicon.svg",                  PUBLIC / "icons" / "icon-512.png", 512),
        (PUBLIC / "icons" / "maskable-source.svg", PUBLIC / "icons" / "maskable-512.png", 512),
        (PUBLIC / "apple-touch-icon.svg",         PUBLIC / "icons" / "apple-touch-180.png", 180),
        # Side-by-side proof renders for the readme.
        (HERE / "logomark-art-dark.svg", HERE / "logomark-dark.png", 480),
        (HERE / "logomark-art.svg",      HERE / "logomark-light.png", 480),
    ]
    for src, dst, size in rasters:
        dst.parent.mkdir(parents=True, exist_ok=True)
        _rasterize(src, dst, size)
        print(f"  png  {dst.relative_to(REPO)}")


if __name__ == "__main__":
    main()
