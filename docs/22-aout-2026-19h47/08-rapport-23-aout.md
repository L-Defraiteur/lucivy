# Rapport du 23 août 2026 — le moteur v3 exact sur ses trois modes

Lisible seul. La chronologie détaillée est dans `06-progression-23-aout.md`, les
chantiers restants dans `07-suggestions-et-chantiers.md`, le mode d'emploi des
mesures dans `docs/BENCHMARKS.md`. Branche `v3-recovery`, de `2d17267` à `65ef904`.

## En une phrase

Contains (strict et relaxed), fuzzy et regex donnent maintenant **exactement** les
occurrences que le disque contient — spans d'octets vérifiés un à un contre un grep
de référence, sur 4 600 fichiers rag3db et 50 000 fichiers du kernel Linux, sur
l'index naturel comme sur l'index fusionné — à des temps de 30 à 200 ms, là où la
journée avait commencé avec des highlights faux, des fusions qui perdaient des
documents et un regex qui répondait 0.

## État final mesuré (kernel 50k, 24 cœurs, spans exacts)

| mode | requête | search | spans |
|---|---|---|---|
| plancher | requête sans résultat | 29 ms | — |
| strict | `kmalloc`, `spin_lock`, `__init` | 28-32 ms | exacts |
| strict | `include` (36 824 docs) | 55 ms | 214 692 |
| strict | `->`, `:`, `\t\t`, `\n\n`, `;\n\t` | 137-665 ms | 1,2 à 7,2 M |
| relaxed | `uint64_t`, `__init` | 40 / 63 ms | exacts |
| fuzzy d=1 | `kmallc`, `inclde`, `uint64` | 71 / 142 / 67 ms | exacts |
| fuzzy d=2 | `kmalloc` | 201 ms | 77 050 |
| regex | `/\*[^*]*\*/` | 191 ms | 421 036 |
| regex | `[0-9]{8}` (sans littéral : balayage complet) | 190 ms | 201 764 |

rag3db : 15/15 contains, 11/11 fuzzy, 19/19 regex. Index sous policy (segments
plafonnés à 10 000 docs) : indexation + fusions 72 s pour 50k, `include` 79 ms.

## Ce qui a été corrigé, et ce qui l'a trouvé

Chaque point a d'abord été réduit à une reproduction de quelques secondes.

**Index et merge**
- Une clé FST couvrait plusieurs *formes* (`init` = mot `init`, ou `in`+overlap `it`) sous
  un ordinal portant les métas de la première occurrence internée ; l'ordre des
  segments changeait le gagnant → fusionné ≠ frais, et les deux parfois faux.
  Internement par (texte, forme). *Trouvé par `v3_merge_equals_fresh_by_spans`.*
- Le tokenizer émettait un **chunk vide** sur les textes multi-octets → occurrences
  manquantes devant du CJK. *Trouvé par bisect + coupe caractère par caractère.*
- Deux **falaises d'encodage** silencieuses en release (ordinal 24 bits, compteur de
  parents u16) : la fusion 50k → 1 segment était à 1 600 parents de la troncature.
  Gardes + u32. *Trouvé en mesurant avant de corriger.*
- La **policy de merge** n'était jamais consultée ; branchée au commit, cascade, plafond
  de sortie 10k. Ça a fait sortir deux bugs de concurrence : le **GC supprimait les
  `.sfx` des segments en cours d'écriture** (36 824 → 14 247 docs) et un `persist`
  pendant une fusion → SIGSEGV. Puis `atomic_write` de `StdFsDirectory` tronquait
  avant d'écrire (meta.json vide lu par un reload).
- Le merge « par paliers » du harnais avait produit un segment de 48 078 docs + des
  miettes : l'index « 32 segments » d'hier n'en était pas un.

**Fuzzy**
- Les highlights étaient les étendues des chaînes de trigrammes (26-40 octets pour
  10) : le document était vérifié, jamais le span. Une **définition partagée**
  (`fuzzy_spans`) entre moteur et vérité terrain, alignement sur fenêtre, retour aux
  octets source.
