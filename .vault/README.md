# .vault — publication credentials, local only

Everything in this directory is git-ignored except this file and `load.sh`.
The directory is `chmod 700`; keep the token files `chmod 600`.

One token per file, the bare token, no newline needed:

| file | where to get it | used by |
|---|---|---|
| `crates-io.token` | https://crates.io/settings/tokens — scopes publish-new + publish-update | `cargo publish` (`CARGO_REGISTRY_TOKEN`) |
| `npm.token` | https://www.npmjs.com/settings/~/tokens — granular, publish on `lucivy` | `npm publish` (generated `.vault/npmrc`) |
| `pypi.token` | https://pypi.org/manage/account/token/ — scope project `lucivy` | `maturin upload` (`MATURIN_PYPI_TOKEN`) |

Write them without echoing to the terminal history, e.g.:

```bash
read -rs t && printf '%s' "$t" > .vault/pypi.token && chmod 600 .vault/pypi.token; unset t
```

Then, for one command or one shell:

```bash
source .vault/load.sh          # exports what exists, says what is missing
cargo publish -p luciole
```

`load.sh` never prints a token. Nothing here is ever read by the engine,
the bindings or the playground.
