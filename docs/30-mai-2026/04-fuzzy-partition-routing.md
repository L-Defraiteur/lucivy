# Fuzzy — routage par partition selon strict_separators

> 1 juin 2026

## Constat

Le fuzzy `strict_sep=false` strippe les separateurs de la query avant
d'extraire les trigrams. Les trigrams sont donc **pure content** :
"functin" → "fun", "unc", "nct", "cti", "tin".

Or les chunks (partitions 0x00/0x01) ont un overlap de 2 bytes depuis
le **chunk suivant**, qui inclut souvent un separateur :
- "mutex" → extended "mutex_l" → overlap "_l" (sep + content)
- trigram "x_l" existe dans le FST mais ne matche jamais une query
  content-only comme "mutexlck"

Les word-stripped (partition 0x02) ont un **content overlap** : 2 bytes
depuis le prochain **mot** (pas chunk), content seulement :
- "mutex" → word-stripped "mutexlo" → overlap "lo" (content du mot "lock")
- trigram "xlo" matche "mutexlck" en fuzzy

## Regle

| Mode | Partition a utiliser | Overlap utile |
|---|---|---|
| `strict_sep=true` | 0x00/0x01 (chunks) | chunk overlap (content + sep) |
| `strict_sep=false` | **0x02 (word-stripped)** | content overlap |

## Impact sur le pipeline fuzzy

### Avant (actuel)
```
resolve_all_trigrams:
  fst_candidates_v3 → toutes partitions
  resolve_single_v3 (chunks 0x00/0x01) + resolve_single_word_v3 (ws 0x02)
  → melange chunk + word-stripped hits
```

### Apres
```
resolve_all_trigrams:
  if strict_sep:
    fst_candidates_v3 strict → partitions 0x00/0x01
    resolve_single_v3 (chunks)
  else:
    fst_candidates_v3 relax → partition 0x02
    resolve_single_word_v3 (word-stripped)
```

## Benefices attendus

1. **Moins de FP** : les hits chunk avec sep-overlap ne polluent plus
   les chaines en mode relax
2. **Plus rapide** : moins de hits a chainer (un seul type de resolve)
3. **Plus correct** : les byte_from/byte_to viennent du meme espace
   (word-level) → adjacence coherente dans build_trigram_chains
4. **Plus simple** : pas de melange de coordonnees chunk/word dans les hits

## Structures index deja disponibles

Le contexte fuzzy charge deja :
- `.word_sfxpost` (line 82 fuzzy_query_v3.rs)
- `.posmap` (line 80)
- `.bytemap` (line 81)

Tout est pret — c'est juste le routage dans `resolve_all_trigrams` qui
doit conditionner sur `strict_separators`.

## Consideration pour l'adjacence

En mode word-stripped, les byte_from/byte_to sont des coordonnees
**debut de mot / fin de mot** (pas debut de chunk). L'adjacence dans
`build_trigram_chains` compare des byte gaps. Deux trigrams dans le
meme mot ont des byte_from contigus. Deux trigrams cross-word ont un
gap qui inclut le separateur — ce gap est correct car le query stripped
n'a pas de sep non plus.

Pas de changement necessaire dans `build_trigram_chains` — les byte
coordonnees word-level sont coherentes entre elles.
