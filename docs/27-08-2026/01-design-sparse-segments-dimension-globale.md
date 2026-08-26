# Design — segments sparse et dimension globale

27 août 2026. Révision du design du 24 mars
(`docs/24-mars-2026-20h35/07-design-sparse-segments-incremental-sync.md`),
après le cahier des charges de la session rag3weaver
(`../rag3db/extension/rag3weaver/docs/26-aout-2026-20h29/06-…`) et notre
réponse (`…/07-…`).

Le design de mars tient toujours. Ce qu'il manque, c'est un détail de format
qui fait s'effondrer trois demandes en une seule primitive.

## 1. La mesure

`sparse.mmap` est réécrit en entier à chaque commit, donc **insérer un
vecteur coûte ce que coûte insérer tout l'index** (`bench_commit_cost.rs`,
machine 24 cœurs, release) :

| index | commit après chargement en masse | commit après **un seul** vecteur | fichier |
|---|---|---|---|
| 10 000 | 26 ms | **26 ms** | 6,9 Mo |
| 50 000 | 97 ms | **95 ms** | 31,3 Mo |
| 100 000 | 178 ms | **171 ms** | 61,8 Mo |
| 200 000 | 320 ms | **320 ms** | 122,8 Mo |

Linéaire dans la taille du fichier : à deux millions de vecteurs, c'est
plusieurs secondes par insertion. C'est le vrai problème, avant tout besoin
de partage.

## 2. Ce que le 24 mars proposait

Des segments WORM, comme lucivy : un commit écrit un **nouveau** segment
(UUID), `meta.json` liste les segments actifs, la recherche fusionne, un
merge de fond recompacte, les suppressions vivent dans un bitset par
segment. Rien à redire : le motif est prouvé dans ce dépôt.

Le doc identifiait aussi le blocage de la fusion, sans le lever :

> le `dim_map` (`token_id → dimension dense`) est local à chaque index,
> donc absorber un index dans un autre demande de remapper les dimensions.

## 3. Ce qu'on ajoute : la dimension **est** le token id

Aujourd'hui, l'en-tête d'une dimension est

```rust
#[repr(C)]
struct DimHeader { offset: u64, count: u32, _pad: u32 }
```

et sa position dans la table *est* la dimension : un indice dense, local,
que seul `sparse_dims.bin` sait retraduire en `token_id`.

Le changement tient en un mot : **`_pad` devient `token_id`, et la table est
triée par `token_id`.** Même taille (16 octets), même disposition, une
version de format de plus.

```rust
#[repr(C)]
struct DimHeader { offset: u64, count: u32, token_id: u32 }   // v3, triée
```

Ce que ça donne, dans l'ordre d'importance :

1. **Fusionner deux segments devient un merge-join** sur des ids triés, avec
   concaténation des postings. Aucun remappage, aucune réécriture d'id.
2. **Fusionner deux index est la même opération.** Les scores sparse sont
   déjà comparables entre corpus — pur produit scalaire, aucune statistique
   globale (`sharded.rs:5`). Donc un index n'est plus qu'« un ensemble de
   segments » : le monter, le démonter, l'échanger, c'est la même primitive.
   Les demandes **L4** (fusion sparse), **L1 côté sparse** (monter un shard
   déjà construit) et le **delta sync** cessent d'être trois chantiers.
3. **`sparse_dims.bin` disparaît** : la table d'en-têtes *est* le mapping.
   Un fichier de moins à écrire, à synchroniser, à corrompre — et un
   `HashMap<u32, usize>` de tout le vocabulaire en moins par index ouvert.

Coût : la traduction `token_id → dimension` passe d'un `HashMap` à une
recherche binaire dans la table du segment. ~17 comparaisons pour 10⁵
dimensions, sur les 10 à 100 dimensions d'une requête. Négligeable, et
mieux localisé en cache que la table de hachage.

## 4. Le format

```
sparse_index/
  meta.json                     ← segments actifs, suppressions
  seg_<id>.mmap                 ← immutable : en-têtes triés par token_id + postings
  seg_<id>.ids                  ← ses ids, triés, 8 octets pièce
```

Pas de `vectors.bin` par segment : les vecteurs d'origine n'étaient gardés
que pour savoir quelles dimensions toucher à une suppression, et une
suppression est devenue un tombstone. Ce qu'il faut savoir, c'est **quel
segment porte un id** — c'est `seg_<id>.ids`, lu seulement quand on supprime
ou met à jour, et il donne au merge sa liste d'ids gratuitement.

