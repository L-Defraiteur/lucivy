# Analyse : x11b — query stripped cross-word traversant des pure-sep tokens

**Date** : 17 mai 2026  
**Test** : `x11b_stripped_traverse_pure_sep`

---

## Le cas

```
Texte : "internationalization________initialization"

Tokens :
  TI=0 "interna"   content=7, sep=0          → extended "internati"
  TI=1 "tionali"   content=7, sep=0          → extended "tionaliza"
  TI=2 "zation_"   content=6, sep=1          → extended "zation___", stripped "zationin" (content_overlap)
  TI=3 "_______"   content=0, sep=7          → extended "_______in"
  TI=4 "initial"   content=7, sep=0          → extended "initializ"
  TI=5 "ization"   content=7, sep=0          → extended "ization"

Query : "nationalizationinit" strict_sep=false
Stripped : "nationalizationinit" (déjà sans seps)
```

## Pourquoi le chain échoue

Le falling walk sur "nationalizationinit" doit enchaîner :

```
1. "na" → TI=0 "internati" STI=5, split à 2 bytes (own_len 7 - sti 5 = 2)
   Remainder : "tionalizationinit" (17 bytes)

2. "tionali" → TI=1 "tionaliza" STI=0, split à 7 bytes
   Remainder : "zationinit" (10 bytes)

3. "zationin" → TI=2 stripped "zationin" STI=0, split à content_len 6
   Content_overlap validé "in" (2 bytes)
   Remainder : "it" (2 bytes)

4. "it" → cherche fst_candidates("it") → trouvé dans TI=4 "initializ" STI=2
   → Chain terminée : [ord_internati, ord_tionaliza, ord_zationin_stripped, ord_initializ]
```

Adjacence :
- TI=0 → TI=1 : pos 0+1=1 ✓
- TI=1 → TI=2 : pos 1+1=2 ✓  
- TI=2 → TI=4 : pos 2+1=3 ≠ 4 ✗ (TI=3 pure-sep entre les deux)

Le chain ÉCHOUE à la vérification d'adjacence car TI=3 est un pure-sep token intermédiaire.

## Le vrai problème

Ce n'est PAS un problème d'index (les entrées stripped sont correctes), c'est un problème de **résolution d'adjacence** : `resolve_chains_v3` exige pos+1 strict, mais il y a un gap de 1 token (TI=3 pure-sep) entre TI=2 et TI=4.

## Solutions possibles

### Option A — Resolve relaxé avec ByteMap (en cours)

`resolve_chains_v3_relaxed` avec PosMap + ByteMap pour vérifier que les tokens intermédiaires sont pure non-alphanum. **Problème** : les tests d'intégration n'ont pas de PosMap/ByteMap (pas générés dans le test harness). Faut les générer.

### Option B — Générer PosMap + ByteMap dans le test harness

Ajouter la construction du PosMap et du ByteMap dans le helper `build()` des tests d'intégration. Puis passer le tout à `find_literal_v3_full` au lieu de `find_literal_v3`.

### Option C — Indexer des suffixes word-level dans la partition stripped

Au lieu d'indexer les suffixes de chaque CHUNK dans 0x02, indexer les suffixes du MOT ENTIER :
- Mot "internationalization" (20 bytes) → 20 suffixes word-level dans 0x02
- + content_overlap du prochain mot ("in" de "initialization")
- Le suffix "nationalizationin" serait directement dans le FST

**Avantage** : pas besoin de chain multi-hop pour traverser les chunks internes d'un même mot. La query "nationalizationinit" matche en 2 hops (word 1 suffix → word 2 prefix) au lieu de 4.

**Inconvénient** : plus d'entrées dans le FST. Pour un mot de W bytes, on ajoute W suffixes (au lieu de C ≤ MAX_TOKEN per chunk). Mais le FST partage les préfixes.

### Option D — Hybride : word-level stripped + chunk-level normal

La partition 0x02 indexe les suffixes du **mot entier** (concaténation des content bytes de tous les chunks du mot) + content_overlap. Les partitions 0x00/0x01 restent chunk-level (avec seps, pour strict_sep=true).

C'est le meilleur des deux mondes :
- strict_sep=true : chunk-level, byte par byte, exact match des seps ✓
- strict_sep=false : word-level, pas besoin de chaîner les chunks internes ✓

Le chain cross-WORD est toujours nécessaire (mot 1 → mot 2), mais il n'y a plus besoin de chaîner les chunks internes du même mot. Et le gap pure-sep entre les mots est géré par le resolve relaxé (Option A).

## Recommandation

**Option D** est la plus propre. Ça simplifie le query (moins de hops dans la chain) et résout le problème de fond (les chunks internes d'un mot sont transparents dans la partition stripped).

Le coût en index est modéré : pour un mot de W bytes chunké en N chunks de ~8 bytes, on passe de N×8 = W suffixes chunk-level à W suffixes word-level. C'est le MÊME nombre ! Juste organisé différemment (un long mot au lieu de N courts).
