# Plan — Pre-filter node_ids via AliveBitSet

## Problème

`search_filtered` utilise un `FilterCollector` : le scorer itère **tous**
les docs du posting list, score chacun, et le collector jette ceux dont
`_node_id` n'est pas dans le set autorisé. Gaspillage.

Le prescan SFX aussi résout et score des docs qui seront jetés ensuite.

## Solution

Injecter un `AliveBitSet` custom dans le `SegmentReader` avant la recherche.
Le moteur respecte déjà `alive_bitset` partout :

- `default_collect_segment_impl` : skip les docs non-alive avant collect
- `Weight::count` : `scorer.count(alive_bitset)`
- Le prescan SFX résout des ordinals → doc_ids, les docs hors bitset seront
  ignorés au scoring

### Mécanisme existant

`SegmentReader::open_with_custom_alive_set(segment, custom_bitset)` accepte
un `Option<AliveBitSet>` qu'il **intersecte** avec le delete bitset existant.
Après ça, toute la chaîne (prescan, scorer, collector) ne voit que les docs
autorisés. Zéro changement dans le moteur.

### Étapes

1. **Construire le bitset par segment** : scanner le fast field `_node_id`,
   marquer alive les doc_ids dont le node_id est dans `allowed_ids`.

   ```rust
   fn build_filter_bitset(
       reader: &SegmentReader,
       allowed_ids: &HashSet<u64>,
   ) -> AliveBitSet {
       let node_id_reader = reader.fast_fields().u64("_node_id").unwrap();
       let max_doc = reader.max_doc();
       // AliveBitSet : 1 = alive, 0 = filtered out
       let mut bitset = BitSet::with_max_value(max_doc);
       for doc in 0..max_doc {
           if allowed_ids.contains(&node_id_reader.get_val(doc)) {
               bitset.insert(doc);
           }
       }
       AliveBitSet::from(bitset) // intersecté avec le delete bitset ensuite
   }
   ```

   Coût : O(max_doc) scan linéaire sur un fast field mmap. Très rapide
   (~1-5ms pour 100K docs).

2. **Injecter dans le reader** : avant la recherche, recréer les
   `SegmentReader` avec le custom bitset. Deux options :

   - **(a)** `open_with_custom_alive_set` — ré-ouvre le segment (lourd,
     rouvre tous les fichiers)
   - **(b)** Ajouter un `SegmentReader::with_alive_bitset(bitset)` qui
     clone le reader avec un nouveau bitset (léger, juste remplace le champ)

   L'option (b) est préférable. Si elle n'existe pas, l'ajouter.

3. **Intégrer dans le DAG** : un nouveau node `PreFilterNode` après
   `flush`, avant `prescan`. Il construit les bitsets et injecte dans
   les shards. Le `BranchNode` existant (`needs_prescan`) reste inchangé.

   ```
   drain → flush → has_filter?
                     ├── then → prefilter → needs_prescan? → ...
                     └── else ──────────→ needs_prescan? → ...
   ```

   Ou plus simple : `PreFilterNode` est un no-op si pas de filtre.

4. **Supprimer le FilterCollector** : `search_filtered` n'a plus besoin
   de `FilterCollector`. Le filtre est dans le bitset, le collector est
   un `TopDocs` standard.

### Ce que ça filtre

| Étape | Avant (FilterCollector) | Après (AliveBitSet) |
|-------|------------------------|---------------------|
| Prescan SFX | Résout tous les docs | Skip docs hors bitset |
| Scorer | Score tous les docs | Skip docs hors bitset |
| Collector | Jette les non-autorisés | Tous les docs sont autorisés |

### Impact API

Aucun changement d'API publique. `search_filtered(config, top_k, sink,
allowed_ids)` fonctionne pareil, l'implémentation passe de FilterCollector
à AliveBitSet en interne.

### Risques

- **Fast field scan** : O(max_doc) par segment. Pour 90K docs × 4 shards,
  c'est ~360K lookups. Fast field mmap = ~0.5ms. Négligeable.
- **Bitset mémoire** : max_doc bits par segment. 90K docs = ~11KB. Rien.
- **`with_alive_bitset` n'existe pas** : faut l'ajouter sur SegmentReader.
  Petit changement, le struct a déjà le champ `alive_bitset_opt`.
