# Fin de session du 25 août — état exact, CI rouge, comment publier

Écrit en urgence à la fin de la session (contexte épuisé). Autonome.

## 1. Publié — tout est en 3.0.2 depuis la nuit du 25 au 26 (3.0.1 avant)

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

### Suite (nuit du 25 au 26) — corrigé, mesuré, 3.0.2 préparée

Décision : republier en **3.0.2** une fois la CI GitHub verte (pas avant,
pour ne pas devoir faire une .3). Fait dans cet ordre :

- `index_meta.rs` : `..Default::default()` dans le test zstd, JSON attendu
  avec `"sfx_version":3`.
- clippy : les 6 lints des crates dérivées, puis — la CI n'était jamais
  arrivée jusqu'au moteur — **194 erreurs dans `ld-lucivy`** : 164
  `missing_docs` (écrites en quatre lots parallèles, tous les items publics
  du v3), ~30 vraies lints (code mort `TokenCaptureV3`, `doc_values`
  write-only, accesseurs SFP3 inutilisés ; `Entry::Vacant` au lieu de
  `contains_key`+`insert` dans le memo du walk, `strip_prefix`, `to_vec`,
  `is_none_or`, `div_ceil`…). Aucune ne coûte : vérifié au bench.
- `clippy.toml` avec `msrv = "1.85"` : évite que clippy pousse vers des API
  std trop récentes (`as_chunks` est 1.88) — et a révélé un
  `is_multiple_of` (1.87) déjà présent dans `lucivy-fst`, remis en `% 2`.
- luciole : `test_set_pipe_sender_dies` et `test_guard_auto_unregister`
  comparaient le **compte global** du wait-graph et flakaient sous le runner
  parallèle (2 échecs sur 6 runs) ; ils vérifient leur propre edge via
  `wait_graph::contains(id)` (nouveau). 8/8 verts ensuite.
- Playground : `importFiles` (drop et `?corpus=`) dupliquait la boucle
  d'indexation sans le rechargement `?open=` au-delà de 2 Go ; la page qui
  avait indexé servait, et une requête tapée pendant le panel a tué le worker
  (`memory access out of bounds`, 3ᵉ passe). Passe maintenant par
  `indexFiles` comme le clone git. Revérifié : rechargement, 3 passes + 11
  requêtes concurrentes, 0 erreur.

Mesures après les fixs, même protocole que §8 du knowledge dump :
**natif** 10k compacté 79 → 79 ms/requête (1664 → 1667 ms), 21/21 comptes
identiques, indexation 19,5 s, 2307 Mo ; **navigateur** indexation 40 s,
préchargement 4,8 s, panel **117 / 115 ms** (médiane 65 / 63) contre
124-133 hier, 21/21 comptes identiques. Fichiers :
`/tmp/parity_native_10k_302.json`, `/tmp/parity_wasm_302c_{1,2}.json`.

**Et un vrai bug trouvé par Lucie en tapant « t » puis « te », « tes »,
« test »** dans le playground sur le corpus 10k : le worker mourait
(`memory allocation of 25165824 bytes failed`, puis `unwind` sur toute
requête suivante). Trois structures explosaient sur une lettre (des dizaines
de millions d'occurrences) : le `HighlightSink` (un `String` par span !),
les `Vec<MatchV3>` des resolvers (40 o/occurrence, × 48 segments), et par
ricochet le cache de fichiers (une relecture qui échoue sous pression donne
une tranche courte → `data[addr]` panique dans le FST). Corrigé en trois
temps, chacun mesuré dans le navigateur :

1. sink compact (champ interné, offsets `u32` : les postings les portent
   déjà en `u32`) + plafond `LUCIVY_HIGHLIGHT_SPAN_CAP` (4 M / 1 M wasm) ;
   au débordement `ShardedHandle::search_internal` relance la recherche
   filtrée aux ids du top-k dans le sink vidé. Pour que ça serve, le
   `SfxWeight` n'émet plus dans le sink les docs que le bitset alive du
   lecteur exclut (`emit_highlights`) — avant, une recherche filtrée
   enregistrait tout le segment. Test `test_highlight_cap.rs`.
2. plafond `LUCIVY_MAX_MATCHES_PER_SEGMENT` dans les 11 sites de `push` de
   `resolve.rs` (script), 250 k d'abord — encore trop avec 48 segments
   (480 Mo) → 50 k puis **20 k** en wasm (après le panel, un doublement de Vec à 4 Mo échouait encore), 4 M natif. Compteur `truncations()`, trace en
   verbeux.
3. résultat : « t » 857 ms, « te » 531, « tes » 86, « test » 49,
   « kmalloc » 25, 0 erreur ; panel 21/21 identique, 114 ms, **aucune
   troncature** sur les requêtes normales.

Deux autres trouvés en relançant les suites après ces changements :
`test_export_uncommitted_raises` (Python) flakait 1/3 — le drapeau
« uncommitted » n'était posé que par l'acteur du shard, après coup ;
`ShardedHandle::add_document`/`delete_by_node_id` marquent maintenant tous
les shards à l'entrée. Et `jaro_winkler_fuzzy_end_to_end` flakait 2/3 : le
palier fuzzy (`coverage`) calculé par `fuzzy_v3` était **jeté** par
`FuzzyQueryV3` et `SfxWeight` n'appelait jamais `with_coverage` — en v3 ni
le palier Levenshtein ni le JW n'entraient dans le score, l'ordre des ex
æquo dépendait du découpage en segments. Raccordé (`CachedPrescan.coverage`).
Les comptes ne changent pas, l'ordre des requêtes fuzzy si : référence
native du panel à régénérer (`/tmp/parity_native_10k_302c.json`).

Suites notées : signaler la troncature dans la réponse (tableau nu
aujourd'hui) ; câbler `filter_docs` jusqu'aux resolvers v3 (toujours `None`
dans `contains/fuzzy/regex_query_v3`) pour que la recherche filtrée ne
*vérifie* que les docs autorisés.

Local avant push : `cargo clippy --lib -- -D warnings` vert, `cargo test
--lib` 1435 / all 1440 / minimal 1401 verts, luciole 169, lucivy-core vert
sauf `bench_sharding` t01/t04 (connus), job C++ reproduit (build, g++,
`All tests passed!`) dans `target/ci-cpp` — `target/release` contient 368
fichiers **root** laissés par le build docker du wheel :
`sudo chown -R lucied:lucied target/release`.

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
