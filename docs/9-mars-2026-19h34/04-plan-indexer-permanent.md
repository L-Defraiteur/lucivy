# Plan : Indexer thread permanent dans ld-lucivy

## Contexte

Actuellement, l'`IndexWriter` de ld-lucivy utilise un pattern "drain & restart" pour
le(s) thread(s) indexeur(s) : à chaque `commit()`, le channel crossbeam est fermé, les
workers drainent leurs docs restants, sont `join`és, puis re-spawnés avec un nouveau
channel. Ce design hérité de tantivy est correct mais coûteux sur WASM/emscripten où
`pthread_create`/`pthread_join` passent par des `postMessage` et des `Atomics.wait`.

Le segment_updater (1 thread rayon) et les merge threads (pool rayon de N threads) sont
déjà permanents — ils vivent pour toute la durée de l'IndexWriter. Seul l'indexer est
éphémère.

## Objectif

Rendre le(s) thread(s) indexeur(s) **permanents et réutilisables** : ils vivent de la
création de l'IndexWriter jusqu'à son `drop()`, sans être join+respawnés à chaque commit.

## Analyse du design actuel

### Le cycle de vie actuel d'un indexer worker

```
IndexWriter::new()
  └→ start_workers() → add_indexing_worker() × N
       └→ thread::Builder::new("thrd-lucivy-index{id}").spawn(move || {
              loop {
                  peek document_receiver → None? → defuse bomb, return Ok(())
                  index_documents(mem_budget, segment, &mut iter, &segment_updater, delete_cursor)
              }
          })

IndexWriter::prepare_commit()
  └→ recreate_document_channel()      // ferme l'ancien channel
       └→ ancien sender droppé → workers voient None → sortent de la boucle
  └→ for worker in take(workers_join_handle):
       └→ worker.join()                // BLOQUE — attend que le worker finisse
       └→ add_indexing_worker()        // RESPAWN — nouveau thread, nouveau channel
  └→ PreparedCommit::new()

IndexWriter::drop()
  └→ segment_updater.kill()
  └→ drop_sender()                    // ferme channel
  └→ for worker in workers: worker.join()
```

### Pourquoi le channel est fermé au commit

Le seul mécanisme de signalisation est la **fermeture du channel** crossbeam. Quand le
sender est droppé, `receiver.iter()` retourne `None` après avoir drainé les messages
restants. C'est le signal pour le worker de terminer proprement.

Il n'y a pas de message "flush" ou de flag atomique — le channel est le signal.

### Coûts du pattern actuel

| Opération            | Coût natif | Coût WASM/emscripten           |
|----------------------|------------|--------------------------------|
| `pthread_create`     | ~µs        | postMessage + Atomics + pool   |
| `pthread_join`       | ~µs        | Atomics.wait (bloquant!)       |
| channel create+drop  | ~ns        | idem                           |

Sur emscripten, le `join()` est le problème : il fait un `Atomics.wait` qui bloque le
thread appelant. Avec le writer thread (plan 03), c'est OK car le writer thread peut
bloquer. Mais éliminer ces join/spawn rend le code plus propre et plus rapide.

### Fichiers et lignes concernés

| Fichier | Contenu clé | Lignes |
|---------|-------------|--------|
| `src/indexer/index_writer.rs` | IndexWriter struct, add_indexing_worker, prepare_commit, recreate_document_channel, start_workers, rollback, Drop | 71-815 |
| `src/indexer/index_writer_status.rs` | IndexWriterStatus, IndexWriterBomb | 8-91 |
| `src/indexer/mod.rs` | AddBatch, AddBatchSender/Receiver, PIPELINE_MAX_SIZE_IN_DOCS | 57-59, 40 |

## Architecture cible

### Principe

Au lieu de fermer le channel pour signaler "flush tes docs", on envoie un **message
sentinel** `FlushAndWait` dans le channel. Le worker le reçoit, termine son segment en
cours, signale qu'il a fini via un `oneshot` channel, puis retourne attendre le prochain
batch de documents — sans jamais quitter sa boucle.

```
Avant (drain & restart):
────────────────────────
  add_document ──→ [channel] ──→ worker loop { recv → index }
  commit:
    drop channel → worker exits → join → spawn new worker + new channel

Après (permanent + flush sentinel):
────────────────────────────────────
  add_document ──→ [channel] ──→ worker loop { recv → index | Flush → signal done, continue }
  commit:
    send Flush(oneshot_sender) → recv oneshot → done (worker toujours vivant)
```

### Nouveau type de message

