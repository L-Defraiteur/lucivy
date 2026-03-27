# Doc 06 — Arena adjacency + splits parasites

Date : 26 mars 2026
Branche : `feature/cross-token-search`

## Diagnostic actuel

Le split graph (phase 1) est efficace : chaque remainder unique n'est exploré qu'une fois.
Le bottleneck est la **phase 2** (adjacency walk) qui crée des `HashMap<(u32,u32), Vec<(usize,u32)>>`
à chaque niveau de récursion × chaque candidate. En WASM ça donne un facteur 50x vs natif
(HashMap = beaucoup de petites allocations, catastrophique pour l'allocateur WASM linéaire).

Bench natif (.luce, 846 docs, 6 segments) :
- rag3weaver : 3.4ms
- getElementById : 10ms

WASM (même .luce) :
- rag3weaver : 400-600ms
- getElementById : pas testé mais probablement pire

## Problème 1 : splits parasites

Pour "rag3weaver", le graph trouve des chaînes comme :

```
"r" (dernier byte de "weaver" SI=5, 5+1=6=token_len)
  → "ag3" (suffixe de "rag3" SI=1, 1+3=4=token_len)
    → "weaver" (terminal)
```

C'est techniquement "valide" par adjacency (les tokens sont consécutifs) mais c'est
du garbage — ça matche le 'r' à la fin d'un token "weaver" suivi de "rag3weaver"
dans le document. Le highlight résultant est incohérent.

### Pourquoi ça arrive

Le falling_walk retourne TOUS les suffixes qui tombent sur une frontière de token.
Pour "rag3weaver" (10 bytes), ça inclut des splits à prefix_len=1 :
- "r" est le dernier byte de plein de tokens (weaver, writer, parser, etc.)

### Solutions possibles

#### A. Minimum prefix_len

Filtrer `prefix_len < MIN_SPLIT` (genre 2). Mais ça empêche de trouver des
vrais cas comme "aBC" → "a"(token) + "BC"(token suivant). Est-ce que ça existe ?
Avec CamelCaseSplit min=4, un token de 1 char n'existe que s'il est isolé
(pas mergé). Rare mais possible pour des chiffres comme "3".

#### B. Vérifier la continuité byte

Le vrai problème : le cross-token doit vérifier que les bytes entre les tokens
matchés sont **contigus dans le texte source**. Un split "r"|"ag3weaver"
implique que 'r' et 'a' sont adjacents dans le texte (pas de gap).

On pourrait utiliser le **GapMap** pour vérifier : le gap entre token[N] et
token[N+1] doit être vide (0 bytes) pour que le match soit valide en mode
cross-token sans séparateur.

Mais le GapMap n'est pas disponible dans `cross_token_search` actuellement —
il faudrait le passer en paramètre.

#### C. Vérifier byte_to == byte_from du token suivant

Plus simple que le GapMap : pour chaque adjacency check, vérifier que
`left_posting.byte_to == right_posting.byte_from`. Ça garantit que les
tokens sont **immédiatement adjacents** dans le texte, sans gap.

C'est gratuit (on a déjà byte_from et byte_to dans les postings) et
ça élimine tous les faux positifs. Si les tokens ont un espace entre eux,
`byte_to != byte_from` → le split est rejeté.

**C'est probablement la meilleure solution.**

Pour le mode `strict_separators`, on pourrait relâcher cette contrainte et
accepter les gaps qui matchent un séparateur donné.

#### D. Ne split que depuis SI=0

Limiter le premier split à SI=0 uniquement (le query prefix doit matcher
le DÉBUT d'un token, pas un suffixe). Les splits suivants aussi à SI=0.

Problème : ça empêche de trouver "g3weaver" → "g3"(suffixe de "rag3" SI=2) + "weaver".
Le contains search doit pouvoir commencer n'importe où dans un token.

Variante : le PREMIER split peut être à n'importe quel SI, mais les splits
INTERMÉDIAIRES et le terminal doivent être à SI=0. C'est logique : une fois
qu'on a traversé une frontière de token, on doit être au début du token suivant.

**Attends, c'est déjà le cas !** Le prefix_walk_si0 et le falling_walk terminal
ne cherchent que des SI=0 entrées. Le problème c'est les splits INTERMÉDIAIRES
dans le graph qui acceptent n'importe quel SI.

En fait non — le falling_walk retourne des candidates de toutes les partitions
(SI=0 et SI>0). Pour les splits intermédiaires (pas le premier), on devrait
filtrer à SI=0 car on doit être au début du token.

### Recommandation

**Option C** (byte_to == byte_from) est la plus propre :
- Élimine tous les faux positifs
- Zéro coût CPU (comparaison u32)
- Pas besoin de GapMap
- Compatible avec le graph

Si byte_to/byte_from n'est pas dispo dans le contexte actuel,
**Option D-variante** (SI=0 pour splits intermédiaires) est un bon fallback.

## Problème 2 : allocations dans la phase 2

### Constat

La phase 2 (adjacency walk) utilise `HashMap<(u32,u32), Vec<(usize,u32)>>` comme
"active set" à chaque niveau de récursion. En WASM :
- HashMap::new() = allocation de buckets
- .entry().or_default() = allocations de Vec
- Chaque niveau × chaque candidate = O(depth × candidates) HashMaps créées

### Solution : flat arena

Remplacer les HashMaps par des `Vec` triés + merge join.

```rust
// Active entry: (doc_id, expected_position, byte_from, first_ti)
// Sorted by (doc_id, expected_position)
type ActiveVec = Vec<(u32, u32, usize, u32)>;
```

À chaque niveau :
1. Resolve postings pour le candidate → `Vec<(doc_id, position)>` (déjà trié par doc_id)
2. **Merge join** entre postings et active_vec (les deux triés par doc_id)
3. Produit le next_active_vec (en réutilisant un buffer pré-alloué)

```rust
// Merge join: O(|postings| + |active|) — pas de HashMap lookup
let mut next_active = Vec::new(); // ou réutiliser un buffer
let mut ai = 0;
for p in postings {
    while ai < active.len() && active[ai].0 < p.doc_id { ai += 1; }
    // Scan all active entries matching (doc_id, expected_position == p.token_index)
    let mut j = ai;
    while j < active.len() && active[j].0 == p.doc_id {
        if active[j].1 == p.token_index {
            next_active.push((p.doc_id, p.token_index + 1, active[j].2, active[j].3));
        }
        j += 1;
    }
}
```

### Avantages
- Zéro allocation HashMap (juste des Vec qui grossissent monotoniquement)
- Cache-friendly (accès séquentiel)
- Merge join O(N+M) au lieu de HashMap lookup O(N)
- Deux buffers pré-alloués qu'on swap entre niveaux (ping-pong)

### Buffer ping-pong

```rust
let mut buf_a: ActiveVec = Vec::with_capacity(1024);
let mut buf_b: ActiveVec = Vec::with_capacity(1024);

// Level 0: seed into buf_a
// Level 1: join postings × buf_a → buf_b
// Level 2: join postings × buf_b → buf_a
// ...
```

Aucune allocation après le capacity initial.

### Variante : combined approach

Si le merge join est complexe à implémenter pour le graph récursif,
on peut garder le graph récursif mais remplacer `HashMap` par `Vec` trié
+ `binary_search`. Moins optimal que le merge join mais déjà beaucoup
mieux que HashMap en termes d'allocations.

```rust
// Au lieu de HashMap::get(&(doc_id, position)):
active.binary_search_by_key(&(doc_id, position), |e| (e.0, e.1))
```

## Plan d'implémentation

1. **D'abord** : Option C (byte_to == byte_from) pour éliminer les splits parasites
   → Moins de chaînes → moins de travail en phase 2 → perf boost gratuit

2. **Ensuite** : Remplacer HashMap par Vec trié dans la phase 2
   → Réduction drastique des allocations → bon pour WASM

3. **Optionnel** : Buffer ping-pong pour zéro allocation dans la boucle chaude
