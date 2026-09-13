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

## Convertir ses propres index d'abord

L'import d'un index étranger n'est pas le premier pas : c'est le dernier. Le
premier est la **régénération de features sur nos propres index** — « j'ai indexé
sans positions, je veux maintenant des phrases » —, et c'est la même machinerie.

**Ce qu'on sait déjà faire**, et qui le prouve :

- `derived_in_ram` **reconstruit `.posmap`, `.word_pos_map` et `.sibling_v3` octet
  pour octet** depuis les postings, à l'ouverture. C'est déjà une régénération.
- Les fusions réinternent les textes (v3) et remappent les `.gmap` (v4).
- `test_compat_308` **convertit** un index 3.0.8 : le premier commit en 4.0 le
  réécrit sans perte.

**La règle qui découpe le problème** : une feature manquante est soit *dérivable*
de ce qui est sur le disque, soit reconstructible à partir du **texte stocké**.

| cas | source | exemple |
|---|---|---|
| dérivable | les structures existantes | dérivés depuis les postings ; postings de trigrammes depuis la FST des suffixes |
| re-tokenisable | le document store | ajouter les positions à un index `positions: false`, ajouter les entrées mot pour le mode relâché |
| impossible | rien ne rend le texte | un index de trigrammes sans champs stockés : les trigrammes ne reconstituent pas la source |

Autrement dit : **tant que le texte est stocké, toute feature est régénérable sans
le corpus d'origine**. C'est déjà la contrainte de `positions: false` (les champs
texte doivent être stockés), et elle devient ici une propriété utile.

**L'utilitaire** : `convert <index> --features …` qui élague ce qui ne sert plus,
dérive ce qui se dérive, et ne re-tokenise depuis le document store que le reste.
Jamais un accès aux données de l'utilisateur.

**La garantie à prouver, et elle est testable aujourd'hui** : un index converti
répond exactement comme un index bâti directement avec les mêmes features — mêmes
documents, mêmes spans, mêmes scores, sur le panel de vérité terrain ; et pour les
fichiers dérivés, égalité octet pour octet, que `derived_in_ram` vérifie déjà.

**Et alors seulement, l'étranger.** Il ne demande plus de logique de conversion,
seulement un lecteur qui rend `(document, texte)` — `_source` chez Elasticsearch,
champs `STORED` chez tantivy. « Ils fonctionnent en trigrammes » devient sans
importance : on n'importe pas leurs trigrammes, on importe leur texte, et on
régénère nos structures avec le même utilitaire. Ce qui n'a pas de texte stocké ne
s'importe pas, et il faut le dire franchement plutôt que de rendre un index dégradé.

## Lien avec le reste

- Les trois options d'aujourd'hui et leurs mesures : `ARCHITECTURE.md`,
  `docs/11-09-2026/03-architecture.md`.
- Pourquoi la vérification sur le texte stocké rend tout cela possible :
  `docs/08-09-2026/01-chantier-positions-optionnelles.md`.
- L'import d'index tierce, qui rejoint la famille « ressembler à un autre moteur » :
  `docs/06-09-2026/02-import-tantivy-elasticsearch.md`.

## Note du 14 septembre — les spans exacts comme base d'un « remplacer »

Idée de Lucie : ce que le moteur rend et que les autres ne rendent pas — **tous** les
spans, exacts à l'octet, sur tout le corpus, en une requête — est la matière d'un
`replace` par regex sur un corpus indexé : la regex trouve ses occurrences avec leurs
captures (voir `docs/07-09-2026/05-captures-agregees-et-casse.md`), les spans disent
quels octets réécrire, l'index dit quels documents relire, et le reste du corpus n'est
pas touché. Un `sed` avec un index devant. À cadrer après la 4.3.0, dans le catalogue des
questions ci-dessus (une question de plus : « où et quoi réécrire »).
