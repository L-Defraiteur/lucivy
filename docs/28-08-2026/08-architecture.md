# Architecture de lucivy — état au 28 août 2026

Rappel écrit pour être lu seul. Ce qui suit est ce qui a été **vérifié** dans
le code ou mesuré ; là où je m'appuie sur la documentation du projet sans
l'avoir revérifiée, c'est dit.

---

## 1. Les crates, et qui dépend de qui

```
ld-lucivy        le moteur : index, requêtes, scoring, merger, segments, SFX
  ├── lucivy-fst           FST avec automates de Levenshtein
  ├── ld-lucivy-columnar / -stacker / -query-grammar / -bitpacker /
  │   -common / -sstable / -tokenizer-api / ld-ownedbytes   (fork tantivy, 0.27.0)
  │
lucivy-core      le handle unifié : ShardedHandle, query builder, tokenizers,
  │              snapshot/delta, blob store, le DAG de recherche
  ├── luciole              framework acteurs / DAG, compatible WASM
  └── lucistore            persistance : BlobStore, ShardStorage, snapshot, sync

sparse-vector    index sparse (postings + WAND), crate ami, sur lucistore
```

**Les cinq crates qui portent le numéro de version partagé** — et qui repartent
à chaque release — sont `luciole`, `lucistore`, `ld-lucivy`, `lucivy-core`,
`sparse-vector`, publiés **dans cet ordre**. Les `ld-lucivy-*` en 0.27.0 sont
les crates vendorisés du fork tantivy : figés, republiés seulement si leur code
change.

**Les bindings** (cinq crates, non publiés sur crates.io) : `lucivy-napi`
(Node), `lucivy` (Python, PyO3 → PyPI), `lucivy-cpp` (bridge CXX),
`lucivy-emscripten` (WASM), `lucivy-fts` (bridge pour l'extension rag3db).

---

## 2. Le SFX engine — ce qui fait la différence

L'index construit une **Suffix FST** : chaque suffixe de chaque token y entre,
partitionné selon l'endroit où il commence.

- **SI = 0** : le suffixe commence un token — sert à `startsWith` et
  `anchor_start`.
- **SI > 0** : le suffixe commence à l'intérieur — sert au `contains`.
- **Cross-token** : une `falling_walk` plus une **table de siblings** qui
  enregistre qui suit qui et avec quel séparateur. C'est ce qui permet à
  `rag3weaver` de trouver `rag3_weaver`, et à `ror::lucivyer` de trouver
  `Error::LucivyError`.

**Fichiers par segment et par champ (format v3)** : `.sfx`, `.sfxpost`,
`.termtexts`, `.posmap`, `.bytemap`, `.word_sfxpost`, `.word_pos_map`,
`.sibling_v3`. (`.gapmap` et `.sepmap` étaient le v2.)

`sfx_version` vaut **3** par défaut depuis le 23 août. Un `meta.json` sans le
champ est un index v2.

### Le fuzzy, et le piège corrigé le 28

Un candidat fuzzy vient d'un **pigeonhole de trigrammes** : à distance *d*,
assez de trigrammes de la requête doivent apparaître exactement. Deux
générateurs produisent ces candidats, et une estimation de coût choisit :

- **`pieces`** — résout par morceaux ; **voit les occurrences à cheval** sur
  des séparateurs.
- **`pivot`** — ne garde que les trigrammes les moins coûteux et s'appuie
  uniquement sur les postings de trigrammes, qui vivent **à l'intérieur** des
  chunks d'un token. Il est donc **structurellement aveugle au cross-token**.

Depuis 3.0.7 : séparateurs relâchés ⇒ `pivot` interdit. `V3_FUZZY_MODE=pieces|
pivot|auto` force le choix, `V3_DIAG_FUZZY=1` explique ce qui a été retenu, et
`V3_DIAG_FUZZY_MAX=0` montre tous les rejets.

Le candidat retenu est ensuite **vérifié contre le texte** — le pigeonhole est
une condition nécessaire, jamais suffisante. La validation se fait par
Levenshtein, ou par **Jaro-Winkler** au-dessus d'une similarité
(`fuzzy_metric`, `min_similarity`, 0,9 par défaut).

**Différence importante entre les deux métriques** : le chemin Levenshtein
rend *tous* les spans d'une fenêtre candidate (`fuzzy_spans` donne un `Vec`),
le chemin Jaro-Winkler n'en rend **qu'un**, le meilleur de la fenêtre
(`best_window`). C'est pourquoi le panel de vérité terrain n'a pas de référence
pour Jaro-Winkler : ce qu'il rapporte dépend du découpage en fenêtres, qui est
un artefact de l'index.

---

## 3. Le DAG de recherche

```
drain → flush → [prescan par segment …] → merge_prescan → build_weight
                                                              ↓
                                    [search_shard …] → merge → output
```

**Le point structurant : le prescan crée un nœud par segment, pas par shard.**
Sur 50 segments, 50 nœuds. C'est là qu'est tout le parallélisme des requêtes ;
les acteurs de shard n'interviennent qu'à la phase de recherche, devenue
négligeable.

Conséquence pratique, vérifiée le 28 : **comparer 1 shard contre 4 pour la
vitesse de requête ne mesure plus grand-chose.** `bench_sharding` reste utile
pour le débit d'indexation et la distribution du routeur, pas pour la latence.

---

## 4. Sharding, filtre et fédération

**`ShardedHandle`** : N shards, routage configurable par `balance_weight`.
Attention au piège documenté : `1.0` est le défaut du `ShardRouter`
(round-robin, indexation rapide), mais un index sans configuration explicite
applique **0,2** (token-aware, co-localise les documents similaires). Les deux
commentaires du code divergent ; c'est 0,2 qui s'applique.

**Recherche filtrée (`allowed_ids`)** — un vrai pré-filtre depuis le 26 août.
Le jeu d'ids descend jusqu'au prescan v3 par un canal séparé du bitset de
suppression. Une recherche filtrée score **comme si l'index était le
sous-ensemble** : `doc_freq` compté sur le sous-ensemble, `N` = sa taille.
L'ordre d'une requête mono-terme est inchangé ; les scores n'égalent le non
filtré que si tout est autorisé.

**Fédération** — chaque nœud exporte ses statistiques, un coordinateur les
fusionne, chaque nœud score ensuite sur le corpus de la fédération, sans rien
copier ni monter. `export_stats` → `merge` → `search_with_global_stats`, plus
la variante filtrée. Depuis 3.0.6 ce mode **passe par le DAG** comme une
recherche normale : shards en parallèle, top-k borné, réparation des
highlights. `test_federated_search.rs` vérifie que l'union de deux nœuds égale
un index unique **aux mêmes scores**.

---

## 5. Bornes mémoire côté requête

Deux plafonds, et ils comptent — le second a été rencontré le 28 sur une
requête de deux caractères :

- **`LUCIVY_HIGHLIGHT_SPAN_CAP`** (4 M natif, 1 M wasm) — le sink d'highlights
  s'arrête là, et `ShardedHandle` relance alors une recherche filtrée aux ids
  du top-k pour ne remplir que leurs spans.
- **`LUCIVY_MAX_MATCHES_PER_SEGMENT`** (4 M natif, 20 k wasm) — plafond de
  matches par segment et par requête. Au-delà la requête est **tronquée**,
  jamais interrompue, et **la recherche le dit** :
  `last_search_truncated()` (Python `last_search_truncated`, Node
  `lastSearchTruncated()`, wasm `memory_status.last_search_truncated`).

`0` désactive l'un comme l'autre. Sur 93 983 fichiers, `de` (deux caractères)
rend 2 420 339 spans avec les plafonds, et **7 695 534 exacts** sans.

---

## 6. Persistance et formats

| type | usage | I/O |
|---|---|---|
| `StdFsDirectory` | natif et WASM/OPFS | différée : tout en RAM jusqu'au `terminate()` |
| `RamDirectory` | tests | pure RAM |
| `BlobDirectory` | ACID (mmap + blob en base) | extensible : Postgres, S3, MinIO |

**Formats d'échange** : **LUCE** (snapshot complet), **LUCID** (delta d'un
shard), **LUCIDS** (delta shardé, seulement les shards modifiés).