`seg_<id>.mmap`, version 3 :

```
[FileHeader]                    16 o   magic "SPRS", version, num_dims, num_vectors
[DimHeader × num_dims]          16 o × N   { offset, count, token_id }, triés par token_id
[PostingEntry × total]          16 o × M   { record_id, weight, max_next_weight }
[Footer]                        8 o    crc32 du tout, puis le magic
```

Le pied de CRC et l'écriture atomique (temporaire + `rename` + `sync`) sont
déjà là depuis hier soir. Les versions 1 et 2 restent lisibles : leur table
est dense, elles ont besoin de `sparse_dims.bin`, et un index v1/v2 s'ouvre
comme **un segment unique**. Aucune migration à écrire : le premier commit
d'un index existant écrit un segment v3 à côté.

`meta.json` :

```json
{
  "version": 1,
  "segments": [
    { "id": "a1b2c3d4", "num_vectors": 5000, "deleted": [42, 99] },
    { "id": "e5f6a7b8", "num_vectors": 1200, "deleted": [] }
  ]
}
```

## 5. Ce que devient chaque opération

| opération | aujourd'hui | avec les segments |
|---|---|---|
| `insert` | RAM, marque `dirty` | inchangé |
| `commit` | réécrit tout l'index | écrit **un segment** des seuls vecteurs en RAM, met à jour `meta.json` — O(delta) |
| `search` | un mmap | un par segment, fusion des top-k (option B du 24 mars) |
| `remove` | retire des postings RAM | ajoute l'id à `deleted` du segment qui le porte (trouvé dans `seg_<id>.ids`) ; le merge l'applique |
| `merge` | n'existe pas | merge-join des tables de dimensions, concaténation des postings, suppressions appliquées |
| fusionner deux index | impossible | monter les segments de l'autre — la même opération que `merge` |
| delta sync | impossible | les ids de segments qui manquent |

## 6. Les risques, tels quels

- **Élagage WAND par segment**, pas global. Le 24 mars l'assumait déjà
  (option B) : le pruning reste efficace dans chaque segment, la fusion des
  top-k coûte O(N × k). À surveiller quand N monte — d'où la politique de
  merge.
- **Recherche sur N segments** : N mappings à parcourir par dimension de
  requête. Il faut une politique de merge qui garde N petit (lucivy fait
  ça : `max_merged_docs`).
- **Suppression d'un id qu'on n'a pas en RAM** : réglé par `seg_<id>.ids`
  (recherche binaire, chargé à la demande).
- **Le backend blob** synchronisait une liste de fichiers fixe
  (`INDEX_FILES`) ; elle est devenue variable (`IndexMeta::files`), et un
  commit ne pousse que les fichiers qu'il vient d'écrire.

## 7. L'ordre

- **A — format v3, dimension globale.** Écrivain, lecteur (v1/v2/v3), accès
  par `token_id`. La recherche mmap n'a plus besoin du `dim_map`.
  *Contrat : un fichier v2 se lit toujours, un v3 rend exactement les mêmes
  réponses.*
- **B — segments.** `meta.json`, commit qui n'écrit que le delta, recherche
  fusionnée, suppressions dans la méta.
  *Contrat : chercher sur N segments == chercher sur l'index compacté ;
  le coût d'un commit ne dépend plus que du delta (mesuré).*
- **C — merge.** Merge-join des dimensions, suppressions appliquées,
  politique de compactage.
  *Contrat : après merge, mêmes réponses, un seul segment.*
- **D — delta sync et montage d'un index dans un autre.** Tombe tout seul
  une fois C écrit : ce sont des segments qu'on copie et qu'on liste.

## 8. Ce qui a été fait, et ce que ça donne

A, B et C sont écrits et testés. D attend leur chiffre.

**Le commit ne dépend plus de la taille de l'index** (`bench_commit_cost.rs`,
release, même machine) :

| index | commit après chargement en masse | commit après **un seul** vecteur |
|---|---|---|
| 10 000 | 51 ms | **29 ms** (avant : 26) |
| 50 000 | 93 ms | **29 ms** (avant : 95) |
| 100 000 | 108 ms | **28 ms** (avant : 171) |
| 200 000 | 168 ms | **33 ms** (avant : 320) |

Ce qui reste est constant : trois `fsync` (le segment, ses ids, la méta).
À deux millions de vecteurs, l'ancien commit prendrait ~3 s ; celui-ci en
prend toujours 30.

