# Design : Partition sep-stripped + falling walk chain à travers les tokens pure-sep

**Date** : 17 mai 2026  
**Contexte** : strict_separators=false doit fonctionner pour fuzzy (trigrams) et pour le falling walk cross-token, même avec beaucoup de séparateurs consécutifs.

---

## 1. Le problème : trigrams cross-sep en strict_sep=false

### 1.1 Trigrams perdus

```
Texte : "mutex________lock" (8 underscores)
Tokens v3 : "mutex____" (content=5, sep=2, ov=2) + ...

Query "mutexlock" → trigrams : "mut","ute","tex","exl","xlo","loc","ock"

Trigram "exl" → cherche dans le FST → PAS TROUVÉ
  Le FST a "ex____" (avec seps) pas "exl" (sans seps)
```

Avec `d=1`, les 2 trigrams perdus ("exl","xlo") sont dans la tolérance Levenshtein (n*d=3). **Ça passe.**

### 1.2 Quand ça ne passe plus

```
Texte : "a____b____c____d"
Query : "abcd" d=1

Bigrams : "ab", "bc", "cd" (3 grams)
Threshold = max(3 - 2*1, 1) = 1

"ab" → FST a "a____b" pas "ab" → MISS
"bc" → FST a "b____c" pas "bc" → MISS
"cd" → FST a "c____d" pas "cd" → MISS

0 matches < 1 → PAS TROUVÉ ✗
```

Tous les trigrams traversent une frontière content/sep. Aucun n'est trouvable dans le FST avec un lookup exact.

---

## 2. Solution : partition sep-stripped (0x02)

### 2.1 Principe

Pour chaque token ayant des trailing seps, indexer AUSSI les suffixes sans les bytes de séparateur. Ces suffixes vont dans une 3ème partition du FST (prefix byte 0x02).

```
Token "mutex__lo" (content="mutex", sep="__", overlap="lo")

Partition 0x01 (normale, avec seps) :
  "mutex__lo" STI=0
  "utex__lo"  STI=1
  "tex__lo"   STI=2
  "ex__lo"    STI=3     ← "exl" ne matche PAS
  "x__lo"     STI=4
  "__lo"      STI=5
  "_lo"       STI=6
  "lo"        STI=7

Partition 0x02 (sep-stripped) :
  "mutexlo"   STI=0     ← content + overlap, sep supprimé
  "utexlo"    STI=1
  "texlo"     STI=2
  "exlo"      STI=3     ← "exl" MATCHE ici ✓
  "xlo"       STI=4
```

### 2.2 Quels suffixes sont ajoutés

Seulement ceux qui DIFFÈRENT de la version avec seps. C'est les suffixes qui commencent dans la zone content (STI < content_len) :

```
Pour un token de content_len=C, sep_len=S, overlap_len=O :
  Suffixes stripped : STI = 0 à C-1
  Chaque suffix = token[STI..C] + token[C+S..C+S+O]  (content restant + overlap, skip sep)
  
  Suffixes à STI ≥ C : soit dans la zone sep (pas utile stripped), soit dans l'overlap (identiques)
```

Nombre d'entrées ajoutées : **C par token ayant sep_len > 0**.

### 2.3 Ce que le FST partage

Le FST trie partage naturellement les préfixes communs :

```
           m → u → t → e → x → _ → _ → l → o   (partition 0x01, avec seps)
                               ↘
                                l → o             (partition 0x02, sans seps)
```

Le préfixe "mutex" est partagé entre les deux versions. Le coût réel est juste la branche après le point de divergence (2 bytes "lo" dans cet exemple). Très compact.

### 2.4 Encoding dans le parent entry

Les suffixes stripped utilisent le MÊME `ParentEntryV3` mais le STI est relatif au content+overlap (pas au content+sep+overlap). On ajoute un flag pour distinguer :

**Option retenue** : partition 0x02 comme prefix byte. Le reader sait que les entrées 0x02 sont stripped. Le STI dans le parent entry est toujours relatif au TEXTE INDEXÉ (qui est content+overlap pour la partition stripped, content+sep+overlap pour les autres).

Pour reconstruire la position originale dans le token complet :
```
original_byte_offset = si STI < content_len : STI
                       si STI >= content_len : STI + sep_len  (réinjecter les seps)
```

### 2.5 Partitions FST v3

```
0x00 — STI=0 (début de token), avec seps
  Usage : startsWith, term (avec filtre is_word_start)

0x01 — STI>0 (substring), avec seps
  Usage : contains strict_sep=true

0x02 — STI≥0 (substring), sans seps
  Usage : contains strict_sep=false, fuzzy strict_sep=false
  Seulement pour les tokens ayant sep_len > 0
```