```rust
// Avant : le channel transporte uniquement des AddBatch<D>
type AddBatch<D> = SmallVec<[AddOperation<D>; 4]>;
type AddBatchSender<D> = channel::Sender<AddBatch<D>>;

// Après : le channel transporte un enum qui inclut le flush
enum WorkerMessage<D: Document> {
    /// Batch de documents à indexer.
    Docs(SmallVec<[AddOperation<D>; 4]>),
    /// Signal de flush : le worker doit finir son segment en cours et signaler via le sender.
    Flush(oneshot::Sender<crate::Result<()>>),
    /// Signal d'arrêt : le worker quitte sa boucle (utilisé par Drop).
    Shutdown,
}

type WorkerSender<D> = channel::Sender<WorkerMessage<D>>;
type WorkerReceiver<D> = channel::Receiver<WorkerMessage<D>>;
```

### Nouvelle boucle du worker

```rust
fn worker_loop<D: Document>(
    receiver: WorkerReceiver<D>,
    segment_updater: SegmentUpdater,
    index: Index,
    mem_budget: usize,
    mut delete_cursor: DeleteCursor,
    bomb: IndexWriterBomb<D>,
) -> crate::Result<()> {
    loop {
        // Attendre le premier message
        let first = match receiver.recv() {
            Ok(msg) => msg,
            Err(_) => {
                // Channel fermé (Drop sans Shutdown explicite) — sortir proprement
                bomb.defuse();
                return Ok(());
            }
        };

        match first {
            WorkerMessage::Shutdown => {
                bomb.defuse();
                return Ok(());
            }
            WorkerMessage::Flush(done_tx) => {
                // Pas de segment en cours → signaler immédiatement
                let _ = done_tx.send(Ok(()));
                continue;
            }
            WorkerMessage::Docs(batch) => {
                if batch.is_empty() { continue; }
                delete_cursor.skip_to(batch[0].opstamp);

                // Créer un segment et indexer
                let segment = index.new_segment();
                let mut segment_writer = SegmentWriter::for_segment(mem_budget, segment.clone())?;

                // Indexer le premier batch
                for doc in batch {
                    segment_writer.add_document(doc)?;
                }

                // Continuer à recevoir des batches jusqu'à :
                // - Flush reçu → finaliser le segment, signaler, continuer la boucle externe
                // - Shutdown reçu → finaliser le segment, retourner
                // - Budget mémoire atteint → finaliser le segment, continuer
                // - Channel vide (try_recv) → continuer à attendre
                let flush_done = loop {
                    // Vérifier le budget mémoire
                    if segment_writer.mem_usage() >= mem_budget - MARGIN_IN_BYTES {
                        break None; // Flush le segment, reprendre la boucle externe
                    }

                    match receiver.recv() {
                        Ok(WorkerMessage::Docs(batch)) => {
                            if batch.is_empty() { continue; }
                            delete_cursor.skip_to(batch[0].opstamp);
                            for doc in batch {
                                segment_writer.add_document(doc)?;
                            }
                        }
                        Ok(WorkerMessage::Flush(done_tx)) => {
                            break Some(done_tx);
                        }
                        Ok(WorkerMessage::Shutdown) => {
                            // Finaliser le segment avant de quitter
                            finalize_segment(segment, segment_writer, &segment_updater, &mut delete_cursor)?;
                            bomb.defuse();
                            return Ok(());
                        }
                        Err(_) => {
                            // Channel fermé — finaliser et quitter
                            finalize_segment(segment, segment_writer, &segment_updater, &mut delete_cursor)?;
                            bomb.defuse();
                            return Ok(());
                        }
                    }
                };

                // Finaliser le segment (écrire sur disque + enregistrer auprès du segment_updater)
                if !segment_updater.is_alive() { return Ok(()); }
                finalize_segment(segment, segment_writer, &segment_updater, &mut delete_cursor)?;

                // Si on a reçu un Flush, signaler qu'on a fini
                if let Some(done_tx) = flush_done {
                    let _ = done_tx.send(Ok(()));
                }
            }
        }
    }
}

fn finalize_segment<D: Document>(
    segment: Segment,
    segment_writer: SegmentWriter,
    segment_updater: &SegmentUpdater,
    delete_cursor: &mut DeleteCursor,
) -> crate::Result<()> {
    let max_doc = segment_writer.max_doc();
    if max_doc == 0 { return Ok(()); }
    let doc_opstamps = segment_writer.finalize()?;
    let segment_with_max_doc = segment.with_max_doc(max_doc);
    let alive_bitset = apply_deletes(&segment_with_max_doc, delete_cursor, &doc_opstamps)?;
    let meta = segment_with_max_doc.meta().clone();
    meta.untrack_temp_docstore();
    let entry = SegmentEntry::new(meta, delete_cursor.clone(), alive_bitset);
    segment_updater.schedule_add_segment(entry).wait()?;
    Ok(())
}
```

