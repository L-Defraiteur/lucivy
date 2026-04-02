# 11 — Tentatives ratées : Plan A step 2 + Plan B

Date : 2 avril 2026

## Plan A step 2 : cache ord→text dans le DFA walk

### Tentative

Pré-construire 3 HashMaps depuis `all_matches` :
- `bf_to_pos: (doc_id, byte_from) → position` (déjà en place, fonctionne)
- `ord_text_cache: ordinal → String` (texte du token)
- `doc_pos_to_ord: (doc_id, position) → ordinal`

Puis dans le concat building, utiliser les caches au lieu de
`pm.ordinal_at()` + `ord_to_term()`.

### Résultat

**Plus lent.** Le TermTextsReader est déjà O(1) (offset table direct),
et le HashMap ajoute un coût de hashing qui dépasse le gain.

De plus, `doc_pos_to_ord` ne couvre que les positions des matches trigrams,
pas les positions voisines dans la fenêtre du concat. La majorité des
lookups sont des cache miss → double travail (HashMap lookup + fallback posmap).

### Conclusion

Le cache ord→text ne vaut le coup que combiné avec le Plan B (group by doc)
où le concat est construit une seule fois par doc et amorti sur N candidats.
En isolation, le overhead > gain.

L'ordinal propagé dans LiteralMatch reste utile pour d'autres usages futurs
(validate_path, sibling DFS, etc.).

## Plan B : grouper candidats par doc_id

### Tentative

Après `intersect_trigrams_with_threshold` :
1. Séparer proven (traités en premier, gratuits) et unproven
2. Grouper unproven par doc_id
3. Pour chaque doc : trouver le range de positions le plus large,
   construire UN concat, valider tous les candidats dessus

### Résultat

**"rak3weaver" d=1 : 50ms → 10 000ms.** Catastrophe.

### Pourquoi

Quand un doc a beaucoup de candidats dispersés (positions 10, 500, 2000...),
le `global_start_pos` et `global_end_pos` couvrent presque tout le doc.
Le concat résultant est énorme (des milliers de tokens).

Pour "rak3weaver" d=1, il y a 1143 candidats avec 0 proven → tous unproven.
Un seul doc peut avoir 50+ candidats à des positions très éloignées.
Le concat partagé couvre tout → le DFA doit être feedé sur un concat de
milliers de bytes pour chaque candidat.

Résultat : le "gain" de partager le concat est annulé par le coût d'un
concat beaucoup plus gros que nécessaire.

### Piste pour fix

Ne PAS fusionner les ranges en un seul global. Au lieu de ça :
- Grouper les candidats par (doc_id, region) où "region" = les candidats
  dont les fp sont proches (à ± query_len positions)
- Construire un concat par region, pas par doc
- Les candidats dans la même region partagent le concat

Ou plus simplement : garder un concat par candidat, mais **cacher le résultat**
quand deux candidats consécutifs (triés par doc_id + position) ont des
ranges qui se chevauchent → réutiliser le même concat.

### Autre piste : content_byte_starts aussi redondant

Le `content_byte_starts` est recalculé pour chaque candidat même quand
le concat est partagé. Il dépend de `fp` et `first_si` qui changent
par candidat. Mais les gap reads sont les mêmes → cacher les gaps dans
token_spans (Plan C) réduirait ce coût.

## État final

Les deux tentatives sont revertées. Le code est dans l'état du commit
`22c3610` (HashMap bf_to_pos + ordinal propagé, pas de cache ni groupement).

Timings actuels (4307 docs, release) :
- "rag3weaver" d=1 : 35ms
- "rak3weaver" d=1 : 50ms  
- "rag3db" d=1 : 74ms

C'est acceptable pour l'instant. Les Plans C/D/E (gaps cache, precompute,
span index) sont triviaux et peuvent être faits indépendamment.
Le Plan B nécessite une approche par "regions" plutôt que par doc entier.
