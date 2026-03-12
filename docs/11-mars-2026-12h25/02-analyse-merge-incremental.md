# Doc 02 — Analyse du pipeline merge pour découpage incrémental

**Date** : 11 mars 2026
**Objectif** : Comprendre le pipeline merge actuel pour le transformer en state machine
**Réf** : doc `9-mars/14` (§1 merge incrémental), doc `01` (Phase 1)

---

## Pipeline merge actuel

### Point d'entrée : `merge()` (`segment_updater.rs:146-183`)

```
merge(index, segment_entries, target_opstamp)
  ├── 1. Early exit si num_docs == 0
  ├── 2. index.new_segment() → merged_segment
  ├── 3. advance_deletes() sur chaque segment_entry
  ├── 4. Extraire delete_cursor de segment_entries[0]
  ├── 5. IndexMerger::open(schema, segments)
  ├── 6. SegmentSerializer::for_segment(merged_segment)
  ├── 7. merger.write(serializer) ← LE GROS MORCEAU
  └── 8. Construire SegmentEntry avec segment_meta
```

Les étapes 1-6 et 8 sont rapides. Le travail est dans `write()`.

### `IndexMerger::write()` (`merger.rs:528-553`)

```
write(serializer)
  ├── A. get_doc_id_from_concatenated_data() → SegmentDocIdMapping
  ├── B. write_fieldnorms(fieldnorms_serializer, &mapping)
  ├── C. write_postings(postings_serializer, fieldnorm_readers, &mapping) ← BOTTLENECK
  ├── D. write_storable_fields(store_writer)
  ├── E. write_fast_fields(fast_field_write, mapping)
  └── F. serializer.close()
```

### Analyse de coût par phase

| Phase | Complexité | Commentaire |
|-------|-----------|-------------|
| A. Doc ID mapping | O(N docs) | Une passe, rapide |
| B. Fieldnorms | O(N docs × N fields avec fieldnorms) | Linéaire, rapide |
| **C. Postings** | **O(N termes × N docs/terme)** | **Bottleneck — itère tous les termes × toutes les postings** |
| D. Store | O(N docs) ou O(N blocs) | Deux stratégies : iter+rewrite ou stack (fast path) |
| E. Fast fields | O(N docs × N colonnes) | Délégué à `columnar::merge_columnar()` |
| F. Close | O(1) | Flush buffers |

---

## Phase C en détail : `write_postings` / `write_postings_for_field`

### Structure de `write_postings_for_field` (`merger.rs:286-466`)

```
write_postings_for_field(field, field_type, serializer, fieldnorm_reader, mapping)
  ├── Préparation (une fois par champ) :
  │   ├── field_readers : Vec<Arc<InvertedIndexReader>>
  │   ├── field_term_streams : Vec<TermStreamer>
  │   ├── merged_terms = TermMerger::new(field_term_streams)
  │   ├── merged_doc_id_map : Vec<Vec<Option<DocId>>>  ← remap old→new par segment
  │   ├── total_num_tokens (estimation BM25)
  │   ├── field_serializer = serializer.new_field(...)
  │   └── segment_postings_option
  │
  └── Boucle principale :  ← C'EST ICI QU'IL FAUT YIELDER
      while merged_terms.advance()   ← terme suivant
        ├── Collecter segment_postings pour ce terme
        ├── Calculer total_doc_freq
        ├── Si total_doc_freq == 0 → skip (terme supprimé)
        ├── Vérifier cohérence has_term_freq
        ├── field_serializer.new_term(term_bytes, doc_freq, has_term_freq)
        ├── Pour chaque (segment_ord, segment_postings) :
        │   while doc != TERMINATED
        │     ├── Remap doc_id via merged_doc_id_map
        │     ├── Lire positions + term_freq
        │     └── field_serializer.write_doc(remapped_id, freq, positions)
        └── field_serializer.close_term()
```

### État à capturer entre les steps

Pour pouvoir yielder dans la boucle `while merged_terms.advance()`, il faut
stocker :

