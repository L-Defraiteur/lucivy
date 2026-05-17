# Vérification d'adjacence exacte pour cross-token chains (v3)

## Problème

Avec les content-prefix ordinals, les postings de tokens partageant le même contenu
(mais des seps différents) sont agrégés sous un seul ordinal. Le falling walk valide
les bytes sur une clé FST spécifique, mais le resolve récupère les postings de TOUTES
les variantes de sep. L'adjacency check (position+1, byte_from continu) passe par
coïncidence pour des docs où le texte réel ne contient pas la query.

Exemple : query "struct", falling walk matche "...s" à SI=6 d'un token avec sep "_".
L'ordinal couvre aussi les tokens avec sep "\n-". Un doc avec "atabas\n-truction" a un
token "atabas\n-" adjacent à "truction" → adjacence OK, mais "struct" n'est PAS dans
le texte.

## Option A — TermTexts verification (choisie)

**Principe** : après le resolve, vérifier que les bytes du token correspondent à la query.

**Données nécessaires** :
- Ordinal (déjà dans le chain)
- `own_len` réel = `byte_to - byte_from` du posting
- TermTexts reader : ordinal + own_len → texte du token

**Vérification** :
```
text = termtexts.lookup(ordinal, own_len)
if text[sti..sti+N] != query[0..N]:
    → faux positif, rejeté
```

**Avantages** :
- Vérification byte-exact
- PosMap et TermTexts existent déjà, pas de nouvelle structure
- O(1) lookup + O(query_len) comparaison par candidat
- Les overlaps ne posent pas de problème : ils sont après own_len,
  et pour sti < own_len le contenu est identique entre variantes d'overlap

**Inconvénients** :
- Faut passer le TermTexts reader au resolve (refactor mineur)
- Deux tokens même ordinal, même own_len, mais sep char différent au même byte :
  TermTexts peut avoir les deux textes → faut checker contre tous

## Option B — SWI (Suffix Word Index) map

**Principe** : pour chaque byte du texte indexé, stocker l'index dans le mot (SWI).
Au resolve, vérifier la continuité structurelle.

**Structure** :
```
SWI map : (ordinal, byte_offset_dans_token) → index_dans_le_mot
```

Ou plus compact : par ordinal, un tableau de SWI pour chaque byte.

**Vérification** :
```
swiA = swi_map[ordA][sti]           // position dans le mot du premier byte matché
swiB = swi_map[ordB][0]             // position dans le mot du premier byte du token suivant
if swiB != swiA + (own_lenA - sti):
    → pas continu dans le mot, faux positif
```

**Avantages** :
- Vérification structurelle : on sait que "truct" suit "s" DANS LE MÊME MOT
- Lookup O(1) très rapide (tableau indexé)
- Conceptuellement simple : "à quelle position dans le mot suis-je ?"
- Permettrait aussi de valider des chains intra-mot vs inter-mot

**Inconvénients** :
- Nouvelle structure à construire et stocker (coût espace)
- Ne couvre pas les chains inter-mots (query "mutex_lock" = 2 mots)
  → faudrait un mécanisme séparé pour les frontières de mots
- Le SWI est relatif au mot tokenisé, pas au texte brut

## Option C — Map token → mots + positions

**Principe** : pour chaque ordinal, lister dans quels mots il apparaît et à quelle
position (chunk index) dans chaque mot.

**Structure** :
```
word_map : ordinal → [(word_content, chunk_index), ...]
```

**Vérification** :
```
words_A = word_map[ordA]  // mots contenant le token A
words_B = word_map[ordB]  // mots contenant le token B
// Vérifier qu'il existe un mot W où A est chunk[i] et B est chunk[i+1]
// ET que le contenu de W à la position correspondante matche la query
```

**Avantages** :
- Vue complète : on sait exactement de quel mot vient chaque token
- Permet de reconstruire le texte du mot pour vérification

**Inconvénients** :
- Gros volume : chaque ordinal peut apparaître dans des centaines de mots
- Redondant avec les postings + TermTexts
- Plus complexe à requêter que le lookup direct TermTexts

## Décision

Option A (TermTexts verification) : le plus simple, le plus efficace, utilise des
structures existantes. La vérification byte-exact couvre tous les cas.

Options B et C documentées pour référence future si on a besoin de vérifications
structurelles plus poussées (ex: validation intra-mot, SWI pour highlight précis).
