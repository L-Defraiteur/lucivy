# Rapport d'investigation — Session 5 (19-24 mai 2026)

## Objectif

Éliminer les faux positifs (FP) strict du ground truth SFX v3 sur 500 fichiers C++.

## État initial (début de session)

Ground truth : 7 pass, 8 fail sur 15 queries.
- Strict : 7/10 (function +1 FP, struct +7 FP, rag3db +11 FP)
- Relaxed : 0/5 (tous fail, beaucoup de FP)

## Root cause identifiée

### Le chain builder mélangeait des ordinals de consumed différents

Le `cross_token_chain_v3` dans `fst_walk.rs` collectait TOUS les ordinals de TOUS les sub_splits dans une même position de la chaîne, mais n'utilisait que le remainder du meilleur split. Résultat : des ordinals de splits courts (consumed=2) étaient placés à une position de chaîne qui supposait plus de bytes consommés (consumed=5).

**Exemple concret** : query "struct" (6 bytes), doc contenant "ListPrependListOfStrings"

1. Falling walk : marker entry `"\x01s"` → parent "pendLis" à sti=6, consumed=1, remainder="truct"
2. Sub-falling walk sur "truct" : collecte ALL sub_splits incluant :
   - consumed=5 (tokens contenant "truct") — ordinals de ces tokens
   - consumed=2 (marker `"\x01tr"`) — ordinal de "tOfStr" (chunk suivant dans le doc FP)
3. TOUS ces ordinals pushés à positions[1]. Remainder = "" (du best consumed=5).
4. Chain de 2 positions. Resolve : "pendLis" à pos P + "tOfStr" à pos P+1 → adjacence OK.
5. **FP** : bytes réels = "stOfStr" ≠ "struct"

### Pourquoi l'ordinal "tOfStr" est dans les sub_splits

Les **marker entries** créent des clés FST très courtes (1-2 bytes) partagées par des centaines de tokens. La clé `"\x01tr"` est le marker pour le chunk "tOfStr" à SI=4 (split_at = own_len - sti = 6-4 = 2). Mais cette même clé est partagée par tous les tokens dont le suffixe commence par "tr" avec split_at=2 — des centaines de tokens sans rapport.

Le diag builder a confirmé : jusqu'à **11157 parents** sous une seule clé FST, et **32251 clés multi-parent** avec ordinals distincts sur 183K clés.

## Fix appliqué

### Chain builder : filtrer par query_consumed

```rust
// Avant (bug) :
let mut unique_ords: Vec<u64> = sub_splits.iter()
    .map(|s| s.parent.raw_ordinal).collect();

// Après (fix) :
let best_consumed = sub_splits[0].query_consumed;
let mut unique_ords: Vec<u64> = sub_splits.iter()
    .filter(|s| s.query_consumed == best_consumed)
    .map(|s| s.parent.raw_ordinal).collect();
```

**Pourquoi ce n'est PAS une perte de données** : les ordinals des autres consumed ont des overlaps qui divergent de la query à la frontière du split. L'overlap dans l'index encode les 2 premiers bytes du token suivant. Si le falling walk a matché N bytes au-delà du split point (overlap_consumed = N), ça confirme que la query correspond à la frontière. Les ordinals avec consumed différent ont des frontières différentes → les bytes du doc ne correspondent pas → ce sont toujours des FP, jamais des TP.

### Extended ordinals (changement structurel, gardé)

Chaque texte étendu unique a son propre ordinal et ses propres postings. Plus de groupement par `text[..own_len]`. Implémenté dans `collector_v3.rs::into_data()` et `sfx_dag_v3.rs::merge_segments_v3()`.

Ce n'était pas la cause des FP (le chain builder bug était la vraie cause), mais c'est une meilleure séparation architecturale.

## Résultats

### Strict : 10/10 parfait ✓

| Query | Grep | V3 | Status | Avant |
|-------|------|----|--------|-------|
| function | 62 | 62 | **OK** | 63 (+1 FP) |
| return | 463 | 463 | **OK** | OK |
| struct | 71 | 71 | **OK** | 78 (+7 FP) |
| void | 18 | 18 | **OK** | OK |
| rag3db | 51 | 51 | **OK** | 62 (+11 FP) |
| include | 29 | 29 | **OK** | OK |
| uint64_t | 11 | 11 | **OK** | OK |
| std::unique_ptr | 8 | 8 | **OK** | OK |
| ku_dynamic_cast | 0 | 0 | **OK** | OK |
| TableFunction | 0 | 0 | **OK** | OK |

### Relaxed : 2/5 (3 FP restants)

| Query | Grep | V3 | Status |
|-------|------|----|--------|
| function relax | 62 | 64 | 2 FP |
| uint64_t relax | 23 | 64 | 41 FP, 1 FN |
| std::unique_ptr relax | 8 | 8 | **OK** |
| ku_dynamic_cast relax | 0 | 0 | **OK** |
| TableFunction relax | 5 | 6 | 1 FP |