### prepare_commit modifié

```rust
pub fn prepare_commit(&mut self) -> crate::Result<PreparedCommit<'_, D>> {
    info!("Preparing commit");

    // Envoyer Flush à chaque worker et collecter les résultats
    let mut flush_receivers = Vec::new();
    for _ in 0..self.options.num_worker_threads {
        let (tx, rx) = oneshot::channel();
        self.operation_sender.send(WorkerMessage::Flush(tx))
            .map_err(|_| error_in_index_worker_thread("Failed to send Flush"))?;
        flush_receivers.push(rx);
    }

    // Attendre que tous les workers aient fini de flusher
    for rx in flush_receivers {
        rx.recv()
            .map_err(|_| error_in_index_worker_thread("Flush receiver disconnected"))?
            .map_err(|e| error_in_index_worker_thread(&format!("Flush failed: {e}")))?;
    }

    // Plus besoin de recreate_document_channel ni de join/respawn !

    let commit_opstamp = self.stamper.stamp();
    let prepared_commit = PreparedCommit::new(self, commit_opstamp);
    info!("Prepared commit {commit_opstamp}");
    Ok(prepared_commit)
}
```

### Drop modifié

```rust
impl<D: Document> Drop for IndexWriter<D> {
    fn drop(&mut self) {
        self.segment_updater.kill();

        // Envoyer Shutdown à chaque worker
        for _ in 0..self.options.num_worker_threads {
            let _ = self.operation_sender.send(WorkerMessage::Shutdown);
        }

        // Joindre les workers (ils vont sortir proprement après Shutdown)
        for handle in self.workers_join_handle.drain(..) {
            let _ = handle.join();
        }
    }
}
```

### Rollback modifié

```rust
pub fn rollback(&mut self) -> crate::Result<Opstamp> {
    info!("Rolling back to opstamp {}", self.committed_opstamp);

    self.segment_updater.kill();

    // Prendre le lock du directory pour la reconstruction
    let directory_lock = self._directory_lock.take()
        .expect("IndexWriter has no lock");

    // Les workers actuels seront droppés avec l'ancien self
    // (le Drop enverra Shutdown + join)

    // Créer un nouvel IndexWriter (avec de nouveaux workers permanents)
    *self = IndexWriter::new(&self.index, self.options.clone(), directory_lock)?;

    Ok(self.committed_opstamp)
}
```

## Plan d'implémentation

### Phase 1 : Nouveau type de message (`WorkerMessage<D>`)

**Fichier :** `src/indexer/mod.rs`

1. Ajouter `oneshot` comme dépendance (déjà utilisé par `FutureResult`)
2. Créer l'enum `WorkerMessage<D>` avec `Docs`, `Flush`, `Shutdown`
3. Remplacer les type aliases `AddBatchSender`/`AddBatchReceiver`
   par `WorkerSender<D>`/`WorkerReceiver<D>`

**Impact :** Change le type du channel → tout ce qui envoie/reçoit doit s'adapter.

### Phase 2 : Adapter `add_document` / `run` pour envoyer `WorkerMessage::Docs`

**Fichier :** `src/indexer/index_writer.rs`

Partout où on fait `self.operation_sender.send(batch)`, wrapper dans
`WorkerMessage::Docs(batch)`.

Fonctions impactées :
- `send_add_documents_batch()` (~ligne 759)
- `run()` (~ligne 768) — envoi par batches

### Phase 3 : Réécrire `add_indexing_worker` → worker permanent

**Fichier :** `src/indexer/index_writer.rs`

Remplacer la boucle actuelle (lignes 414-462) par la nouvelle `worker_loop` qui :
- Reçoit `WorkerMessage` au lieu de `AddBatch`
- Gère `Flush(oneshot::Sender)` en finalisant le segment et signalant
- Gère `Shutdown` en finalisant et sortant
- Ne sort **jamais** sur channel fermé pendant le fonctionnement normal

Extraire `finalize_segment()` comme fonction helper (la logique est actuellement inline
dans `index_documents`, lignes 182-226).

### Phase 4 : Réécrire `prepare_commit` — Flush au lieu de drain+restart

**Fichier :** `src/indexer/index_writer.rs`

Remplacer la séquence actuelle (lignes 618-649) :
```rust
// AVANT
recreate_document_channel();
for worker in take(workers_join_handle) {
    worker.join()?;
    add_indexing_worker()?;
}
```

