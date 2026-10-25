# Le pipeline de publication de lucivy, tel qu'il est (25 octobre 2026)

*Relevé sur `main` = `ae71a0c` (4.3.0, publiée le 14 septembre 2026). Sources : `RELEASE.md`,
`.github/workflows/{ci,build,release}.yml`, `CLAUDE.md` § « Packages publiés »,
`docs/14-09-2026/01-rapport-nuit-4.3.0.md`. Ce document ne change rien au pipeline.*

## En une ligne

Branche de travail → CI verte → PR vers `main` → fusion → **CI verte sur le SHA exact de
`main`** → tag `vX.Y.Z` poussé par Lucie → `release.yml` rejoue la CI, bâtit tout, attache
les artefacts à la release GitHub, publie PyPI, npm et le WASM, puis les crates en dernier.
Aucun jeton ni OTP : tout passe par le trusted publishing (OIDC).

## 1. Ce qui porte un numéro

Un seul `X.Y.Z` pour l'espace de travail, à changer à la main partout (le commit
`8e47953` de la 4.3.0 en est le modèle) :

- `Cargo.toml` racine (`ld-lucivy`), `lucivy_core`, `luciole`, `lucistore`, `sparse_vector`,
  les quatre `bindings/*/Cargo.toml` (neuf `Cargo.toml`) ;
- les dépendances internes `version = "…"` : racine (`luciole`), `lucivy_core`
  (`luciole`, `lucistore`), `sparse_vector` (`luciole`, `lucistore`) ;
- `bindings/nodejs/package.json` et ses cinq gabarits `bindings/nodejs/npm/*`,
  `bindings/emscripten/package.json` (sept `package.json`), `bindings/python/pyproject.toml` ;
- en-têtes des README, bannière et BUILD de la page, `CHANGELOG.md` daté.

## 2. Les quatorze crates publiés sur crates.io

| Crates | Version | Qui les publie |
|---|---|---|
| `ld-ownedbytes`, `ld-lucivy-common`, `ld-lucivy-bitpacker`, `ld-lucivy-sstable`, `ld-lucivy-columnar`, `ld-lucivy-stacker`, `ld-lucivy-query-grammar`, `ld-lucivy-tokenizer-api` (8) | **0.27.0, gelés** | personne : **absents de la boucle de `release.yml`** ; aucun commit dans leurs dossiers depuis 3.0.8 |
| `lucivy-fst` | 0.1.1, sa propre version | le workflow, en premier ; sauté s'il existe déjà |
| `luciole`, `lucistore`, `ld-lucivy`, `lucivy-core`, `sparse-vector` (5) | 4.3.0, le numéro commun | le workflow, dans cet ordre |

**Ordre de `publish-crates`** : `lucivy-fst` → `luciole` → `lucistore` → `ld-lucivy` →
`lucivy-core` → `sparse-vector`. Chaque version est lue par `cargo metadata`, une version déjà
sur crates.io est sautée, et le job attend que chaque crate apparaisse dans l'index avant le
suivant. `cargo publish` sans `--no-verify` : il recompile contre les versions publiées.

**Piège** : un changement dans l'un des huit dossiers gelés (`common/`, `columnar/`…) exige de
monter sa version **et** de l'ajouter à la boucle, sinon `ld-lucivy` se vérifie contre le 0.27.0
publié et échoue — exactement l'incident de la 4.2.0 avec `lucivy-fst`
(`MapBuilder::with_registry` absent du 0.1.0 publié).

Non publiés : `lucivy-fts`, `lucivy-columnar-cli`, `lucivy-columnar-inspect`, `lucivy-fst-bench`,
`fuzz_test`, et les crates des bindings (`lucivy`, `lucivy-napi`, `lucivy-cpp`,
`lucivy-emscripten`), qui sortent sous forme de wheels, d'addons et de WASM.

## 3. Les tests exigés avant

**En local, avant le commit de version** (`RELEASE.md` § Before, plus la règle du 6 septembre) :

