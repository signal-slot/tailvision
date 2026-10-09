#!/usr/bin/env python3
"""Case for a tailvision unit: Raspberry Pi Zero W + Camera Module 3.

Two printed parts, no supports, no screws, no glue:
  body   - holds the Pi on four pegs rising from the floor, an opening for
           the USB port (the one that powers the unit), a slot for the
           microSD card in the end wall, and a pocket at the camera end for
           the cable to U-turn; the back is flat.
  lid    - carries the camera (lens out) on pegs and two clips, and snaps
           into the body: ridges on its lip drop into grooves in the long
           walls. Four posts press the Pi onto the body's pegs.

Dimensions come from the official drawings:
  Raspberry Pi Zero mechanical drawing (RP-008365)
  Camera Module 3 standard mechanical drawing (RP-008153)

Run:  python3 case.py           writes stl/*.stl and preview.png
      python3 case.py --copy-to ~/shared   also copies the STLs there (a
      fresh file each time, so a synced folder notices the change)
Needs: manifold3d, numpy (pillow for the preview).

Coordinates while modelling: the Pi board's bottom-left corner is (0, 0),
USB ports along y = 0, microSD at x = 0, camera connector at x = 65. z = 0 is
the top of the floor inside the body.
"""
import argparse
import math
import os
import shutil
import struct
import sys

import numpy as np
from manifold3d import CrossSection, FillRule, JoinType, Manifold, set_circular_segments

set_circular_segments(64)

# ---- parameters (mm) ------------------------------------------------------
PI_L, PI_W, PI_R, PI_T = 65.0, 30.0, 3.0, 1.4
PI_HOLES = [(3.5, 3.5), (61.5, 3.5), (3.5, 26.5), (61.5, 26.5)]
CLR = 0.4           # gap between the Pi outline and the inner wall
PORT_CLR = 1.4      # on the USB/HDMI side: the closed ports' shells stick out past the board edge
WALL = 1.6
FLOOR = 1.2
LID_T = 2.0
POCKET = 6.0        # extra room beyond the Pi's camera end for the cable loop
STANDOFF = 0.6      # floor top -> Pi underside: low pads under the holes keep the
                    # solder joints on the Pi's underside off the floor
H_IN = 8.5          # Pi top -> lid underside: the camera's back connector has to
                    # clear the PWR micro USB shell (2.9 tall), and the two cable
                    # ends want ~3 mm between them for the U-turn
STANDOFF_D = 5.0
PEG_D, PEG_H, PEG_TIP = 2.3, 3.0, 0.6     # pegs through the Pi's 2.75 mm holes; 2.5 printed too fat, 2.0 loose
POST_D, POST_RECESS_D, POST_RECESS_H = 5.0, 3.2, 2.2   # lid posts over the pegs
LIP_T, LIP_H, LIP_CLR = 1.0, 3.0, 0.2
SNAP_PROUD, SNAP_H, SNAP_LEN, SNAP_Z = 0.6, 1.0, 10.0, 1.9   # ridges on the lip, SNAP_Z below the plate
GROOVE_D, GROOVE_H = 0.5, 2.0                           # matching grooves in the long walls

# openings: the USB (OTG) port, which carries power too, and a slot for the
# microSD, which sticks out past the board edge by about 2 mm when it is in
# (so a closed wall at 0.4 mm keeps the card from seating). Mini HDMI (x 12.4)
# and PWR IN (x 54.0) stay closed. Heights relative to Pi top.
USB_X, USB_W, USB_Z = 41.4, 11.0, (-2.0, 5.0)
SD_Y, SD_W, SD_Z = 16.9, 13.0, (-0.4, 2.6)   # card is 11 wide, 1 thick, 0.3..1.3 above the board
SD_OUT = 2.0                                  # how far the seated card protrudes past the board edge

