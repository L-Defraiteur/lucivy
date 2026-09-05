# Importer un index tantivy ou Elasticsearch dans lucivy — proposition (6 septembre 2026, nuit)

Question de Lucie : « on pourrait faire une version qui convertit Elasticsearch
et tantivy à la volée ? ». Réponse courte : pas à la volée, mais oui par
réindexation depuis les documents stockés, en une commande. À planifier comme
une **4.1** (une fonctionnalité, pas un correctif), et à annoncer dans la
réponse à #15 comme chantier ouvert, sans date.

## Pourquoi pas « à la volée »

Ni un index Lucene ni un index tantivy ne contiennent ce que le moteur de
sous-chaînes exige : les suffixes de chaque token (la FST), les cartes de
positions par octet, le dictionnaire par shard. Ces structures se bâtissent
depuis le **texte** ; lire leurs postings ne donne ni le texte ni les
positions d'octets. Et lucivy ne lit pas un index tantivy : fork de tantivy
0.22, format de segment divergé depuis (fichiers `.sfx`, `.posmap`,
`.gmap`, `dict-*`, champs propres dans `meta.json`).

## Ce qui est faisable

**Depuis tantivy.** Ouvrir l'index avec tantivy 0.25 en bibliothèque (déjà
dépendance de bench : `lucivy_core/benches/compare_tantivy.rs`), lire le
schéma, parcourir le doc store, traduire les champs (text → text ; u64, i64,
f64, bool, date ; bytes et JSON → à décider, probablement ignorés avec un
avertissement), réindexer par `ShardedHandle`. **Limite à dire avant de
commencer** : seuls les champs `STORED` ont encore leur texte ; un champ
indexé sans être stocké est perdu, et l'outil doit le lister et refuser ou
continuer selon un drapeau explicite.

**Depuis Elasticsearch.** Parcourir `_source` par scroll ou point-in-time
(`benches/compare_elasticsearch.py` fait déjà l'indexation par `_bulk` dans
l'autre sens), lire le mapping (`text`, `keyword` → text ; `long`, `double`,
`boolean`, `date` → leurs types ; `object`/`nested` → aplatis en `a.b` ou
ignorés avec avertissement), réindexer. Limite : `_source` désactivé = rien à
lire ; les analyseurs ES ne se traduisent pas (lucivy a le sien).

## Forme

Deux options, la première plus courte :

1. **Deux scripts Python** appuyés sur le wheel `lucivy` : `import_tantivy.py`
   (via `tantivy-py`) et `import_elasticsearch.py` (via `elasticsearch` ou
   `requests`). Une journée pour les deux avec un test chacun (un index
   tantivy synthétique bâti par `tantivy-py` ; un conteneur ES comme le banc).
2. **Un binaire Rust `lucivy-import`** dans `lucivy_core` (`--from tantivy
   <dir>` / `--from elasticsearch <url>/<index>`), tantivy et `reqwest` en
   dépendances optionnelles derrière une feature. Plus propre pour un
   utilisateur Rust, plus long (deux jours), et un premier binaire natif
   `lucivy` à maintenir.

Dans les deux cas : afficher le schéma traduit avant d'indexer, le nombre de
documents, l'estimation de temps (0,3 ms par document + 0,27 s par Mo de
texte, la formule de la page), les champs perdus ; sortir non nul si un champ
demandé n'est pas récupérable.

## Ce que ça change pour la présentation

- #15 (« migration depuis tantivy ») reçoit une réponse concrète : pas
  d'ouverture d'un index tantivy, mais un import en une commande, plus le
  port d'un projet réel si le demandeur le fournit.
- La page et le README : une ligne « migrate from tantivy or Elasticsearch in
  one command » quand ça existe, pas avant.

## Décision à prendre

Scripts Python d'abord ou binaire Rust ; tantivy d'abord (le public de #15).
