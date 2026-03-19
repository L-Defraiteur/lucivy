# Diagnostic — Segments mergés invisibles au search

Date : 19 mars 2026
Status : cause racine identifiée, fix proposé

## Symptôme

`contains 'mutex'` retourne 36 hits au lieu de ~600. Les segments mergés
(>500 docs) ont un FST reconstruit (num_suffix_terms > 0) mais
`prefix_walk("mutex")` retourne 0 résultats. Les petits segments
(non-mergés) fonctionnent parfaitement.

## Chaîne de diagnostic

### 1. Ground truth vs term dict (diagnostics.rs)
```
Term "mutex" — doc_freq=606 | ground_truth=610 (MISMATCH)
Term "lock"  — doc_freq=1102 | ground_truth=2455 (MISMATCH)
```
Le ground truth cherche la substring dans les stored docs. Le doc_freq
compte le token exact dans le term dict. La différence "lock" (1102 vs 2455)
est normale — "lock" comme substring apparaît dans "block", "clock", etc.
La petite différence "mutex" (606 vs 610) indique quelques docs manquants.

### 2. SFX path diagnostic
```
shard_0 seg c5c160af (500 docs): walk=0 parents=0 → 0 docs  ← MERGÉ
shard_1 seg 9bad39b7 (129 docs): walk=2 parents=2 → 15 docs ← OK
```
Les segments mergés ont 0 walk hits. Les petits segments fonctionnent.

### 3. Key dump
```
Segment c5c160af (500 docs):
  Term dict: num_terms=23481 stream_count=23481  ← term dict OK
  No SFX file                                     ← SFX non chargé !
```
Le term dict a 23481 termes. Mais `load_sfx_files` dit "No SFX file".

### 4. Vérification fichiers sur disque
```
c5c160af.1.sfx      → num_suffix_terms=516   (FST valide)
c5c160af.2.sfx      → num_suffix_terms=23481 (FST valide)
c5c160af.sfx         → manifest (fields 1 et 2)
c5c160af.1.sfxpost  → existe
c5c160af.2.sfxpost  → existe
```
Tous les fichiers existent et ont un FST valide (num_terms > 0).

## Cause racine

Le `rebuild_deferred_sfx()` reconstruit les .sfx via `atomic_write`
(rename atomique du fichier). Mais le `SegmentReader` qui est ouvert
ensuite par `reader.reload()` utilise le **mmap cache** de MmapDirectory.

Le problème : `load_sfx_files()` ouvre les .sfx et vérifie
`num_suffix_terms`. Si le mmap cache retourne l'**ancien** contenu
(avant rebuild, avec num_terms=0), le fichier est considéré "deferred"
et skippé. Le nouveau contenu sur disque (avec num_terms > 0) n'est
jamais lu.

Mais il y a un problème plus fondamental : **pourquoi les segments
deferred existent-ils après le rebuild ?**

### Séquence des événements

1. Ingestion : `commit_fast()` × N → merges async avec `merge_sfx_deferred`
   → segments mergés ont FST vide (num_terms=0)

2. Final `commit()` :
   a. `drain_all_merges()` → exécute les merges pending → **crée de
      NOUVEAUX segments deferred** (car merge_sfx_deferred est utilisé)
   b. `rebuild_deferred_sfx()` → scanne les segments, reconstruit les FST
   c. `save_metas()`

3. `reader.reload()` → les .sfx sur disque sont valides MAIS le mmap
   cache peut retourner le contenu pré-rebuild

### Double problème