# camera module 3: u across the 25 mm width, v up from the connector edge
CAM_W, CAM_H = 25.0, 23.862
CAM_HOLES_U, CAM_HOLES_V = (2.0, 23.0), (2.0, 14.5)
CAM_LENS_U, CAM_LENS_V = 12.5, 14.4
CAM_HOUSING = 10.8          # square sensor housing around the lens
CAM_FRONT = 7.5             # board front -> lens tip (6.98 + adhesive layer)
CAM_HOUSING_H = 3.9         # board front -> housing front
CAM_BOTTOM_X = 57.0         # where the connector edge (v = 0) sits, toward the Pi's CSI end
CAM_CY = 15.0               # camera centre line = Pi centre line
CAM_BOSS_D, CAM_BOSS_H = 4.2, 1.2
CAM_PEG_D, CAM_PEG_H = 1.4, 2.0           # into the camera's 2.2 mm holes; 1.9 printed too fat
CLIP_T, CLIP_W, CLIP_BARB, CLIP_CLR = 1.0, 6.0, 0.6, 0.4    # clips over the board's long edges; 0.15 printed too tight
WINDOW_CLR = 0.4

# ---- derived ---------------------------------------------------------------
IN_X0, IN_Y0 = -CLR, -PORT_CLR
IN_L, IN_W = PI_L + 2 * CLR + POCKET, PI_W + CLR + PORT_CLR
IN_R = PI_R + 0.2
OUT_X0, OUT_Y0 = IN_X0 - WALL, IN_Y0 - WALL
OUT_L, OUT_W, OUT_R = IN_L + 2 * WALL, IN_W + 2 * WALL, IN_R + WALL
Z_PI = STANDOFF
Z_PT = Z_PI + PI_T
Z_LID = Z_PT + H_IN
Z_TOP = Z_LID + LID_T
LENS_X = CAM_BOTTOM_X - CAM_LENS_V
LENS_Y = CAM_CY + (CAM_LENS_U - CAM_W / 2)


def cam_xy(u, v):
    """Camera board (u, v) -> case (x, y): connector edge faces +x."""
    return CAM_BOTTOM_X - v, CAM_CY + (CAM_W / 2 - u)


def rrect(x0, y0, w, h, r):
    cs = CrossSection.square([w - 2 * r, h - 2 * r], center=True).offset(r, JoinType.Round)
    return cs.translate([x0 + w / 2, y0 + h / 2])


def box(x0, y0, z0, w, h, d):
    return Manifold.cube([w, h, d]).translate([x0, y0, z0])


def cyl(x, y, z0, d, h):
    return Manifold.cylinder(h, d / 2).translate([x, y, z0])


def poly(points):
    return CrossSection([points], FillRule.EvenOdd)


# ---- body ------------------------------------------------------------------
def body():
    outer = rrect(OUT_X0, OUT_Y0, OUT_L, OUT_W, OUT_R)
    inner = rrect(IN_X0, IN_Y0, IN_L, IN_W, IN_R)
    b = outer.extrude(FLOOR + Z_LID).translate([0, 0, -FLOOR])
    b -= inner.extrude(Z_LID + 1).translate([0, 0, 0])
    # pads with pegs that go through the Pi's holes
    for hx, hy in PI_HOLES:
        b += cyl(hx, hy, 0, STANDOFF_D, STANDOFF)
        b += cyl(hx, hy, STANDOFF, PEG_D, PEG_H - PEG_TIP)
        b += Manifold.cylinder(PEG_TIP, PEG_D / 2, PEG_D / 2 - 0.4).translate([hx, hy, STANDOFF + PEG_H - PEG_TIP])
    # snap grooves along the inside of the long walls
    z0 = Z_LID - SNAP_Z - GROOVE_H / 2
    b -= box(IN_X0 + IN_R, IN_Y0 - GROOVE_D, z0, IN_L - 2 * IN_R, GROOVE_D + 0.01, GROOVE_H)
    b -= box(IN_X0 + IN_R, IN_Y0 + IN_W - 0.01, z0, IN_L - 2 * IN_R, GROOVE_D + 0.01, GROOVE_H)
    # USB port on the y = 0 wall
    b -= box(USB_X - USB_W / 2, OUT_Y0 - 1, Z_PT + USB_Z[0], USB_W, WALL + PORT_CLR + 2, USB_Z[1] - USB_Z[0])
    # microSD slot through the x = 0 end wall
    b -= box(OUT_X0 - 1, SD_Y - SD_W / 2, Z_PT + SD_Z[0], WALL + CLR + 2, SD_W, SD_Z[1] - SD_Z[0])
    return b


# ---- lid -------------------------------------------------------------------
def window_cs(grow=0.0):
    s = CAM_HOUSING + 2 * (WINDOW_CLR + grow)
    return CrossSection.square([s, s], center=True).translate([LENS_X, LENS_Y])


