# 06 — Design : fuzzy multi-segment (multi-token avec séparateurs)

Date : 31 mars 2026

## Problème

La query "use rak3weaver::config" d=1 ne fonctionne pas car :
1. Les trigrams sont générés sur la query entière (skip les séparateurs)
2. Le span check entre trigrams de segments différents ("use" vs "3we")
   échoue car la distance en bytes dans le doc ≠ dans la query
3. L'ancien split par segment ne vérifiait pas l'adjacence des positions

## Solution

Traiter chaque segment alphanumeric comme une sous-query fuzzy indépendante,
puis vérifier l'adjacence des positions entre segments.

### Étape 1 : Split la query en segments

```
"use rak3weaver::config" → ["use", "rak3weaver", "config"]
```

Split sur `!c.is_alphanumeric()`.

### Étape 2 : Recherche fuzzy par segment

Pour chaque segment, lancer `fuzzy_contains_via_trigram` indépendamment.
Chaque segment retourne des `LiteralMatch { doc_id, position, byte_from, byte_to }`.

### Étape 3 : Vérifier adjacence inter-segment

Pour chaque doc qui contient TOUS les segments, vérifier :
- Le segment N+1 a une position = position du dernier token du segment N + 1
  (ou + nombre de tokens du gap)
- Le byte_from du segment N+1 > byte_to du segment N
- Il y a un gap (séparateur) entre les deux — pas d'autres tokens

C'est similaire à `intersect_literals_ordered` mais avec des fuzzy matches
au lieu de literals.

### Schéma

```
Query: "use rak3weaver::config"
       [seg0] [   seg1   ] [seg2 ]

Doc:   "use rag3weaver::config::CatalogConfig"
       [use] [rag3weaver] [config] [catalog] [config]
        pos0    pos1-2      pos3     pos4      pos5

Segment 0 "use" d=1 → matches at pos0
Segment 1 "rak3weaver" d=1 → matches at pos1 (cross-token rag3+weaver)
Segment 2 "config" d=1 → matches at pos3, pos5

Adjacence check:
  pos0 → pos1: need gap between them ✓ (espace)
  pos1 → pos3: need gap between them... pos1 is "rag3" (pos=1),
    "weaver" is pos=2, "config" is pos=3 → pos2+1 = pos3 ✓

Result: match ✓
```

### Ce qu'on veut PAS

- "use" au début du fichier + "rak3weaver" 200 lignes plus bas → ✗
- "use" dans un commentaire + "config" dans le code → ✗

### Ce qu'on VEUT

- Tolérance de position : les segments doivent être "proches" mais pas
  forcément immédiatement adjacents. Un gap de 1-2 tokens entre segments
  est acceptable (pour les séparateurs multi-char comme "::")
- Le fuzzy est sur chaque segment, pas entre les segments
