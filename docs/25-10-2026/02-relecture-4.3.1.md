# La 4.3.1 : relecture des correctifs de G et ce qui a été ajouté (25 octobre 2026)

*Branche `4.3.1` sur origin, partie de `main` = `ae71a0c`. Rien n'est taggé ni fusionné. Les
chiffres ci-dessous ont été lus dans les journaux de luciepc
(`~/git_workspaces/lucivy-review-logs*`), worktree détaché `~/git_workspaces/lucivy-review`.*

## 1. Ce que porte la branche

| commit | quoi |
|---|---|
| `58ecdd2` | G : un seul commit en vol par shard (le commit figé sous lectures), repris tel quel |
| `452393f` | G : les écritures perdues d'un shard sans écrivain dites par le commit suivant, repris tel quel |
| `c220fe2` | le rejeu des messages différés **sur place et dans l'ordre** ; code, commentaires et messages d'erreur en anglais (règle du dépôt) |
| `e0f0cc7` | la page : le moteur ne démarre que sur une page isolée ; icône vide (404 du favicon) |
| `0db49a7` | un commit qui trouve des écritures perdues **valide quand même** celles qui ont pris, puis rend l'erreur (décision de Lucie) |
| `522534f` | le test `test_updates_during_commits_keep_their_order`, garde de non-régression |
| `b5a7436` | le numéro 4.3.1 (neuf `Cargo.toml` et dépendances internes, sept `package.json`, `pyproject.toml`), le CHANGELOG |

La branche de G (`bb07627`) partait de `8e47953`, pas de `main` : fusionnée telle quelle, elle
aurait effacé le rapport du 14 septembre. Ses deux commits ont été repris par `cherry-pick`
sur `main`, sans conflit.

## 2. Ce que la relecture a trouvé

1. **Rejeu dans le désordre** (corrigé, `c220fe2`). G renvoyait les messages différés dans la
   boîte de l'acteur, *derrière* ceux qui y attendaient déjà : un `Delete` différé puis rejoué
   derrière un `Insert` arrivé après la fin du commit effacerait la nouvelle version d'une
   mise à jour. **Défaut lu dans le code, pas provoqué** : le témoin, renforcé trois fois
   (commits rapides en boucle, quatre lecteurs pour occuper l'acteur, une pause variable entre
   les deux moitiés de la mise à jour), est resté vert quinze fois sur le code de G. Le test
   reste comme garde de non-régression ; il ne prouve pas le défaut.
2. **Un interblocage dans mon premier jet**, trouvé à la relecture avant le push :
   rejouer les différés pendant qu'`IndexersDrained` tenait encore le verrou de l'écrivain.
   Le verrou est rendu avant `finish_commit`.
3. **Commit à moitié** (corrigé, `0db49a7`) : un commit qui trouvait des écritures perdues
   rendait l'erreur sans committer ce shard pendant que les autres committaient. Il valide
   maintenant, puis rend l'erreur (jointe à la sienne s'il en a une). Le cas « même shard,
   une écriture refusée par l'écrivain » n'a pas de témoin : je n'ai pas trouvé comment faire
   refuser un document à l'écrivain dans un test.
4. **Langue** : commentaires, identifiants (`ajout`, `premieres`, `lecteurs`…) et messages
   d'erreur étaient en français ; CLAUDE.md veut le code et les commentaires en anglais, et
   les autres erreurs de la crate le sont.

## 3. La suite complète, sur luciepc, `-j 8`

| étape | branche de G (`bb07627`) | 4.3.1 (`b5a7436`) |
|---|---|---|
| `cargo clippy --lib -- -D warnings` | vert | vert |
| `cargo clippy -p lucivy-core -p lucistore -p luciole -p sparse-vector --lib -- -D warnings` | vert | vert |
| `cargo test --lib` | vert | 1 479 passés, 0 échec, 24 ignorés |
| `cargo test --lib --no-default-features` | vert | 1 445 passés, 0 échec, 18 ignorés |
| `cargo test --lib` + `mmap,stopwords,lz4,zstd,failpoints` | vert | 1 484 passés, 0 échec, 24 ignorés |
| `cargo test -p lucivy-core --no-fail-fast` | vert (94 dans `sharded_handle`) | 208 passés, 0 échec, 35 ignorés (les trois tests des correctifs verts) |
| `cargo test -p lucivy-cpp` | vert | 19 passés |
| Python, `pytest tests` | 113 passés, 4 sautés (2ᵉ passe) | **1 rouge sur 11 passes complètes**, puis 113 passés, 4 sautés ×5 |
| Node, `test.mjs` + `tests/*.mjs` | vert (2ᵉ passe) | vert, `FAILS 0` partout |

**Le rouge Python** : `TestPersistence::test_uncommitted_not_persisted` — un document ajouté
sans commit, l'index lâché par `del idx`, retrouvé à la réouverture (`num_docs` 2 au lieu de 1).
Vu une fois, dans la suite finale de 4.3.1. Rejoué seul : 0 sur 10 sur chacun de `b5a7436`,
`0db49a7`, `c220fe2` et `452393f` (le code de G) ; suite complète : 0 sur 5 sur 4.3.1, 0 sur 5
sur `main`. **Non imputé, non écarté** : une intermittence de la fermeture (un commit qui part
pendant le `drop` ?) à regarder avant la publication, ou à ouvrir en ticket.

## 4. La page en ligne

Reproduit dans Chrome sans tête sous un serveur sans COOP/COEP (comme GitHub Pages) : la page
de `main`, après un Ctrl+Shift+R, est restée bloquée sur « Loading WASM engine », non isolée,
non contrôlée, jamais rechargée — une passe sur deux. Avec `e0f0cc7`, le même cas s'est
rechargé seul et a indexé ses 1 314 fichiers. Un cas reste mal compris : un rechargement en
pleine indexation a laissé deux chargements de suite non contrôlés (la page affichait encore
« Preparing the page » au relevé). La page se déploie au push de `main` (`pages.yml`), pas au
tag.

## 5. Ce qui manque avant le tag

- **Le WASM de la page n'est pas rebâti** : `playground/pkg/lucivy.wasm` est celui de la 4.3.0,
  sans les deux correctifs (le job `wasm` de `release.yml` bâtit son propre paquet pour npm,
  la page non). Rebâtir par `bindings/emscripten/build.sh` et changer le `BUILD` de la page.
- Le rouge Python ci-dessus.
- PR `4.3.1` → `main`, CI verte sur le SHA exact de `main` après la fusion, puis le tag
  `v4.3.1` par Lucie (voir `01-pipeline-de-publication.md`). `bindings/nodejs/package-lock.json`
  porte encore 3.0.0, comme en 4.3.0 : laissé tel quel.