def lid():
    outer = rrect(OUT_X0, OUT_Y0, OUT_L, OUT_W, OUT_R)
    inner = rrect(IN_X0, IN_Y0, IN_L, IN_W, IN_R)
    l = outer.extrude(LID_T).translate([0, 0, Z_LID])
    lip = inner.offset(-LIP_CLR, JoinType.Round) - inner.offset(-LIP_CLR - LIP_T, JoinType.Round)
    l += lip.extrude(LIP_H).translate([0, 0, Z_LID - LIP_H])
    # snap ridges on the lip's outer faces, lead-in chamfers both ways
    zc = Z_LID - SNAP_Z
    for y0, dirn in [(IN_Y0 + LIP_CLR, -1), (IN_Y0 + IN_W - LIP_CLR, 1)]:
        prof = poly([[0, -SNAP_H / 2 - SNAP_PROUD], [dirn * SNAP_PROUD, -SNAP_H / 2],
                     [dirn * SNAP_PROUD, SNAP_H / 2], [0, SNAP_H / 2 + SNAP_PROUD]])
        for xc in (IN_X0 + IN_L * 0.25, IN_X0 + IN_L * 0.75):
            l += prof.extrude(SNAP_LEN).rotate([90, 0, 90]).translate([xc - SNAP_LEN / 2, y0, zc])
    # posts that press the Pi onto the pegs, recessed for the peg tips
    for hx, hy in PI_HOLES:
        l += cyl(hx, hy, Z_PT + 0.2, POST_D, H_IN - 0.2)
        l -= cyl(hx, hy, Z_PT - 1, POST_RECESS_D, POST_RECESS_H + 1)
    # camera bosses with pegs, and two clips hooking over the board's long edges
    for u in CAM_HOLES_U:
        for v in CAM_HOLES_V:
            x, y = cam_xy(u, v)
            l += cyl(x, y, Z_LID - CAM_BOSS_H, CAM_BOSS_D, CAM_BOSS_H)
            l += cyl(x, y, Z_LID - CAM_BOSS_H - CAM_PEG_H, CAM_PEG_D, CAM_PEG_H)
    z_back = Z_LID - CAM_BOSS_H - 1.12          # the camera board's back face
    z_bot = z_back - CLIP_CLR - CLIP_BARB - 0.7  # hook tip
    h = Z_LID - z_bot
    yt = z_back - CLIP_CLR - z_bot               # barb's flat, in profile height
    xc = CAM_BOTTOM_X - sum(CAM_HOLES_V) / 2   # midway between the two bosses on each edge
    for u, dirn in [(0.0, -1), (CAM_W, 1)]:     # dirn points toward the board (u = 0 is the +y edge)
        _, ye = cam_xy(u, 0)
        yo = ye - dirn * CLIP_CLR               # clip face just off the board edge
        stem = poly([[0, 0], [-dirn * CLIP_T, 0], [-dirn * CLIP_T, h], [0, h]])
        barb = poly([[0, yt], [dirn * CLIP_BARB, yt], [dirn * CLIP_BARB, yt - 0.4], [0, yt - 0.4 - CLIP_BARB]])
        l += (stem + barb).extrude(CLIP_W).rotate([90, 0, 90]).translate([xc - CLIP_W / 2, yo, z_bot])
    # lens window
    l -= window_cs().extrude(LID_T + 2).translate([0, 0, Z_LID - 1])
    return l


# ---- mock parts for clearance checks ----------------------------------------
def mock_pi():
    """Pi Zero W with its tallest top-side parts, from the Zero drawing."""
    p = rrect(0, 0, PI_L, PI_W, PI_R).extrude(PI_T).translate([0, 0, Z_PI])
    for hx, hy in PI_HOLES:
        p -= cyl(hx, hy, Z_PI - 1, 2.75, PI_T + 2)
    p += box(12.4 - 5.6, -1.0, Z_PT, 11.2, 8.0, 3.2)              # mini HDMI
    for x in (USB_X, 54.0):
        p += box(x - 4.0, -1.0, Z_PT, 8.0, 6.5, 2.9)              # micro USB
    p += box(0, SD_Y - 7, Z_PT, 14.0, 14.0, 1.5)                  # microSD socket
    p += box(-SD_OUT, SD_Y - 5.5, Z_PT + 0.3, SD_OUT + 10.0, 11.0, 1.0)  # seated card, sticking out
    p += box(60.0, 7.0, Z_PT, 5.0, 16.0, 1.6)                     # CSI connector
    p += box(24.0, 8.0, Z_PT, 14.0, 14.0, 1.5)                    # SoC
    return p