1. **Le drain crée des deferred** : `drain_all_merges()` utilise
   `merge_sfx_deferred` (car c'est le code dans `step_sfx`). Les segments
   créés par le drain sont deferred. Le rebuild les corrige, mais le
   timing est fragile.

2. **Le mmap cache est stale** : même si le rebuild écrit le bon contenu,
   le reader peut voir l'ancien via le cache.

## Fix proposé : merge_sfx complet dans le drain

La solution la plus propre : pendant `drain_all_merges()` dans un
`commit(rebuild_sfx: true)`, les merges doivent utiliser `merge_sfx`
**complet** (pas deferred). Comme ça les segments produits par le drain
ont un FST valide immédiatement, sans besoin de rebuild.

### Implémentation

#### Option A : flag global sur le merger (recommandée)

Ajouter un flag `use_deferred_sfx: bool` au `MergeState` :

```rust
pub(crate) struct MergeState {
    // ...
    use_deferred_sfx: bool,
}

fn step_sfx(&mut self) -> crate::Result<StepResult> {
    let serializer = self.serializer.as_mut().unwrap();
    let doc_mapping = self.sfx_doc_mapping.take().unwrap_or_default();
    if self.use_deferred_sfx {
        self.merger.merge_sfx_deferred(serializer, &doc_mapping)?;
    } else {
        self.merger.merge_sfx(serializer, &doc_mapping)?;
    }
    self.phase = MergePhase::Close;
    Ok(StepResult::Continue)
}
```

Le flag est `true` par défaut (commit_fast path). Le `drain_all_merges()`
dans un commit avec `rebuild_sfx: true` set le flag à `false` pour
les merges qu'il exécute, forçant le FST complet.

#### Avantages

- Pas besoin de `rebuild_deferred_sfx` du tout — le drain produit des
  segments complets directement
- Pas de problème de mmap cache — le FST est écrit une fois, correctement
- Simple, prévisible, pas de double-pass

#### Coût

Le drain avec FST complet prend plus de temps (le FST rebuild O(E log E)
est fait pendant le drain). Mais c'est le dernier commit — on veut un
index complet, pas un index rapide-mais-incomplet.

Estimation : pour 90K docs, le drain avec FST complet prendrait ~30-60s
au lieu de ~5s. Acceptable pour un commit final.

#### Option B : supprimer rebuild_deferred_sfx entièrement

Avec l'option A, `rebuild_deferred_sfx` n'est plus nécessaire. Le seul
moment où on a besoin de FST complets c'est au `commit()`, et le drain
les produit directement.

On peut garder `rebuild_deferred_sfx` comme fallback de sécurité
(log warning si des segments deferred sont détectés après le drain)
mais il ne devrait jamais se déclencher.

### Changements nécessaires

1. `merge_state.rs` : ajouter `use_deferred_sfx: bool` au MergeState,
   utiliser dans `step_sfx()`
2. `segment_updater_actor.rs` : `drain_all_merges()` passe
   `use_deferred_sfx: false` aux MergeStates qu'il crée
3. Supprimer ou simplifier `rebuild_deferred_sfx` (garder comme safety check)
4. Supprimer le code de skip/rebuild dans `load_sfx_files` (plus besoin)

### Fichiers concernés

| Fichier | Changement |
|---------|------------|
| `src/indexer/merge_state.rs` | flag use_deferred_sfx dans MergeState + step_sfx |
| `src/indexer/segment_updater_actor.rs` | drain passe use_deferred_sfx=false |
| `src/index/segment_reader.rs` | simplifier load_sfx_files (plus de skip) |
| `lucivy_core/src/diagnostics.rs` | garder les outils de diagnostic |

## Outils de diagnostic créés

### diagnostics.rs (lucivy_core)

- `inspect_term(handle, field, term)` → TermReport (doc_freq par segment)
- `inspect_term_verified(...)` → + ground truth count depuis stored docs
- `inspect_sfx(handle, field, term)` → SfxTermReport (prefix_walk → parents → docs)
- `dump_segment_keys(handle, field, n)` → term dict keys + FST probes par segment
- `inspect_segments(handle)` → SegmentSummary (num_docs, sfx status)
- Versions `_sharded` pour ShardedHandle

### Bench post-mortem

- `LUCIVY_VERIFY=1` active la vérification ground truth (lent mais exhaustif)
- Section "Key dump" compare term dict vs FST pour le premier shard
- Section "SFX path diagnostic" trace le chemin complet prefix_walk → docs

## Problèmes restants

### Lock file non relâché après bench

Le bench ne close pas proprement les ShardedHandle → les lock files restent.
Le script `test_diagnostics.rs` (read-only post-mortem) ne peut pas ouvrir
l'index via `LucivyHandle::open` car il crée un writer qui veut le lock.

**À faire** :
- Ajouter un mode read-only à `LucivyHandle` (open sans writer)
- Ou ajouter `handle.close()` dans le bench cleanup
- Ou faire les diagnostics sur un `Index::open` + `IndexReader` directement

### Query times élevées (350-450ms)

Sur 90K docs les queries prennent 350-450ms au lieu de 30ms. C'est parce
que le SFX FST des gros segments (200K+ suffix terms) est traversé en entier
par `prefix_walk`. À optimiser :
- Limiter le nombre de résultats dans prefix_walk
- Utiliser les posting lists plus tôt pour filtrer
- Ou pré-filtrer par doc_freq