### 2.6 Routing dans fst_candidates_v3

```rust
fn fst_candidates_v3(sfx, query, anchor_start, strict_sep) -> Vec<FstCandidateV3> {
    let partitions = if strict_sep {
        if anchor_start { vec![0x00] }
        else { vec![0x00, 0x01] }
    } else {
        if anchor_start { vec![0x00] }  // startsWith ne change pas avec strict_sep
        else { vec![0x00, 0x01, 0x02] }  // ajouter la partition stripped
    };
    
    // Pour chaque partition : range scan dans le FST
}
```

---

## 3. Falling walk chain à travers les tokens pure-sep

### 3.1 Le problème

Quand beaucoup de séparateurs produisent des tokens intermédiaires purement composés de sep bytes :

```
Texte : "mutex____________________lock" (20 underscores, MAX_TOKEN=8)

Tokens :
  TI=0 : "mutex__"  (content=5, sep=2)  → extended "mutex____"
  TI=1 : "______"   (content=0, sep=6)  → extended "________"   ← pure sep
  TI=2 : "______"   (content=0, sep=6)  → extended "________"   ← pure sep
  TI=3 : "______"   (content=0, sep=6)  → extended "______lo"   ← pure sep, overlap vers "lock"
  TI=4 : "lock"     (content=4, sep=0)  → extended "lock"
```

Le falling walk chaîné pour "mutexlock" strict_sep=false doit traverser TI=1, TI=2, TI=3 sans consommer de bytes de la query.

### 3.2 Mécanisme : sep-skip immédiat pour content_len=0

Quand un token a `content_len=0`, le sep-skip s'applique immédiatement au byte 0 :

```
TI=1 "________" : content_len=0
  → Sep-skip : skip 6 bytes (toute la zone sep)
  → On est à own_len → SPLIT immédiat
  → query_consumed = 0 (rien consommé de la query)
  → TI+1
```

Le token est "traversé" en 0 bytes de la query. C'est comme s'il n'existait pas pour le matching.

### 3.3 Trace complète

```
Query : "mutexlock" strict_sep=false

TI=0 "mutex____" (content=5, sep=2, overlap=2) :
  Walk m-u-t-e-x → 5 bytes = content_len
  Sep-skip : skip 2 sep bytes dans le token, skip 0 dans la query
  Overlap : token[7]='_' vs query[5]='l' → NO MATCH (overlap = "__" du token TI=1)
  Split : query_consumed=5, overlap_validated=0
  → TI+1

TI=1 "________" (content=0, sep=6, overlap=2) :
  content_len=0 → sep-skip immédiat → skip 6 bytes → own_len atteint
  Overlap : token[6]='_' vs query[5]='l' → NO MATCH (overlap = "__" du token TI=2)
  Split : query_consumed=0
  → TI+1

TI=2 "________" (content=0, sep=6, overlap=2) :
  Pareil → query_consumed=0 → TI+1

TI=3 "______lo" (content=0, sep=6, overlap=2) :
  content_len=0 → sep-skip immédiat → skip 6 bytes → own_len atteint
  Overlap : token[6]='l' vs query[5]='l' → MATCH ✓
           token[7]='o' vs query[6]='o' → MATCH ✓
  Split : query_consumed=0, overlap_validated=2
  → TI+1

TI=4 "lock" (content=4, sep=0, overlap=0) :
  Walk l-o-c-k → 4 bytes, query entièrement consommée
  Match terminal ✓

Chain : [TI=0, TI=1, TI=2, TI=3, TI=4] → positions consécutives ✓
```

### 3.4 Algorithme

```rust
/// Falling walk chaîné à travers une séquence de tokens.
/// Gère naturellement les tokens pure-sep (content_len=0) en les traversant.
fn cross_token_chain_v3(
    sfx: &SfxFileReaderV3,
    query: &str,
    strict_separators: bool,
    fuzzy_distance: u8,
    max_depth: usize,
) -> Vec<TokenChainV3> {
    // Phase 1 : falling_walk_v3 pour trouver le premier split
    let splits = falling_walk_v3(sfx, query, strict_separators, fuzzy_distance);
    
    let mut chains = Vec::new();
    
    for split in &splits {
        let mut chain = vec![split.parent.raw_ordinal];
        let mut remainder = &query[split.remainder_start..];
        let mut current_ti = split.parent.raw_ordinal; // TI du premier token
        let mut depth = 0;
        
        while !remainder.is_empty() && depth < max_depth {
            // Chercher le prochain token (TI+1)
            // Le falling walk sur le token suivant va :
            // - Si content_len > 0 : consommer des bytes normalement
            // - Si content_len == 0 : sep-skip immédiat, query_consumed=0
            
            let next_walk = falling_walk_on_next_token(sfx, current_ti + 1, remainder, strict_separators);
            
            match next_walk {
                Some(result) => {
                    chain.push(result.ordinal);
                    if result.query_consumed > 0 {
                        remainder = &remainder[result.query_consumed..];
                    }
                    // Si query_consumed == 0 (pure sep token), on avance juste TI
                    current_ti += 1;
                    depth += 1;
                }
                None => break, // Pas de match possible
            }
        }
        
        if remainder.is_empty() {
            chains.push(TokenChainV3 {
                ordinals: chain,
                first_sti: split.parent.sti,
                total_query_consumed: query.len(),
            });
        }
    }
    
    chains
}
```

