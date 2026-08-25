# Fin de session du 25 août — état exact, CI rouge, comment publier

Écrit en urgence à la fin de la session (contexte épuisé). Autonome.

## 1. Publié — tout est en 3.0.1

| registre | paquet | version | vérifié |
|---|---|---|---|
| crates.io | `luciole`, `lucistore`, `ld-lucivy`, `lucivy-core`, `sparse-vector` | 3.0.1 | oui |
| PyPI | `lucivy` (wheel `cp39-abi3-manylinux_2_28_x86_64` + sdist) | 3.0.1 | oui |
| npm | `lucivy` (Linux x64) | 3.0.1 | oui |
| npm | `lucivy-wasm` | 3.0.1 | oui |
| GitHub | `main` = `wip/publication-3.0.0` = `3b56695`, tags `v3.0.0`, `v3.0.1` | poussés | oui |

3.0.0 puis 3.0.1 le même soir : les crates 3.0.0 étaient partis **avant** deux
correctifs du cœur (interblocage lazy sans `blob_len`, message de
finalisation perdu) et avec leurs README 2.x. **Règle : publier les crates
en dernier.** `v3-recovery` n'a pas bougé (session rag3weaver).

## 2. Travail non commité au moment de l'arrêt

Un seul fichier modifié : `.github/workflows/ci.yml` — ajout de
`-I bindings/cpp/include` à l'étape « Compile test » du job `lucivy-cpp`
(le header généré inclut désormais `lucivy/blob_backend.h`). **À commiter.**

## 3. La CI est rouge — trois causes, toutes cernées

Dernier run analysé : `gh run view 32893577439`.

1. **`lucivy-cpp`** : `fatal error: lucivy/blob_backend.h: No such file` →
   corrigé par le `-I` ci-dessus (non commité).
2. **`test-all`** (features `zstd-compression,failpoints`) :
   `error[E0063]: missing field sfx_version in initializer of IndexSettings`
   à `src/index/index_meta.rs:499` — le test
   `test_serialize_metas_zstd_compressor` (sous `#[cfg(feature =
   "zstd-compression")]`) construit `IndexSettings { docstore_compression,
   docstore_blocksize, .. }` sans `sfx_version`. Ajouter `sfx_version: 3,`
   (ou `..Default::default()`). `test-default` et `test-minimal` étaient
   *annulés* par ce fail-fast, pas rouges.
3. **`clippy`** (`cargo clippy --lib -- -D warnings`, stable 1.98 en CI et
   en local) — six lints, toutes dans les crates dérivées de tantivy ou
   luciole, aucune dans le moteur :
   - `common/src/bitset.rs:358` — `chunks_exact(8)` à taille constante →
     `as_chunks::<8>()` ;
   - `common/src/file_slice.rs:124` — `write!(f, "…", &self.data, …)` →
     retirer le `&` ;
   - `lucivy-fst/src/raw/common_inputs.rs:260` — un `"…".as_bytes()` (ou
     équivalent) à écrire `b"…"` ;
   - `lucivy-fst/src/raw/ops.rs:224` et `lucivy-fst/src/raw/mod.rs:686` —
     `match … { None => return None, Some(x) => … }` → `?` ;
   - `luciole/src/dag.rs:314` — `nodes_mut` jamais utilisé → supprimer.
   Boucle : `cargo clippy --lib -- -D warnings > /tmp/clippy.txt 2>&1` puis
   `grep -E "^error: |^\s+--> " /tmp/clippy.txt`, jusqu'à zéro.

Ces trois correctifs ne touchent pas le comportement : pas de 3.0.2 requis
pour la CI seule.

## 4. Comment publier (procédure validée ce soir)

Détail dans `RELEASE.md` (réécrit ce soir). Résumé :

