# Rapport de la nuit du 13 au 14 septembre 2026 — 4.3.0 publiée

*Suite de `docs/13-09-2026/08-rapport-session-soir.md`. Le détail technique de chaque
pièce est dans `docs/13-09-2026/07-indexation-profil.md`, § 5 sexies à undecies.*

## 1. En une page

| | avant (4.2.0) | après (4.3.0) |
|---|---|---|
| noyau entier, dictionnaire (101 141 fichiers), même index | 48,2 s | **35 s** (308 segments, 22 535 540 ids, mêmes réponses) |
| comparatif, quatre dispositions lucivy | 46-51 s | 29 / 35 / 35 / 32 s, tailles égales |
| Linux 2.6.0, natif / navigateur | 9 s / 37 s (même méthode) | 6,7 s / 34 s |
| exactitude | deux défauts depuis 4.0.0 (voir § 3) | corrigés, tests rouges sur l'ancien code |
| la paire d'un segment | `.newtexts` / `.newsfx` | `.minted.termtexts` / `.minted.sfx`, les deux lus |

Publiée le 14 septembre au petit matin : PyPI (5 wheels + sdist), npm ×6, `lucivy-wasm`,
crates.io ×5 (`lucivy-fst` reste 0.1.1), release GitHub `v4.3.0` avec 12 artefacts.
`main` = `8e47953` (PR #19, rebase). Décision de Lucie avant de dormir : « si tout est vert
et que tout est à jour, tu pourras publier ».

## 2. Les pièces, dans l'ordre (branches empilées `v4.3` → `4.3.0.1-commit-path` → `4.3.0.2-minted-pair`)

1. **`.pidx`** (§ 5 sexies) — index dérivé des groupes de parents, mesuré avant d'être
   dessiné (2,9 % des records groupés portent 79 % des parents) ; 47,4 s.
2. **Époques** (§ 5 septies) — le profil lu par fil et par instant (`benches/gdb_timeline.py`)
   montre des vallées à un fil occupé à chaque commit : le `retain` à `String` des textes en
   attente, 6,7 s de chemin sériel ; table par époque de commit ; 39,9 s. Un premier jet
   hachait différemment à l'insertion et au rehash : 2,5 M d'ids en double, attrapé par le
   compteur `minted` — règle : un A/B compare aussi les compteurs.
3. **Collecteur** (§ 5 octies) — une entrée de mot par ordinal, intervalles, tampons, champ
   mort ; 35 s. Contrôle à l'octet : dictionnaire identique ; en v3 le record d'un mot porte
   la forme de la première occurrence (celle de `.termtexts`) au lieu de la dernière vue.
4. **Sonde d'abord** (§ 5 octies bis) — textes en attente avant les parties ; marches FST
   68 → 38 s de CPU.
5. **Défaut 1** (§ 5 nonies) — l'expérience des 24 fils d'écriture (rien à gagner) fait perdre
   3 spans de `de` : la coupe `.gmap` d'un segment petit face à la liste gardait un item par
   id. Présent dans 4.0.0-4.2.0 (la 4.2.0 rebâtie le confirme).
6. **Défaut 2** (§ 5 nonies) — `de` relâché rendait 6 spans finissant dans un caractère
   multi-octets : recouvrement de contenu pris sur le mot d'après, candidats ancrés non
   filtrés à `sti` 0. Reproduit en isolation, tracé, corrigé des deux côtés.
7. **Essai retiré** (§ 5 decies) — la paire dans son propre nœud du DAG : rien de mesurable.
8. **Renommage** (§ 5 undecies) — la paire mintée, décision de Lucie.

## 3. Méthode, ce qui a marché et ce qui a coûté

- **Le rejeu du panel à chaque forme nouvelle** (règle de Lucie) a rendu deux défauts
  publiés. La forme des segments est un paramètre de test comme un autre.
- **Alterner base et nouveau deux fois** : une mesure isolée a dit 45 s pour un binaire qui
  fait 35. Un `git worktree` avec son `CARGO_TARGET_DIR` pour l'ancien binaire.
- **Les compteurs** (`minted`, `pending`, segments, comptes) valent le chrono.
- **Clippy comme la CI le lance** (`--lib -- -D warnings`) : deux pushes rouges avant.
- **Le panel de parité dans Chrome avant chaque commit de feature**, rappelé par Lucie.
- Un push est parti sur un clippy rouge (chaîne non conditionnée) ; les scripts conditionnent
  maintenant le push aux deux clippy verts.
- Un index rouvert depuis l'OPFS par l'onglet répond en ~4 s par requête tant qu'il n'est pas
  préchargé (`[preload] 0 files`) — antérieur, à regarder.

## 4. Ce qui reste

Le plateau : 16 fils d'écriture qui tiennent 16-17 cœurs, le dictionnaire à 32 % de leur CPU
(`lookup_or_mint`) ; les vallées de commit à 0,5-0,75 s (queue de finalisation) ; les
libérations des sorties du DAG. Puis les dispositions à la carte (`docs/13-09-2026/01`), le
« remplacer par regex » sur les spans exacts (note du 14 dans le même doc), la vérification
sans positions avec le harnais des traversées, le préchargement à la réouverture OPFS.
