# Piste de recherche : séparateurs comme termes ngram (Niveau 0 complet)

Date : 14 mars 2026

Statut : exploratoire, post-implémentation du Token Map + positions composées

## Contexte

Le design Token Map + Ngram positions composées (doc 03) résout les séparateurs
en mode **relaxed** au Niveau 0 (positions consécutives = tokenizer garantit un
séparateur non-alnum). Le mode **strict** nécessite le Token Map (Niveau 1) pour
lire les chars exacts du séparateur.

## Idée

Stocker le séparateur entre deux tokens consécutifs **comme un terme** dans le
posting list ngram, avec un ngram_seq réservé (1023 = marqueur séparateur).

Le séparateur est indexé exactement comme un trigram, dans le même champ, avec
une position composée qui le rattache au token qui le précède.

## Encoding

```
position = token_pos × 1024 + ngram_seq

ngram_seq 0..1022 : trigrams du token (comme dans doc 03)
ngram_seq 1023    : SÉPARATEUR après ce token
```

### Exemple

```
Texte : "import rag3db from 'rag3db_core';"

Ngram posting list :

  pos=0 "import" :
    "imp" pos=0    "mpo" pos=1    "por" pos=2    "ort" pos=3
    " "   pos=1023                                              ← séparateur

  pos=1 "rag3db" :
    "rag" pos=1024  "ag3" pos=1025  "g3d" pos=1026  "3db" pos=1027
    " "   pos=2047                                              ← séparateur

  pos=2 "from" :
    "fro" pos=2048  "rom" pos=2049
    " '"  pos=3071                                              ← séparateur (2 chars)

  pos=3 "rag3db" :
    "rag" pos=3072  "ag3" pos=3073  "g3d" pos=3074  "3db" pos=3075
    "_"   pos=4095                                              ← séparateur

  pos=4 "core" :
    "cor" pos=4096  "ore" pos=4097
    "';"  pos=5119                                              ← trailing (après dernier)

Aussi : prefix gap (avant le premier token) :
    ""    pos=1023  (token_pos=-1 × 1024 + 1023, ou convention spéciale)
    ou simplement omis si vide
```

## Vérification au Niveau 0

### Query "rag3db main" (séparateur " ", strict)

```
① Trigrams "rag3db" :
   Intersection pos → token_pos=1 ✓ (couverture complète, seq 0-3 consécutifs)

② Trigrams "main" :
   Intersection pos → token_pos=2 ✓ (couverture complète)

③ Positions consécutives : 1 → 2 ✓

④ Séparateur strict :
   Lookup terme " " dans ngram posting list
   Chercher position = 1 × 1024 + 1023 = 2047 dans le posting de " "
   → doc=42, position 2047 trouvée → SÉPARATEUR CONFIRMÉ ✓

→ Match confirmé à 100% au Niveau 0. Zéro token map. Zéro stored text.
```

### Query "rag3db core" (séparateur " ", strict)

```
① "rag3db" → token_pos=3 ✓
② "core"   → token_pos=4 ✓
③ Consécutifs ✓

④ Séparateur :
   Lookup " " à position 3 × 1024 + 1023 = 4095
   Le posting de " " ne contient PAS position 4095 pour doc=42
   (le séparateur à pos=3 est "_", pas " ")
   → REJETÉ ✓

   (Si fuzzy d≥1 : lookup "_" à 4095 → trouvé, edit_distance("_"," ")=1 ≤ budget)
```

### Séparateur fuzzy

Pour le mode strict avec budget > 0, il faut trouver quel séparateur est à cette
position et calculer edit_distance. Deux approches :

**A. Lookup inverse** : on ne sait pas quel terme est à position 4095. Il faudrait
   un index inversé position → terme, qui n'existe pas dans le posting list standard.

**B. Candidats probables** : les séparateurs les plus fréquents sont " ", "\n", "\t",
   "_", ".", "/", "-". Tester les ~10 termes les plus communs. Si aucun ne matche →
   fallback token map.

**C. Stocker le séparateur dans le token map quand même** : le Niveau 0 couvre le
   cas exact (d=0). Le Niveau 1 (token map) couvre le fuzzy. Pas besoin de résoudre
   le fuzzy depuis les ngrams.

L'option C est la plus pragmatique : le cas exact strict est couvert au Niveau 0,
le cas fuzzy strict (rare) tombe au Niveau 1.

## Impact sur l'index

- **+1 terme par frontière** dans le posting list ngram (vs +3 pour cross-boundary trigrams)
- Sur 5000 tokens/doc → ~5000 termes séparateurs supplémentaires
- Le vocabulaire des séparateurs est très petit : " ", "\n", "\t", "_", ".", etc.
  (~10-20 termes uniques). Presque pas d'impact sur le FST du ngram field.
- Les posting lists de " " et "\n" seront très longues (quasi tous les docs,
  quasi toutes les positions). Delta encoding efficace car les positions sont
  régulièrement espacées.

## Ce que ça change au Niveau 0

```
AVANT (doc 03) :
  Niveau 0 couvre : d=0, query ≥ 3 chars, séparateurs relaxed
  Niveau 1 couvre : d>0, query < 3 chars, séparateurs stricts

APRÈS (avec séparateurs ngram) :
  Niveau 0 couvre : d=0, query ≥ 3 chars, séparateurs relaxed ET stricts
  Niveau 1 couvre : d>0, query < 3 chars, séparateurs stricts fuzzy (rare)
```

Le token map reste nécessaire pour le fuzzy et les queries courtes, mais son
utilisation diminue encore.

## Prérequis

Implémenter d'abord le Token Map + positions composées (doc 03). Valider. Puis
ajouter les séparateurs comme termes ngram.

L'ajout est incrémental : il suffit de modifier le tokenizer ngram pour émettre
un terme supplémentaire par frontière, avec ngram_seq=1023. Pas de changement
de format, pas de nouveau fichier. C'est juste plus de termes dans le même
posting list.
