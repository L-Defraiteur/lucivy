# Releasing lucivy

One version number for the whole workspace — `ld-lucivy`, `lucivy-core`,
`luciole`, `lucistore`, `sparse-vector`, and the Python, Node.js, C++ and
WASM bindings all carry the same `X.Y.Z`. Nothing is published without an
explicit go from the maintainer.

## Before

1. `main` is the last published state; the changelog is written against it
   (`git diff <last-tag>..HEAD`).
2. Bump every `version` (root `Cargo.toml`, the four crates, `bindings/*/Cargo.toml`,
   `bindings/nodejs/package.json`, `bindings/emscripten/package.json`,
   `bindings/python/pyproject.toml`) and the path dependencies' `version = "..."`
   (`lucivy_core/Cargo.toml`, `sparse_vector/Cargo.toml`, root `Cargo.toml`).
3. Green: `cargo test --lib`, `cargo test -p lucivy-core --no-fail-fast`,
   `cargo test -p lucivy-cpp`, `bindings/python`: `bash build.sh` then
   `.venv/bin/python -m pytest tests`, `bindings/nodejs`: `npm run build`,
   `node test.mjs`, `node tests/*.mjs`, `bash bindings/emscripten/build.sh` and a
   browser indexing run (`playground/`, see `docs/25-08-2026/07-knowledge-dump-outils.md`).
4. Dry-run what can be: `cargo publish --dry-run -p luciole -p lucistore`
   (the dependents can only be verified once those are on the registry),
   `npm pack --dry-run` in `bindings/nodejs` and `bindings/emscripten`.
5. Build the Python artefacts in the official container — one `abi3` wheel for
   every CPython ≥ 3.9, `manylinux_2_28`, plus the sdist:
   `bash bindings/python/build-wheel.sh` → `bindings/python/dist/`. Install the
   wheel in a fresh venv and run a smoke test.
6. Fast-forward `main` to the release branch; the working tree is clean.

## Credentials

`.vault/` (git-ignored; `chmod 700`) holds the tokens — `crates-io.token`,
`npm.token`, `pypi.token` or a `.env` with `PYPI_TOKEN=` — and
`source .vault/load.sh` exports them for one shell (`CARGO_REGISTRY_TOKEN`,
`MATURIN_PYPI_TOKEN`, a generated npmrc). `cargo login` / `npm login` work too.
npm asks for a one-time code at publish time: `npm publish --otp=<code>`.
The npm session expires within a day: run `npm whoami` first — an expired
session makes `npm publish` fail with a misleading **404** "not in this
registry" (seen on 26 August), and `npm login` is the fix, not a new code.

## Publish, in this order, stopping at the first error

The Python wheels and the Node.js addons for the five platforms (Linux
x86_64 and aarch64 on glibc ≥ 2.28, macOS x86_64 and arm64, Windows x86_64)
are built by `.github/workflows/release.yml` on the tag — see below. The
crates and the WASM package are published by hand, the crates **last**.

```bash
git push origin main <release-branch>
git tag -a vX.Y.Z -m "..." && git push origin vX.Y.Z   # builds the wheels and addons, attaches them to the release
# on the run page: approve the `release` environment → PyPI and npm are published
(cd bindings/emscripten && npm publish --otp=<code>)
cargo publish -p luciole
cargo publish -p lucistore          # cargo waits for the index before returning
cargo publish -p ld-lucivy
cargo publish -p lucivy-core
cargo publish -p sparse-vector
```

Verify each on its registry (`cargo search`, the PyPI JSON API, `npm view`)
before the next; the CDN can lag a minute. Then record the versions in
`CLAUDE.md` and the day's recap.

## The release workflow

`release.yml` runs on a `v*` tag (or by hand, `workflow_dispatch`, with
`publish` unticked to only build). Jobs:

- `python` × 5 — `maturin-action` builds the `abi3` wheel (manylinux_2_28
  on Linux, in the official image; Intel macOS cross-compiled from the arm64
  runner, GitHub having retired `macos-13`), installs it and runs a
  create/add/search smoke test on the runner that built it (not the
  cross-compiled one); `sdist` on top.
- `node` × 5 — `cargo build --release --target` of `bindings/nodejs`, the
  Linux ones inside `quay.io/pypa/manylinux_2_28_*` so the addon loads on any
  glibc since 2018; the addon becomes `lucivy.<platform>.node`, smoke-tested
  through `index.js` where Node is available.
- `release` — attaches wheels, sdist and addons to the GitHub release of the tag.
- `publish-pypi`, `publish-npm` — behind **two locks**: the `release`
  environment (Settings → Environments → `release` → required reviewer:
  the maintainer; the run waits for the Approve button) and the repository
  variable `PUBLISH_ENABLED=true` (Settings → Variables). PyPI goes through
  trusted publishing (pypi.org → the project → Publishing → GitHub:
  `L-Defraiteur/lucivy`, `release.yml`, environment `release`); npm through
  the `NPM_TOKEN` secret (a granular token with "bypass 2FA", needed the
  first time a platform package is published) — with no secret it tries
  trusted publishing with provenance, which npm allows only for packages
  that already exist.

npm packaging: `lucivy` no longer carries a binary; it lists
`lucivy-linux-x64-gnu`, `lucivy-linux-arm64-gnu`, `lucivy-darwin-x64`,
`lucivy-darwin-arm64`, `lucivy-windows-x64` as `optionalDependencies`
(templates in `bindings/nodejs/npm/`, one addon each, `os`/`cpu` filters so
npm installs one) and `index.js` loads the one present — or a local
`lucivy.node` from `npm run build`. The platform packages take the version
of `package.json` at publish time; bumping that one file is enough.

Elsewhere (Alpine/musl, FreeBSD, 32-bit) `npm install` still succeeds and
`require('lucivy')` says which platforms are prebuilt and how to build.
