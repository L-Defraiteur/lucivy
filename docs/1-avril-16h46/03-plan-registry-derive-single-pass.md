# 03 — Plan : registry dérivé single-pass pour WriteSfxNode

Date : 1 avril 2026

## Problème actuel

WriteSfxNode reconstruit posmap/bytemap/termtexts manuellement (3 boucles
séparées sur tokens+sfxpost) et oublie le sepmap. Le registry n'est jamais
utilisé au merge. Ajouter un index = modifier WriteSfxNode à la main.

## Solution : trait SfxDerivedIndex + single-pass

### Principe

Une seule boucle parcourt les données primaires (tokens, sfxpost entries, gaps).
Chaque index dérivé reçoit les événements et accumule ses données.

### Nouveau trait

```rust
/// Index dérivé des données primaires (sfxpost + tokens + gapmap).
/// Construit en une seule passe au merge et au segment initial.
pub trait SfxDerivedIndex: Send {
    fn id(&self) -> &'static str;
    fn extension(&self) -> &'static str;

    /// Called once per token in ordinal order.
    fn on_token(&mut self, _ord: u32, _text: &str) {}

    /// Called for each sfxpost entry (one token can have many entries).
    fn on_posting(&mut self, _ord: u32, _doc_id: u32, _position: u32,
                  _byte_from: u32, _byte_to: u32) {}

    /// Called for each gap between consecutive tokens in a document.
    /// ord = ordinal of the token AFTER the gap (via posmap lookup).
    fn on_gap(&mut self, _doc_id: u32, _pos: u32, _ord: u32, _gap: Option<&[u8]>) {}

    /// Serialize accumulated data to bytes.
    fn serialize(&self) -> Vec<u8>;
}
```

### Implémentations

| Index | on_token | on_posting | on_gap | serialize |
|-------|----------|------------|--------|-----------|
| PosMapIndex | — | `writer.add(doc_id, position, ord)` | — | posmap bytes |
| ByteMapIndex | `writer.record_token(ord, text.as_bytes())` | — | — | bytemap bytes |
| TermTextsIndex | `writer.add(ord, text)` | — | — | termtexts bytes |
| SepMapIndex | — | — | `writer.record_gap(ord, gap_bytes)` | sepmap bytes |

### Single-pass dans WriteSfxNode

```rust
// 1. Créer les index dérivés
let mut derived: Vec<Box<dyn SfxDerivedIndex>> = all_derived_indexes();

// 2. Passe tokens + sfxpost
for (ord, token) in tokens.iter().enumerate() {
    let ord = ord as u32;
    for d in &mut derived {
        d.on_token(ord, token);
    }
    if let Some(ref reader) = sfxpost_reader {
        for entry in reader.entries(ord) {
            for d in &mut derived {
                d.on_posting(ord, entry.doc_id, entry.token_index,
                             entry.byte_from, entry.byte_to);
            }
        }
    }
}

// 3. Passe gaps (gapmap + posmap pour l'ordinal)
// Note: posmap vient d'être construit dans la passe précédente (on_posting).
// On peut le lire ou le reconstruire en mémoire.
// Alternative: on_gap reçoit juste (doc_id, pos, gap_bytes) et le SepMapIndex
// se débrouille avec un posmap interne.

// 4. Write
for d in &derived {
    let data = d.serialize();
    if !data.is_empty() {
        write_file(segment, d.extension(), &data)?;
    }
}
```

### Quid du on_gap ?

Le sepmap a besoin de savoir quel ordinal est à quelle position pour associer
les bytes séparateurs au bon token. Deux options :

**Option A** : SepMapIndex maintient un mini-posmap interne (rempli par on_posting),
puis parcourt le gapmap lui-même dans serialize(). Avantage : pas besoin de
on_gap dans le trait. Inconvénient : SepMapIndex doit accéder au gapmap.

**Option B** : on_gap est appelé après la passe tokens+sfxpost, en parcourant
le gapmap. Le PosMapIndex (déjà rempli) fournit l'ordinal. Avantage : clean.
Inconvénient : besoin d'une deuxième passe.

**Option C** : SepMapIndex est reconstruit depuis le gapmap dans serialize(),
en recevant le gapmap_data et le posmap fraîchement construit via un contexte.

→ **Option B recommandée** : deux passes (tokens+sfxpost puis gaps), le trait
est propre, chaque index reçoit les événements qui le concernent.

### Passe gaps (option B détail)

```rust
// Après la passe tokens+sfxpost, le PosMapIndex a un writer rempli.
// On peut lui demander l'ordinal à (doc_id, pos).
// Mais on ne veut pas coupler les index entre eux.

// Alternative : on_gap reçoit l'ordinal directement.
// WriteSfxNode fait le lookup posmap → ordinal, et passe l'ordinal à on_gap.
// Le posmap est disponible car PosMapIndex::serialize() peut être appelé.
// Ou mieux : on garde le PosMapWriter en mémoire et on l'interroge.
```

