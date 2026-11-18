"""Writes background.svg next to this file: the Rudder.dmg window art, 660x420.

Icon centers must match settings.py: Rudder.app (170, 196), Applications
(490, 196), icon size 128. After editing, render the PNGs (README.md, "DMG").
"""
import math
import os

W, H = 660, 420
APP = (170, 196)
APPS = (490, 196)
ACCENT = "#3a5fe0"
INK = "#1b1f24"
INK2 = "#4a5058"
PAPER = "#f6f4ef"


def bez(p, t):
    (x0, y0), (x1, y1), (x2, y2), (x3, y3) = p
    u = 1 - t
    x = u**3 * x0 + 3 * u * u * t * x1 + 3 * u * t * t * x2 + t**3 * x3
    y = u**3 * y0 + 3 * u * u * t * y1 + 3 * u * t * t * y2 + t**3 * y3
    return x, y


def even_points(p, n, t_end):
    samples = [bez(p, t_end * i / 2000) for i in range(2001)]
    lengths = [0.0]
    for a, b in zip(samples, samples[1:]):
        lengths.append(lengths[-1] + math.dist(a, b))
    total = lengths[-1]
    out, j = [], 0
    for k in range(n):
        target = total * k / (n - 1)
        while lengths[j] < target and j < len(lengths) - 1:
            j += 1
        out.append(samples[j])
    return out


# The wake: from Rudder's right edge, arcing up, down into Applications.
WAKE = [(252, 184), (294, 112), (374, 110), (414, 172)]
dots = []
points = even_points(WAKE, 16, 0.86)
for i, (x, y) in enumerate(points):
    s = i / (len(points) - 1)
    r = 1.3 + 1.9 * s
    o = 0.16 + 0.74 * s
    dots.append(f'<circle cx="{x:.1f}" cy="{y:.1f}" r="{r:.2f}" fill="{ACCENT}" fill-opacity="{o:.2f}"/>')

# Arrowhead just past the last dot, along the curve's tangent there.
def tangent(p, t, h=1e-3):
    (ax, ay), (bx, by) = bez(p, t - h), bez(p, t + h)
    n = math.hypot(bx - ax, by - ay)
    return (bx - ax) / n, (by - ay) / n

tipx, tipy = bez(WAKE, 1.0)
dx, dy = tangent(WAKE, 0.97)
bx, by = tipx - 15 * dx, tipy - 15 * dy
nx, ny = -dy, dx
arrow = (
    f'<path d="M{tipx:.1f} {tipy:.1f} L{bx + 4.6 * nx:.1f} {by + 4.6 * ny:.1f} '
    f'L{bx - 4.6 * nx:.1f} {by - 4.6 * ny:.1f} Z" fill="{ACCENT}" stroke="{ACCENT}" '
    f'stroke-width="1.6" stroke-linejoin="round"/>'
)

# Depth contours, like a nautical chart, faded out behind the icons.
contours = []
for i in range(12):
    base = 6 + i * 37
    pts = []
    for x in range(-12, W + 13, 6):
        y = (
            base
            + 10 * math.sin(x / 103 + i * 0.73)
            + 5 * math.sin(x / 47 + i * 1.91)
            + 2.5 * math.sin(x / 23 + i * 2.7)
        )
        pts.append(f"{x} {y:.1f}")
    contours.append(f'<path d="M{" L".join(pts)}"/>')

cx, cy = APPS
svg = f"""<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}">
  <!-- Rudder.dmg window art. Generated; icon centers match mac/dmg/settings.py. -->
  <defs>
    <style>@import url("https://fonts.googleapis.com/css2?family=Archivo:wght@500&amp;display=block");</style>
    <radialGradient id="paper" cx="50%" cy="42%" r="75%">
      <stop offset="0" stop-color="#fbfaf6"/>
      <stop offset="1" stop-color="{PAPER}"/>
    </radialGradient>
    <radialGradient id="vignette" cx="50%" cy="48%" r="72%">
      <stop offset="0.6" stop-color="{INK}" stop-opacity="0"/>
      <stop offset="1" stop-color="{INK}" stop-opacity="0.06"/>
    </radialGradient>
    <radialGradient id="fade" gradientUnits="userSpaceOnUse" cx="330" cy="200" r="330"
                    gradientTransform="translate(330 200) scale(1 0.66) translate(-330 -200)">
      <stop offset="0.45" stop-color="#000"/>
      <stop offset="0.95" stop-color="#fff"/>
    </radialGradient>
    <mask id="edges"><rect width="{W}" height="{H}" fill="url(#fade)"/></mask>
    <filter id="soft" x="-30%" y="-60%" width="160%" height="220%"><feGaussianBlur stdDeviation="5"/></filter>
    <mask id="label-gap">
      <rect width="{W}" height="{H}" fill="#fff"/>
      <rect x="{cx - 64}" y="{cy + 62}" width="128" height="30" rx="12" fill="#000" filter="url(#soft)"/>
    </mask>
    <radialGradient id="glow" gradientUnits="userSpaceOnUse" cx="{cx}" cy="{cy}" r="138">
      <stop offset="0" stop-color="{ACCENT}" stop-opacity="0.13"/>
      <stop offset="0.55" stop-color="{ACCENT}" stop-opacity="0.05"/>
      <stop offset="1" stop-color="{ACCENT}" stop-opacity="0"/>
    </radialGradient>
  </defs>

  <rect width="{W}" height="{H}" fill="url(#paper)"/>
  <g mask="url(#edges)" fill="none" stroke="{INK}" stroke-opacity="0.075" stroke-width="1">
    {"".join(contours)}
  </g>
  <rect width="{W}" height="{H}" fill="url(#vignette)"/>

  <circle cx="{cx}" cy="{cy}" r="138" fill="url(#glow)"/>
  <g fill="none" stroke="{ACCENT}" mask="url(#label-gap)">
    <circle cx="{cx}" cy="{cy}" r="84" stroke-opacity="0.28" stroke-width="1.4"/>
    <circle cx="{cx}" cy="{cy}" r="102" stroke-opacity="0.15" stroke-width="1.2"/>
    <circle cx="{cx}" cy="{cy}" r="121" stroke-opacity="0.07" stroke-width="1"/>
  </g>

  {"".join(dots)}
  {arrow}

  <text x="{W / 2}" y="356" text-anchor="middle" fill="{INK2}"
        font-family="Archivo, -apple-system, Helvetica, sans-serif" font-size="15" font-weight="500"
        letter-spacing="0.1">Drag Rudder into Applications</text>
</svg>
"""
open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "background.svg"), "w").write(svg)
