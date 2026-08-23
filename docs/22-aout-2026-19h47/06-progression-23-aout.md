# Progression du 23 août 2026 — ce qui a été fait, mesuré, commité

Chaque ligne est une mesure, pas une estimation. Les commits sont sur `v3-recovery`.

## Le résultat en une table

50 000 fichiers du kernel Linux, 800 segments naturels, index mmap, moteur seul
(hors désérialisation des documents), spans vérifiés un à un contre le disque.

| Query | hier soir | ce matin | 14h | **16h** | grep (même tâche, disque) |
|---|---|---|---|---|---|
| requête sans résultat | — | — | 190 ms | **29 ms** | 172 ms |
| `kmalloc` strict | 143 ms* | 104 ms* | 177 ms | **29 ms** | 199 ms |
| `spin_lock` strict | 3 526 ms** | 297 ms | 183 ms | **28 ms** | 318 ms |
| `net_device` strict | 14 001 ms** | 220 ms | 207 ms | **34 ms** | 292 ms |
| `include` strict (36 824 docs) | 34 891 ms** | 560 ms | 205 ms | **58 ms** | 342 ms |
| `__init` strict | 49 000 ms | 6 296 ms | 328 ms | **42 ms** | 323 ms |
| `kmalloc` relax | — | — | 184 ms | 175 ms | 1 081 ms |
| `uint64_t` relax | 1 353 ms | — | 216 ms | 170 ms | 1 126 ms |

(*) à 20k documents. (**) sur index fusionné à 32 segments.

Le « grep » de droite lit chaque fichier depuis le disque et trouve **toutes** les
occurrences en spans d'octets — le travail exact du moteur. Hier, la colonne grep
était un `contains` sur un `Vec` préchargé en RAM (90 ms), ce qui donnait « on est plus
lent que grep ». C'était une comparaison truquée dans les deux sens.

## Chronologie

### Matin — mesurer ce que le harnais mesurait vraiment

- `476a724` — Les cinq sidecars étaient copiés (`.to_vec()`) par segment et par
  requête. Zéro-copie via `OwnedBytes`. Strict −21/−27 %, relax inchangé.
- `1f4d19e` — **Le chronomètre du harnais englobait le grep de référence.** Le
  « pipeline word ×15 plus lent » n'a jamais existé : c'était `grep_docs_relaxed` qui
  appliquait `strip_seps` à tout le corpus. Six hypothèses de la §5 d'hier répondaient
  à une question inventée. Séparé en `(search, +fetch, grep)`.
  Ajout de `briques/profile.rs` (compteurs opt-in sous `V3_PROFILE`).
  Puis mémoïsation des remainders dans `build_chains_from_splits` : redondance 15×
  à 78× mesurée ; `include` 2 675 → 213 ms CPU.
- `4eaf367` — À nombre de segments **égal**, un index fusionné était 5 à 29× plus lent
  qu'un index par commits. La conclusion d'hier « moins de segments = plus lent, 46× »
  était fausse : c'était un quadratique en taille de segment dans
  `resolve_chains_impl` (appariement `active × entries`). Trois correctifs : index par
  document, `resolve_filtered` sur les docs actifs seulement (264 M → 289 k entrées),
  mémo de la position 0 (39 M → 365 k postings). `spin_lock` fusionné 3 546 → 324 ms.
- `720951b` — Docs corrigées (la §3 d'hier).
- `83d9695` — **Cache d'index** `V3_INDEX_DIR` (53,9 s → 0 s), timings de merge,
  `LUCIVY_VERBOSE` qui était documenté et jamais lu.

### Midi — trois audits en parallèle (agents), puis le merge

- Audit contrats : cinq de mes soupçons faux, et P5 (collision de clé 0x02) annoncée
  « impact moyen-faible » — elle se révélera réelle l'après-midi.
- Audit quadratiques du merge : a **mesuré** (pas estimé) — `merge_segments_v3` n'est
  pas le coût ; le tri FST estimé à 300-700 ms en fait 30-50 (mesuré après) ; le vrai
  quadratique est dans `word_map.rs` (`contains` linéaire, Θ(D^1.9)).
- Audit requêtes : trois sidecars morts (`word_pos_map`, `chunk_word_map`,
  `next_word_map`), `.sfxpost` encore copié, `entries_filtered` qui promettait une
  dichotomie et balayait, et **l'inversion posmap** comme vrai gisement.
- `1b92465` — Gaspillage supprimé (copies de `.sfx` jetées, port DAG mort, quadratique
  des word maps, `contains` du collector). Indexation −18 %. Merge total **inchangé**
  (18,9 s) — dit franchement dans le commit.
- `5425490` — **`__init` 49 s → 976 ms** : résolution stricte par `posmap` (la question
  dans le bon sens : « quel ordinal à pos+1 ? »), 0 collision / 0 mismatch sur
  27,8 M de lookups ; et `Arc` sur les listes d'alternatives (3,4 M de chaînes clonaient
  la même liste : chunk walk 50 s → 0,87 s).