**État par champ (préparation)** :
- `field_readers: Vec<Arc<InvertedIndexReader>>`
- `merged_terms: TermMerger` — l'itérateur de termes fusionnés
- `merged_doc_id_map: Vec<Vec<Option<DocId>>>` — remapping par segment
- `field_serializer: FieldSerializer` — le serializer en cours
- `positions_buffer: Vec<u32>`
- `delta_computer: DeltaComputer`
- `segment_postings_option: IndexRecordOption`

**État global** :
- `serializer: SegmentSerializer` (contient store_writer, fast_field_write, etc.)
- `doc_id_mapping: SegmentDocIdMapping`
- `fieldnorm_readers: FieldNormReaders`
- Index du champ courant dans le schéma

---

## Design de la state machine

### Approche choisie : granularité par phase + par champ

On ne yield pas au milieu du traitement d'un terme (trop complexe, la
boucle interne doc-par-doc est rapide). On yield entre les termes ou entre
les phases.

```rust
enum MergePhase {
    /// Phases A-B : mapping + fieldnorms (exécutées d'un bloc, rapides)
    Init,
    /// Phase C : postings — un champ à la fois, N termes par step
    Postings {
        field_index: usize,
        field_state: Option<FieldMergeState>,
    },
    /// Phase D : store
    Store,
    /// Phase E : fast fields
    FastFields,
    /// Phase F : close
    Close,
    /// Merge terminé
    Done,
}
```

### `MergeState`

```rust
pub(crate) struct MergeState {
    // Input
    merger: IndexMerger,
    serializer: SegmentSerializer,
    doc_id_mapping: SegmentDocIdMapping,
    segment: Segment,           // merged_segment pour le résultat
    delete_cursor: DeleteCursor,

    // Progression
    phase: MergePhase,
    total_docs: u32,
    docs_processed: u32,  // pour observabilité

    // État postings (optionnel, présent pendant Phase C)
    fieldnorm_readers: Option<FieldNormReaders>,
}
```

### `FieldMergeState` — état pendant le merge d'un champ

```rust
struct FieldMergeState {
    field: Field,
    field_readers: Vec<Arc<InvertedIndexReader>>,
    merged_terms: TermMerger,
    merged_doc_id_map: Vec<Vec<Option<DocId>>>,
    field_serializer: FieldSerializer,
    positions_buffer: Vec<u32>,
    delta_computer: DeltaComputer,
    segment_postings_option: IndexRecordOption,
    terms_processed: usize,  // pour observabilité
}
```

### `MergeState::step(budget) -> StepResult`

```rust
pub enum StepResult {
    /// Budget épuisé, rappeler plus tard
    Continue,
    /// Merge terminé, voici le résultat
    Done(Option<SegmentEntry>),
    /// Erreur
    Error(crate::LucivyError),
}

impl MergeState {
    pub fn step(&mut self, term_budget: usize) -> StepResult {
        match &self.phase {
            MergePhase::Init => {
                // Phases A+B d'un bloc (rapides)
                // Construire doc_id_mapping, écrire fieldnorms
                // Ouvrir fieldnorm_readers
                self.phase = MergePhase::Postings { field_index: 0, field_state: None };
                StepResult::Continue
            }
            MergePhase::Postings { .. } => {
                // Traiter N termes du champ courant
                // Si champ terminé, passer au suivant
                // Si tous les champs terminés → phase Store
                self.step_postings(term_budget)
            }
            MergePhase::Store => {
                // Phase D d'un bloc (ou découpable par segment reader)
                self.phase = MergePhase::FastFields;
                StepResult::Continue
            }
            MergePhase::FastFields => {
                // Phase E d'un bloc (délégué à columnar::merge_columnar)
                self.phase = MergePhase::Close;
                StepResult::Continue
            }
            MergePhase::Close => {
                // serializer.close()
                // Construire SegmentEntry
                StepResult::Done(Some(segment_entry))
            }
            MergePhase::Done => unreachable!(),
        }
    }
}
```

### Budget

Le budget est en nombre de **termes traités** dans la boucle postings.
Un terme traite tous ses documents d'un coup (la boucle interne est rapide).
Budget typique : 1000-5000 termes par step.

Pourquoi pas en nombre de docs ? Parce que l'unité de travail naturelle dans
la boucle postings est le terme (on ne peut pas facilement yielder au milieu
du traitement d'un terme — `new_term` / `write_doc` × N / `close_term` est
atomique du point de vue du serializer).