def mock_camera():
    z_front = Z_LID - CAM_BOSS_H
    x0, y0 = cam_xy(CAM_W, CAM_H)
    cam = box(x0, y0, z_front - 1.12, CAM_H, CAM_W, 1.12)
    for u in CAM_HOLES_U:
        for v in CAM_HOLES_V:
            x, y = cam_xy(u, v)
            cam -= cyl(x, y, z_front - 2, 2.2, 3)
    s = CAM_HOUSING
    cam += box(LENS_X - s / 2, LENS_Y - s / 2, z_front, s, s, CAM_HOUSING_H)
    cam += cyl(LENS_X, LENS_Y, z_front, 5.75, CAM_FRONT)
    cx0, cy0 = cam_xy(CAM_W / 2 + 19.61 / 2, 5.71)
    cam += box(cx0, cy0, z_front - 1.12 - 2.75, 5.71, 19.61, 2.75)  # back FPC connector
    for u in CAM_HOLES_U:                       # the connector body stops short of the holes
        x, y = cam_xy(u, CAM_HOLES_V[0])
        cam -= cyl(x, y, z_front - 5, 2.2, 6)
    return cam


def check(b, l):
    pi, cam = mock_pi(), mock_camera()
    probs = []
    for name, a, c in [("Pi/body", pi, b), ("Pi/lid", pi, l), ("camera/lid", cam, l),
                       ("camera/body", cam, b), ("camera/Pi", cam, pi)]:
        v = (a ^ c).volume()
        if v > 1e-6:
            probs.append(f"{name} overlap {v:.2f} mm3")
    # the lid tubes must land on bare board: tube footprint vs Pi parts above the board
    posts = Manifold.compose([cyl(hx, hy, Z_PT, POST_D, 1) for hx, hy in PI_HOLES])
    v = (posts ^ (pi - rrect(0, 0, PI_L, PI_W, PI_R).extrude(PI_T).translate([0, 0, Z_PI]))).volume()
    if v > 1e-6:
        probs.append(f"posts sit on Pi parts, overlap {v:.2f} mm3")
    v = (l ^ b).volume()
    if v > 1e-6:
        probs.append(f"lid/body overlap {v:.2f} mm3 (snap ridges aside this must be 0)")
    return probs, pi, cam


# ---- export ----------------------------------------------------------------
def write_stl(path, man):
    mesh = man.to_mesh()
    v = np.asarray(mesh.vert_properties, dtype=np.float32)[:, :3]
    t = np.asarray(mesh.tri_verts, dtype=np.int64)
    a, b, c = v[t[:, 0]], v[t[:, 1]], v[t[:, 2]]
    n = np.cross(b - a, c - a)
    n /= np.maximum(np.linalg.norm(n, axis=1, keepdims=True), 1e-12)
    rec = np.zeros(len(t), dtype=[("n", "<f4", 3), ("v", "<f4", (3, 3)), ("attr", "<u2")])
    rec["n"] = n
    rec["v"][:, 0], rec["v"][:, 1], rec["v"][:, 2] = a, b, c
    with open(path, "wb") as f:
        f.write(b"tailvision case".ljust(80, b"\0"))
        f.write(struct.pack("<I", len(t)))
        f.write(rec.tobytes())


def print_ready(man):
    """Translate so the part sits on z = 0."""
    bb = man.bounding_box()
    return man.translate([0, 0, -bb[2]])