- `40af7c7` — Vérité terrain relaxed : **ses trois « faux positifs » étaient des bugs
  du harnais** (fenêtre glissante coupée trop tôt). v3 avait raison.
- `803a174` — **`word_pos_map` réaffecté** : même forme que posmap, contenu inutile
  (compteur que rien ne lisait). Stocke désormais l'ordinal word-stripped + span,
  construit depuis les mêmes `word_postings` que `word_sfxpost`. `word_pairs`
  57 M → 0, `uint64_t` relax word resolve 6,7 s → 3 ms CPU. Format `WMP2`.
- `cdd577d` — Indexation sur disque 464 s → 6,7 s : **btrfs+zstd, un fdatasync = 65 ms**,
  25 fsyncs par segment, 8 finalizes en parallèle sérialisés par le FS. Construire en
  RAM, copier sans fsync, tampon `.v3_shape` écrit en dernier.
- `2eb6426` — **Merge parallèle** : le DAG avait N nœuds `merge_i` depuis toujours, mais
  `execute_dag` force l'inline dans un acteur. Fusions en tâches luciole, réponse par
  continuation (`collect_replies_to`), `IndexWriter::merge_many`. 10k : 18,9 → 5,6 s.
- `76dcbc1` — Index fusionné au niveau du naturel : `include` 34,9 s → 1,0 s (chaînes
  groupées par tête commune : 459 M → 780 k lookups ; `OrdinalHeader` emprunte au lieu
  d'allouer trois `Vec`, 40 Go de trafic d'allocation supprimés).
- `75577be` — Le chronomètre « v3 » comptait aussi `searcher.doc()` pour chaque
  résultat (36 824 fetches sur `include`). Séparé.

### Après-midi — la vérité terrain fait le même travail que le moteur

- `456bd58` — **Les documents étaient exacts, les highlights faux.** Une seule
  occurrence par document sur les chaînes (pré-existant : `position` d'émission écrasée
  par le dedup), fins tronquées (clamp sur le contenu propre), relaxed arrêté avant le
  token suivant (`overlap_overflow` placé via posmap).
- `4779915` — Plus aucun span en trop : milieu de chaîne à sti > 0 (sautait des octets
  en silence), fuite de partition 0x02 dans le DFS chunk, et la **collision de clé
  0x02** (`"0ui"` = `"0"+"ui"` ou `"0u"+"i"`, même ordinal) — fin de contenu exacte via le
  premier chunk du mot portant un séparateur.
- `4f3e7a9` — **Le plancher.** Une requête sans résultat coûtait 190 ms. 3 803 ms de
  CPU par requête dans `SfxFileReaderV3::open` : copie du FST entier + table des
  parents + désérialisation de word maps mortes, **par segment, par requête, depuis le
  début du projet**. `open_owned` emprunte le slice mmap. 3 803 → 2 ms. Tout le strict
  passe sous 60 ms.

## Ce que la journée enseigne (trois fois chacune)

1. **Vérifier qu'un écart existe avant de l'expliquer.** Le « ×15 du relaxed », le
   « 46× des segments », le « 300-700 ms du tri FST » : trois chiffres non vérifiés,
   trois explications argumentées, trois fois faux.
2. **Le coût est souvent hors des compteurs.** Le fetch des documents dans le timer,
   le grep dans le timer, l'ouverture du FST hors de `find_literal_v3`. Une requête
   **vide** est la mesure du plancher et doit être la première du panel.
3. **Cohérent ≠ correct.** Les tests existants vérifiaient la cohérence interne ; la
   vérité terrain par spans, depuis le disque, a trouvé six bugs en une heure, dont un
   qui perdait deux occurrences sur trois de `std::unique_ptr`.

## Harnais, état final

```bash
V3_INDEX_DIR=/tmp/v3idx_50k V3_CORPUS=/tmp/linux-bench V3_MAX_DOCS=50000 \
V3_COMMIT_EVERY=500 V3_PROFILE=1 \
V3_QUERIES='zzqqxxyyww:strict,spin_lock:strict,__init:strict,uint64_t:relax' \
cargo test --release -p lucivy-core --test test_sfx_v3_ground_truth v3_ground_truth_contains -- --nocapture
```

Sortie par requête : `(search, +fetch, grep) spans gt=… v3=… miss=… extra=…`, puis les
trois premiers spans manquants/en trop avec contexte et chemin de fichier.
Ajouts du jour : `V3_INDEX_DIR`, `V3_PROFILE`, `V3_MERGE_AT_END`, `V3_DIAG_LITERAL`,
`V3_DIAG_BYTE`, `V3_DIAG_RESOLVE`, `LUCIVY_VERBOSE` (fonctionne maintenant).
