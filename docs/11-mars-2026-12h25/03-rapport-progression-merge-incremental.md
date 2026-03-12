# Doc 03 — Rapport de progression : merge incrémental

**Date** : 11 mars 2026
**Branche** : `scheduler-beta`
**Réf** : doc `01` (plan), doc `02` (analyse)

---

## État : Phase 1 — étapes 1-3 en cours

### Étape 1 — Sémantique `poll_idle` ✅

Vérifié dans le scheduler et le trait Actor :
- `Poll::Ready(())` = "j'ai fait du travail, rappelle-moi"
- `Poll::Pending` = "rien à faire"

Le test `IdleWorker` dans `scheduler.rs` confirme le pattern. La sémantique
est contre-intuitive (doc 08 §2) mais **fonctionnellement correcte**. Pas de
changement nécessaire — renommage cosmétique différé.

### Étape 2 — `MergeState` state machine ✅ (code écrit, tests à lancer)

**Nouveau fichier** : `src/indexer/merge_state.rs`

Structure :
```
MergeState {
    index, merger, serializer, merged_segment, delete_cursor,
    doc_id_mapping, fieldnorm_readers, phase, indexed_fields
}

MergePhase::Init       → mapping + fieldnorms (1 step)
MergePhase::Postings   → 1 step par champ indexé
MergePhase::Store      → 1 step
MergePhase::FastFields → 1 step
MergePhase::Close      → finalisation, retourne SegmentEntry
```

- `MergeState::new(index, segment_entries, target_opstamp)` — même interface
  que l'ancien `merge()`, fait advance_deletes + création merger/serializer
- `MergeState::step() -> StepResult` — avance d'une phase ou d'un champ
- `StepResult::Continue` / `StepResult::Done(Option<SegmentEntry>)`
- `merge_incremental()` — wrapper synchrone qui boucle sur step()

**Granularité** : par champ indexé (pas par terme). Les `TermMerger` et
`FieldSerializer` ont des lifetimes qui empruntent des données internes,
rendant le stockage inter-step impossible en safe Rust. Champ-par-champ est
un bon compromis : pour un schéma typique (5-20 champs, 1-3 indexés), ça
donne ~7 yield points au lieu de 0 actuellement.

**Modifications `merger.rs`** : visibilité `pub(crate)` sur les champs
`schema`, `max_doc` et les méthodes `write_fieldnorms`,
`write_postings_for_field`, `write_postings`, `write_storable_fields`,
`write_fast_fields`.

### Étape 3 — Intégration dans SegmentUpdaterActor ✅ (code écrit, tests à lancer)

**Fichier modifié** : `src/indexer/segment_updater_actor.rs`

Changements :
- Nouveaux champs : `active_merge: Option<ActiveMerge>`,
  `pending_merges: VecDeque<MergeOperation>`
- **`poll_idle()`** implémenté : appelle `state.step()`, gère Continue/Done,
  lance le merge suivant quand un se termine
- **`enqueue_merge_candidates()`** remplace l'ancien `consider_merge_options()`
  blocking — collecte les candidats et les met en file d'attente
- **`start_next_incremental_merge()`** démarre le prochain merge de la queue
- **`finish_incremental_merge()`** appelle `do_end_merge`, re-collecte les
  candidats (l'état des segments a pu changer)
- **Merges explicites** (`StartMerge` depuis `IndexWriter::merge()`) restent
  **blocking** dans le handler — backward compatible
- **Merges automatiques** (via AddSegment/Commit) sont **incrémentaux** via
  poll_idle

**Module** : `mod merge_state` ajouté dans `src/indexer/mod.rs`.

### Compilation ✅

`cargo check` passe sans erreur. 2 warnings "unused" attendus :
- `estimated_steps()` — pour observabilité future
- `merge_incremental()` — wrapper synchrone pas encore câblé

### Tests ⏳

`cargo test` lancé mais bloqué (lock Cargo entre deux instances concurrentes).
Processus killés. **À relancer proprement.**

---

## Prochaines actions

1. **Lancer `cargo test`** — valider que les 1085 tests passent toujours
2. **Si échecs** : probablement liés au fait que `enqueue_merge_candidates`
   ne fait plus de merge blocking immédiat — certains tests pourraient
   dépendre du fait que les merges automatiques sont terminés après un commit.
   Si c'est le cas, il faudra soit :
   - Ajouter un drain des merges incrémentaux dans `wait_merging_threads`
   - Ou garder le mode blocking pour les tests (feature flag)
3. **Étape 4** — Events d'observabilité merge (MergeStarted, etc.)
4. **Étape 5** — Benchmarks avant/après

---

## Risques identifiés

### Tests qui dépendent du merge synchrone

L'ancien `consider_merge_options()` exécutait les merges blocking dans le
handler. Certains tests vérifient l'état des segments immédiatement après un
commit — avec le merge incrémental, les merges se font en background via
poll_idle et pourraient ne pas être terminés au moment de l'assertion.

**Mitigation** : `wait_merging_threads()` doit attendre la fin des merges
incrémentaux. Ou bien certains tests utilisent `NoMergePolicy` et ne sont
pas affectés.

### Granularité champ vs terme

Pour un schéma avec un seul gros champ texte (millions de termes), le merge
bloque toujours pendant toute la durée de ce champ. C'est une limitation
documentée dans le doc 02. Solution future : unsafe self-referential struct
ou refactoring de TermMerger pour le rendre owned.