---

## Problèmes identifiés

### 1. Ownership du SegmentSerializer

`write()` prend `mut serializer` par move. Les sous-méthodes extraient des
composants :
- `serializer.extract_fieldnorms_serializer()` → `Option<FieldNormsSerializer>`
- `serializer.get_postings_serializer()` → `&mut InvertedIndexSerializer`
- `serializer.get_store_writer()` → `&mut StoreWriter`
- `serializer.get_fast_field_write()` → `&mut WritePtr`
- `serializer.close()`

Le serializer reste intact entre les phases, chaque phase extrait un composant
différent. C'est compatible avec notre state machine — on stocke le serializer
dans MergeState et chaque phase accède à son composant.

### 2. `IndexMerger` est `&self` dans `write()`

Les méthodes `write_postings`, `write_fieldnorms`, etc. prennent `&self`.
Le merger est immutable pendant toute l'opération. OK pour la state machine —
on le stocke dans MergeState et on passe `&self.merger` aux sous-méthodes.

### 3. `TermMerger` est l'itérateur clé

`TermMerger::advance()` avance au terme suivant. C'est un itérateur stateful
qui maintient un heap de term streams. Il est déjà conçu pour être appelé
terme par terme — parfait pour notre budget par termes.

### 4. `FieldSerializer` lifecycle

Le `FieldSerializer` est créé par `serializer.new_field()` et consommé par
`field_serializer.close()`. Entre les deux, on appelle `new_term()` et
`write_doc()`. Ce lifecycle est compatible avec le stockage dans
`FieldMergeState` — on crée le FieldSerializer au début du champ et on le
close quand le champ est terminé.

### 5. Les phases D (store) et E (fast fields) appellent des fonctions bulk

`write_storable_fields` itère par reader et pourrait être découpé par reader.
`write_fast_fields` appelle `columnar::merge_columnar()` qui est une opération
bulk externe. On ne peut pas facilement la découper.

**Stratégie** : exécuter D et E d'un bloc. Elles sont généralement plus rapides
que C (postings). Si les benchmarks montrent qu'elles sont un problème, on
pourra les découper plus tard (D par reader, E nécessiterait un changement dans
la lib columnar).

---

## Sémantique poll_idle — déjà correcte

Vérifié dans le scheduler (`scheduler.rs`) et le trait Actor (`actor/mod.rs`) :

- `Poll::Ready(())` = "j'ai fait du travail interne, rappelle-moi"
- `Poll::Pending` = "rien à faire, ne me rappelle pas"

Le test `IdleWorker` dans `scheduler.rs:834-841` confirme exactement le
pattern qu'on veut. Le scheduler :
1. Mailbox vide → appelle `poll_idle()`
2. `Ready` → continue (l'acteur a du travail interne)
3. `Pending` → marque l'acteur idle

Le doc 08 §2 notait que `Ready = encore du travail` est contre-intuitif
(l'inverse de la convention Rust où Ready = terminé), mais fonctionnellement
c'est correct. On garde tel quel — renommer serait cosmétique.

---

## Résumé : ce qu'il faut coder

1. **`MergeState`** + **`MergePhase`** enum — dans un nouveau fichier
   `src/indexer/merge_state.rs`

2. **`MergeState::new()`** — prend les mêmes args que `merge()` actuel,
   fait les étapes 1-6 (advance_deletes, create merger, create serializer)

3. **`MergeState::step(budget)`** — exécute un step, retourne `StepResult`

4. **Refactorer `write_postings_for_field`** — extraire la préparation
   (field_readers, merged_terms, merged_doc_id_map) en `FieldMergeState::new()`
   et la boucle en `FieldMergeState::step(budget)`

5. **`SegmentUpdaterActor`** — ajouter `merge_state: Option<MergeState>`,
   implémenter `poll_idle()` pour appeler `step()`

6. **Garder `merge()` comme wrapper synchrone** — pour l'API existante
   (`IndexWriter::merge()`), on garde une version qui appelle `step()` en
   boucle jusqu'à `Done`. Pas de breaking change.
