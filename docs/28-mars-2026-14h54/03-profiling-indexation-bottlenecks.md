# 03 — Profiling : bottlenecks indexation SFX

Date : 28 mars 2026

## Contexte

5000 docs (kernel Linux) indexés en 4 shards round-robin : **36.2 secondes**.
90K docs : commits de plus en plus lents (32s → 40s → 78s → 129s pour 20K docs).

Le profiling instrumenté (`[sfx-profile]`) montre que PosMap et ByteMap ne coûtent rien.
Le bottleneck est le SuffixFstBuilder + sibling table.

## Résultats profiling (segment type : ~6K tokens, ~180K entries)

| Phase | Temps | % du total | Complexité |
|---|---|---|---|
| **FST build** (add_token + sort + dedup + MapBuilder.insert) | 200-640ms | 50-65% | O(K × L × log K) + O(K × D × M) |
| **Sibling table** (build + serialize) | 100-225ms | 15-25% | O(P) HashMap+HashSet |
| sfxpost_finish (sort + serialize) | 45-115ms | 10-15% | O(E log E) |
| postings pass (sfxpost + posmap + bytemap) | 22-58ms | 5-10% | O(E) |
| sort tokens | 9-86ms | 2-8% | O(T log T) |
| posmap serialize | 3-19ms | <2% | O(D × P) |
| bytemap serialize | 0ms | 0% | O(T) |
| gapmap | 0-2ms | 0% | O(D) |
| sfx assemble | 0ms | 0% | O(1) |

K = suffix entries (~48K pour 6K tokens), L = longueur moyenne suffix (~4 bytes),
T = tokens (~6K), E = posting entries (~180K), P = sibling pairs (~180K),
D = docs, M = transitions par noeud FST.

## Bottleneck 1 : SuffixFstBuilder (50-65% du temps)

### add_token() — génération des suffixes

`src/suffix_fst/builder.rs` ligne 174-200.

Pour chaque token de N bytes, génère N suffix entries (un par position byte).
Chaque suffixe = `String::with_capacity()` + `push_str()` = allocation.

Pour 6K tokens × 8 bytes avg = **48K allocations String**.

De plus, le `diag_emit!(SuffixAdded { token: token.to_string(), suffix: suffix.to_string() })`
fait 2 clones String supplémentaires par suffixe. A vérifier si le macro est zero-cost
quand il n'y a pas de subscriber.

### sort + dedup entries

48K entries triées par comparaison lexicographique de Strings : O(48K × log(48K) × 4 bytes).
~20ms mais pourrait être optimisé avec des bytes slices au lieu de Strings owned.

### MapBuilder.insert() — construction du FST

48K appels à `fst_builder.insert(key, output)`. Chaque appel :
- `check_last_key()` : O(L) comparaison
- `find_common_prefix_and_set_output()` : O(D) traversée trie
- `compile_from()` : O(D) compilations de noeud, chacune avec hash lookup dans Registry(10_000, 2)

**~150ms pour 48K insertions.** C'est le coeur du coût et c'est dans lucivy-fst (fork BurntSushi/fst).

### Pistes d'optimisation FST

1. **Réduire le nombre de suffixes** : `min_suffix_len` est déjà en place mais avec une valeur faible.
   Augmenter = moins d'entries FST mais perte de couverture suffix search.

2. **Bytes au lieu de Strings** : les clés FST sont déjà des bytes. Remplacer
   `Vec<(String, ParentEntry)>` par `Vec<(Vec<u8>, ParentEntry)>` ou mieux,
   un buffer contigu avec offsets pour éviter les allocations individuelles.

3. **Supprimer les diag clones** : vérifier que `diag_emit!` est zero-cost sans subscriber.
   Si non, conditionner les clones au `has_subscribers()`.

4. **FST builder en mode batch** : certains FST builders acceptent un itérateur trié
   au lieu d'inserts individuels. `MapBuilder::from_iter()` pourrait être plus rapide
   (moins de check_last_key, meilleure localité mémoire).