- `MAX_CHAINS_PER_DOC = 8` perdait 280 occurrences sur 1 107 (plafond silencieux) ;
  positions de hits word mal prises ; marge en positions au lieu d'octets de contenu.
- Audit agent : prescan séquentiel, FST parcouru deux fois, 96 % de hits en écho,
  `resolve_doc` par position (49 postings décodés pour 1). 7,3 s → 142 ms sur `inclde`.
- Trois générateurs de candidats comparables (`ngram`, `pivot`, `pieces`, `auto`) avec
  spans identiques : aucun ne domine, `auto` choisit par coût estimé.

**Regex**
- L'ancien approximait le motif sur l'index : 0 doc sur 1 142 pour `std::[a-z_]+_ptr`.
  Réécrit par vérification : littéraux requis (`regex-syntax`), occurrences par le
  contains, fenêtre **prouvée** par la longueur maximale du motif, sinon document entier,
  `regex::Regex` décide. 19/19 et 11/11.
- Audit agent : un **double free dans luciole** dès qu'une tâche du scatter échoue —
  latent pour contains et fuzzy depuis le merge parallèle.

**API**
- `LucivyHandle::search` — l'API des bindings — ne marchait pas sur un index v3
  (prescan v2 inconditionnel, « invalid .sfx magic bytes »).

## Ce que ça a coûté de comprendre

Trois fois dans la journée, l'explication plausible était fausse avant mesure : « le
merge est bugué » (c'était l'internement), « l'overlap UTF-8 » (c'était un chunk
vide), « le vocabulaire des autres segments » (c'était intra-document). Et
l'expérience « sauter les chaînes chunk en relaxed » passait tout le panel rag3db
avec moitié moins de CPU — un corpus SKU synthétique l'a réfutée en 0,1 s
(identifiants de 400 octets). Les cas spéciaux avant la conclusion.

## Suite, même jour (après-midi) — les quatre points dans l'ordre inverse

Tout est dans `07` (B2 bis, C bis, E) ; le résumé :

| Chantier | Commit | Résultat |
|---|---|---|
| B2 bis — saut des chaînes chunk en relaxed, prouvé par `.termtexts` | `70bd8bc` | 798/800 segments sautent ; `uint64_t` relax 40 → 32 ms, `__init` 63 → 49, fuzzy `inclde` 142 → 109, `kmallc` 71 → 56, d=2 201 → 175 |
| Nettoyage index — `.freqmap`, `.chunk_word_map`, `.next_word_map`, sections mortes du `.sfx`, `overlap_siblings`, regex legacy | `e164437` | −2 270 lignes, 8 sidecars par champ au lieu de 11 |
| Avertissements honnêtes — `query_warnings` dans core et les 5 bindings | `8e7b07d` | 12 règles, tests unitaires + bout en bout |
| `sfx_version = 3` par défaut | `132ae15` | a révélé et corrigé : `startsWith`/`term` faux sur v3, `close()` sous merge en vol, course de `LucivyDeltaExporter` |

Le dernier point est celui qui a rapporté le plus : trois bugs réels que la suite
ne voyait pas parce que tout tournait en v2 par défaut. `startsWith` était faux
depuis `8aeb093` et son test ignoré « parce que pas sur le chemin critique ».

## À faire, par ordre

1. `LucivyDeltaExporter` : fait pour la course ; vérifier que les bindings qui
   exportent des snapshots drainent aussi (`export_snapshot` pendant un merge).
2. Compteurs `n_rx_*` dédiés ; littéraux préfixes/suffixes par coût avec intersection.
3. Emscripten n'a jamais été compilé depuis le début du v3 (`lucivy_query_warnings`
   y est ajouté sans compilation).
4. Mode « identifiant entier » pour `term` (délimité par des blancs, pas par `_`) —
   si un utilisateur le demande.
5. Les trois échecs unitaires pré-existants (fixtures mortes) : réparer ou supprimer.
