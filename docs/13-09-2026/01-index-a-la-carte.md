# L'index à la carte : déclarer les questions, pas la disposition

*Vision notée le 13 septembre 2026, après la publication de 4.1. Rien n'est
engagé ici : c'est la forme que pourrait prendre la suite des options d'index.*

## L'idée

Aujourd'hui l'utilisateur choisit une **disposition** (`shared_dictionary`,
`derived_in_ram`, `positions: false`) et doit deviner ce qu'elle lui coûte.
Demain il déclarerait, à la création, **les questions qu'il veut pouvoir
poser** :

```json
{
  "fields": [{"name": "content", "type": "text"}],
  "features": ["contains:strict", "contains:relaxed", "spans", "score"]
}
```

et le moteur choisirait la forme de stockage la plus petite qui répond
exactement à cette liste. Ici, par exemple : pas de `phrase`, donc pas de
positions — c'est `positions: false`, mais déduit au lieu d'être demandé.

L'utilisateur parlerait de **ce qu'il cherche**, pas de `.posmap` ni de table
des voisins.

## Pourquoi c'est devenu pensable

4.1 a établi les deux moitiés du raisonnement :

1. **Chaque capacité a un coût identifiable.** Les positions valent la moitié de
   l'index du noyau (5 289 → 2 598 Mo sans elles) ; les trois fichiers dérivés en
   valent un tiers ; les entrées « mot sans séparateurs » servent le mode relâché
   et rien d'autre.
2. **On sait vérifier sur le texte stocké.** `briques::stored` prouve chaque match
   avec les prédicats mêmes de la vérité terrain. Une structure absente de l'index
   ne rend donc pas la réponse approximative : elle la rend plus lente.

## Le catalogue (état des lieux, à compléter)

| question | ce qu'elle exige aujourd'hui |
|---|---|
| `contains` strict (sous-chaîne à travers les jetons) | FST des suffixes (partitions `0x00`/`0x01`), postings, chaînes entre jetons |
| `contains` relâché (`spin_lock` = `spinlock`) | en plus : entrées mot sans séparateurs (partition `0x02`) |
| `startsWith`, `term` | partition `0x00` et vérification des bornes de mot |
| `phrase`, adjacence | **positions** |
| fuzzy (Levenshtein, Jaro-Winkler) | candidats par pigeonhole de trigrammes, puis vérification |
| regex | littéraux requis extraits du motif, puis vérification |
| **spans** (savoir *où*) | placement des octets (`.posmap` + `.termtexts`) ou vérification sur le texte stocké |
| score BM25 | fréquences par document et normes de champ |
| aiguille de 1-2 caractères | la FST (un index de trigrammes ne peut pas) |

De ce tableau se déduisent des dispositions qui n'existent pas encore :

- `["contains:strict", "score"]` sans spans ni relâché : ni positions, ni entrées
  mot, ni placement d'octets — un index probablement autour du texte lui-même ;
- `["term", "phrase"]` seulement : un index inversé classique suffit, sans FST de
  suffixes ; c'est tantivy, et c'est dix fois plus petit ;
- `["contains:relaxed", "fuzzy", "spans"]` sans `phrase` : la disposition 4.1
  d'aujourd'hui ;
- un **dos de candidats par trigrammes** au lieu de la FST, avec notre
  vérification : l'index des autres, nos réponses — plus petit, plus lent.

## Les règles à ne pas casser

1. **Une disposition ne change jamais une réponse, seulement son coût.** C'est ce
   qui distingue nos options de celles des autres moteurs, où changer l'analyseur
   change ce qu'on trouve.
2. **Une question hors contrat échoue bruyamment**, ou retombe sur un balayage
   annoncé par `query_warnings`. Jamais un zéro silencieux — c'est exactement ce
   qu'on reproche aux index de trigrammes sur deux caractères.
3. **Le défaut reste « tout est répondu »**, sans rien déclarer. La liste de
   features est une optimisation, pas un passage obligé.
4. **La vérité terrain tourne sur chaque disposition**, comme aujourd'hui sur les
   quatre. Une disposition qu'on ne sait pas vérifier n'existe pas.

## Ce que ça coûterait

- **Un contrat de compatibilité par disposition** : `meta.json` porte déjà la liste
  des features ; un binaire plus ancien doit refuser proprement ce qu'il ne sait
  pas lire (aujourd'hui un index sans positions s'ouvre en 4.0.x mais échoue à la
  recherche — c'est précisément le genre de chose à corriger avant d'en multiplier).
- **Une explosion combinatoire de tests.** La parade : les features se composent en
  un petit nombre de structures, et c'est *par structure* qu'on teste, pas par
  combinaison.
- **Une surface de décision pour l'utilisateur.** À compenser par un calculateur :
  « voici ce que ta liste coûterait sur 1 Go de texte », dérivé des mesures du banc.

## Étapes plausibles

1. **Sucre au-dessus de l'existant** : accepter `features: [...]` et le traduire
   dans les trois options actuelles. Rien de nouveau dans le format, tout dans la
   validation et la documentation.
2. **Élaguer pour de vrai** : ne pas écrire les entrées mot quand le relâché n'est
   pas demandé, ni la table des voisins quand rien ne traverse les jetons.
3. **Un second dos de candidats** (postings de trigrammes) derrière la même
   vérification, pour les corpus où la taille prime sur la latence.

## Lien avec le reste

- Les trois options d'aujourd'hui et leurs mesures : `ARCHITECTURE.md`,
  `docs/11-09-2026/03-architecture.md`.
- Pourquoi la vérification sur le texte stocké rend tout cela possible :
  `docs/08-09-2026/01-chantier-positions-optionnelles.md`.
- L'import d'index tierce, qui rejoint la famille « ressembler à un autre moteur » :
  `docs/06-09-2026/02-import-tantivy-elasticsearch.md`.
