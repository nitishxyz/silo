#!/usr/bin/env python3
"""Silo mark + icons from the provided 80-unit SVG, with an inner light
leaking through the slit and the slat gaps ("always running")."""
import os
import sys

OUT = sys.argv[1] if len(sys.argv) > 1 else "out"

SLAB = "m44.25 15.53c-0.17-0.04-0.26 0.16-0.26 0.43l-0.13 50.98c0 0.2 0.1 0.31 0.27 0.33 0.48 0.05 1.05 0.01 1.52-0.12l12.59-3.82c0.64-0.18 1.07-0.77 1.07-1.43v-31.1c0-9.49-8.05-13.28-15.06-15.27z"
SLATS = [
    "m40.98 16.06c0-0.36-0.26-0.65-0.62-0.65-0.51-0.02-1.01 0.11-1.46 0.36l-10.51 5.96c-3.43 1.97-5.52 5.64-5.56 9.56-0.02 1.03 0.91 1.63 1.84 1.36l14.52-4.59c1.06-0.32 1.73-1.26 1.73-2.35l0.06-9.65z",
    "m40.9 32.29c0.01-1.11-1.02-1.77-2.05-1.49l-14.42 4.17c-1.06 0.3-1.73 1.25-1.73 2.35l-0.03 5.62c-0.01 0.87 0.79 1.46 1.62 1.3l14.78-2.86c1.08-0.2 1.8-1.11 1.8-2.21l0.03-6.88z",
    "m40.87 45.47c0-1.07-0.96-1.75-1.98-1.59l-14.67 2.66c-0.98 0.16-1.58 1-1.58 2v5.7c0 0.82 0.72 1.39 1.51 1.32l14.73-1.3c1.14-0.11 1.98-1.06 1.98-2.24l0.01-6.55z",
    "m40.86 58.44c0-1.11-0.95-1.92-2.02-1.86l-14.93 0.75c-0.76 0.03-1.27 0.57-1.27 1.31v2.49c0 1.06 0.73 1.92 1.73 2.22 4.93 1.59 11.14 3.12 15.24 3.9 0.35 0.06 0.72 0.06 0.98 0.04 0.15-0.02 0.26-0.15 0.26-0.35l0.01-8.5z",
]
# Mark bounds in its own units: x 22.6..59.4, y 15.4..67.3.
CX, CY, MH = 41.0, 41.35, 51.9


def glow(uid, color, strength=1.0):
    """Light behind the mark, seen only through the slit and the slat gaps:
    brightest along the slit, fading left along the gaps and towards the
    slit's open ends."""
    return f'''<linearGradient id="{uid}-gx" x1="44" y1="0" x2="23" y2="0" gradientUnits="userSpaceOnUse">
  <stop offset="0" stop-color="{color}" stop-opacity="{strength}"/>
  <stop offset="0.25" stop-color="{color}" stop-opacity="{0.85 * strength:.3f}"/>
  <stop offset="1" stop-color="{color}" stop-opacity="0"/>
</linearGradient>
<linearGradient id="{uid}-gy" x1="0" y1="15" x2="0" y2="68" gradientUnits="userSpaceOnUse">
  <stop offset="0" stop-color="#fff" stop-opacity="0"/>
  <stop offset="0.12" stop-color="#fff" stop-opacity="0.55"/>
  <stop offset="0.75" stop-color="#fff" stop-opacity="1"/>
  <stop offset="0.97" stop-color="#fff" stop-opacity="0.35"/>
  <stop offset="1" stop-color="#fff" stop-opacity="0"/>
</linearGradient>
<clipPath id="{uid}-hull"><path d="M40.4,15.3 L44.3,15.3 L44.3,67.4 L41.2,67.4 C36,66.4 29,64.6 24.6,63.3 C23.4,62.9 22.9,61.6 22.9,60.6 L22.9,32.2 C23.4,28.2 25.6,24.1 29,22 Z"/></clipPath>
<mask id="{uid}-gm" maskUnits="userSpaceOnUse" x="0" y="0" width="80" height="80">
  <rect x="24" y="15" width="20.2" height="53" fill="url(#{uid}-gy)"/>
</mask>''', f'<g clip-path="url(#{uid}-hull)"><rect x="22" y="15" width="22.3" height="53" fill="url(#{uid}-gx)" mask="url(#{uid}-gm)"/></g>'


