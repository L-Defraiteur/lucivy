# Findings partition v3 — analyse des 2 derniers fails (FN + FP)

> 30 mai 2026 — branche `feature/dag-query-refactor`
> Revue de code statique du pipeline de partitionnement chunk / word-stripped.
> État au commit `35984ce` (skip 0x02 dans resolve_single).

## ⚠️ Message pour la prochaine session

**Tous les points ci-dessous sont STRUCTURELS.** Ils ne se règlent pas avec
un scotch local (filtre ad-hoc, `if doc_id == X`, retain supplémentaire,
seuil magique). Chaque scotch posé sur un de ces points **déplace** le bug
d'un FN vers un FP (ou inversement) parce que les deux fails restants sont
**les deux faces du même problème de partitionnement** :

- 0x00/0x01 (chunk) et 0x02 (word-stripped) partagent le même texte mais ont
  des **postings dans deux stores différents** (sfxpost vs WordSfxPost) et des
  **sémantiques de coordonnées différentes** (chunk-level vs word-level).
- Tant que la partition n'est pas un **invariant porté par le type** (et non par
  une convention de préfixe `C:`/`W:`/`\x00ws:` qu'on peut casser sans erreur de
  compilation), on continuera à faire l'aller-retour FN ⇄ FP à chaque fix.

L'objectif n'est pas « 15/15 sur le ground truth courant ». L'objectif est
**zéro FN ET zéro FP par construction**, c'est-à-dire un partitionnement où il
est *impossible* d'utiliser un ordinal chunk comme word-stripped ou l'inverse.
Si un fix rend un test vert mais repose sur une coïncidence (distance, ordre,
seuil), il sera rouge sur le prochain corpus.

---

## Rappel de l'état

| Query | Mode | Grep | V3 | Status |
|---|---|---|---|---|
| `function` | relax | 1467 | 1468 | 1 FP (doc en trop) |
| `TableFunction` | relax | 221 | 220 | 1 FN (doc manquant) |

Tous les modes strict passent. Les deux fails sont **relax uniquement** →
ils impliquent forcément le **pipeline word (partition 0x02)**, qui n'est
activé qu'en relax (`composite.rs:94`).

---

## Finding 1 — (FN probable) Le skip de `resolve_single_v3` crée une dépendance fragile

### Constat (confirmé dans le code)

`resolve.rs:54-57` :
```rust
// Word-stripped ordinals (partition 0x02) have empty postings in sfxpost.
if cand.is_word_stripped() { continue; }
```

Ce skip tue le phantom FP (postings vides → match byte_from==byte_to==0), c'est
correct. **Mais** il fait que le *seul* chemin de récupération d'un match
word-stripped devient le pipeline word (`composite.rs:94-132`), qui a **trois
conditions cumulatives** :

