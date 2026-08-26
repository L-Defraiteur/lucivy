# Page de présentation et release 3.0.5 (26 août 2026, après-midi et soir)

Autonome : ce qui a été construit, ce qui a cassé, ce qui reste.

## 1. La page de présentation (`playground/index.html`)

Une seule page, deux vues sur le même worker et le même index :

- **Accueil** (par défaut) : pitch (« Search code the way you grep it »),
  deux boutons (**Replay demo**, **Open the playground**), puis le terminal.
- **Playground** (`#playground`, `?playground`, ou les paramètres
  d'automatisation `?corpus`, `?open`, `?nodemo`) : l'ancienne vue,
  inchangée, avec un lien « ← lucivy ». Les tests (`test-playground.mjs`,
  `test_playground.py`, `test_regex_bench.mjs`) pointent sur `?playground`.

**Le terminal ne simule rien.** Il démarre seul dès que le worker est prêt
(décision de Lucie : « sinon on arrive on attend pour rien »), et à chaque
Play il **reclone et réindexe** — la démo, c'est l'indexation. Les lignes
viennent d'un `MutationObserver` sur les mêmes `#status` / `#memory` que
le playground (`Downloading…` → ligne réécrite en place ; `Indexing N files
from…` → `$ lucivy index /lucivy_source` + barre ; `… N/M` → barre mise à
jour ; `Final commit`, `Merging segments`, `indexed here in Ns`, la ligne
de résidence du préchargement). Puis huit recherches réelles via
`demoIndex.search` / `searchFiltered`, chacune chronométrée (top 3 avec
highlights et champs = ce que coûte une requête utilisateur ; le compte
« N hits » vient d'une seconde requête non chronométrée, limite 100) :

| commande | ce que ça montre |
|---|---|
| `"very suffix of every token"` | commence **dans** un token (« Every ») et traverse cinq tokens |
| `"suffixfst"` | séparateurs relaxés : `suffix_fst`, `SuffixFst`, « suffix FST » |
| `--fuzzy 2 "the requird literals of the patern drive the search"` | deux fautes dans neuf mots, 1 hit (README.md) |
| `"rust🦀lang"` | emoji = octets comme les autres |
| `--regex "Sharded[A-Z]\w+"` | le littéral guide, la regex valide |
| `"suffix AND (fst OR automaton) AND NOT tantivy"` | syntaxe booléenne |
| `"lock_init"` | la courte, pour la perf |
| `--allowed 12 "index"` | vrai pré-filtre sur 12 documents |

Sur petit écran (< 720 px) : quatre (`phone: true`). Puis un **prompt réel**
`$ lucivy search ` (un `<input>` transparent dans la ligne : caret natif,
↑/↓ historique, Échap, clic dans le terminal pour reprendre le focus) —
parseur `parseSearchCommand` : `--fuzzy N`, `--jw [S]`, `--regex`,
`--phrase`, `--prefix`, `--exact`, `--strict`, `--allowed N`, `--limit N`,
`--help` (tableau détaillé), AND/OR/NOT/parenthèses détectés → `parse`.
Habillage : bannière glitchée (2,5 % des caractères au repos, 0,6 % pendant
la démo, 0 sous `prefers-reduced-motion`), scanlines CSS, typewriter cadencé
par le temps réel (un onglet en arrière-plan rattrape au lieu de taper une
lettre par seconde). Sections dessous : What you just saw, Numbers (table du
README), Install (4 onglets), How it works, Honest limits, pied.

Pièges rencontrés :
- la phrase fuzzy vivait dans `index.html`, qui est passé à 105 Ko avec la
  page → **sauté par le clone (limite 100 Ko)** → 0 hit ; phrase prise dans
  le README ;
- un index stocké d'une visite précédente était préchargé pour rien avant
  le reclone (ligne « in memory » qui s'intercalait) : sur l'accueil on
  n'ouvre plus l'ancien index ;
- « downloaded: N files » n'apparaissait que si le téléchargement durait
  plus de 400 ms (le tick des points) — la ligne est créée à la demande.

## 2. La matrice de release (`.github/workflows/release.yml`)

Cinq cibles, wheels Python (`maturin-action`, `abi3`) et addons Node
(`cargo build --target`, Linux dans `quay.io/pypa/manylinux_2_28_*`),
smoke test create/add/search sur le runner qui a construit, artefacts
attachés à la GitHub Release, publication derrière **deux verrous** :
l'environnement `release` (reviewer : Lucie) et la variable **de dépôt**
`PUBLISH_ENABLED=true`. PyPI par trusted publishing (OIDC). Déclencheurs :
tag `v*`, `workflow_dispatch` (`publish` coché ou non), push sur `main`
touchant `bindings/**` ou le workflow (build seul).

Ce qui a cassé, dans l'ordre (cinq runs) :
1. `cp target/<triple>/…` : le crate `lucivy-napi` est membre du
   workspace, sa lib est dans `../../target` ;
2. `find ../../target target` sur un `target` absent → code 1 → le shell
   `-e -o pipefail` des Actions tue l'étape avant le `test` ;
3. le smoke test Node chargeait `bindings/nodejs/lucivy.node` **commité
   dans git** (binaire Linux x64 de 12 Mo, tracké depuis toujours) : « not a
   mach-o », « not a valid Win32 application » → le loader essaie d'abord
   `lucivy.<plateforme>.node`, puis le paquet plateforme, puis le build
   local, et saute un fichier qui ne charge pas ; le binaire sort de git ;
4. `macos-13` (Intel) n'a jamais eu de runner en une heure : retiré par
   GitHub → cross-compilation depuis `macos-14`, sans smoke test ;
5. GitHub Actions en panne majeure au moment du tag (`major_outage`) : le
   run est parti tout seul au retour.

Résultat : 11/11 verts, moteur et bindings compilent sur les cinq
plateformes **sans toucher au Rust** (héritage tantivy : `winapi`, mmap
portable).

## 3. La publication 3.0.5

- PyPI ✓ par la CI après approbation : 5 wheels + sdist.
- npm ✗ par la CI : `EOTP` — le token `NPM_TOKEN` ne contournait pas la
  2FA. À la main (OTP par paquet) : `lucivy-linux-x64-gnu`,
  `lucivy-linux-arm64-gnu`, `lucivy-darwin-x64`, `lucivy-darwin-arm64`, puis
  **npm a refusé `lucivy-win32-x64-msvc`** (« Package name triggered spam
  detection », 403, deux fois) → publié en **`lucivy-windows-x64`** ; le
  loader, `optionalDependencies` et le template `npm/win32-x64-msvc/`
  (dossier inchangé, `name` changé) suivent. Puis `lucivy@3.0.5` (13,5 Ko,
  sans binaire) ; installation fraîche vérifiée (ne tire que le paquet Linux
  x64, search OK). `lucivy-wasm@3.0.5` (wasm identique à celui du
  playground, build `2fcf506`).
- crates.io ✓ à la main, sur feu vert : `luciole → lucistore → ld-lucivy →
  lucivy-core → sparse-vector`, tous 3.0.5.
- GitHub Release `v3.0.5` : 11 artefacts, auteur `github-actions[bot]`.

## 4. Le proxy CORS, durci (soir)

`playground/lucivy-proxy-worker.js` (Cloudflare Worker, à coller dans le
tableau de bord — pas de wrangler ici) : n'était pas un proxy ouvert (cible
limitée à `api.github.com`), mais relayait toute l'API depuis n'importe quel
site, et la limite anonyme GitHub (60 req/h, partagée via les IP de
Cloudflare) était à la merci du premier script. Maintenant : `Origin`
obligatoire dans une liste (`l-defraiteur.github.io`, `localhost`,
`127.0.0.1` — forgeable hors navigateur, ce qui est documenté), cible
limitée à `/repos/<owner>/<repo>/tarball[/<ref>]`, `GET` seul, **cache 30
min des réponses anonymes uniquement** (jamais avec `Authorization` : un
dépôt privé ne doit pas ressortir chez le visiteur suivant ; sur un
`workers.dev` nu le Cache API peut être un no-op, ça marche sur un domaine
custom), `Content-Length` et `X-RateLimit-*` exposés. La page affiche la
progression du téléchargement (`Downloading … 4.2 MB / 13.5 MB`) et, sur
quota épuisé, « try again in N min, or add a token ». Le filet que rien ne
contourne : une règle de rate limiting Cloudflare par IP, à créer dans le
tableau de bord.

**Plan B** : le déploiement Pages (`pages.yml`) bundle
`playground/lucivy-source.tar.gz` (les fichiers texte du commit, ~8,6 Mo,
jamais commité) ; si le proxy ou GitHub échoue, la démo le prend et le dit
(« using the snapshot of the source bundled with this page »). Test :
`?noproxy`.

## 5. À faire

- **Trusted publishing npm** : sur npmjs.com, pour chacun des 6 paquets
  (ils existent maintenant), Settings → Trusted Publisher → GitHub
  `L-Defraiteur/lucivy`, `release.yml`, environnement `release` ; puis
  supprimer le secret `NPM_TOKEN` et le token — le workflow passe seul en
  `--provenance` quand le secret est absent. Restreindre ou révoquer le
  token existant.
- Cloudflare : coller le Worker durci et le déployer ; créer la règle de
  rate limiting par IP.
- Smoke test macOS Intel : absent (cross-compilé) ; acceptable, plateforme
  en fin de vie.
- Playground : GIF pour le README et les posts ; test sur le téléphone de
  Lucie de la vue accueil (quatre recherches, terminal 58 vh).
- `bindings/nodejs/node_modules/@napi-rs/cli` est tracké dans git (vestige) ;
  à sortir un jour, comme le binaire.