### 3.5 Pourquoi TI+1 suffit (pas besoin de siblings)

Le falling walk chaîné avec TI+1 fonctionne dans TOUS les cas :

| Cas | Mécanisme |
|-----|-----------|
| Adjacent normal (gap=0) | TI+1, seps dans le token |
| Seps courts (1-3 bytes) | TI+1, sep-skip dans le falling walk |
| Seps longs (> MAX_TOKEN) | TI+1, traverse les tokens pure-sep (query_consumed=0) |
| Mots différents | TI+1, chaque mot commence par un token is_word_start=true |
| strict_sep=true | TI+1, compare les seps byte par byte |
| strict_sep=false | TI+1, sep-skip + traverse pure-sep tokens |

Les siblings en v2 étaient nécessaires parce que :
1. Les seps n'étaient pas dans les tokens → le walk cassait à la frontière
2. Il fallait connaître "quel token suit" → la sibling table

En v3 :
1. Les seps sont dans les tokens → le walk continue
2. TI+1 est toujours le suivant (tokens linéaires)
3. Les tokens pure-sep sont traversés sans consommer de query bytes

---

## 4. Impact sur le builder v3

### 4.1 Ajout de la partition 0x02

Dans `SuffixFstBuilderV3::add_token`, quand `sep_len > 0`, ajouter les suffixes stripped :

```rust
// Suffixes normaux (partitions 0x00 et 0x01) — déjà implémenté
for si in 0..extended_len {
    let prefix = if si == 0 { 0x00 } else { 0x01 };
    // ... comme avant
}

// Suffixes sep-stripped (partition 0x02) — NOUVEAU
if sep_len > 0 {
    let content = &extended_bytes[..content_len];
    let overlap = &extended_bytes[own_len..extended_len];
    let stripped: Vec<u8> = [content, overlap].concat();
    
    for si in 0..content_len {
        // Seuls les suffixes commençant dans la zone content
        // (ceux dans l'overlap sont identiques aux normaux)
        let suffix = &stripped[si..];
        let prefix = 0x02;
        // Encoder avec le même ordinal, STI = si
        // Le reader sait que 0x02 = stripped, STI est relatif au stripped text
    }
}
```

### 4.2 Coût additionnel

| Type de token | Suffixes normaux | Suffixes stripped | Total |
|:---:|:---:|:---:|:---:|
| Sans sep (content=5, sep=0) | 5+1 (SI=0+SI>0) | 0 | 6 |
| Avec sep court (content=5, sep=1, ov=2) | 8 | +5 | 13 |
| Avec sep long (content=5, sep=4, ov=2) | 11 | +5 | 16 |
| Pure sep (content=0, sep=6, ov=2) | 8 | 0 (rien à stripper) | 8 |

Pour les tokens avec sep, **+C entrées** (C = content_len). Le FST partage les préfixes → le coût réel en bytes est bien inférieur à +C × entry_size.

**Estimation globale** : dans du code typique, ~40% des tokens ont un trailing sep d'1 byte. content_len moyen ~5. Coût additionnel : ~40% × 5 / (5+1+2) = ~25% d'entrées en plus dans le FST. Avec le partage de préfixes du FST, l'impact sur la taille du fichier .sfx est ~10-15%.

---

## 5. Impact sur les queries fuzzy

### 5.1 Résolution des trigrams

```
strict_sep=true :
  fst_candidates_v3 cherche dans partitions 0x00 + 0x01
  → trigrams avec seps matchent les suffixes avec seps ✓

strict_sep=false :
  fst_candidates_v3 cherche dans partitions 0x00 + 0x01 + 0x02
  → trigrams sans seps matchent les suffixes stripped ✓
  → trigrams avec seps matchent aussi les suffixes normaux ✓
```

### 5.2 Exemple corrigé

```
Texte : "a____b____c____d"
Query : "abcd" d=1, strict_sep=false

Tokens v3 :
  TI=0 : "a____b" (content=1, sep=4, ov=1) → stripped : "ab"
  TI=1 : "____c" (content=0, sep=4, ov=1) → wait, c est alphanum...
```