def shapes(slab_fill, slat_fill):
    fills = slat_fill if isinstance(slat_fill, list) else [slat_fill] * 4
    return "\n".join([f'<path d="{SLAB}" fill="{slab_fill}"/>'] + [f'<path d="{d}" fill="{f}"/>' for d, f in zip(SLATS, fills)])


def svg(view, defs, body, size=None, title="Silo"):
    vx, vy, vw, vh = view
    w = h = size or vw
    return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{vx} {vy} {vw} {vh}" width="{w}" height="{h}">\n'
            f'<title>{title}</title>\n<defs>\n{defs}\n</defs>\n{body}\n</svg>\n')


MARK_BOX = (CX - 30, CY - 30, 60, 60)  # square, mark centred


def mark(ink, light=None, uid="m"):
    defs, body = ("", "")
    if light:
        defs, body = glow(uid, light)
    return svg(MARK_BOX, defs, body + "\n" + shapes(ink, ink))


def gradient_mark(uid="g"):
    """The colour gradient mark (concept: cool blue to violet)."""
    defs = f'''<linearGradient id="{uid}-slats" x1="0" y1="15" x2="0" y2="68" gradientUnits="userSpaceOnUse">
  <stop offset="0" stop-color="#8FD8FF"/><stop offset="0.45" stop-color="#8B7BFF"/><stop offset="1" stop-color="#3D63FF"/>
</linearGradient>
<linearGradient id="{uid}-slab" x1="44" y1="15" x2="58" y2="68" gradientUnits="userSpaceOnUse">
  <stop offset="0" stop-color="#F4F0FF"/><stop offset="0.5" stop-color="#A996FF"/><stop offset="1" stop-color="#4A5BF0"/>
</linearGradient>'''
    gd, gb = glow(uid, "#7FA2FF")
    return svg(MARK_BOX, defs + "\n" + gd, gb + "\n" + shapes(f"url(#{uid}-slab)", f"url(#{uid}-slats)"))


SQUIRCLE = "M400 100H624C876 100 924 148 924 400V624C924 876 876 924 624 924H400C148 924 100 876 100 624V400C100 148 148 100 400 100Z"


def app_icon(dark):
    uid = "d" if dark else "l"
    scale = 470 / MH
    tx, ty = 512 - CX * scale, 500 - CY * scale
    if dark:
        plate = ("#222A38", "#0A0D13")
        slats = f"url(#{uid}-slats)"
        slab = f"url(#{uid}-slab)"
        light = "#6F8DFF"
        mark_defs = f'''<linearGradient id="{uid}-slats" x1="0" y1="15" x2="0" y2="68" gradientUnits="userSpaceOnUse">
  <stop offset="0" stop-color="#FFFFFF"/><stop offset="0.55" stop-color="#C9D3FF"/><stop offset="1" stop-color="#4C6BFF"/>
</linearGradient>
<linearGradient id="{uid}-slab" x1="0" y1="15" x2="0" y2="68" gradientUnits="userSpaceOnUse">
  <stop offset="0" stop-color="#FFFFFF"/><stop offset="0.6" stop-color="#E3E8FA"/><stop offset="1" stop-color="#5D6FE8"/>
</linearGradient>'''
        pool = ('<radialGradient id="d-pool">'
                '<stop offset="0" stop-color="#4C6BFF" stop-opacity="0.55"/><stop offset="1" stop-color="#4C6BFF" stop-opacity="0"/></radialGradient>')
        rim = 0.10
    else:
        plate = ("#FFFFFF", "#ECEFF5")
        slats = slab = "#111D2D"
        light = "#4F7CFF"
        mark_defs = ""
        pool = ('<radialGradient id="l-pool">'
                '<stop offset="0" stop-color="#111D2D" stop-opacity="0.16"/><stop offset="1" stop-color="#111D2D" stop-opacity="0"/></radialGradient>')
        rim = 0.0
    gd, gb = glow(uid, light, 1.0)
    defs = "\n".join([
        f'<linearGradient id="{uid}-plate" x1="0" y1="100" x2="0" y2="924" gradientUnits="userSpaceOnUse">'
        f'<stop offset="0" stop-color="{plate[0]}"/><stop offset="1" stop-color="{plate[1]}"/></linearGradient>',
        pool, mark_defs, gd])
    body = "\n".join([
        f'<path d="{SQUIRCLE}" fill="url(#{uid}-plate)"/>',
        f'<path d="{SQUIRCLE}" fill="none" stroke="#FFFFFF" stroke-opacity="{rim}" stroke-width="3"/>' if rim else "",
        f'<ellipse cx="512" cy="790" rx="300" ry="90" fill="url(#{uid}-pool)"/>',
        f'<g transform="translate({tx:.2f} {ty:.2f}) scale({scale:.4f})">',
        gb,
        shapes(slab, slats),
        "</g>",
    ])
    return svg((0, 0, 1024, 1024), defs, body)


