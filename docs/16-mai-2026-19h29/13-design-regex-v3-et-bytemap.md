# Design : Regex v3 + ByteMap ajustement

**Date** : 17 mai 2026

---

## 1. Pipeline regex v3

Même approche que v2 : **littéraux d'abord, DFA ensuite**.

```
1. analyze_regex(pattern) → littéraux + gaps typés
   Réutilise regex_gap_analyzer.rs tel quel (inchangé)

2. Résoudre les littéraux via find_literal_v3 (briques v3)
   - fst_candidates_v3 + resolve_single_v3 (single-token)
   - cross_token_chain_v3 + resolve_chains_v3 (cross-token)
   - Sélectivité rarest-first, doc_filter progressif

3. Intersect les littéraux par doc (position ordonnée)
   Réutilise literal_resolve::intersect_literals_ordered

4. Valider les gaps entre littéraux :
   - AcceptAnything (.*) → accept direct
   - ByteRangeCheck ([a-z]+) → vérifier via ByteMap
   - DfaValidation → walk DFA token par token via PosMap
```

### Ce qui change vs v2

| Étape | v2 | v3 |
|-------|----|----|
| Résolution littéraux | literal_pipeline (sibling DFS) | find_literal_v3 (falling walk chaîné) |
| Gap feeding | gapmap.read_separator() manuellement | seps dans les tokens, DFA traverse naturellement |
| Cross-token DFA | continuation_score_sibling | PosMap walk (plus de sibling) |
| ByteMap | bytes du token complet | bytes de content+sep seulement (PAS overlap) |

### strict_separators = true toujours pour regex

Le regex définit lui-même ce qui matche. Si l'utilisateur veut tolérer les seps, il écrit `.*` ou `[_\s]*` dans le pattern. Pas de partition 0x02 pour regex.

---

## 2. ByteMap v3 : exclure l'overlap

### Problème

En v3, les tokens étendus incluent 2 bytes d'overlap du token suivant.
Token `"mutex_lo"` → ByteMap contiendrait {m,u,t,e,x,_,l,o}.

Les bytes "l" et "o" appartiennent au token SUIVANT. Si le regex gap est `[a-z_]+`, le ByteMap check passerait à cause de "l" et "o" même si le token courant ne contient que "mutex_".

### Solution

Construire le ByteMap sur `content + sep` seulement (own_len bytes), PAS sur `content + sep + overlap`.

```
Token "mutex_lo" (content=5, sep=1, overlap=2, own_len=6)
ByteMap v2 (tout) : {m,u,t,e,x,_,l,o}  ← "l" et "o" polluent
ByteMap v3 (own)  : {m,u,t,e,x,_}       ← correct
```

### Implémentation

Dans le SfxIndexFile pour ByteMap, le callback `on_token(ord, text)` reçoit le texte du token. En v3, il faut passer `text[..own_len]` au lieu du texte complet. C'est un changement dans le DAG d'indexation (quand on appelle `build_derived_indexes`), pas dans le ByteMap lui-même.

---

## 3. Composants à réutiliser de v2

| Composant | Fichier | Réutilisable ? |
|-----------|---------|:-:|
| `analyze_regex()` | `regex_gap_analyzer.rs` | ✅ tel quel |
| `GapKind` enum | `regex_gap_analyzer.rs` | ✅ tel quel |
| `intersect_literals_ordered()` | `literal_resolve.rs` | ✅ adapter pour MatchV3 |
| `validate_path()` | `literal_resolve.rs` | ✅ adapter pour PosMap v3 |
| `dfa_accepts_anything()` | `literal_resolve.rs` | ✅ tel quel |
| `group_by_doc()` | `literal_resolve.rs` | ✅ adapter pour MatchV3 |
| ByteMap reader | `bytemap.rs` | ✅ tel quel (format inchangé) |
| PosMap reader | `posmap.rs` | ✅ tel quel |

Le regex_v3 orchestrateur composera les briques v3 (find_literal_v3) + les utilitaires v2 réutilisés (analyze_regex, intersect, validate_path).