Hmm, reprenons la tokenization. "a____b____c____d" :
- Segments : ["a____", "b____", "c____", "d"]
  - "a" + sep "____" = segment "a____" (5 bytes)
  - "b" + sep "____" = segment "b____" (5 bytes)
  - "c" + sep "____" = segment "c____" (5 bytes)
  - "d" + pas de sep = segment "d" (1 byte)

Tokens (MAX_TOKEN=8, tous ≤ 8) :
  TI=0 : "a____" (content=1, sep=4) + overlap "b_" → extended "a____b_" (7)
  TI=1 : "b____" (content=1, sep=4) + overlap "c_" → extended "b____c_" (7)
  TI=2 : "c____" (content=1, sep=4) + overlap "d"  → extended "c____d" (6)
  TI=3 : "d"     (content=1, sep=0) → extended "d" (1)

Partition 0x02 (stripped) :
  TI=0 : stripped = "a" + overlap "b_" = "ab_" → suffixes "ab_" STI=0
  TI=1 : stripped = "b" + overlap "c_" = "bc_" → suffixes "bc_" STI=0
  TI=2 : stripped = "c" + overlap "d"  = "cd"  → suffixes "cd" STI=0

Query "abcd" d=1, strict_sep=false :
  Bigrams (len=4 ≤ 6) : "ab", "bc", "cd" (3 grams)
  Threshold = max(3 - 2*1, 1) = 1

  "ab" → partition 0x02 : "ab_" contient "ab" à STI=0 ✓
  "bc" → partition 0x02 : "bc_" contient "bc" à STI=0 ✓
  "cd" → partition 0x02 : "cd" contient "cd" à STI=0 ✓

  3 matches ≥ 1 → TROUVÉ ✓
```

**Le problème initial est résolu.** Les trigrams qui traversent une frontière content/sep sont maintenant trouvables dans la partition stripped.

---

## 6. Récapitulatif des décisions

| Décision | Choix |
|----------|-------|
| Partition sep-stripped | **0x02**, prefix byte séparé |
| Quand indexer la partition 0x02 | Quand `sep_len > 0`, pour STI = 0 à content_len-1 |
| Contenu des suffixes stripped | content[STI..] + overlap (sep retiré) |
| Routing fst_candidates | strict_sep=true : 0x00+0x01 / strict_sep=false : 0x00+0x01+0x02 |
| Falling walk cross-token | TI+1 en boucle, tokens pure-sep traversés (query_consumed=0) |
| Besoin de siblings | **NON** — TI+1 + sep-skip + traverse pure-sep suffit |
| Coût index additionnel | ~10-15% taille .sfx (partage préfixes FST) |

---

## 7. Implémentation réalisée

### 7.1 Code

La partition 0x02 est implémentée dans `src/suffix_fst/builder_v3.rs` :

- Constante `SI_STRIPPED_PREFIX = 0x02`
- Dans `add_token`, quand `sep_len > 0 && content_len > 0` :
  - Construit le texte stripped = `content[si..] + overlap` (sep retiré)
  - Indexe avec prefix byte 0x02 et le même ordinal/metadata que les entrées normales
  - Le STI est relatif au content (position dans le contenu alphanumérique)

### 7.2 Tests (15 verts)

| Test | Ce qu'il vérifie |
|------|-----------------|
| `test_stripped_partition_exists` | "mutexlo" et "exlo" trouvables dans partition 0x02 |
| `test_stripped_trigram_cross_sep` | trigram "exl" introuvable en 0x01, trouvable en 0x02 |
| `test_no_stripped_when_no_sep` | pas d'entrées 0x02 si sep_len=0 |
| `test_stripped_long_sep` | "a____bc" → stripped "abc" correct |
| `test_stripped_preserves_ordinal` | même ordinal entre normal et stripped |

### 7.3 Le FST partage les préfixes

```
Entrées pour "mutex_lo" dans le FST :

  0x00 "mutex_lo"     ← SI=0 normal
  0x01 "utex_lo"      ← SI>0 normal
  0x01 "tex_lo"       
  0x01 "ex_lo"        ← "exl" trigram NE matche PAS ici
  0x01 "x_lo"
  0x01 "_lo"
  0x01 "lo"
  0x01 "o"
  0x02 "mutexlo"      ← stripped
  0x02 "utexlo"
  0x02 "texlo"
  0x02 "exlo"         ← "exl" trigram MATCHE ici ✓
  0x02 "xlo"

Le FST trie partage le préfixe avant le point de divergence (la partition byte).
Coût réel : ~content_len entrées supplémentaires par token avec sep.
```
