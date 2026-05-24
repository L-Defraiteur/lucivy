# État fin session 6 — 24 mai 2026

## Résultats

| Échelle | Score | Détail |
|---------|-------|--------|
| 500 docs | **15/15** | 0 FP, 0 FN |
| 5000 docs | **9/15** | 4 FN scale-dependent, 2 FP grep-logic |

### Détail 5000 docs

| Query | Mode | Grep | V3 | Problème | Verdict |
|-------|------|------|-----|----------|---------|
| function | strict | 1478 | 1477 | 1 FN | SCALE-DEPENDENT |
| function | relax | 1478 | 1478 | 1 FP | grep logic |
| rag3db | strict | 3084 | 3083 | 1 FN | SCALE-DEPENDENT |
| uint64_t | relax | 722 | 723 | 1 FP | grep logic |
| std::unique_ptr | relax | 1000 | 971 | 29 FN | SCALE-DEPENDENT |
| TableFunction | relax | 224 | 223 | 1 FN | SCALE-DEPENDENT |

## Ce qui a été mis en place

### Index-time

1. **WordSfxPost cross-join fix** : utilisation directe des intern_ords
   au lieu de content_key_to_interns. Ajout de num_chunks pour distance
   exacte.

2. **Sibling table v3** : ordinal → [(next_ordinal, content_len)].
   Chunk siblings + word siblings construits dans add_value().
   Fichier "sibling_v3" dans le registry.

### Query-time

3. **BriquesContext** : struct unique remplaçant 10+ params Option.
   Champs : reader, resolver, filter_docs, debug, posmap, bytemap,
   word_sfxpost, sibling_v3, termtexts. require_*() panic si manquant.

4. **sibling_chain_dfs** : DFS via sibling links avec comparaison
   content (pas full text). Utilise content_len stocké dans gap_len
   du SiblingEntry.

5. **splits_from_fst_candidates** : rattrape les premiers splits que
   le falling walk rate (query épuisé dans l'overlap).

6. **V3_DIAG mode** : env var V3_DIAG=1 → export fails en JSON +
   re-test FN docs en isolation (SCALE-DEPENDENT vs PER-DOC BUG) +
   V3_DEBUG_QUERY pour traces [DBG] dans les briques.

7. **Ground truth grep word-adjacency** : matche par mots adjacents
   au lieu de concaténation linéaire globale.

### Approches testées et abandonnées

- Content-only keys : DFS look-ahead explose
- Split table (word_splits.rs) : remplacée par sibling table
- DFS look-ahead : explose à 500+ docs
- Retrait du break dans resolve : n'a pas fixé les 29 FN

## Branche

`feature/sibling-table-v3` — 10 commits depuis feature/sfx-v3-overlap-tokenizer.

## Ce qui reste à investiguer

### Les 29 FN de std::unique_ptr relax (SCALE-DEPENDENT)

Le diag montre que les splits ET les sibling chains sont trouvés
(57-104 chains par segment). Le problème est en aval — soit dans
le resolve, soit dans le word_sfxpost, soit dans l'intermediates check.

Le break dans resolve a été testé et n'est PAS la cause. Mais il reste
un risque théorique (deux ordinals du même mot avec des last_pos
différents → le break prend le mauvais).

Hypothèses restantes :
- word_sfxpost entries corrompues pour certains ordinals à grande échelle
- intermediates_are_pure_sep échoue car des chunks content s'intercalent
- Le resolve itère les entries dans un ordre qui ne couvre pas tous les docs

### Les 2 FP

- function relax : v3 trouve un doc que le grep word-adjacency ne trouve pas.
  Probablement un match cross-word valide que le grep rate.
- uint64_t relax : "castUint64" → v3 matche mais grep non.
  Probablement le même type de divergence grep.

### Le break dans resolve

Non confirmé comme cause des 29 FN, mais architecturalement fragile.
Devrait être remplacé par un collect-all + dedup en fin de step.

## Prochaine session

1. Implémenter le QueryTrace graph (doc 10) pour investiguer les 29 FN
2. Analyser un FN spécifique en détail via le trace
3. Fixer la cause racine
4. Traiter les 2 FP (ajuster le grep ou le pipeline)
5. Objectif : 15/15 à 5000 docs
