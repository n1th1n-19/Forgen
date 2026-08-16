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
install_data(
  'icons/hicolor/scalable/apps/@0@.svg'.format(app_id),
  install_dir: datadir / 'icons' / 'hicolor' / 'scalable' / 'apps',
)

install_data(
  'icons/hicolor/symbolic/apps/@0@-symbolic.svg'.format(app_id),
  install_dir: datadir / 'icons' / 'hicolor' / 'symbolic' / 'apps',
)

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