**Le coût d'une recherche par segment : il a fallu trois corpus pour le
savoir, et les deux premiers mentaient en sens inverse.** Sur 40 000
documents et 200 requêtes, temps de recherche rapporté à la même recherche
après merge (`bench_segment_search.rs`) :

| corpus | 5 segments | 20 | 50 | 100 |
|---|---|---|---|---|
| dimensions uniformes, tous poids à 1.0 | ×1,8 | ×5,3 | — | — |
| mots de vrai texte, `tf · idf` | ×1,4 | ×1,1 | ×1,0 | ×1,0 |
| **BGE-M3 (le dump du 27 août)** | **×1,6** | **×3,1** | **×4,9** | **×7,8** |

Pourquoi les deux premiers se trompent :

- **Poids plats** : le WAND n'a rien avec quoi élaguer, chaque segment
  recommence un parcours complet — le coût paraît pire qu'il n'est.
- **Mots de vrai texte** : un vocabulaire de 112 000 dimensions dont la
  liste médiane fait *deux* documents. Rien à élaguer, donc rien à perdre en
  découpant.
- **Vecteurs de modèle** : vocabulaire borné et partagé (6 583 dimensions,
  liste médiane 52, la plus longue 29 630). Le WAND saute loin dans une
  longue liste, et c'est exactement ce que le découpage lui retire : chaque
  segment remplit son propre top-k depuis zéro avant que sa borne puisse
  élaguer quoi que ce soit.

Ce n'était donc pas la forme de Zipf qui comptait, mais la **taille du
vocabulaire** — et c'est ce que le texte ne peut pas imiter : les mots sont
quasi uniques, les dimensions d'un modèle sont partagées.

**La leçon dépasse le sparse : un bench sur données synthétiques mesure le
générateur.** Les deux mauvaises réponses étaient reproductibles, stables
d'un run à l'autre, et fausses.

**Ce qui croît aussi, c'est l'écriture** : une insertion demande à chaque
segment s'il porte l'id (`Segment::holds`) — 3,4 µs sur un segment, 8,5 µs
sur cent.

**Le seuil de compactage est donc huit** (`LUCIVY_SPARSE_MAX_SEGMENTS`,
`0` = jamais) : une recherche reste sous le double de son temps compacté,
sept commits sur huit ne paient que leur delta, et le nombre de fichiers
reste borné.

**Ce que le merge prouve.** `segments::merge_segments` marche les tables de
tokens ensemble et concatène — aucun remappage, aucun dictionnaire
reconstruit. Il prend `&[&Segment]` : lui passer les segments d'un *autre*
index est exactement le même appel. C'est là que L4 (fusion sparse) et L1
côté sparse deviennent une ligne de code, quand on en aura besoin.

**Les vecteurs de test.** La session rag3weaver a produit le dump le 27
août au matin : **2 924 documents et 200 requêtes BGE-M3** (burn/Vulkan),
`~/lucivy_bench/sparse/*.jsonl`, plus un extrait de 500 documents commité
dans `sparse_vector/tests/fixtures/` pour que la CI ait de vrais vecteurs.
`corpus_vectors::from_dump` les lit et les réplique avec décalage d'id quand
un bench en veut plus (la réplication multiplie chaque liste par le même
facteur et laisse nnz, les poids et le déséquilibre intacts — ce n'est pas
de la donnée nouvelle, et le bench le dit).

Ce que le dump apprend, au-delà des chiffres :

- **les token ids montent à 245 156** : l'espace `u32` est creux, pas dense —
  ce qui valide directement la table indexée par token global, une table
  dense aurait été absurde ;
- **5 dimensions de requête n'apparaissent dans aucun document** : le chemin
  « dimension inconnue » doit rendre vide, pas paniquer ;
- **les requêtes n'ont rien à voir avec les documents** (nnz moyen 10,4
  contre 45,2), donc les demander séparément était juste — et rag3weaver
  précise que BGE-M3 n'a pas de tête « requête » : c'est la même passe
  avant, la différence vient du texte.

Le générateur dérivé du texte (`corpus_vectors::build`) reste comme
solution de repli quand le dump n'est pas là, mais ce n'est plus la
référence.

**Vérité** : `test_global_dims.rs` (4), `test_segments.rs` (7),
`test_mmap_durability.rs` (4) — dont la recherche sur N segments comparée à
un index unique qui tient tout, l'update qui masque la copie ancienne, la
suppression qui survit à une réouverture, le merge qui ne déplace aucune
réponse, et l'index d'avant les segments qui s'ouvre puis se convertit.
