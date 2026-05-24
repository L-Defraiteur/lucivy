# Design : Content-only keys pour le word pipeline — Session 6

## Problème

Le falling walk détecte les splits aux **noeuds finaux** du FST. Mais la
finalité d'un noeud est un artefact de la structure du FST, pas de la
sémantique du mot. Quand l'index grandit (plus de docs → plus de clés),
des noeuds qui étaient finaux deviennent non-finaux car le FST fusionne
les branches.

### Exemple concret

Mot "uint64" suivi de "to" → clé FST `\x02uint64to` (8 bytes après partition).

Query "uint64t" (7 bytes, strippé de "uint64_t").

```
Walk:  u → i → n → t → 6 → 4 → t     ← query épuisée (7 bytes)
                               │
                               o       ← noeud final ici (byte 8)
```

Avec 5 docs : il existe une clé `\x02uint64` (mot en fin de texte, sans
overlap) → noeud final au byte 6 → split détecté.

Avec 25+ docs : toutes les occurrences de "uint64" ont un overlap →
pas de clé `\x02uint64` → noeud au byte 6 n'est PAS final → split perdu.

### Données

- Ground truth 500 docs : 3 splits avec 5 docs, **1 split** avec 25 docs
- Falling walk perd les splits pour "uint64t" → 5 FN sur uint64_t relaxed
- Score passe de 37 matches à 17 matches juste en ajoutant 20 docs

## Solution : Content-only keys

Pour chaque word-stripped entry, ajouter une **clé sans overlap** dans le
FST. Cette clé est finale exactement au `content_len` boundary → le
falling walk trouve toujours le split, indépendamment du FST.

### Avant

```
\x02uint64to     (final au byte 8 — inaccessible si query < 8 bytes)
```

### Après

```
\x02uint64       (final au byte 6 — toujours accessible)
\x02uint64to     (inchangé, pour les single-token matches via fst_candidates)
```

### Implémentation

**Fichier** : `builder_v3.rs` — `add_word_stripped()`

Actuellement, `add_word_stripped` ajoute :
- Clé complète : `partition + word_content + content_overlap` (avec overlap)
- Suffixes SI>0 de la clé complète

Changement : ajouter AUSSI la clé content-only :
- `partition + word_content` (sans overlap)
- Même ordinal, même parent entry mais avec `overlap_len = 0`

Le parent entry de la content-only key :
- `raw_ordinal` : même que la clé complète
- `sti` : même (0 pour la clé principale, SI pour les suffixes)
- `own_len` : `content_len + sep_len` (inchangé)
- `sep_len` : inchangé
- `overlap_len` : **0** (pas d'overlap)
- `is_word_start` : inchangé

### Impact FST

~1 clé supplémentaire par word-stripped entry. Les clés content-only
**partagent le préfixe** avec les clés complètes (ex: `\x02uint64` est
un préfixe de `\x02uint64to`). Le FST compresse très bien les préfixes
partagés → impact minimal sur la taille.

### Pourquoi ça ne crée pas de FP

La content-only key a le **même ordinal** que la clé avec overlap.
Les postings (WordSfxPost) sont les mêmes. Le resolve fait la même
vérification d'adjacence. La seule différence : le falling walk
TROUVE le split qu'il ratait avant.

### Pourquoi pas les suffixes content-only ?

Pour SI>0, la content-only key serait `\x02uint64`[SI..] sans overlap.
Ex: SI=3 → `\x02nt64` (4 bytes). C'est assez long pour éviter les
collisions multi-parent (contrairement aux markers chunk qui font 1-2 bytes).

On ajoute les suffixes content-only exactement comme pour les clés
complètes (boucle sur SI de 1 à content_len-1). Même parent entry
avec overlap_len=0.

## Changements nécessaires

| Fichier | Changement |
|---------|------------|
| `builder_v3.rs` | `add_word_stripped()` : ajouter les clés content-only |
| Aucun autre | Le falling walk, le resolve, le chain builder sont inchangés |

## Tests

1. `test_uint64t_relaxed_scale_diag` : les 5 FN docs doivent passer à toute échelle
2. Ground truth 500 docs : uint64_t relaxed 23/23 (au lieu de 18/23)
3. Régression : `cargo test --lib` — les 3 fails pré-existants, aucun nouveau

## Résultat attendu

Ground truth : **15/15** (actuellement 14/15). 0 FP, 0 FN.

## Conclusion : le chunk pipeline et best_consumed

Les content-only keys résolvent le word pipeline (partition 0x02). Le
chunk pipeline (0x00/0x01) utilise toujours `best_consumed` pour gérer
les collisions de markers courts.

**Le `best_consumed` est un scotch query-time** qui fait des choix
gloutons : il garde seulement les ordinals du meilleur `consumed` et
ignore les autres. Aujourd'hui, 0 FN observé sur 500 docs. Mais
c'est structurellement fragile — le même genre de problème que les
content-only keys viennent de résoudre pour le word pipeline :

- Les markers courts ("\x01ex_", "\x01s") sont multi-parent par nature
  (des suffixes courts collisionnent inévitablement avec d'autres tokens)
- Le `best_consumed` fait un tri glouton qui PEUT éjecter des vrais positifs
  si le "best" consumed vient d'un marker parasite
- Plus il y a de docs, plus il y a de collisions, plus le risque augmente

**Il faut absolument re-réfléchir à une structure adaptée aux chunks
pour éliminer le besoin de `best_consumed`.**

Pistes :
- **Per-ordinal markers** : encoder l'ordinal dans la clé marker pour
  éliminer les collisions multi-parent. Coût : plus de clés FST.
- **Split table externe** : stocker les split points dans un fichier
  séparé (ordinal → split_byte), consultable sans dépendre du FST.
- **Éliminer les markers** : utiliser uniquement les content-only keys
  (comme pour le word pipeline) et fst_candidates (range query) pour
  la détection de splits. Le falling walk devient un "hint" rapide,
  pas la source de vérité.

Cette réflexion est la prochaine étape après l'implémentation des
content-only keys pour le word pipeline.