def render(man, direction, up, res=0.3, pad=3):
    """Orthographic shaded render of a part, looking along `direction`."""
    from PIL import Image
    d = np.array(direction, float)
    d /= np.linalg.norm(d)
    u = np.cross(np.array(up, float), d)
    u /= np.linalg.norm(u)
    v = np.cross(d, u)
    bb = np.array(man.bounding_box()).reshape(2, 3)
    corners = np.array([[x, y, z] for x in bb[:, 0] for y in bb[:, 1] for z in bb[:, 2]])
    cu, cv, cd = corners @ u, corners @ v, corners @ d
    us = np.arange(cu.min() - pad, cu.max() + pad, res)
    vs = np.arange(cv.max() + pad, cv.min() - pad, -res)
    start, end = cd.min() - 1, cd.max() + 1
    light = np.array([0.3, -0.5, 0.8])
    light /= np.linalg.norm(light)
    img = np.ones((len(vs), len(us), 3))
    dep = np.full((len(vs), len(us)), 1e3)
    for i, vv in enumerate(vs):
        for j, uu in enumerate(us):
            o = uu * u + vv * v + start * d
            e = uu * u + vv * v + end * d
            hits = man.ray_cast(list(o), list(e))
            if hits:
                n = np.array(hits[0].normal)
                t = hits[0].distance
                shade = 0.25 + 0.4 * max(0.0, float(n @ light)) + 0.35 * max(0.0, float(-n @ d))
                img[i, j] = np.array([0.75, 0.82, 0.95]) * shade * (1 - 0.3 * t)
                dep[i, j] = t * (end - start)
    edge = np.zeros(dep.shape, dtype=bool)
    edge[1:, :] |= np.abs(dep[1:, :] - dep[:-1, :]) > 0.6
    edge[:, 1:] |= np.abs(dep[:, 1:] - dep[:, :-1]) > 0.6
    img[edge] = 0.1
    return Image.fromarray((img * 255).astype(np.uint8))


def preview(path, b, l):
    try:
        from PIL import Image
    except ImportError:
        print("pillow missing, no preview", file=sys.stderr)
        return
    tiles = [
        render(b, (-0.5, 0.6, -0.6), (0, 0, 1)),
        render(l, (-0.5, 0.6, 0.6), (0, 0, 1)),
        render(b, (0, 1, 0), (0, 0, 1)),
        render(l, (0, 0, -1), (0, 1, 0)),
        render(b, (0, 0, 1), (0, 1, 0)),
        render(l, (0, 0, 1), (0, 1, 0)),
    ]
    w = max(t.width for t in tiles)
    h = max(t.height for t in tiles)
    sheet = Image.new("RGB", (3 * w, 2 * h), "white")
    for i, t in enumerate(tiles):
        sheet.paste(t, ((i % 3) * w + (w - t.width) // 2, (i // 3) * h + (h - t.height) // 2))
    sheet.save(path)


def copy_fresh(src, dst_dir):
    """Copy as a new file swapped into place, not a rewrite of the old one:
    folder watchers (Syncthing) see the rename, and nothing hard-linked to
    the old copy changes under it."""
    dst = os.path.join(dst_dir, os.path.basename(src))
    tmp = dst + ".tmp"
    shutil.copyfile(src, tmp)
    os.replace(tmp, dst)
    return dst


def main():
    ap = argparse.ArgumentParser(description="Build the case STLs.")
    ap.add_argument("--copy-to", metavar="DIR", action="append", default=[],
                    help="also copy the STLs into DIR (repeatable)")
    args = ap.parse_args()
    here = os.path.dirname(os.path.abspath(__file__))
    out = os.path.join(here, "stl")
    os.makedirs(out, exist_ok=True)
    b = body()
    l = lid()
    probs, pi, cam = check(b, l)
    for p in probs:
        print("CHECK:", p)
    if not probs:
        print("checks: Pi, camera and posts clear")
    parts = {"body": b, "lid": l.rotate([180, 0, 0])}
    for name, man in parts.items():
        man = print_ready(man)
        path = os.path.join(out, f"{name}.stl")
        write_stl(path, man)
        for d in args.copy_to:
            copy_fresh(path, os.path.expanduser(d))
        bb = man.bounding_box()
        print(f"{name:6s} {bb[3]-bb[0]:5.1f} x {bb[4]-bb[1]:5.1f} x {bb[5]-bb[2]:5.1f} mm, "
              f"{man.volume()/1000:.1f} cm3, {man.num_tri()} tris, genus {man.genus()}")
    print(f"outer {OUT_L:.1f} x {OUT_W:.1f} x {Z_TOP + FLOOR:.1f} mm "
          f"(lens tip +{Z_LID - CAM_BOSS_H + CAM_FRONT - Z_TOP:.1f}), lens at x={LENS_X:.1f} y={LENS_Y:.1f}")
    print("no screws: Pi on pegs under the lid posts, camera on pegs under two clips, lid snaps in")
    preview(os.path.join(here, "preview.png"), b, l)
    for d in args.copy_to:
        print(f"copied to {d}")


if __name__ == "__main__":
    main()