### Lib tests : 1420 pass, 1 fail connu (diag_include_vs_inclusive)

## Analyse des FP relaxed restants

Les FP relaxed sont un problème DIFFÉRENT du bug strict. Exemples :

- **uint64_t relax** : `"UINT64) -> LIST"` — le moteur trouve "uint64" dans un mot, puis le "t" au début d'un autre mot séparé par des tokens pure-sep ") -> ". Le resolve relaxed accepte car les intermédiaires sont pure-sep.
- **function relax** : `"tringPropTest"` — même mécanisme de matching cross-mots via la partition 0x02 + relaxed adjacency.
- **TableFunction relax** : highlight [118..18530] = 18K bytes — chain qui traverse tout le fichier.

Root cause relaxed : la partition 0x02 (word-stripped) permet des splits cross-MOTS, et le resolve relaxed (ByteOrdered fallback quand posmap/bytemap absents) ne vérifie pas les tokens intermédiaires.

## Inquiétudes potentielles sur les vrais positifs

### 1. Filtrage par best_consumed — risque de FN ?

**Risque : faible mais non-nul.** Le filtre élimine les ordinals dont le `query_consumed` diffère du best. Si un vrai positif nécessite un chemin avec un consumed plus court (plus de positions dans la chaîne), il est perdu à cette position.

**Cas théorique** : query "struct" (6 bytes). Un doc a "...str|uct..." avec split à 3 bytes (consumed=3, remainder="uct"). Un autre split existe avec consumed=5 (le best). Le consumed=3 est filtré. Si le chemin consumed=5 ne résout pas dans ce doc (ordinal different), le match consumed=3 est perdu.

**Pourquoi c'est improbable** : le consumed=3 signifie que le walk a matché "str" dans le FST et détecté un split à own_len. Le best (consumed=5) a matché "struc" dans un AUTRE token (différent ordinal, différent overlap). Le token consumed=3 a overlap divergent au byte 4 (sinon consumed serait > 3). Si l'overlap diverge, le token suivant dans le doc NE COMMENCE PAS par les mêmes bytes → le match n'est pas valide.

**Où vérifier** : si on veut être 100% sûr, on pourrait comparer avant/après sur un corpus plus large (>500 fichiers) avec des queries variées. Chercher des cas où le ground truth grep trouve un match que v3 ne trouve pas (FN), et vérifier si c'est lié au filtre best_consumed.

### 2. Extended ordinals — risque de FN ?

**Risque : nul.** Les extended ordinals séparent les ordinals par texte étendu unique. Les `Vec<Vec<u64>>` alternatives collectent toujours tous les ordinals matchants à chaque position. Aucun ordinal n'est perdu par cette séparation. Au contraire, les postings sont plus précis (chaque ordinal ne contient que les docs avec ce texte exact).

### 3. Marker entries non-modifiées — risque résiduel ?

Les marker entries restent dans le FST et le falling walk les utilise toujours. Le filtre best_consumed empêche le mélange dans le chain builder, mais les markers causent quand même un grand nombre de split candidates (centaines par query). Ça n'impacte pas la correction (les faux sont filtrés), mais ça impacte la **performance** : beaucoup de candidates inutiles sont générés puis éliminés.

**Optimisation future** : valider les split candidates via termtexts (vérifier que le texte de l'ordinal correspond aux bytes de la query au sti). Ou restructurer les markers (per-ordinal markers, boundary table séparée).

## Prochaines étapes

1. **Fixer les FP relaxed** (partition 0x02) :
   - Le resolve relaxed doit passer posmap/bytemap au lieu du fallback ByteOrdered
   - Éventuellement : limiter les chains 0x02 aux matches intra-mot (pas cross-mots)
   - Investiguer le 1 FN uint64_t relaxed

2. **Fixer le test diag_include_vs_inclusive** (connu, pre-existing)

3. **Optimiser les marker entries** (performance, pas correction)

## Fichiers modifiés

| Fichier | Changement |
|---------|------------|
| `src/suffix_fst/briques/fst_walk.rs` | Chain builder : filtre sub_splits par best_consumed |
| `src/suffix_fst/collector_v3.rs` | Extended ordinals : 1:1 mapping intern → final |
| `src/suffix_fst/builder_v3.rs` | Diag conditionnel V3_DIAG_BUILD (multi-parent stats) |
| `src/indexer/sfx_dag_v3.rs` | Extended ordinals : BuildFstV3Node, merge, AssembleV3Node |
| `src/suffix_fst/briques/orchestrator.rs` | Debug trace amélioré (dedup matches) |
| `lucivy_core/tests/test_sfx_v3_ground_truth.rs` | Debug enrichi (tokenisation, bytes réels) |

## Commit

`8f041e4` — fix: extended ordinals + chain builder consumed filter — 19 strict FP eliminated