1. `cargo test --lib`, `cargo test --lib --no-default-features`, `cargo test -p lucivy-core --no-fail-fast`, `cargo test -p lucivy-cpp` ;
2. `cargo clippy --lib -- -D warnings` et `cargo clippy -p lucivy-core -p lucistore -p luciole -p sparse-vector --lib -- -D warnings`, **comme la CI les lance** ;
3. Python : `bash bindings/python/build.sh` puis `.venv/bin/python -m pytest tests` ;
4. Node.js : `npm run build`, `node test.mjs`, `node tests/*.mjs` ;
5. WASM : `bash bindings/emscripten/build.sh`, puis une indexation dans le navigateur (`playground/`) **jusqu'à 10 000 fichiers** (au-delà du palier de 2 000 où cassaient les fusions) et le panel de parité ;
6. à blanc : `cargo publish --dry-run -p luciole -p lucistore`, `npm pack --dry-run` dans `bindings/nodejs` et `bindings/emscripten` ; la wheel `abi3` bâtie dans le conteneur officiel (`bindings/python/build-wheel.sh`) et essayée dans un venv neuf.

**En CI (`ci.yml`)**, sur chaque push de `main`, d'une branche `v*` ou `[0-9]*`, sur chaque PR
vers `main`, et rappelée par `release.yml` : `test` × 3 jeux de features (défaut,
`…,zstd-compression,failpoints`, aucune), `clippy` (les deux commandes ci-dessus), `core`
(`lucivy-core`, `lucivy-cpp`), `ground-truth` (contains, fuzzy et regex contre grep),
`thread-spawn-audit`, `lucivy-python` (pytest), `lucivy-nodejs`, `lucivy-cpp`.
**Une branche nommée autrement (`correctifs-…`) ne déclenche pas la CI** tant qu'elle n'a pas
de PR vers `main`.

## 4. Le tag et la release

- La règle : **jamais de tag sur une CI rouge**. Attendre le run `success` du SHA exact de
  `main` (`gh run list`) ; si GitHub rebase à la fusion, c'est le **nouveau** SHA qui compte
  (4.2.0).
- Le tag est le geste de Lucie : le garde-fou de session refuse le push d'un tag `v*`.
  Commande : `git tag -a vX.Y.Z -m "…" && git push origin vX.Y.Z`.
- `release.yml` sur `v*` : `ci` (rappel de `ci.yml`), `build` (`build.yml` : wheels × 5 plateformes
  + sdist, addons Node × 5 dont Linux dans `manylinux_2_28`, `wasm`), `release` (attache wheels,
  sdist, addons et `lucivy.wasm` à la release GitHub — 12 artefacts en 4.3.0), puis
  `publish-pypi`, `publish-npm` (`lucivy` + 5 paquets de plateforme), `publish-wasm`
  (`lucivy-wasm`), et `publish-crates` qui attend les trois.
- Les publications partent **dès le tag** : l'environnement `release` n'a pas de réviseur requis ;
  le seul interrupteur est la variable de dépôt `PUBLISH_ENABLED=true`.
- Rattrapage d'une publication partielle : `gh workflow run release.yml --ref main -f publish=true`
  — chaque job saute ce qui existe déjà ; `release` (la page GitHub) ne tourne que sur un tag.

## 5. Ce qu'il faut à chaque étape

| Étape | Il faut |
|---|---|
| Push des branches | la clé SSH perso ; identité `luciedefraiteur@gmail.com`, aucune ligne d'attribution |
| PR, `gh run`, `gh workflow run`, release | `gh auth switch -u L-Defraiteur` d'abord (le compte actif peut être le pro) ; vérifier l'auteur ensuite |
| Tag | Lucie |
| PyPI, npm ×7, crates.io ×6 | **rien** : trusted publishing, `L-Defraiteur/lucivy` / `release.yml` / environnement `release`, configuré une fois par paquet (le nom de *fichier* du workflow) |
| Vérifier npm : `npx -y npm@latest trust list <paquet> --otp=<code>` | un OTP de Lucie par appel |
| Publication à la main (secours seulement) | `source .vault/load.sh` (jetons crates.io, PyPI, npm) ; `npm publish --otp=<code>` après `npm whoami` (une session expirée répond par un faux **404**) ; crates dans l'ordre du § 2 |
| Après | vérifier chaque registre (API JSON de PyPI, `npm view`, API crates.io **avec un User-Agent**) et noter les versions dans `CLAUDE.md` et le rapport du jour |

## 6. Leçons qui ont façonné l'ordre

3.0.0 : crates publiés en premier, 3.0.1 le soir même — d'où les crates en dernier. 4.0.0 :
tag sur une CI rouge, 4.0.1 dans l'heure — d'où le job `checks` puis le rappel de `ci.yml`.
4.2.0 : `ld-lucivy` refusé par crates.io faute du bon `lucivy-fst` — d'où `lucivy-fst` en tête
et les sauts de version déjà présente. 4.3.0 : sortie d'un coup, sans aléa.