Par :
```rust
// APRÈS
for _ in 0..num_workers {
    send(Flush(oneshot_tx));
}
for rx in flush_receivers {
    rx.recv()??;
}
```

**Supprimer** `recreate_document_channel()` — elle n'est plus nécessaire pour le commit.

### Phase 5 : Réécrire `Drop` et `rollback`

**Fichier :** `src/indexer/index_writer.rs`

- `Drop` : envoyer `Shutdown` × N → join workers
- `rollback` : laisser le `Drop` de l'ancien `self` gérer le shutdown des anciens
  workers, le `new()` crée les nouveaux workers permanents

### Phase 6 : Adapter `IndexWriterStatus` / bomb

**Fichier :** `src/indexer/index_writer_status.rs`

Le pattern bomb reste utile pour détecter les paniques dans les workers. Mais le
`operation_receiver()` doit retourner un `WorkerReceiver<D>` au lieu d'un
`AddBatchReceiver<D>`.

Le channel n'est plus recréé à chaque commit → `IndexWriterStatus::from()` est appelé
une seule fois à la construction.

### Phase 7 : Tests

1. **Tests unitaires existants** — doivent tous passer (le comportement observable est
   identique : add → commit → search fonctionne pareil)

2. **Nouveau test : multiple commits sans re-open**
   ```rust
   let mut writer = index.writer_with_num_threads(1, 50_000_000)?;
   for i in 0..5 {
       writer.add_document(doc!(field => format!("doc {i}")))?;
       writer.commit()?;
   }
   // Vérifier que les 5 docs sont là
   ```

3. **Nouveau test : flush avec segment vide** (commit sans avoir ajouté de docs)
   ```rust
   writer.commit()?; // Flush sans docs → oneshot signale immédiatement
   ```

4. **Nouveau test : rollback après add**
   ```rust
   writer.add_document(doc)?;
   writer.rollback()?;
   writer.add_document(other_doc)?;
   writer.commit()?;
   // Seul other_doc est visible
   ```

## Récapitulatif des threads après changement

| Thread | Avant | Après | Lifetime |
|--------|-------|-------|----------|
| Indexer worker(s) | Éphémère (join+spawn à chaque commit) | **Permanent** | IndexWriter::new → Drop |
| Segment updater | Permanent (rayon pool 1 thread) | Inchangé | IndexWriter::new → Drop |
| Merge threads | Permanent (rayon pool N threads) | Inchangé | IndexWriter::new → Drop |

**Sur WASM avec `writer_with_num_threads(1, ...)` :**
- 1 indexer thread permanent
- 1 segment_updater thread (rayon)
- 4 merge threads (rayon, default)
- **Total : 6 threads stables**, zéro spawn/join pendant les commits

## Risques et mitigations

| Risque | Mitigation |
|--------|------------|
| Le worker reçoit Flush alors qu'il n'a pas de segment en cours | Signaler `Ok(())` immédiatement (cas géré dans la boucle) |
| Panic dans le worker pendant Flush | La bomb tue l'IndexWriter, le `oneshot::Receiver` retourne Err (sender droppé) → l'erreur remonte au caller |
| Le channel se remplit (backpressure) | Inchangé — le channel bounded crossbeam bloque `add_document` si plein, comme avant |
| `delete_cursor` doit survivre entre les commits | Le worker garde son `delete_cursor` vivant — il continue de tracker les deletes entre les commits, comme avant |
| Segments multiples par commit (si budget mémoire atteint avant Flush) | Géré : la boucle interne flush le segment et recrée un nouveau, le Flush vient plus tard |

## Compatibilité

- **API publique** : aucun changement. `IndexWriter::add_document()`, `commit()`,
  `rollback()`, `prepare_commit()` gardent les mêmes signatures.
- **Comportement observable** : identique. Les documents sont indexés et commités de la
  même façon, les segments sont créés avec la même logique de budget mémoire.
- **Tous les bindings** : aucun changement côté emscripten, nodejs, python, cpp, wasm.
- **Tests existants** : doivent passer sans modification.

## Relation avec le plan 03 (writer thread WASM)

Ce plan est **complémentaire** au plan 03. Le writer thread résout le deadlock en
déportant les mutations sur un pthread permanent. L'indexer permanent élimine les
`pthread_create`/`pthread_join` à chaque commit, rendant les mutations sur le writer
thread encore plus rapides.

Ordre recommandé :
1. **Plan 04 d'abord** (ce plan) — modifie ld-lucivy core, bénéficie à tous les bindings
2. **Plan 03 ensuite** — modifie le binding emscripten uniquement, utilise l'indexer permanent
