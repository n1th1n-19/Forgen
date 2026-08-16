#!/bin/sh
# Skipped under Flatpak and distro packaging — both run these themselves, and
# running them twice against a staged DESTDIR just fails noisily.
if [ -n "$DESTDIR" ]; then
    exit 0
fi

datadir="${MESON_INSTALL_PREFIX:-/usr/local}/share"

echo "Compiling GSettings schemas..."
glib-compile-schemas "$datadir/glib-2.0/schemas" || true

echo "Updating desktop database..."
update-desktop-database -q "$datadir/applications" || true

echo "Updating icon cache..."
gtk4-update-icon-cache -qtf "$datadir/icons/hicolor" || true
