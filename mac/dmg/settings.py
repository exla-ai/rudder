# dmgbuild settings for Rudder.dmg; mac/scripts/make-dmg.sh passes
# -D app=<Rudder.app> -D dmgdir=<this folder>.
#
# The Finder window: Rudder on the left, Applications on the right, and the
# art in background.png (+ @2x, which dmgbuild joins into one Retina TIFF).
# Icon centers here must match APP and APPS in background.py.
import os.path

app = defines["app"]
dmgdir = defines["dmgdir"]

volume_icon = os.path.join(app, "Contents", "Resources", "AppIcon.icns")
if not os.path.isfile(volume_icon):
    raise SystemExit(f"{volume_icon} is missing: the app was not built from the AppIcon asset catalog")

format = "ULFO"
filesystem = "HFS+"
files = [app]
symlinks = {"Applications": "/Applications"}
hide_extensions = ["Rudder.app"]
icon = volume_icon

background = os.path.join(dmgdir, "background.png")
window_rect = ((200, 140), (660, 420))
default_view = "icon-view"
show_status_bar = False
show_tab_view = False
show_toolbar = False
show_pathbar = False
show_sidebar = False
show_icon_preview = False
icon_size = 128
text_size = 13
label_pos = "bottom"
icon_locations = {
    "Rudder.app": (170, 196),
    "Applications": (490, 196),
}