En pratique, le plus simple : **SepMapIndex reçoit le gapmap + le posmap
sérialisé dans une méthode `finalize_with_context()`** :

```rust
pub trait SfxDerivedIndex: Send {
    // ... on_token, on_posting, serialize ...

    /// Optional: called after all on_token/on_posting, with access to
    /// already-built derived data (posmap, gapmap) for cross-index derivation.
    fn finalize(&mut self, _ctx: &SfxFinalizeContext) {}
}

struct SfxFinalizeContext<'a> {
    pub gapmap_data: &'a [u8],
    pub num_docs: u32,
    // posmap accessible via PosMapWriter directement
}
```

Le SepMapIndex dans `finalize()` parcourt le gapmap, utilise le posmap
(construit par PosMapIndex juste avant), et enregistre les separator bytes.

### Intégration avec le segment initial

Le même trait `SfxDerivedIndex` peut remplacer le code dans `SfxCollector::build()`.
Au lieu de construire via `SfxBuildContext` → `all_indexes()`, on ferait :

```rust
// Dans SfxCollector::build()
let mut derived = all_derived_indexes();
for (ord, token) in tokens.iter().enumerate() {
    for d in &mut derived { d.on_token(ord, token); }
    for entry in sfxpost.entries(ord) {
        for d in &mut derived { d.on_posting(...); }
    }
}
for d in &mut derived { d.finalize(&ctx); }
for d in &derived {
    registry_files.push((d.extension(), d.serialize()));
}
```

→ **Une seule implémentation pour segment initial ET merge.**

### Index primaires (hors registry dérivé)

Les index primaires restent gérés par les nodes DAG dédiés :

| Index | Node DAG | Raison |
|-------|----------|--------|
| .sfx (FST) | BuildFstNode | O(E log E), parallélisable |
| .sfxpost | MergeSfxpostNode | N-way merge complexe |
| .gapmap | CopyGapmapNode | Copy + remap doc_ids |
| .sibling | MergeSiblingLinksNode | OR-merge + remap ordinals |

Ces 4 sont les **inputs** de la passe single-pass. Ils ne changent pas.

### Étapes d'implémentation

1. **Créer le trait `SfxDerivedIndex`** dans `index_registry.rs`
   - `on_token`, `on_posting`, `finalize`, `serialize`
   - `all_derived_indexes()` retourne les 4 implémentations

2. **Implémenter pour chaque index dérivé** :
   - PosMapIndex : on_posting → PosMapWriter
   - ByteMapIndex : on_token → ByteBitmapWriter
   - TermTextsIndex : on_token → TermTextsWriter
   - SepMapIndex : finalize → walk gapmap + posmap interne

3. **Modifier WriteSfxNode** : remplacer la reconstruction manuelle par
   la boucle single-pass + serialize + write

4. **Modifier SfxCollector::build()** : même boucle single-pass, retire
   le code BuildContext/all_indexes() pour les dérivés

5. **Supprimer l'ancien `SfxIndexFile::build()`/`merge()`** pour posmap,
   bytemap, termtexts, sepmap — remplacés par SfxDerivedIndex

6. **Test** : vérifier que les fichiers produits sont identiques bit-à-bit
   (ou au moins fonctionnellement équivalents) avant/après le refactoring

### Version finale : depends_on + build_from_deps

Le `finalize()` est remplacé par un système de dépendances déclaratives :

```rust
pub trait SfxDerivedIndex: Send {
    fn id(&self) -> &'static str;
    fn extension(&self) -> &'static str;

    fn on_token(&mut self, _ord: u32, _text: &str) {}
    fn on_posting(&mut self, _ord: u32, _doc_id: u32, _pos: u32, _bf: u32, _bt: u32) {}

    fn depends_on(&self) -> Vec<&'static str> { vec![] }
    fn build_from_deps(&mut self, _ctx: &SfxDeriveContext) {}

    fn serialize(&self) -> Vec<u8>;
}

pub struct SfxDeriveContext<'a> {
    pub derived: &'a HashMap<String, Vec<u8>>,  // index déjà sérialisés
    pub gapmap_data: &'a [u8],                  // données primaires du DAG
    pub num_docs: u32,
}
```

SepMapIndex :
- `depends_on() → ["posmap"]`
- `build_from_deps()` : ouvre PosMapReader (dérivé) + GapMapReader (primaire),
  walk chaque doc/position, lookup ordinal, record separator bytes.

### Résultat attendu

- WriteSfxNode utilise le registry ✅
- Ajouter un index dérivé = implémenter SfxDerivedIndex + ajouter à all_derived_indexes() ✅
- Sepmap inclus au merge ✅
- Une seule passe sur tokens+sfxpost (au lieu de 3-4 boucles séparées) ✅
- Même code pour segment initial et merge ✅
- Dépendances inter-index déclaratives, zero coupling ✅
