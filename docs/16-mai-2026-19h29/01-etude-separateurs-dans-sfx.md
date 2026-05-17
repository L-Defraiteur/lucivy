# Étude : séparateurs dans le Suffix FST

**Date** : 16 mai 2026
**Objectif** : évaluer l'impact d'inclure les séparateurs (_, ::, ., -, whitespace) dans le FST de suffixes plutôt que de les traiter comme des coupures de tokens.

## Problème actuel

Aujourd'hui les séparateurs sont **invisibles** pour le SFX engine :

1. Le tokenizer strip tout caractère non-alphanumérique
2. `"pthread_mutex_lock"` → tokens `["pthread", "mutex", "lock"]`
3. Chaque token est décomposé en suffixes indépendamment
4. Les séparateurs sont stockés comme metadata dans GapMap/SiblingTable
5. Pour reconstruire une correspondance cross-token, il faut traverser la sibling table (DFS)

### Impact sur le fuzzy

Quand on cherche `"mutex_lock"` fuzzy d=1 :

```
1. concat_query("mutex_lock") → "mutexlock"  (underscore stripped)
2. 8 trigrams générés sur la string concaténée
3. 3 trigrams tombent sur la frontière → non résolus
4. 16 FST walks (8 trigrams × 2 : single-token + cross-token)
5. Cross-token falling walk = DFS sur sibling table = BOTTLENECK
```

Le coût de Phase A (estimation sélectivité) est **100-500ms** principalement à cause du cross-token falling walk qui doit parcourir les chaînes de siblings pour chaque trigram.

## Proposition : Inclure les séparateurs dans le FST

### Idée A — Tokens composés avec séparateurs

Au lieu de couper sur `_`, `::`, `.`, `-`, on indexe le token complet :

```
"pthread_mutex_lock" → un seul token, 18 bytes
Suffixes :
  SI=0:  pthread_mutex_lock
  SI=1:  thread_mutex_lock
  ...
  SI=7:  _mutex_lock
  SI=8:  mutex_lock
  SI=9:  utex_lock
  ...
  SI=14: _lock
  SI=15: lock
  SI=16: ock
  SI=17: ck
  SI=18: k
```

**Avantage** : `"mutex_lock"` fuzzy d=1 = un seul FST walk sur la partition SI>0. L'underscore est là, le DFA Levenshtein le traverse naturellement. Pas besoin de sibling table, pas de cross-token falling walk.

**Inconvénient** : explosion de l'index. Un token de 18 bytes produit 18 suffixes au lieu de 7+5+4=16 suffixes pour 3 tokens. Et les tokens longs (ex: paths `/usr/lib/x86_64-linux-gnu/libstdc++.so.6`) produiraient des centaines de suffixes.

### Idée B — Double indexation (token + composé)

