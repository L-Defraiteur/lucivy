# Journal — 6 septembre 2026 : l'indexation du dictionnaire, la vitrine, 4.0.0 puis 4.0.1

Suite de [`../05-09-2026/10-journal-session-5-septembre-nuit.md`](../05-09-2026/10-journal-session-5-septembre-nuit.md).
Pour repartir : ce fichier (§11 : les objectifs), puis
[`04-architecture.md`](04-architecture.md) et
[`05-knowledge-dump.md`](05-knowledge-dump.md), autonomes ; le détail
d'avant dans `../05-09-2026/` (04 pour l'état et le todo au fil de l'eau, 09
la présentation, 11 et 12 l'architecture et le dump de la nuit précédente).
Branche `v4` = `main`. **4.0.0 et 4.0.1 publiées.**

## 1. Le chantier indexation : ×2,1 → ×1,5 (matin)

Lucie : « on reprend les pistes pour réduire le temps d'indexation ? », avec la
consigne de surveiller le pic mémoire WASM après chaque correctif.
Chronométré d'abord (compteurs `LUCIVY_VERBOSE` dans `lookup_or_mint` et au
commit) : le cadrage de la veille visait le chemin par jeton (46 s cumulées
sur les fils), qui tourne **en parallèle** du flux ; le mur était **le
commit** — un seul shard dans le harnais, écriture de la génération 8,8 s,
compaction 3,4, réouverture 1,4, en série. Étapes mesurées (30 000 fichiers,
v3 15,3 s) : lecteurs `.termtexts` et vues FST ouverts une fois par champ,
verrou sans allocation, 16 tranches → 29,7 ; cache des clés trouvées → 110 s
(éviction quadratique) puis 31,0 (neutre, refusé) ; moins de générations →
36 et 55 (refusé) ; FST par segment + fusion en flux au commit → 31,6 (une
fusion coûte 1,2 µs la clé, comme bâtir) ; passes FST/textes en parallèle →
30,1 ; **repli différé** (paires nommées dans `meta.json`, tâche de fond,
`meta.json` réécrit par l'acteur, recherche qui attend par défaut) → **23,2**.
Puis Bloom sur la clé d'internement (97,5 % des marches pour rien sautées,
FST 28 → 20,6 s cumulées, **mur inchangé** : les collecteurs ne bornent rien
en natif) et les ids lus sans décoder les textes (−2 s) → **23,0 s**. Noyau :
131 → **106,8 s**, `derived_in_ram` 134 → 110,9.

**La règle de Lucie sur la fenêtre** : « je veux pas que les gens voient de
faux temps de requête ». La recherche attend le repli (`dictionary_wait`,
défaut), la fermeture de l'écrivain attend `meta.json`, et la fenêtre
(3 → 20 ms sur le panel) n'est visible qu'avec `LUCIVY_DICT_WAIT=0`.

**WASM** : le repli de fond ne gagnait rien (41 → 42 s) et les **FST par
segment bâties en parallèle** montaient le pic (2.6.0 2 023 → 2 279 Mo, Godot
1 778 → 1 894) ; le repli synchrone seul ne rendait pas le pic (2 279) ; sans
FST par segment sur wasm32 : 2 023 / 42 s et 1 766 / 31 s. wasm32 garde le
chemin d'avant, le différé est natif.

## 2. Jaro-Winkler : toutes les occurrences, vérifié

Lucie : « il y avait une limitation à un seul match par fenêtre ». `best_window`
(une occurrence par fenêtre candidate) → `jaro_spans` : sous-chaînes à ±d
caractères, similarité ≥ seuil **et ≤ d éditions**, une par groupe chevauchant.
Vérité terrain `grep_spans_jaro`, ligne `jw1` du panel vérifiée : **10/10**
sur 10 000 (228 documents, 876 spans), 30 000 dictionnaire (707, 2 284,
11,4 ms), **noyau entier** (5 196, 18 824, 72,7 ms contre 98 en `fz1`).

## 3. Bindings et préparation de la publication

`dictionary_wait` dans Python, Node (typings napi), C++, WASM ; README des
bindings et du cœur alignés ; CHANGELOG 4.0.0 complété. Tests Node : deux
suites comparaient l'ordre exact des résultats — les ex æquo reviennent dans
l'ordre des segments, qui dépendait des fusions de fond ; comparés triés
par document, puis **l'ordre des segments dans `meta.json` rendu
déterministe** (taille puis id). Versions 4.0.0 partout, dry-runs crates et
npm verts, `main` ancêtre de `v4`.

## 4. La vitrine (revue UX de Lucie, soir)

- **Un seul index en mémoire** : `closeAllOpen()` avant toute indexation ou
  ouverture (terminal, clone GitHub, fichiers, snapshot, ↻) — deux index dans
  4 Go, c'est le plantage.
- **Onglets dynamiques** : un par index en OPFS (registre `lucivy_corpora`),
  clic = réouverture depuis l'OPFS avec témoin, croix = suppression.
- **OPFS chaud borné** : budget min(8 Gio, moitié du quota annoncé), éviction
  du moins récemment ouvert avant d'indexer, jamais la source lucivy ni les
  index utilisateur ; quota vérifié en ligne (Chrome 60 % du disque, Firefox
  10 Gio, Safari 60 % et 7 jours).