def icon_layers():
    """Layers of the Icon Composer icon (Silo.icon, scripts/make-icons.sh):
    the dark plate is the icon's own fill, so these are only the mark (its
    inner light included) and the pool of light under it."""
    uid = "d"
    mark_defs = f'''<linearGradient id="{uid}-slats" x1="0" y1="15" x2="0" y2="68" gradientUnits="userSpaceOnUse">
  <stop offset="0" stop-color="#FFFFFF"/><stop offset="0.55" stop-color="#C9D3FF"/><stop offset="1" stop-color="#4C6BFF"/>
</linearGradient>
<linearGradient id="{uid}-slab" x1="0" y1="15" x2="0" y2="68" gradientUnits="userSpaceOnUse">
  <stop offset="0" stop-color="#FFFFFF"/><stop offset="0.6" stop-color="#E3E8FA"/><stop offset="1" stop-color="#5D6FE8"/>
</linearGradient>'''
    gd, gb = glow(uid, "#6F8DFF", 1.0)
    mark_svg = svg(MARK_BOX, mark_defs + "\n" + gd, gb + "\n" + shapes(f"url(#{uid}-slab)", f"url(#{uid}-slats)"))
    pool_svg = ('<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 600 180" width="600" height="180">\n'
                '<defs><radialGradient id="p"><stop offset="0" stop-color="#4C6BFF" stop-opacity="0.55"/>'
                '<stop offset="1" stop-color="#4C6BFF" stop-opacity="0"/></radialGradient></defs>\n'
                '<ellipse cx="300" cy="90" rx="300" ry="90" fill="url(#p)"/>\n</svg>\n')
    return mark_svg, pool_svg


os.makedirs(OUT, exist_ok=True)
files = {
    "silo-mark.svg": mark("#111D2D", "#4F7CFF", "m"),
    "silo-mark-dark.svg": mark("#F4F6FA", "#6F8DFF", "md"),
    "silo-mark-mono.svg": mark("#000000"),
    "silo-mark-gradient.svg": gradient_mark(),
    "silo-menubar-template.svg": svg(MARK_BOX, "", shapes("#000", "#000"), size=18, title="Silo (menu bar template)"),
    "silo-app-icon.svg": app_icon(False),
    "silo-app-icon-dark.svg": app_icon(True),
}
layer_mark, layer_pool = icon_layers()
files["icon-layer-mark.svg"] = layer_mark
files["icon-layer-pool.svg"] = layer_pool
for name, text in files.items():
    with open(os.path.join(OUT, name), "w") as f:
        f.write(text)
