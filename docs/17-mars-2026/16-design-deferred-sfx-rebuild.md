# Design — Deferred SFX Rebuild at Commit Time

Date : 18 mars 2026

## Architecture

### Flow actuel (problématique)
```
merge(seg_A, seg_B) → merge_sfx() → FST rebuild O(E log E) ← BOTTLENECK
                                   → gapmap copy O(D)
                                   → sfxpost merge O(P)
```

### Flow proposé (deferred)
```
merge(seg_A, seg_B) → merge_sfx_deferred()
                       → gapmap copy O(D) ← rapide
                       → sfxpost merge O(P) ← modéré
                       → FST vide (skip)

commit() → pour chaque segment sans FST :
             → rebuild FST depuis term dictionary
             → réécrire .sfx avec FST + gapmap existant
```

## Implémentation

### 1. merge_sfx_deferred (FAIT)

`merger.rs` — écrit un .sfx partiel : FST vide + gapmap + sfxpost.
Les queries retournent EmptyScorer pour les segments sans FST (FAIT).

### 2. Rebuild FST au commit — point d'injection

**Fichier** : `src/indexer/segment_updater_actor.rs`, dans `handle_commit()`

```rust
fn handle_commit(&mut self, opstamp, payload) {
    let segment_entries = self.shared.purge_deletes(opstamp)?;
    self.shared.segment_manager.commit(segment_entries);

    // ICI : rebuild .sfx FST pour les segments mergés
    self.rebuild_sfx_for_committed_segments()?;

    self.shared.save_metas(opstamp, payload)?;
    garbage_collect_files(&self.shared);
}
```

### 3. rebuild_sfx_for_committed_segments

Pour chaque segment commité qui a un .sfx avec FST vide :

```rust
fn rebuild_sfx_for_committed_segments(&self) -> Result<()> {
    let index = &self.shared.index;
    let schema = index.schema();

    for meta in self.shared.segment_manager.committed_segment_metas() {
        let segment = index.segment(meta.clone());

        for (field, _) in schema.fields() {
            // Lire le .sfx existant
            let sfx_data = match segment.open_read_custom(&format!("{}.sfx", field.field_id())) {
                Ok(slice) => slice.read_bytes()?,
                Err(_) => continue, // pas de .sfx pour ce champ
            };
            let sfx_reader = SfxFileReader::open(&sfx_data)?;

            // Si le FST a déjà des terms, skip (pas un merge deferred)
            if sfx_reader.num_suffix_terms() > 0 {
                continue;
            }

            // Rebuild FST depuis la term dictionary
            let reader = SegmentReader::open(&segment)?;
            let inv_idx = reader.inverted_index(field)?;
            let mut sfx_builder = SuffixFstBuilder::new();
            let mut stream = inv_idx.terms().stream()?;
            let mut ordinal = 0u64;
            while stream.advance() {
                if let Ok(token) = std::str::from_utf8(stream.key()) {
                    sfx_builder.add_token(token, ordinal);
                    ordinal += 1;
                }
            }
            let (fst_data, parent_list_data) = sfx_builder.build()?;

            // Extraire le gapmap du .sfx existant
            let gapmap_data = sfx_reader.gapmap().raw_data().to_vec();

            // Réécrire le .sfx complet
            let sfx_file = SfxFileWriter::new(
                fst_data,
                parent_list_data,
                gapmap_data,
                sfx_reader.num_docs(),
                ordinal as u32,
            );
            let sfx_bytes = sfx_file.to_bytes();

            // Écriture atomique via le directory
            let path = format!("{}.{}.sfx", segment.id().uuid_string(), field.field_id());
            index.directory().atomic_write(Path::new(&path), &sfx_bytes)?;
        }
    }
    Ok(())
}
```

## Accès disponibles depuis handle_commit()

| Ressource | Accès | Méthode |
|-----------|-------|---------|
| Index | `self.shared.index` | Pour segment() et directory() |
| Segments committés | `self.shared.segment_manager` | committed_segment_metas() |
| Term dictionary | Via SegmentReader::open() | inverted_index(field).terms() |
| .sfx existant | segment.open_read_custom() | Gapmap + sfxpost déjà écrits |
| Directory write | index.directory().atomic_write() | Réécriture atomique |

## Points importants

### Détection des segments à rebuild
- `sfx_reader.num_suffix_terms() == 0` → FST vide → besoin de rebuild
- Les segments créés par SegmentWriter.finalize() ont un FST complet → skip
- Seuls les segments issus d'un merge deferred ont un FST vide

### Pas d'invalidation de cache
- Les SegmentReaders sont créés à chaque search (pas de cache)
- `load_sfx_files()` charge les .sfx depuis le disque à chaque open
- Après le rebuild, le prochain search voit automatiquement le nouveau FST

### Gapmap extraction
- Le SfxFileReader parse le .sfx et donne accès au gapmap via `.gapmap()`
- On a besoin d'une méthode `gapmap().raw_data()` pour extraire les bytes bruts
- Alternative : parser le header .sfx pour trouver gapmap_offset et copier les bytes

### atomic_write vs open_write_custom
- `atomic_write()` est plus sûr (pas de fichier corrompu si crash)
- Mais pas toutes les implémentations de Directory le supportent
- `open_write_custom()` + `terminate()` est le pattern standard

### Ordre des opérations au commit
1. `segment_manager.commit()` — les segments sont verrouillés
2. `rebuild_sfx_for_committed_segments()` — rebuild les FST manquants
3. `save_metas()` — sauvegarde les métadonnées
4. `garbage_collect_files()` — supprime les vieux fichiers

Le rebuild se fait AVANT save_metas pour que les fichiers .sfx soient visibles
quand le reader reload après le commit.

## Complexité

- Le rebuild FST est O(E log E) par segment (même coût qu'avant)
- MAIS il ne se fait qu'une fois par commit, pas à chaque merge intermédiaire
- Avec max_docs_before_merge = 50K, le coût est borné (~470ms max par segment)
- Parallélisable : chaque champ / chaque segment peut être rebuild en parallèle

## Prochaines étapes

1. Ajouter `gapmap().raw_data()` à SfxFileReader (ou parser le header)
2. Implémenter `rebuild_sfx_for_committed_segments()` dans segment_updater_actor
3. Ajouter max_docs_before_merge configurable dans SchemaConfig
4. Tester sur 212K docs
5. Bench comparatif avec/sans deferred
