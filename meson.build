# Meson wraps cargo rather than replacing it.
#
# Cargo builds the code; meson exists because GNOME's install conventions —
# GSettings schema compilation, AppStream validation, icon cache, desktop file
# validation — are what Flatpak and distro packaging expect, and reimplementing
# them in a build.rs would be strictly more work for the same result.

project(
  'forqen',
  'rust',
  version: '0.1.0',
  license: 'GPL-3.0-or-later',
  meson_version: '>= 1.0.0',
)

app_id = 'io.github.forqen.Forqen'

cargo = find_program('cargo', required: true)
glib_compile_schemas = find_program('glib-compile-schemas', required: false)
desktop_file_validate = find_program('desktop-file-validate', required: false)
appstream_util = find_program('appstream-util', required: false)

prefix = get_option('prefix')
bindir = prefix / get_option('bindir')
datadir = prefix / get_option('datadir')

profile = get_option('profile')
if profile == 'release'
  cargo_profile_args = ['--release']
  cargo_output_dir = 'release'
else
  cargo_profile_args = []
  cargo_output_dir = 'debug'
endif

cargo_env = environment()
cargo_env.set('CARGO_TARGET_DIR', meson.project_build_root() / 'cargo-target')

custom_target(
  'forqen-bin',
  build_by_default: true,
  build_always_stale: true,
  output: 'forqen',
  console: true,
  install: true,
  install_dir: bindir,
  command: [
    'env',
    'CARGO_TARGET_DIR=' + meson.project_build_root() / 'cargo-target',
    cargo,
    'build',
    '--manifest-path', meson.project_source_root() / 'Cargo.toml',
    '--bin', 'forqen',
    cargo_profile_args,
    '&&',
    'cp',
    meson.project_build_root() / 'cargo-target' / cargo_output_dir / 'forqen',
    '@OUTPUT@',
  ],
)

subdir('data')

# Post-install: compile schemas and refresh caches. Skipped by Flatpak and by
# distro packaging, both of which run these themselves.
meson.add_install_script('build-aux/meson-postinstall.sh')