Indexer les tokens individuels (comme aujourd'hui) ET les bi-tokens avec séparateurs :

```
Token individuel : "mutex"  (SI=0..5)
Token individuel : "lock"   (SI=0..4)
Bi-token :         "mutex_lock" (SI=0..10)
```

**Avantage** : le falling walk exact (single-token) reste rapide, et le fuzzy bénéficie du bi-token.

**Inconvénient** : taille d'index ×2 environ. Chaque paire de tokens adjacents génère un bi-token supplémentaire avec tous ses suffixes.

### Idée C — Partition SI négative (séparateur-aware)

Garder l'architecture actuelle (tokens séparés) mais ajouter une 3ème partition dans le FST :

```
0x00 — SI=0 (début de token)
0x01 — SI>0 (substring dans un token)
0x02 — SI cross-token (suffixe qui traverse la frontière)
```

Pour `"mutex_lock"` :
```
Partition 0x02 :
  SI_cross=0: mutex_lock    (commence au début de "mutex", traverse le "_", finit à la fin de "lock")
  SI_cross=1: utex_lock
  SI_cross=2: tex_lock
  SI_cross=3: ex_lock
  SI_cross=4: x_lock
  SI_cross=5: _lock          (commence au séparateur)
```

Le séparateur byte `_` est littéralement dans le FST. Les valeurs encodent `(ordinal_premier_token, si, longueur_totale_jusqu'à_fin_dernier_token)`.

**Avantage** : pas besoin de sibling table pour ces cas. Le fuzzy DFA marche directement. Pas de cross-token falling walk.

**Inconvénient** : on ne fait que les bi-tokens (pas tri-tokens). Et l'index grossit (N-1 chaînes cross-token par document pour N tokens).

### Idée D — Séparateur comme byte spécial dans le FST existant

Au lieu d'une partition séparée, encoder le séparateur comme un byte spécial dans les suffixes SI>0 :

```
Partition 0x01 (SI>0, comme aujourd'hui) :
  ... tous les suffixes normaux ...
  ... PLUS les suffixes cross-token avec 0xFF comme byte séparateur ...

Exemple :
  "mutex" token ord=5, "lock" token ord=6, gap="_"
  Entrée ajoutée : key = [0x01, 'm', 'u', 't', 'e', 'x', 0xFF, 'l', 'o', 'c', 'k']
                    val = encode(ord=5, si=0, cross_len=10, next_ord=6)
```

Le byte 0xFF est un sentinel qui signifie "séparateur ici". Le DFA Levenshtein peut le matcher ou pas selon la distance.

**Avantage** : un seul FST, pas de partition supplémentaire. Le falling walk marche naturellement — le DFA voit le 0xFF comme un byte normal.

**Inconvénient** : le 0xFF est un byte réel possible dans du texte UTF-8 (mais très rare en pratique). On pourrait choisir 0x00 comme sentinel mais il est déjà pris par la partition SI=0.

### Idée E — "Sep-Collapsed" suffixes

Ajouter dans le FST des suffixes qui commencent au séparateur, mais en remplaçant le séparateur réel par un marqueur universel :

```
Partition 0x01 :
  ... suffixes normaux ...
  ... PLUS :
  key = [0x01, SEP, 'l', 'o', 'c', 'k']
  val = encode(ord=5, si=5, token_len=5, is_cross=true, next_ord=6)
```

Où `SEP = 0x1F` (ASCII Unit Separator, jamais utilisé dans du texte réel).

Quand le fuzzy DFA rencontre le SEP, il peut :
- Le matcher comme 1 edit (substitution du vrai séparateur) → coûte 1 distance
- Le matcher exactement si la query contient aussi un séparateur au même endroit

**Avantage** : compact, un seul byte par cross-token link. Le DFA gère naturellement.

**Inconvénient** : un "_" dans la query coûte 1 edit distance si le vrai séparateur est aussi "_" — c'est un faux positif de coût. Il faudrait un traitement spécial dans le DFA.

## Analyse comparative

| Critère | Actuel | A (composé) | B (double) | C (3ème partition) | D (byte 0xFF) | E (SEP collapsed) |
|---------|--------|-------------|------------|-------------------|---------------|-------------------|
| **Fuzzy cross-token** | Sibling DFS (~200ms) | 1 FST walk (~0.1ms) | 1 FST walk | 1 FST walk | 1 FST walk | 1 FST walk |
| **Index size** | baseline | ×1.5-2 | ×1.8-2 | ×1.3-1.5 | ×1.3-1.5 | ×1.2-1.3 |
| **Single-token search** | rapide | rapide | rapide | rapide | rapide | rapide |
| **Complexity impl** | actuel | simple | moyen | moyen | faible | moyen |
| **BM25 impact** | aucun | tokens plus longs → TF/IDF changent | aucun | aucun | aucun | aucun |
| **Tokens longs** | pas de problème | explosion suffixes | explosion bi-tokens | N-1 chains | N-1 chains | N-1 chains |
| **Regex cross-token** | séparée (SepMap) | natif | natif | natif | natif | natif |

## Recommandation

**Idée D (byte 0xFF dans le FST)** semble le meilleur compromis :

1. **Implémentation la plus simple** : 
   - Au build, pour chaque paire de tokens adjacents, ajouter des suffixes cross-token avec 0xFF comme séparateur
   - Le `encode_single_parent` reçoit un flag `is_cross`
   - Au search, le fuzzy DFA traite 0xFF comme un byte normal — le Levenshtein automaton le gère naturellement

2. **Index size raisonnable** :
   - Pour chaque paire (A, B) on ajoute max `len(A)` suffixes (ceux qui partent de A et traversent vers B)
   - En pratique la plupart des tokens sont courts (5-10 bytes), donc ~5-10 entrées supplémentaires par paire

3. **Speedup massif pour fuzzy** :
   - Plus besoin de cross-token falling walk (DFS sur sibling table)
   - Plus besoin de trigram threshold adjustment pour boundary trigrams
   - 1 FST walk au lieu de 16+

4. **Backward compatible** :
   - Les queries exact single-token marchent toujours (partition 0x00/0x01 inchangée)
   - Les nouvelles entrées sont dans la même partition 0x01, juste avec 0xFF dedans
   - Le falling_walk existant ignore naturellement les entrées avec 0xFF (il ne match pas)

### Variante recommandée de D

Plutôt que 0xFF (qui pourrait théoriquement apparaître en UTF-8 invalide), utiliser **0x02** comme prefix byte pour une 3ème partition dédiée :

```
0x00 — SI=0 (début de token)
0x01 — SI>0 (substring dans un token)  
0x02 — cross-token (suffixe qui traverse un séparateur)
```

Les entrées cross-token contiennent le séparateur réel comme byte :
```
key = [0x02, 'm', 'u', 't', 'e', 'x', '_', 'l', 'o', 'c', 'k']
val = encode_cross(first_ord=5, si=0, total_len=10, gap_byte='_')
```

C'est **Idée C** en fait, mais avec le séparateur réel plutôt qu'un sentinel.

**Avantages supplémentaires** :
- Le fuzzy DFA voit `"mutex_lock"` comme une string continue — le `_` dans la query matche le `_` dans le FST
- Distance 0 = match exact y compris le séparateur
- Distance 1 = tolère 1 edit, que ce soit dans "mutex", dans "lock", OU dans le séparateur
- Regex `"mutex.*lock"` marche directement — le `.` matche le `_`

## Impact estimé sur les performances fuzzy

### Avant (actuel)
```
"mutex_lock" fuzzy d=1 :
  concat = "mutexlock"
  8 trigrams, 16 FST walks
  3 boundary trigrams (non résolus)
  Cross-token DFS : ~100-500ms
  Total Phase A : ~200-600ms
```

### Après (partition 0x02)
```
"mutex_lock" fuzzy d=1 :
  Query inchangée (pas de concat)
  1 FST walk sur partition 0x02 avec Levenshtein DFA
  Le DFA traverse les bytes y compris '_'
  Candidates résolues via les postings du suffix FST
  Total : ~1-5ms
```

**Speedup estimé : 50-100x pour les queries fuzzy cross-token.**

## Prochaines étapes

1. **Prototype** : modifier le SfxFileWriter pour générer la partition 0x02 avec les cross-token entries
2. **Mesurer** : taille d'index avec et sans la 3ème partition sur le dataset linux 90K docs
3. **Benchmark** : comparer le temps de fuzzy cross-token search avant/après
4. **Décider** : si le ratio taille/perf est acceptable, intégrer

## Questions ouvertes

- **Tri-tokens** : faut-il aussi indexer A+B+C ? (ex: `"std::collections::HashMap"` → 3 tokens). Si oui, l'index explose. Si non, les recherches fuzzy sur 3+ tokens restent lentes.
- **CamelCase** : les tokens CamelCase-splittés ont gap=0 (contiguous). Faut-il les traiter comme cross-token aussi ? Probablement oui.
- **Longueur max** : limiter les cross-token entries à max 2 tokens (bi-grams) ? Ou autoriser 3 ?
- **Postings** : les entrées cross-token ont-elles leurs propres postings, ou pointent-elles vers les postings des tokens individuels ?
