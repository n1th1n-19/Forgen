conf = configuration_data()
conf.set('APP_ID', app_id)
conf.set('VERSION', meson.project_version())

# --- desktop entry ------------------------------------------------------------
desktop_file = configure_file(
  input: '@0@.desktop.in'.format(app_id),
  output: '@0@.desktop'.format(app_id),
  configuration: conf,
  install: true,
  install_dir: datadir / 'applications',
)

if desktop_file_validate.found()
  test(
    'validate-desktop-file',
    desktop_file_validate,
    args: [desktop_file],
    suite: 'data',
  )
endif

# --- appstream metainfo --------------------------------------------------------
metainfo_file = configure_file(
  input: '@0@.metainfo.xml.in'.format(app_id),
  output: '@0@.metainfo.xml'.format(app_id),
  configuration: conf,
  install: true,
  install_dir: datadir / 'metainfo',
)

if appstream_util.found()
  test(
    'validate-metainfo',
    appstream_util,
    args: ['validate-relax', '--nonet', metainfo_file],
    suite: 'data',
  )
endif

# --- icons ---------------------------------------------------------------------
# Installed under hicolor rather than an app-private directory so the icon is
# found by the shell, the app switcher, and notifications — not just by GTK.
#
# PNG only for the application icon, deliberately. The scalable SVG is the
# source of truth and lives in the repository, but installing it breaks the
# Flatpak export: flatpak validates every exported icon through an image
# loader that cannot read SVG in the build environment, and fails the whole
# export with "is not a valid icon: Format not recognized". 256x256 covers
# HiDPI in practice.
#
# Regenerate after editing the SVG:
#   rsvg-convert -w N -h N icons/hicolor/scalable/apps/<id>.svg \
#       -o icons/hicolor/NxN/apps/<id>.png
foreach size : ['64', '128', '256']
  install_data(
    'icons/hicolor/@0@x@0@/apps/@1@.png'.format(size, app_id),
    install_dir: datadir / 'icons' / 'hicolor' / '@0@x@0@'.format(size) / 'apps',
  )
endforeach

# The symbolic icon is not installed for the same reason as the scalable one:
# flatpak's export-time validation rejects every SVG here with "Format not
# recognized", because the image loader it uses (glycin) needs a nested
# sandbox that will not start inside flatpak-builder. Both SVGs remain in the
# repository as sources; restore these install_data calls on a host where the
# nested sandbox works, or once the runtime ships a loader that does not need
# one.

# --- gsettings schema ----------------------------------------------------------
install_data(
  '@0@.gschema.xml'.format(app_id),
  install_dir: datadir / 'glib-2.0' / 'schemas',
)

# Catches a malformed schema at build time rather than at first launch, where
# it manifests as an abort inside GSettings with no useful message.
if glib_compile_schemas.found()
  test(
    'validate-gschema',
    glib_compile_schemas,
    args: ['--strict', '--dry-run', meson.current_source_dir()],
    suite: 'data',
  )
endif
