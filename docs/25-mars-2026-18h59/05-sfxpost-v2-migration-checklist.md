# Doc 05 — sfxpost V2 : checklist de migration

Date : 25 mars 2026

## Problème actuel

Le collector écrit en V2 ("SFP2" magic) mais le merger et le validateur
lisent toujours en V1. Résultat : SIGSEGV au merge car `SfxPostingsReader::open()`
(V1) ne reconnaît pas le magic "SFP2" → retourne Err → le reader est None →
le merger considère que le segment n'a pas de sfxpost → erreur d'intégrité.

## Points à porter vers V2

### 1. Lecture des sfxpost source dans le merger (BLOQUANT)

**Fichier** : `src/indexer/merger.rs` lignes 735-737
**Fichier** : `src/indexer/sfx_merge.rs` lignes 223-226

```rust
// Actuel — V1 seulement
let sfxpost_readers: Vec<Option<SfxPostingsReader<'_>>> = segment_sfxpost
    .iter()
    .map(|opt| opt.as_ref().and_then(|b| SfxPostingsReader::open(b).ok()))
    .collect();
```

**Fix** : créer une enum/wrapper qui détecte le format et délègue :
```rust
enum SfxPostReader {
    V1(SfxPostingsReader),   // legacy &[u8] borrowed
    V2(SfxPostReaderV2),     // owned Vec<u8>
}

impl SfxPostReader {
    fn open(data: &[u8]) -> Option<Self> {
        if let Some(v2) = SfxPostReaderV2::open_slice(data) {
            return Some(Self::V2(v2));
        }
        SfxPostingsReader::open(data).ok().map(Self::V1)
    }

    fn entries(&self, ordinal: u32) -> Vec<SfxPostingEntry> {
        match self { ... }
    }
}
```

**Impact** : les deux fichiers utilisent `.entries(ordinal)` → l'interface est la même.
Le wrapper est transparent.

### 2. Écriture des sfxpost dans le merger (IMPORTANT)

**Fichier** : `src/indexer/merger.rs` lignes 757-796
**Fichier** : `src/indexer/sfx_merge.rs` lignes 247-295

Le merger écrit les sfxpost mergées en V1 (encode_vint inline). Il faut les
écrire en V2 via `SfxPostWriterV2`.

```rust
// Actuel — V1 inline
for &(doc_id, ti, byte_from, byte_to) in &merged {
    encode_vint(doc_id, &mut posting_bytes);
    ...
}

// Nouveau — V2
let mut writer = SfxPostWriterV2::new(unique_tokens.len());
for (new_ord, token) in unique_tokens.iter().enumerate() {
    for &(doc_id, ti, bf, bt) in &merged_per_token[new_ord] {
        writer.add_entry(new_ord as u32, doc_id, ti, bf, bt);
    }
}
sfxpost_data = Some(writer.finish());
```

**Note** : il y a DEUX chemins de merge :
- `merger.rs` lignes 757-796 — merge inline (ancien code path)
- `sfx_merge.rs` lignes 247-295 — merge via DAG (nouveau code path)

Les deux doivent être portés.

### 3. Validation des sfxpost (IMPORTANT)

**Fichier** : `src/indexer/sfx_merge.rs` lignes 313-370 (`validate_sfxpost`)

Le validateur assume le format V1 (lit num_tokens à offset 0, offset table à offset 4, etc.).
Avec V2, le magic est à offset 0, num_tokens à offset 4.

**Fix** : détecter le format dans `validate_sfxpost` :
```rust
fn validate_sfxpost(data: &[u8], num_docs: u32, num_tokens: u32) -> Option<String> {
    if data.starts_with(b"SFP2") {
        validate_sfxpost_v2(data, num_docs, num_tokens)
    } else {
        validate_sfxpost_v1(data, num_docs, num_tokens)
    }
}
```

### 4. Posting resolver (DÉJÀ FAIT ✅)

**Fichier** : `src/query/posting_resolver.rs`

Le `build_resolver` détecte déjà V2 vs V1. ✅

### 5. Collector (DÉJÀ FAIT ✅)

**Fichier** : `src/suffix_fst/collector.rs`

Le collector écrit déjà en V2. ✅

## Ordre de migration

```
1. Wrapper SfxPostReader (enum V1/V2)           ← DÉBLOQUE le merge
2. Écriture V2 dans merger.rs                    ← segments mergés en V2
3. Écriture V2 dans sfx_merge.rs                 ← merge DAG en V2
4. Validation V2 dans validate_sfxpost           ← validation correcte
5. Tests E2E : create → commit → merge → search  ← valide la chaîne complète
```

## Fichiers touchés (résumé)

| Fichier | Lecture V2 | Écriture V2 | Validation V2 |
|---------|-----------|-------------|---------------|
| `suffix_fst/collector.rs` | — | ✅ | — |
| `suffix_fst/sfxpost_v2.rs` | ✅ | ✅ | — |
| `query/posting_resolver.rs` | ✅ | — | — |
| `indexer/merger.rs` | **À FAIRE** | **À FAIRE** | — |
| `indexer/sfx_merge.rs` | **À FAIRE** | **À FAIRE** | **À FAIRE** |
| `indexer/sfx_dag.rs` | — | — (passe les bytes) | — |

## Risques

- Les index persistants sur disque (bench 90K) ont des segments V1. Après la migration,
  le merge lira V1 en entrée et écrira V2 en sortie. Les segments post-merge seront V2.
  Les vieux segments V1 restent lisibles tant qu'ils ne sont pas mergés.

- Le SIGSEGV actuel vient du merger qui crash quand il ne peut pas ouvrir le sfxpost V2.
  Le fix est simple (wrapper enum) mais touche 2 fichiers critiques.
