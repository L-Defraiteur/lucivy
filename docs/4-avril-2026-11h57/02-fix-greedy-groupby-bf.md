# 02 — Fix greedy chain : group-by byte_from avec sélection du meilleur tri_idx

Date : 4 avril 2026

---

## Problème (rappel)

Le greedy scan de `intersect_trigrams_with_threshold` traite chaque entry individuellement, triées par byte_from. Quand un même byte_from a plusieurs tri_idx (mots répétés dans la query), le greedy saute au mauvais tri_idx et casse la chaîne.

## Solution

Grouper les entries par byte_from. Dans chaque groupe, pick le **plus petit tri_idx > dernier tri_idx de la chaîne**. Un seul élément par groupe entre dans la chaîne.

### Avant (greedy naïf)

```rust
for &(tri_idx, bf, bt, si) in &entries {
    if current_chain.is_empty() || tri_idx > current_chain.last().unwrap().0 {
        current_chain.push((tri_idx, bf, bt, si));
    } else {
        check_chain(&current_chain, &mut results);
        current_chain.clear();
        current_chain.push((tri_idx, bf, bt, si));
    }
}
```

### Après (group-by bf)

```rust
// Group entries by byte_from
let mut i = 0;
while i < entries.len() {
    let bf = entries[i].1;
    let group_start = i;
    while i < entries.len() && entries[i].1 == bf { i += 1; }
    let group = &entries[group_start..i];

    let last_tri = current_chain.last().map(|e| e.0);

    // Pick smallest tri_idx in group that continues the chain
    let best = group.iter()
        .filter(|e| last_tri.map_or(true, |last| e.0 > last))
        .min_by_key(|e| e.0);

    if let Some(&entry) = best {
        current_chain.push(entry);
    }
    // Si aucun tri_idx ne continue → skip le groupe (pas de break)
}
check_chain(&current_chain, &mut results);
```

### Gestion du break de chaîne

Quand aucun tri_idx du groupe ne continue la chaîne, on **skip** le groupe au lieu de casser la chaîne. Raison : un trigram absent n'est pas une rupture — le threshold autorise déjà des trous (on a besoin de 39/43, pas 43/43). Le check_chain en fin valide le threshold.

Par contre si un groupe a un tri_idx **inférieur** au dernier de la chaîne et que c'est le seul choix, c'est une vraie fin de suite : on check la chaîne en cours et on en recommence une nouvelle.

Correction : en fait non. Un groupe avec uniquement des tri_idx ≤ last_tri signifie qu'on a vu ces trigrams à un byte_from antérieur. On peut les ignorer. Le skip suffit.

## Fichier modifié

`src/query/phrase_query/literal_resolve.rs` — fonction `intersect_trigrams_with_threshold`, lignes 247-260.

## Tests

- test_playground_repro : la query multi-token d=1 avec "WASM" répété doit maintenant trouver
- test_fuzzy_ground_truth : non-régression
- cargo test --lib : non-régression
