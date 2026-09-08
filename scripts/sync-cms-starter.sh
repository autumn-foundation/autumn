#!/usr/bin/env bash
# Regenerate `autumn-cli/src/starters/cms` from `examples/cms`.
#
# The two trees are pinned byte-for-byte by `embedded_cms_matches_example_cms`
# in `autumn-cli/src/starters/mod.rs`, modulo two files the starter does not
# ship as-is:
#
#   Cargo.toml            -- the starter ships `Cargo.toml.tmpl` instead, with a
#                            versioned `autumn-web` dependency rather than the
#                            in-workspace path one the example needs.
#   tests/system/smoke.rs -- workspace-internal e2e tooling (issue #1192); it
#                            depends on the path-only `example-e2e` crate and
#                            has no meaning outside this monorepo.
#
# `static/css/app.css` is a Tailwind build artefact and is skipped too.
#
# Workflow: edit `examples/cms`, run `cargo fmt`, then run this. The drift test
# fails loudly if you forget.
#
# Written in bash rather than Python on purpose: `.gitignore` ignores `*.py`,
# so a Python helper here would never be committed and this script would be
# broken for everyone but its author.
set -euo pipefail

cd "$(dirname "$0")/.."

name="cms"
src="examples/${name}"
dst="autumn-cli/src/starters/${name}"

[[ -d "$src" ]] || { echo "no such example: $src" >&2; exit 1; }

# Files the starter owns rather than copies from the example.
preserved_manifest="$(cat "${dst}/autumn-starter.toml")"
preserved_cargo="$(cat "${dst}/Cargo.toml.tmpl")"

rm -rf "$dst"
mkdir -p "$dst"
printf '%s' "$preserved_manifest" > "${dst}/autumn-starter.toml"
printf '%s' "$preserved_cargo" > "${dst}/Cargo.toml.tmpl"

copied=0
while IFS= read -r path; do
  rel="${path#"${src}/"}"
  case "$rel" in
    Cargo.toml|tests/system/smoke.rs|static/css/app.css) continue ;;
    target/*|.git/*) continue ;;
  esac

  mkdir -p "$(dirname "${dst}/${rel}")"
  cp "$path" "${dst}/${rel}"

  # Replace the occurrences of the project name that are genuinely identifiers
  # or configuration -- never prose. Rendering the starter with project name
  # `cms` must reproduce the example byte for byte, which the drift test checks.
  case "$rel" in
    autumn.toml)
      sed -i.bak "s|localhost:5432/${name}|localhost:5432/{{project_name}}|g" "${dst}/${rel}"
      ;;
    docker-compose.yml)
      sed -i.bak "s|POSTGRES_DB: ${name}|POSTGRES_DB: {{project_name}}|g" "${dst}/${rel}"
      ;;
    src/main.rs|tests/integration_test.rs)
      sed -i.bak -e "s|\\b${name}::|{{crate_name}}::|g" \
                 -e "s|-p ${name}|-p {{project_name}}|g" "${dst}/${rel}"
      ;;
    README.md)
      sed -i.bak -e "1s|^# ${name} — |# {{project_name}} — |" \
                 -e "s|-p ${name}|-p {{project_name}}|g" "${dst}/${rel}"
      ;;
  esac
  rm -f "${dst}/${rel}.bak"
  copied=$((copied + 1))
done < <(find "$src" -type f -not -path "*/target/*" -not -path "*/.git/*" | sort)

echo "synced ${copied} files: ${src} -> ${dst}"
echo "now run: cargo test -p autumn-cli --bin autumn embedded_cms_matches_example_cms"
