# 01 — Fix greedy chain builder : LIS pour queries avec mots répétés

Date : 4 avril 2026

---

## Problème

La query fuzzy multi-token `"Build rag3weaver Rust static lib for WASM emscripten Only used in WASM builds Native"` (d=1) retourne 0 résultats, alors que d=0 trouve correctement.

### Cause racine

`intersect_trigrams_with_threshold` utilise un **greedy scan** pour construire des chaînes de trigrams avec tri_idx strictement croissant. Quand la query contient des **mots répétés** (ici "WASM" × 2, "build"/"builds", "static"/"native" → "ati"), un même byte_from dans le contenu produit des entries pour **plusieurs** tri_idx.

Exemple avec "WASM" (trigrams "was", "asm") :
- 1ère occurrence query → tri_idx 10, 11
- 2ème occurrence query → tri_idx 22, 23

La 1ère occurrence "WASM" dans le contenu (byte=50) produit :
```
(tri=10, bf=50), (tri=22, bf=50), (tri=11, bf=51), (tri=23, bf=51)
```

Le greedy scan :
1. `(10, 50)` → chaîne: [10]
2. `(22, 50)` → 22 > 10, ajouté → chaîne: [10, 22]  ← saute au 2ème "wasm" !
3. `(11, 51)` → 11 < 22, **CHAÎNE CASSÉE**

Avec threshold=39/43, aucune chaîne assez longue ne se forme → 0 candidats.

### Pourquoi d=0 fonctionne

d=0 passe par `SuffixContainsQuery` → multi-token path qui tokenise la query en mots individuels et les matche indépendamment. Pas de chaîne de trigrams.

---

## Solution : Longest Increasing Subsequence (LIS)

Remplacer le greedy scan par un algorithme LIS O(n log n) (patience sorting).

### Algorithme

L'input est la liste d'entries `(tri_idx, bf, bt, si)` triée par byte_from. On cherche la plus longue sous-séquence avec tri_idx strictement croissant.

```rust
/// LIS via patience sorting — O(n log n)
/// Returns indices of the longest increasing subsequence of tri_idx values.
fn longest_increasing_subsequence(entries: &[(usize, u32, u32, u16)]) -> Vec<usize> {
    let n = entries.len();
    if n == 0 { return vec![]; }

    // tails[i] = index dans entries du plus petit tri_idx terminant une 
    // sous-séquence croissante de longueur i+1
    let mut tails: Vec<usize> = Vec::new();
    // predecessor[i] = index du prédécesseur de entries[i] dans la meilleure chaîne
    let mut predecessor: Vec<Option<usize>> = vec![None; n];

    for i in 0..n {
        let val = entries[i].0; // tri_idx
        
        // Binary search: position dans tails où val s'insère
        let pos = tails.partition_point(|&t| entries[t].0 < val);
        
        if pos == tails.len() {
            tails.push(i);
        } else {
            tails[pos] = i;
        }
        
        if pos > 0 {
            predecessor[i] = Some(tails[pos - 1]);
        }
    }

    // Reconstruct: remonter les predecessors depuis le dernier élément
    let mut result = Vec::with_capacity(tails.len());
    let mut idx = *tails.last().unwrap();
    loop {
        result.push(idx);
        match predecessor[idx] {
            Some(prev) => idx = prev,
            None => break,
        }
    }
    result.reverse();
    result
}
```

### Intégration dans intersect_trigrams_with_threshold

**Avant** (greedy scan, lignes 247-260 de `literal_resolve.rs`) :
```rust
let mut current_chain: Vec<(usize, u32, u32, u16)> = Vec::new();
for &(tri_idx, bf, bt, si) in &entries {
    if current_chain.is_empty() || tri_idx > current_chain.last().unwrap().0 {
        current_chain.push((tri_idx, bf, bt, si));
    } else {
        check_chain(&current_chain, &mut results);
        current_chain.clear();
        current_chain.push((tri_idx, bf, bt, si));
    }
}
check_chain(&current_chain, &mut results);
```

**Après** (LIS) :
```rust
let lis_indices = longest_increasing_subsequence(&entries);
let chain: Vec<(usize, u32, u32, u16)> = lis_indices.iter()
    .map(|&i| entries[i])
    .collect();
check_chain(&chain, &mut results);
```

### Gestion de multiples chaînes par document

Le LIS donne LA plus longue chaîne. Mais on peut avoir besoin de **plusieurs** chaînes par document (MAX_CHAINS_PER_DOC = 20). Deux approches :

**A. LIS unique (suffisant pour le bug)** : une seule chaîne par document. Si elle passe le threshold + span check, on a le candidat. Simple, correct, suffisant pour 99% des cas.

**B. LIS itératif** : après extraction de la 1ère chaîne, retirer ses entries et refaire un LIS pour trouver la 2ème chaîne, etc. O(k × n log n) pour k chaînes. Plus complet mais plus complexe.

**Recommandation : A.** Le cas multi-chaîne par doc est rare et le MAX_CHAINS_PER_DOC est un cap de sécurité, pas un besoin fonctionnel.

---

## Étapes d'implémentation

1. **Écrire `longest_increasing_subsequence`** dans `literal_resolve.rs`
   - Input : `&[(usize, u32, u32, u16)]` (entries triées par bf)
   - Output : `Vec<usize>` (indices des entries dans la LIS)
   - Complexité : O(n log n) temps, O(n) espace

2. **Remplacer le greedy scan** dans `intersect_trigrams_with_threshold`
   - Appeler LIS au lieu du greedy
   - Construire la chaîne depuis les indices LIS
   - Passer au check_chain existant (threshold + span check inchangés)

3. **Test : query avec mots répétés**
   - Ajouter au test_playground_repro (déjà fait) :
     - `("Build rag3weaver Rust static lib for WASM emscripten Only used in WASM builds Native", 0)` → doit trouver
     - `("Build rag3weaver Rust static lib for WASM emscripten Only used in WASM builds Native", 1)` → **doit aussi trouver**
   - Vérifier que les résultats d=1 ⊇ d=0 (monotonie)

4. **Vérifier non-régression** sur les tests existants
   - `cargo test -p lucivy-core --test test_fuzzy_ground_truth --release`
   - `cargo test -p lucivy-core --test test_playground_repro --release`
   - `cargo test --lib --release` (1155 tests)

---

## Impact performance

- **n** = nombre d'entries par doc = typiquement 20-200
- O(n log n) vs O(n) sur n=100 → ~7x plus de comparaisons
- Temps absolu : < 0.01ms par doc (binary search sur u32)
- Le chain building est < 1% du temps total fuzzy (FST + resolve + DFA dominent)
- **Impact négligeable**

---

## Fichiers modifiés

| Fichier | Modification |
|---------|-------------|
| `src/query/phrase_query/literal_resolve.rs` | Ajouter `longest_increasing_subsequence`, remplacer greedy scan |
| `lucivy_core/tests/test_playground_repro.rs` | Test cases multi-token d=0 et d=1 (déjà ajoutés) |