1. `!strict_separators` (OK en relax).
2. `ctx.has_word_pipeline()` → exige **posmap + bytemap + wsp** tous présents
   (`composite.rs:94-97`). Si un segment est construit sans l'un des trois, le
   match word-stripped disparaît **silencieusement** (pas d'erreur, pas de log).
3. Le match doit ressortir d'un **chain** émis par `cross_word_chain_v3`.

Et le supplément sibling du pipeline word **jette explicitement** les candidats
`sep_len == 0` :

```rust
// composite.rs:108-111
for s in extra {
    if s.parent.sep_len == 0 { continue; }   // ← un mot exact (sep_len=0) éliminé
    all_splits.push(s);
}
```

### Pourquoi c'est le candidat n°1 du FN `TableFunction`

`TableFunction` est un identifiant **camelCase d'un seul mot** (pas de séparateur
interne) → indexé en word-stripped avec `sep_len = 0`. Un match mono-mot exact
sur ce mot n'a plus de chemin de résolution **sauf** si `cross_word_chain_v3`
l'émet de lui-même comme chaîne longueur-1. S'il ne le fait pas (ex. le doc
manquant n'a pas le layout de split attendu), le doc est perdu.

### Fix structurel (PAS un scotch)

Rendre le pipeline word **symétrique** au pipeline chunk : ajouter un
`resolve_single_word_v3(candidates_0x02, wsp, posmap, bytemap)` qui résout les
candidats single 0x02 **directement via WordSfxPost**, au lieu de les jeter dans
`resolve_single` puis de prier pour que les chains les rattrapent.

`FstCandidateV3.partition` existe déjà (commit `35984ce`) — il suffit de router :

```text
fst_candidates_v3  ─┬─ partition ∈ {0x00,0x01} → resolve_single_v3   (sfxpost)
                    └─ partition == 0x02        → resolve_single_word_v3 (WordSfxPost)
```

Ainsi un mot word-stripped exact est résolu par construction, sans dépendre des
chains ni du filtre `sep_len`. Le skip actuel devient le routage explicite d'une
moitié de la partition vers son bon store.

---

## Finding 2 — (FN + FP) Le join multi-chunk de WordSfxPost est un produit cartésien

### Constat (confirmé dans le code)

`collector_v3.rs:707-754`. Pour un mot word-stripped multi-chunk, on joint les
postings du **premier** chunk et du **dernier** chunk, filtrés par la seule
distance de positions :

```rust
let expected_distance = dws.num_chunks - 1;
// first_by_doc = tous les postings d'interns ayant le content-key du first chunk
// puis pour chaque posting du last chunk :
if last_ti >= first_ti && last_ti - first_ti == expected_distance {
    word_sfxpost_writer.add(ws_final_ord, WordPostingEntry { ... });
}
```

`content_key_to_interns` (`collector_v3.rs:553-562`) agrège les chunks **par
contenu, tous mots confondus**. Donc le join apparie `first_content` × `last_content`
de **n'importe quels** mots du doc qui tombent à distance `num_chunks-1`.

### Conséquences (les deux faces)

- **Sur-génération → FP** : `tablefu`…`nction` à distance 1 mais appartenant à
  deux mots distincts du doc crée une entrée WordSfxPost fantôme.
- **Sous-génération → FN** : si l'occurrence réelle a un layout de chunks qui ne
  tombe pas pile sur `num_chunks-1` (le tokenizer divise **également**, pas en
  stride fixe : cf. session 7, `functionality` → `own_len=7` PAS 8), le join rate
  le mot réel.

### Fix structurel

Ancrer le join sur le **`word_id`** (déjà transporté dans `TokenMetaV3.word_id`,
`collector_v3.rs`) plutôt que sur la distance de positions. La distance ne
**prouve pas** que first et last chunk sont le même mot ; le `word_id` si.
Tant que le join repose sur une coïncidence de distance, on ne peut éliminer ni
le FP ni le FN — seulement les échanger.

---

## Finding 3 — (FN + FP) Mélange de coordonnées word-level / chunk-level

### Constat (confirmé dans le code)

La partition 0x02 indexe les **suffixes SI=0..256** (`collector_v3.rs:431`),
donc `first_sti > 0` existe pour les word-stripped. À la résolution :

```rust
// resolve.rs:173-174
byte_from: e.byte_from + chain.first_sti as u32,  // sti ajouté à un byte_from WORD-level
byte_to: e.byte_to,                                // reste WORD-level (fin du mot entier)
```

On ajoute un `sti` (offset de suffixe) à un `byte_from` qui est le **début du mot
entier**, sans ajuster `byte_to`. Le `span = byte_to - byte_from` devient
incohérent et percute deux filtres de l'orchestrateur :

- `orchestrator.rs:65` :
  `retain(|m| m.span > 1 || m.byte_to - m.byte_from >= query_content_len)`
  → peut **droper** un match valide (FN).
- `orchestrator.rs:72` (exact_match) :
  `retain(|m| m.byte_to - m.byte_from == query_content_len)`
  → rate quasi systématiquement un word-stripped dont le span couvre tout le mot
  et non la query.

### Fix structurel

Les `WordPostingEntry` doivent porter des bytes **cohérents avec la sémantique
de la query** : soit byte_from/byte_to du **match** (query dans le mot), soit on
recalcule `byte_to` quand on décale `byte_from` de `sti`. Ne pas mélanger
« début de mot + sti » avec « fin de mot ». Là encore : un filtre de span est
un scotch tant que les bytes eux-mêmes mentent.

---

## Finding 4 — (à confirmer) Asymétrie de sélection des partitions

`fst_walk.rs:115-123` : en `anchor + relax`, on scanne `[0x00, 0x02]` mais
**pas 0x01**. Comme `resolve_single` skip 0x02, l'anchor+relax single n'utilise
effectivement que 0x00. Probablement voulu (anchor = début de token), mais à
**valider** que ça ne perd pas de cas légitimes (ex. suffixe word-stripped ancré).

---

## Finding 5 — (mineur, hygiène) Dedup non déterministe + commentaire périmé

- `orchestrator.rs:68` `dedup_by_key((doc_id, position))` ne retire que les
  doublons **consécutifs**. Quand un même `(doc, position)` vient à la fois de
  0x00 et 0x02 avec des `byte_from/byte_to` différents, le span conservé est
  arbitraire → highlights instables (pas un FN/FP, mais du bruit qui complique
  le debug).
- `collector_v3.rs:412` : commentaire **périmé et contradictoire**
  (« word-stripped uses first chunk's ordinal via intern_to_final ») alors que
  le nouveau design (`into_data`) donne au word-stripped son **propre** ordinal
  avec postings vides + WordSfxPost séparé. À corriger pour ne pas tromper la
  prochaine session.

---

## Synthèse : pourquoi tout est lié (et pourquoi pas de scotch)

```
         même texte "tablefunction"
                  │
      ┌───────────┴───────────┐
   ordinal CHUNK            ordinal WORD-STRIPPED
   (0x00/0x01)              (0x02)
   postings → sfxpost       postings → WordSfxPost
   coords chunk-level       coords word-level (byte_from=début mot, suffixes SI>0)
                  │
   Si on confond les deux dans :
     • resolve_single  → phantom FP (postings vides)   ← Finding 1
     • le join distance → mauvais mot apparié           ← Finding 2
     • les bytes        → span faux → filtre droppe/garde au hasard ← Finding 3
```

Chaque fail restant est une **fuite de partition** : un ordinal traverse une
frontière qu'il ne devrait pas. Le seul fix durable est de faire en sorte que la
frontière soit **infranchissable par construction** :

1. **Routage par partition** des candidats single (Finding 1).
2. **Join par `word_id`**, pas par distance (Finding 2).
3. **Bytes cohérents** dans WordPostingEntry, sans filtre de span correctif (Finding 3).

Si ces trois points sont faits proprement, les filtres `content_len` /
`exact_match` / le skip `sep_len` deviennent **inutiles** : il n'y aura plus ni
FN ni FP à rattraper, donc plus rien à scotcher.

---

## Prochaine étape de diagnostic recommandée

Avant de coder, confirmer le mécanisme du FN :

```bash
V3_DIAG_COLLECTOR=tablefunction V3_DIAG=1 cargo test --lib <ground_truth_test> > /tmp/diag_tf.txt 2>&1
```

Vérifier dans la trace :
- Le doc manquant produit-il un **candidat 0x02** (`fst_candidates_v3`) ?
- Ce candidat génère-t-il **zéro chain word** (`word_chains_total count=0`) ?

Si oui → Finding 1 confirmé, implémenter `resolve_single_word_v3`.

## Fichiers concernés (récap)

| Finding | Fichier:ligne | Nature |
|---|---|---|
| 1 | `briques/resolve.rs:54-57`, `briques/composite.rs:94-132` | routage single 0x02 |
| 2 | `collector_v3.rs:707-754`, `:553-562` | join multi-chunk |
| 3 | `briques/resolve.rs:173-174,186,240`, `orchestrator.rs:65,72` | coords bytes |
| 4 | `briques/fst_walk.rs:115-123` | sélection partitions |
| 5 | `orchestrator.rs:68`, `collector_v3.rs:412` | hygiène |
