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
  seg_<id>.vectors.bin          ← les vecteurs d'origine du segment
```

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
| `remove` | retire des postings RAM | ajoute l'id à `deleted` du segment qui le porte ; le merge l'applique |
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
- **Suppression d'un id qu'on n'a pas en RAM** : il faut savoir quel segment
  le porte. Les `vectors.bin` par segment le disent, mais les charger coûte
  cher ; on parcourt les segments à la suppression, ce qui est rare.
- **Le backend blob** synchronise une liste de fichiers fixe
  (`INDEX_FILES`) ; elle devient variable (les segments actifs). C'est le
  point le plus mécanique, mais il touche `handle.rs`.

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

**Le coût d'une recherche par segment** (`bench_segment_search.rs`,
100 000 vecteurs, 50 requêtes) :

| segments | recherche | après merge |
|---|---|---|
| 1 | 0,03 ms | 0,03 ms |
| 2 | 0,04 ms | 0,03 ms (×1,1) |
| 5 | 0,06 ms | 0,03 ms (×1,8) |
| 10 | 0,06 ms | 0,03 ms (×1,9) |
| 20 | 0,17 ms | 0,03 ms (×5,3) |

D'où la politique : un commit merge au-delà de **huit segments**
(`LUCIVY_SPARSE_MAX_SEGMENTS`, `0` = jamais). Une recherche reste plate, et
sept commits sur huit ne paient que leur delta.

**Ce que le merge prouve.** `segments::merge_segments` marche les tables de
tokens ensemble et concatène — aucun remappage, aucun dictionnaire
reconstruit. Il prend `&[&Segment]` : lui passer les segments d'un *autre*
index est exactement le même appel. C'est là que L4 (fusion sparse) et L1
côté sparse deviennent une ligne de code, quand on en aura besoin.

**Vérité** : `test_global_dims.rs` (4), `test_segments.rs` (7),
`test_mmap_durability.rs` (4) — dont la recherche sur N segments comparée à
un index unique qui tient tout, l'update qui masque la copie ancienne, la
suppression qui survit à une réouverture, le merge qui ne déplace aucune
réponse, et l'index d'avant les segments qui s'ouvre puis se convertit.