**Le blob store ACID** est exposé dans les trois bindings natifs : Python
(`Index.create_with_blob_store`), Node (`BlobIndex`), C++
(`lucivy::BlobBackend`). Règle : les méthodes du store tournent sur les threads
du scheduler, doivent être thread-safe et ne jamais rentrer dans l'index.

---

## 7. WASM — les règles qui ne se négocient pas

- **Jamais de `thread::spawn`** : tout passe par le scheduler. Un job de CI le
  vérifie et fait échouer la build.
- I/O **uniquement** au `terminate()` ; jamais dans un handler d'acteur.
- `LUCIVY_MERGE_CONCURRENCY` à 1, `WRITER_HEAP_SIZE` à 15 Mo (50 en natif).
- L'espace adressable est de **4 Go**, ce qui borne tout le reste.

**Chemins, piège corrigé en 3.0.8** : un index créé vit sous
`/opfs/lucivy/<chemin>`. `lucivy_create` stockait le chemin nu dans son
contexte alors que `lucivy_open` stockait le préfixé — toute fonction passant
par `ctx.index_path` cherchait ailleurs.

**Deux copies de la couche JS** existaient, `bindings/emscripten/js/` et
`playground/js/`, synchronisées à la main et déjà divergentes. `build.sh` les
recopie désormais, et estampille aussi la version et le cache-buster de
`playground/index.html` depuis `Cargo.toml` — ils étaient restés à `3.0.4`
pendant trois versions, ce qui servait l'ancien wasm aux visiteurs déjà venus.

---

## 8. Ce que je ne peux pas affirmer

Par honnêteté sur les limites de ce document :

- Le détail interne du **merger** et de la politique de fusion tiered : je
  connais ses effets (24 segments visés, gain de 40 % sur la taille) mais pas
  son code.
- Le **docstore** et sa compression : je sais qu'il pèse 1,5 % de l'index et
  que `docstore_compress_dedicated_thread` est à `false` en WASM.
- Le détail de `sparse-vector` au-delà de ce que dit `CLAUDE.md` : segments
  WORM, dimension = token id global, filtre par ids triés, commit atomique.
  Je n'ai pas relu ce code cette session.
- L'**index dense** : il n'existe pas.

Pour ces parties, la référence reste `docs/25-08-2026/06-architecture.md` et
`ARCHITECTURE.md`, avec la réserve d'usage : ce sont des observations datées,
à revérifier contre le code avant d'en dépendre.
