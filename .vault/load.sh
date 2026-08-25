# Source this file: exports the publication tokens found in .vault/ as the
# environment variables cargo, npm and maturin read. Prints only which
# ones were loaded. Usage:  source .vault/load.sh
_vault="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
_loaded=""; _missing=""
if [ -s "$_vault/crates-io.token" ]; then
    export CARGO_REGISTRY_TOKEN="$(tr -d '\n' < "$_vault/crates-io.token")"; _loaded="$_loaded crates.io"
else _missing="$_missing crates.io"; fi
if [ -s "$_vault/pypi.token" ]; then
    export MATURIN_PYPI_TOKEN="$(tr -d '\n' < "$_vault/pypi.token")"; _loaded="$_loaded pypi"
else _missing="$_missing pypi"; fi
if [ -s "$_vault/npm.token" ]; then
    # npm reads its token from an npmrc: generate one here (ignored) and
    # point npm at it for this shell only.
    umask 077
    printf '//registry.npmjs.org/:_authToken=%s\n' "$(tr -d '\n' < "$_vault/npm.token")" > "$_vault/npmrc"
    export NPM_CONFIG_USERCONFIG="$_vault/npmrc"; _loaded="$_loaded npm"
else _missing="$_missing npm"; fi
echo "vault: loaded[$_loaded ] missing[$_missing ]"
unset _vault _loaded _missing