## Bottleneck 2 : Sibling table (15-25% du temps)

`src/suffix_fst/sibling_table.rs`.

Structure : `HashMap<u32, HashSet<SiblingEntry>>` avec 180K insertions.

### Problèmes

1. **Nested HashMap + HashSet** : chaque ordinal qui a des siblings alloue un HashSet.
   Pour 6K ordinals, ~3K ont des siblings = 3K HashSet allocations.

2. **HashSet overhead pour petites collections** : la plupart des ordinals ont 1-5 siblings.
   Un HashSet pour 3 éléments a un overhead de ~200 bytes (header + buckets).

3. **serialize() itère tous les ordinals** (0..num_ordinals) même ceux sans siblings.

### Pistes d'optimisation sibling

1. **Vec<Vec<SiblingEntry>>** au lieu de HashMap<u32, HashSet> :
   - Accès O(1) par index, pas de hashing
   - Dedup par sort + dedup sur le Vec final au lieu de HashSet

2. **Buffer plat** : `Vec<(u32, SiblingEntry)>` trié par ordinal, avec offset table.
   Zéro allocation par ordinal. Sort + dedup une seule fois à la fin.

3. **Construction pendant add_token** : les sibling pairs sont connues au moment
   de l'ingestion (tokens consécutifs dans un doc). On pourrait les écrire directement
   dans un buffer trié sans passer par une HashMap.

## Bottleneck 3 : sfxpost_finish (10-15%)

`src/suffix_fst/sfxpost_v2.rs` lignes 51-119.

Pour chaque ordinal : sort des entries par (doc_id, ti), groupement par doc_id,
encode VInt, 4 passes sur les données. ~45-115ms.

### Pistes

1. **Réduire les passes** : grouper + encoder en une seule passe après le sort.
2. **Pré-allouer les payloads** : estimer la taille des payloads VInt.

## Ce qui ne coûte RIEN

| Composant | Temps | Commentaire |
|---|---|---|
| PosMap construction | 3-19ms | `posmap_writer.add()` simple |
| PosMap serialize | inclus | Buffer linéaire |
| ByteMap construction | 0ms | 32 bytes OR par token |
| ByteMap serialize | 0ms | Copie linéaire |
| GapMap serialize | 0-2ms | Déjà optimisé |

**Conclusion : PosMap et ByteMap (ajoutés dans cette session) n'ont aucun impact mesurable
sur le temps d'indexation.** Le bottleneck est le SuffixFstBuilder qui existait avant.

## Merge cascade (LogMergePolicy)

Le problème des 90K docs qui prennent de plus en plus longtemps n'est pas le SFX en soi
mais la **merge policy** :

- `max_docs_before_merge = 10_000` (handle.rs ligne 46)
- `min_num_segments = 8` (défaut LogMergePolicy)
- Chaque commit crée un petit segment (~5K docs)
- Après 8 commits → merge de 8 segments en 1 gros (~40K docs)
- Le merge rebuild le SFX complet du segment fusionné → 1s+ pour les gros segments
- Puis les prochains petits segments s'accumulent et re-déclenchent un merge avec le gros

Pour le **bulk indexing**, la solution est soit :
- Augmenter `max_docs_before_merge` pendant le bulk
- Utiliser `NoMergePolicy` pendant le bulk puis merge final
- Batch les docs en moins de commits (1 commit à la fin)

Pour l'**indexation incrémentale** (cas normal en prod), les segments restent petits
et les merges sont rapides. Le problème est spécifique au bulk.

## Prochaines étapes

1. **Vérifier si diag_emit! est zero-cost** sans subscriber — si non, conditionner les clones
2. **Profiler en release** (pas debug) — les timings debug sont ~3-10x plus lents que release
3. **Essayer `MapBuilder::from_iter()`** au lieu d'inserts individuels
4. **Remplacer HashMap+HashSet sibling par Vec<Vec>** — quick win
5. **Bench le bulk avec NoMergePolicy** pour isoler le coût du merge vs le coût du build
