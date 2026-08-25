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

## Publish, in this order, stopping at the first error

```bash
git push origin main <release-branch>
cargo publish -p luciole
cargo publish -p lucistore          # cargo waits for the index before returning
cargo publish -p ld-lucivy
cargo publish -p lucivy-core
cargo publish -p sparse-vector
(cd bindings/python && .venv/bin/maturin upload --skip-existing dist/*)
(cd bindings/nodejs && npm publish --otp=<code>)
(cd bindings/emscripten && npm publish --otp=<code>)
git tag -a vX.Y.Z -m "..." && git push origin vX.Y.Z
```

Verify each on its registry (`cargo search`, the PyPI JSON API, `npm view`)
before the next; the CDN can lag a minute. Then record the versions in
`CLAUDE.md` and the day's recap.

## After

Prebuilt binaries are Linux x86_64 only (the wheel, the `.node`, the `.wasm`);
other platforms build from source. A CI matrix (`maturin-action`, napi's
per-platform packages) is the next step, not a blocker.