1. Bumper **tout** au même numéro : `Cargo.toml` racine, `lucivy_core`,
   `luciole`, `lucistore`, `sparse_vector`, `bindings/*/Cargo.toml`,
   `bindings/nodejs/package.json`, `bindings/emscripten/package.json`,
   `bindings/python/pyproject.toml`, **et** les `version = "…"` des
   dépendances par chemin (racine → luciole ; `lucivy_core` → ld-lucivy,
   luciole, lucistore ; `sparse_vector` → lucistore, luciole). Titres des
   README, entrée CHANGELOG, tableau de CLAUDE.md.
2. Vert : `cargo test --lib`, `cargo test -p lucivy-core --no-fail-fast`,
   `cargo test -p lucivy-cpp`, Python (`cd bindings/python && bash build.sh
   && .venv/bin/python -m pytest -q tests`), Node (`cd bindings/nodejs && npm
   run build && node test.mjs && node tests/v3_api.mjs && node
   tests/blob_store.mjs && node tests/smoke_warnings.mjs "$(pwd)/index.js"`),
   `bash bindings/emscripten/build.sh` + une indexation navigateur.
3. Artefacts : `bash bindings/python/build-wheel.sh` (docker, image
   `ghcr.io/pyo3/maturin`, sort le wheel abi3 manylinux_2_28 + sdist dans
   `bindings/python/dist/`) ; le `.node` est produit par `npm run build`
   et **suivi par git** ; le wasm par `build.sh` (suivi aussi).
4. Commit, `git checkout main && git merge --ff-only <branche>`, tag
   annoté `vX.Y.Z`, `git push origin main <branche> vX.Y.Z`.
5. Identifiants : `.vault/` (ignoré par git, `chmod 700`). `cargo login`
   et `npm login` ont été faits sur cette machine (`~/.cargo/credentials.toml`,
   `~/.npmrc`). Le token PyPI est `PYPI_TOKEN=` dans `.vault/.env` ;
   `source .vault/load.sh` l'exporte en `MATURIN_PYPI_TOKEN`.
6. Publier, **dans cet ordre, arrêt à la première erreur** :
   ```bash
   for c in luciole lucistore ld-lucivy lucivy-core sparse-vector; do cargo publish -p $c || break; done
   cd bindings/python && bash -c 'source ../../.vault/load.sh; .venv/bin/maturin upload --skip-existing dist/*'
   cd bindings/nodejs   && npm publish --otp=<code>      # OTP demandé à Lucie, expire vite
   cd bindings/emscripten && npm publish --otp=<code>   # second OTP
   ```
   cargo attend lui-même l'indexation entre deux crates. Vérifier ensuite :
   `curl -s https://crates.io/api/v1/crates/<c> -A x | jq .crate.max_version`,
   `curl -s https://pypi.org/pypi/lucivy/json | jq .info.version` (CDN : une
   minute de retard possible), `npm view lucivy version`,
   `npm view lucivy-wasm version`.
7. Reporter les versions dans `CLAUDE.md` (tableau « Packages publiés »)
   et le récap du jour.

Pièges vus ce soir : les backticks dans un message de commit cassent le
shell (`git commit -F fichier`) ; le Python système est 3.14 > pyo3 0.24 →
le venv du binding est un 3.12 géré par `uv` (`uv python install 3.12`) ;
`npm view` met une minute à refléter une publication ; Pages se déploie
seul sur push de `main` touchant `playground/**` (workflow allégé ce soir :
plus de build, le playground est statique).

## 5. Où en est le produit (pour la suite)

Tout est dans `05-recap-progression-et-a-faire.md` (§1 chiffres, §5 à
faire) et `09-ce-qui-a-debloque.md`. En une ligne : navigateur 10 000
fichiers kernel indexés en 55 s, 124-133 ms/requête (natif 79),
blob store ACID exposé dans les trois bindings natifs, Jaro-Winkler,
playground qui clone lucivy depuis GitHub. Prochain chantier technique :
le coût fixe par requête sur les champs courts (05 §5.1), puis SIMD /
`-O3` à remesurer avec mimalloc.
