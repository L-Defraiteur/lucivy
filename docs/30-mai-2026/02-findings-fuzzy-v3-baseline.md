# Findings fuzzy v3 — baseline et plan de fix

> 1 juin 2026 — branche `feature/dag-query-refactor`

## Baseline a 500 docs

| Query | Type | Grep | V3 | FN | FP |
|---|---|---|---|---|---|
| functin | fz1 | 62 | 80 | 2 | 20 |
| strcuture | fz1 | 0 | 2 | 0 | 2 |
| inclde | fz1 | 30 | 69 | 0 | 39 |
| retrun | fz1 | 1 | 232 | 0 | 231 |
| rag3db | fz1 | 51 | 52 | 0 | 1 |
| uint64 | fz1 | 243 | 211 | 40 | 8 |

**0/6 pass.** FP massifs + FN sur 2 queries.

## Pipeline actuel

```
QueryConfig(distance=1) → FuzzyQueryV3 → orchestrator::fuzzy_v3
    → composite::resolve_trigrams_v3
        → generate_trigrams
        → fst_candidates_v3 par trigram (selectivite)
        → resolve_single_v3 par trigram (PAS de chains, PAS de word)
        → group by doc, count distinct trigrams >= threshold
        → AUCUNE verification Levenshtein
```

## Finding 1 — Threshold trop bas

`composite.rs:314` :
```rust
let threshold = (ngrams.len() as i32 - n as i32 * distance as i32).max(1) as usize;
```

Pour "retrun" (6 chars), n=3, d=1 : trigrams = `ret`, `etr`, `tru`, `run` (4 trigrams).
threshold = max(4 - 3*1, 1) = max(1, 1) = **1**.

Un seul trigram suffit. "ret" matche dans tous les docs avec "return" → 232 FP.

**Le probleme** : le `.max(1)` est trop permissif. Le threshold devrait etre
au moins 2 pour les mots courts. Mais meme avec un threshold plus haut, sans
verification Levenshtein finale, il y aura toujours des FP.

## Finding 2 — Pas de verification Levenshtein

Le pigeonhole est un filtre de CANDIDATS, pas une preuve. Apres avoir trouve
les docs candidats, il faut verifier que le texte contient effectivement une
sous-chaine a distance <= d de la query. Actuellement cette verification
n'existe pas.

**Fix** : ajouter une passe de verification Levenshtein sur le texte reel
des docs candidats. Comme on a deja les highlights (byte_from/byte_to),
on peut extraire le texte autour et verifier.

## Finding 3 — Pas de cross-token chains

`composite.rs:334` :
```rust
let matches = resolve::resolve_single_v3(&cands, resolver, doc_filter.as_ref());
```

Uniquement `resolve_single_v3`. Pas de `resolve_chains_v3`, pas de
`resolve_word_chains_v3`. Si un trigram tombe a cheval sur deux tokens,
il est rate → FN.

**Fix** : utiliser `contains_v3(ctx, trigram, false, false, strict_sep)` au
lieu de `fst_candidates + resolve_single`. `contains_v3` fait tout :
single + chunk chains + word chains. C'est exactement la brique qu'on a
validee a 15/15 sur le ground truth.

## Finding 4 — Skip word-stripped (partition 0x02)

`resolve_single_v3` skip les candidats 0x02. Si un trigram n'a de match
qu'en word-stripped (mot long dont le trigram est dans la partie concatenee
sans separateurs), il est perdu → FN.

Notre `resolve_single_word_v3` n'est pas utilise ici.

**Fix** : couvert par Finding 3 — `contains_v3` utilise le DAG complet
qui inclut `resolve_single_word`.

## Finding 5 — doc_filter progressif mal construit

`composite.rs:347-354` : le doc filter est construit en accumulant TOUS les
docs de chaque trigram resolu. Ca ne filtre rien — il grandit a chaque
iteration au lieu de se restreindre.

Pour un vrai filtre pigeonhole, il faudrait :
- Resoudre les `threshold` trigrams les plus selectifs SANS filtre
- Faire l'intersection de leurs doc sets
- Resoudre les trigrams restants AVEC ce filtre

Ou plus simplement : resoudre tous les trigrams sans filtre, puis compter
les hits par doc a la fin (ce que fait deja la Phase C). Le doc_filter
progressif n'apporte rien dans l'implementation actuelle.

## Plan de fix

### Approche : `contains_v3` par trigram

Remplacer le coeur de `resolve_trigrams_v3` par :

```
1. generate_trigrams(query, distance) → ngrams
2. Pour chaque ngram :
     matches = contains_v3(ctx, ngram, false, false, strict_sep)
     → single + chunk_chains + word_chains (complet)
3. Group by doc_id, count distinct trigram indices >= threshold
4. Pour chaque doc candidat : verification Levenshtein
5. Threshold = max(ngrams.len() - n * d, 2)
```

### Prerequis

`contains_v3` prend un `BriquesContext` — il faut passer le ctx complet
a `resolve_trigrams_v3`, pas juste `reader + resolver`. Ca veut dire changer
la signature pour prendre `&BriquesContext` au lieu des champs individuels.

### Verification Levenshtein

Deux options :
- **Option A** : extraire le texte du doc via docstore, sliding window
  Levenshtein. Correct mais lent (I/O docstore).
- **Option B** : utiliser les byte_from/byte_to des matches de `contains_v3`
  pour reconstruire le texte matche. Verifier que la concatenation des
  trigrams adjacents forme bien un mot a distance <= d. Plus rapide, pas
  d'I/O supplementaire.

Option B est preferable si on peut reconstruire le contexte depuis les
matches. Sinon Option A en fallback.

### Impact attendu

- **FP** : elimines par la verification Levenshtein + threshold >= 2
- **FN functin (2)** : probablement fixes par les cross-token chains
- **FN uint64 (40)** : probablement fixes par le word pipeline (partition 0x02)

### Fichiers a modifier

| Fichier | Changement |
|---|---|
| `briques/composite.rs:resolve_trigrams_v3` | Signature → `&BriquesContext`, utiliser `contains_v3` |
| `briques/orchestrator.rs:fuzzy_v3` | Passer ctx complet a `resolve_trigrams_v3` |