- `index owner/repo` depuis le terminal ; `-jw 0.7` et `[0.7]` acceptés ;
  aide de `--jw` récrite.
- **Dictionnaire par défaut** (`?nodict` pour le v3), puis étendu au moteur.

## 5. Le dictionnaire devient le défaut du moteur

Lucie : « ça va pour une regex, personne les fait aussi bien, 240 ms on s'en
fout ». `effective_sfx_version()` rend 4 sauf `shared_dictionary: false` ;
bindings, page, docs alignés. Passer tous les tests par ce chemin a fait
remonter une **course** : un lecteur relisant `meta.json` entre la permutation
du repli et sa réécriture rouvrait le dictionnaire sur des paires supprimées
(zéro résultat une fois sur trois sur le stockage RAM) — un dictionnaire tenu
en avance du disque (`next_generation`) est gardé. Les tests `lazy` des blob
stores (Python, C++) créent leur index par segment : un dictionnaire est lu
entier à l'ouverture (documenté).

## 6. Compat 3.0.8 revérifiée avec un index de `main`

Worktree de `main` (`8301b55`), son harnais compilé à part, 10 000 fichiers
indexés par lui (160 segments, layout `.bytemap`, 1 133 Mo, 9/9), rouverts
sans rebâtir par le harnais v4 : **10/10**.

## 7. Publication : 4.0.0, CI rouge, 4.0.1

`gh auth switch -u L-Defraiteur`, `PUBLISH_ENABLED=true`, environnement
`release` sans réviseur (tags `v*`). `main` ← `v4`, tag `v4.0.0` vers minuit :
douze builds, PyPI, npm (six paquets), crates.io, page déployée — **mais la CI
de `main` était rouge** (clippy : docs manquantes des compteurs, un `mut`, un
`if`, deux `let…else`, une init ; build minimal : le banc de compaction ouvre
un `MmapDirectory` sans la feature). Rien du moteur. Corrigé, CI verte,
**`release.yml` gagne un job `checks`** dont dépendent les publications, et
**4.0.1** republie le même moteur : « pas honnête sinon ». Vérifié sur les
trois registres. `gh` reste sur L-Defraiteur (Lucie rebascule lundi).

## 8. Relecture extérieure

Cinq points fondés, corrigés dans README, ARCHITECTURE.md, la page, le
rapport et le script : « does not leak » → même score sous les mêmes
statistiques, le filtre choisit ce qu'on visite ; deux allers-retours partout ;
navigateur/natif ne prétend plus l'identité des comptes sur deux ensembles de
fichiers ; la commande de reproduction épingle **Linux v7.2** (le `Makefile`
de l'instantané, copié le 28 août) ; « inexpressible » → « not with this
analyzer », plus le disclaimer : toute question du tableau est répondue par
l'index par défaut, sans rien configurer.

## 9. Réponses aux issues, brouillons

[`01-reponses-issues-4.0.1.md`](01-reponses-issues-4.0.1.md) : #12 fait avec
ses limites et la taille réglée, #15 pas de guide et lucivy n'ouvre pas un
index tantivy, suivis #11 #13 #14. Rien de posté.

## 10. Commits

`5170bcd` compteurs et gains sans mémoire · `7358112` repli différé ·
`aa803f9` wasm32 chemin d'avant · `acf3655` filtre de Bloom, ids sans décoder ·
`57fef0c` bindings, Jaro-Winkler vérifié · `bb818a5` JW noyau · `99a2a4e`
étape 3 · `1f65b2a` 3.0.9 n'existe pas · `b1f32d6` un seul index · `fd8a623`
onglets dynamiques et budget · `3b7aa24` `-jw` · `1153050` défaut
dictionnaire, course, compat main, **4.0.0** · `fe301b3` CI · `7f18415`
**4.0.1** · `2a8df77`, `91cf6cc`, `395336a`, `fdd9973`, `608d249` docs et
page · `f7efcbf` brouillons d'issues.

## 11. Objectifs, dans l'ordre

1. **Poster les réponses aux cinq issues** après relecture de Lucie
   (`01-reponses-issues-4.0.1.md`) ; y annoncer l'import comme chantier ouvert.
2. **L'import tantivy / Elasticsearch** ([`02-import-tantivy-elasticsearch.md`](02-import-tantivy-elasticsearch.md)),
   décision scripts Python ou binaire Rust, tantivy d'abord — pour une 4.1.
3. **L'article n° 1** (« notre bench mesurait une réponse que personne n'avait
   vérifiée », enrichi de Jaro-Winkler vérifié, de la compat prouvée et de la
   4.0.1) chez nous, puis dev.to ; **le GIF** de la démo dans le README ; puis
   Show HN, r/rust, This Week in Rust, lobste.rs (`../28-08-2026/04-strategie-diffusion.md`).
4. Moteur : les ~5 s d'écart restants avec le v3 (chronométrer **côté mur**
   d'abord : commit hors dictionnaire, drain des segments, `.newsfx`) ;
   l'ouverture paresseuse d'un blob store avec le dictionnaire (lire les
   `dict-*` par tranches) ; le filtre de Bloom dans WASM (mesurer s'il vaut
   ses 8-25 Mo) ; la 9ᵉ génération d'un index fermé sans compaction — non,
   décidé : on garde la compaction à la fermeture.
5. Hygiène : `docker rm -f lucivy-es` ; le serveur du playground sur 9877.
