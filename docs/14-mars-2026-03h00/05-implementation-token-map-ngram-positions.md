# Implémentation : Token Map + Ngram positions composées

Date : 14 mars 2026

Plan d'implémentation étape par étape pour le design décrit dans le doc 03.

---

## Phase 1 — Ngram positions composées

### Étape 1.1 : Passer le ngram field en WithFreqsAndPositions

**Fichier :** `lucivy_core/src/handle.rs` (lignes ~260-268)

**Actuellement :**
```rust
let ngram_indexing = TextFieldIndexing::default()
    .set_tokenizer(NGRAM_TOKENIZER)
    .set_index_option(IndexRecordOption::Basic);  // ← doc IDs seulement
```

**Changer en :**
```rust
let ngram_indexing = TextFieldIndexing::default()
    .set_tokenizer(NGRAM_TOKENIZER)
    .set_index_option(IndexRecordOption::WithFreqsAndPositions);  // ← + positions
```

Impact : le posting list ngram stocke maintenant les positions de chaque trigram.
Taille de l'index ngram augmente (~2x) mais nécessaire pour les positions composées.

Note : pas besoin de `WithFreqsAndPositionsAndOffsets` — les byte offsets seront dans
le token map, pas dans le posting list ngram.

### Étape 1.2 : Modifier le NgramTokenizer pour émettre des positions composées

**Fichier :** `src/tokenizer/ngram_tokenizer.rs` (lignes ~162-177)

**Actuellement :**
```rust
fn advance(&mut self) -> bool {
    if let Some((offset_from, offset_to)) = self.ngram_charidx_iterator.next() {
        if self.prefix_only && offset_from > 0 {
            return false;
        }
        self.token.position = 0;       // ← toujours 0, pas de position
        self.token.offset_from = offset_from;
        self.token.offset_to = offset_to;
        self.token.text.clear();
        self.token.text.push_str(&self.text[offset_from..offset_to]);
        true
    } else {
        false
    }
}
```

**Problème :** Le NgramTokenizer ne connaît pas le `token_pos` du raw token source.
Il reçoit le texte entier du document et génère les trigrams séquentiellement. Il ne
sait pas où un token raw commence et finit.

**Solution :** Le NgramTokenizer ne doit PAS être modifié directement. C'est le
**segment writer** qui doit changer la logique d'indexation du ngram field.

Au lieu de passer le texte brut au ngram tokenizer et le laisser générer les trigrams,
on tokenise d'abord avec le raw tokenizer, puis pour chaque raw token on génère les
trigrams avec la position composée.

### Étape 1.3 : Changer l'indexation ngram dans le segment writer

**Fichier :** `src/indexer/segment_writer.rs` (lignes ~147-217)

**Actuellement :** Le segment writer appelle `postings_writer.index_text()` une seule
fois par champ, avec le tokenizer associé au champ. Le ngram field utilise le
`NGRAM_TOKENIZER` qui reçoit le texte brut et produit tous les trigrams.

**Le problème :** Le tokenizer ngram ne sait pas quels trigrams viennent de quel
raw token. Il parcourt le texte caractère par caractère.

**Deux approches possibles :**

#### Approche A : Tokenizer ngram "position-aware" (via PreTokenizedText)

Au lieu de laisser le ngram tokenizer traiter le texte brut, on pré-tokenise
avec le raw tokenizer, puis on injecte les trigrams comme `PreTokenizedText`
avec les positions composées.

```rust
// Pseudo-code dans index_document(), après la tokenisation raw du champ texte
let raw_tokens = tokenize_with_raw_analyzer(text);

let mut pre_tokenized_ngrams = Vec::new();
for (raw_pos, raw_token) in raw_tokens.iter().enumerate() {
    let trigrams = generate_trigrams_from_token(&raw_token.text);
    for (ngram_seq, trigram) in trigrams.iter().enumerate() {
        pre_tokenized_ngrams.push(Token {
            text: trigram.clone(),
            position: raw_pos as u32 * 1024 + ngram_seq as u32,
            offset_from: raw_token.offset_from,  // offset du raw token
            offset_to: raw_token.offset_to,
            position_length: 1,
        });
    }
}

// Indexer le ngram field avec les tokens pré-construits
let pre_tok = PreTokenizedText { tokens: pre_tokenized_ngrams, text: text.to_string() };
postings_writer.index_text(
    doc_id,
    &mut PreTokenizedStream::from(pre_tok),
    ngram_term_buffer,
    ctx,
    &mut ngram_indexing_position,
);
```

**Avantage :** Pas de modification du NgramTokenizer existant.
**Inconvénient :** Le segment writer doit savoir que le ngram field est spécial.

#### Approche B : Tokenizer wrapper "composed position"

Créer un tokenizer wrapper qui enchaîne le raw tokenizer + ngram tokenizer et
injecte les positions composées :

```rust
struct ComposedPositionNgramTokenizer {
    raw_tokenizer: TextAnalyzer,
    ngram_size: (usize, usize),  // (min_gram, max_gram)
}
```

Ce tokenizer :
1. Tokenise d'abord avec le raw tokenizer
2. Pour chaque raw token, génère les trigrams
3. Émet chaque trigram avec `position = raw_pos * 1024 + ngram_seq`

**Avantage :** Encapsulé dans le tokenizer, transparent pour le segment writer.
**Inconvénient :** Nouveau tokenizer à maintenir.

#### Recommandation : Approche B

L'approche B est plus propre — le segment writer n'a pas besoin de logique spéciale
par champ. Le tokenizer encapsule la logique de position composée.

**Fichier à créer :** `src/tokenizer/composed_ngram_tokenizer.rs`

**Enregistrement :** dans `lucivy_core/src/handle.rs`, remplacer l'enregistrement
du `NGRAM_TOKENIZER` par le nouveau `ComposedPositionNgramTokenizer` :

```rust
// Actuellement (handle.rs lignes ~200-210)
index.tokenizers().register(
    NGRAM_TOKENIZER,
    NgramTokenizer::new(3, 3, false).unwrap(),
);

// Remplacer par :
index.tokenizers().register(
    NGRAM_TOKENIZER,
    ComposedPositionNgramTokenizer::new(3, 3),
);
```

### Étape 1.4 : Modifier la collecte de candidats ngram

**Fichier :** `src/query/phrase_query/ngram_contains_query.rs` (lignes ~94-133)

**Actuellement :** `ngram_candidates_for_token()` collecte les doc_ids depuis les
posting lists des trigrams et fait un threshold count. Les positions ne sont pas lues.

```rust
fn ngram_candidates_for_token(
    token: &str,
    ngram_field: Field,
    ngram_inverted: &InvertedIndexReader,
    fuzzy_distance: u8,
) -> crate::Result<Vec<DocId>> {
    let trigrams = generate_trigrams(token);
    let threshold = ngram_threshold(trigrams.len(), fuzzy_distance);
    // ... collecte doc_ids, threshold count ...
}
```

**Remplacer par :** une fonction qui collecte les `(DocId, token_pos)` paires en
lisant les positions composées :

```rust
fn ngram_candidates_with_positions(
    token: &str,
    ngram_field: Field,
    ngram_inverted: &InvertedIndexReader,
    fuzzy_distance: u8,
) -> crate::Result<Vec<(DocId, u16)>> {
    let trigrams = generate_trigrams(token);
    let threshold = ngram_threshold(trigrams.len(), fuzzy_distance);

    // Pour chaque trigram, lire les (doc_id, positions) du posting list
    let mut all_hits: Vec<(DocId, u16, u16)> = Vec::new(); // (doc, token_pos, ngram_seq)
    for (query_seq, trigram) in trigrams.iter().enumerate() {
        let term = Term::from_field_text(ngram_field, trigram);
        // Lire posting list avec positions (WithFreqsAndPositions)
        let term_info = match ngram_inverted.get_term_info(&term)? {
            Some(ti) => ti,
            None => continue,
        };
        let mut postings = ngram_inverted.read_postings_from_terminfo(
            &term_info, IndexRecordOption::WithFreqsAndPositions,
        )?;
        loop {
            let doc_id = postings.doc();
            if doc_id == TERMINATED { break; }
            let mut positions = Vec::new();
            postings.positions(&mut positions);
            for &pos in &positions {
                let token_pos = (pos / 1024) as u16;
                let ngram_seq = (pos % 1024) as u16;
                all_hits.push((doc_id, token_pos, ngram_seq));
            }
            postings.advance();
        }
    }

    // Grouper par (doc_id, token_pos), vérifier la séquence ngram_seq
    all_hits.sort_unstable();
    // ... threshold count par (doc_id, token_pos) + vérification séquence ...
    // Voir étape 1.5 pour la logique de vérification
}
```

### Étape 1.5 : Vérification Niveau 0 par séquence ngram_seq

La vérification 100% par ngram consiste à vérifier que les trigrams de la query
apparaissent aux bons ngram_seq consécutifs pour un même (doc_id, token_pos).

```rust
/// Vérifie que les trigrams de la query forment une séquence consécutive
/// dans le token à (doc_id, token_pos).
fn verify_ngram_sequence(
    hits: &[(u16, u16)],  // (ngram_seq du hit, query_seq attendu)
) -> bool {
    // hits trié par ngram_seq
    // Pour chaque starting offset possible, vérifier que tous les query_seq matchent
    // Ex: query "g3db" → trigrams ["g3d" qs=0, "3db" qs=1]
    //     hits pour ce (doc, token_pos) : [(2, 0), (3, 1)]
    //     ngram_seq 2 et 3 sont consécutifs, diff = query_seq diff → OK
    if hits.is_empty() { return false; }
    let first_offset = hits[0].0 as i32 - hits[0].1 as i32; // ngram_seq - query_seq
    hits.iter().all(|&(ns, qs)| ns as i32 - qs as i32 == first_offset)
}
```

### Étape 1.6 : Modifier NgramContainsWeight::scorer()

**Fichier :** `src/query/phrase_query/ngram_contains_query.rs` (lignes ~276-374)

Remplacer le flow actuel (candidats doc_ids → stored text verify) par :

```
candidats (doc_id, token_pos) avec vérification ngram_seq
  │
  ├─ Niveau 0 confirmé → pas besoin de token map
  │   → construire un scorer qui itère les (doc_id, token_pos) confirmés
  │
  └─ Niveau 0 pas confirmé (fuzzy, query courte)
      → fallback token map (phase 2)
      → ou fallback stored text temporairement (avant phase 2)
```

**Pour la phase 1 :** les cas que le Niveau 0 ne couvre pas (fuzzy d>0, query < 3 chars)
restent sur le stored text. Le scorer fait :

```rust
fn scorer(&self, reader: &SegmentReader, boost: Score) -> crate::Result<Box<dyn Scorer>> {
    let ngram_inverted = reader.inverted_index(self.ngram_field)?;

    match &self.verification {
        VerificationMode::Fuzzy(params) if params.fuzzy_distance == 0
            && params.tokens.iter().all(|t| t.len() >= 3) => {
            // Niveau 0 : vérification par ngram positions composées
            let confirmed = ngram_candidates_with_positions(...)?;
            // ... construire scorer depuis les (doc_id, token_pos) confirmés
        }
        _ => {
            // Fallback : stored text (comportement actuel)
            // ... code existant ...
        }
    }
}
```

---

## Phase 2 — Token Map (.tmap)

### Étape 2.1 : Ajouter SegmentComponent::TokenMap

**Fichier :** `src/index/segment_component.rs` (lignes ~3-35)

```rust
pub enum SegmentComponent {
    Postings,
    Positions,
    FastFields,
    FieldNorms,
    Terms,
    Store,
    TempStore,
    Delete,
    Offsets,
    TokenMap,     // ← NOUVEAU
}

impl SegmentComponent {
    pub fn iterator() -> slice::Iter<'static, SegmentComponent> {
        static SEGMENT_COMPONENTS: [SegmentComponent; 10] = [  // ← 9 → 10
            // ... existants ...
            SegmentComponent::TokenMap,
        ];
        SEGMENT_COMPONENTS.iter()
    }
}
```

**Fichier :** `src/index/index_meta.rs` (lignes ~130-148)

```rust
SegmentComponent::TokenMap => ".tmap".to_string(),
```

### Étape 2.2 : Implémenter TokenMapWriter

**Fichier à créer :** `src/store/token_map_writer.rs`

Writer qui accumule les données par document et écrit le fichier .tmap à la fin.
Voir le pseudo-code dans le doc 03 section "Token Map writer".

API :
```rust
impl TokenMapWriter {
    fn new(writer: WritePtr) -> Self;
    fn add_document(&mut self, text: &str, tokens: &[(usize, usize)]);
    fn close(self) -> io::Result<()>;
}
```

Où `tokens` sont les `(byte_offset_from, byte_offset_to)` de chaque raw token.

### Étape 2.3 : Implémenter TokenMapReader

**Fichier à créer :** `src/store/token_map_reader.rs`

Reader mmap'd qui lit les tokens et gaps par (doc_id, position).

API :
```rust
impl TokenMapReader {
    fn open(file: FileSlice) -> crate::Result<Self>;
    fn read_token(&self, doc_id: DocId, position: u16) -> Option<(&[u8], &[u8])>;
    fn read_gap_before(&self, doc_id: DocId, position: u16) -> Option<&[u8]>;
    fn text_byte_offset(&self, doc_id: DocId, position: u16) -> Option<u32>;
    fn num_tokens(&self, doc_id: DocId) -> u16;
}
```

### Étape 2.4 : Brancher le TokenMapWriter dans le SegmentSerializer

**Fichier :** `src/indexer/segment_serializer.rs` (lignes ~11-46)

Ajouter le `TokenMapWriter` au struct et l'ouvrir dans `for_segment()` :

```rust
pub struct SegmentSerializer {
    // ... existants ...
    pub(crate) token_map_writer: Option<TokenMapWriter>,
}

// Dans for_segment() :
let token_map_write = segment.open_write(SegmentComponent::TokenMap)?;
let token_map_writer = Some(TokenMapWriter::new(token_map_write));
```

Et dans `close()` (lignes ~80-88) :

```rust
if let Some(tmap) = self.token_map_writer.take() {
    tmap.close()?;
}
```

### Étape 2.5 : Capturer les tokens dans le segment writer

**Fichier :** `src/indexer/segment_writer.rs` (lignes ~147-217)

Dans `index_document()`, pour les champs texte qui ont un `._raw` counterpart,
capturer les raw tokens avec leurs offsets après tokenisation :

```rust
// Après la tokenisation du champ raw (ou du champ principal)
// Collecter les tokens avec offsets
let mut raw_tokens: Vec<(usize, usize)> = Vec::new();
token_stream.process(&mut |token: &Token| {
    raw_tokens.push((token.offset_from, token.offset_to));
    // ... reste de l'indexation ...
});

// Écrire dans le token map
if let Some(ref mut tmap) = self.segment_serializer.token_map_writer {
    tmap.add_document(text, &raw_tokens);
}
```

**Point d'attention :** Le segment writer indexe les champs dans l'ordre du schema.
Le champ principal (stemmed), le `._raw` et le `._ngram` sont indexés séparément.
Il faut capturer les tokens du `._raw` (pas du stemmed, pas du ngram) et les passer
au token map writer.

Le segment writer a accès au texte original car il itère les valeurs du document
(`value.as_str()`). Il faut stocker temporairement le texte et les tokens raw pour
les passer au tmap writer après la tokenisation du `._raw` field.

### Étape 2.6 : Brancher le TokenMapReader dans le SegmentReader

**Fichier :** `src/core/segment_reader.rs` (ou là où SegmentReader est défini)

Ajouter une méthode pour ouvrir le token map :

```rust
impl SegmentReader {
    pub fn token_map_reader(&self, field: Field) -> crate::Result<TokenMapReader> {
        let file = self.segment().open_read(SegmentComponent::TokenMap)?;
        TokenMapReader::open(file)
    }
}
```

### Étape 2.7 : Utiliser le TokenMapReader dans le scorer

**Fichier :** `src/query/phrase_query/ngram_contains_query.rs`

Pour les cas non couverts par le Niveau 0 (fuzzy d>0, query < 3 chars), le scorer
utilise le token map au lieu du stored text :

```rust
// Niveau 1 : Token Map verification
let (gap, token_bytes) = tmap_reader.read_token(doc_id, token_pos)?;
let token = std::str::from_utf8(token_bytes)?;
let distance = token_match_distance(token, &query_token, fuzzy_distance);
```

---

## Phase 3 — Merger

### Étape 3.1 : Merger le Token Map

**Fichier :** `src/indexer/merger.rs`

Ajouter une méthode `write_token_maps()` similaire à `write_storable_fields()`
(lignes ~514-546) :

```rust
fn write_token_maps(
    &self,
    tmap_writer: &mut TokenMapWriter,
    doc_id_mapping: &DocIdMapping,
) -> crate::Result<()> {
    for old_doc_addr in doc_id_mapping.iter_old_doc_addrs() {
        let reader = &self.readers[old_doc_addr.segment_ord as usize];
        let tmap_reader = reader.token_map_reader(self.target_field)?;
        // Copier les données brutes du doc (tokens + gaps inchangés)
        let doc_data = tmap_reader.raw_doc_data(old_doc_addr.doc_id);
        tmap_writer.add_raw_doc_data(doc_data);
    }
    Ok(())
}
```

Le token map des segments source est mmap'd. On copie les bytes bruts de chaque doc
dans le nouveau token map, dans l'ordre du remapping doc_id.

### Étape 3.2 : Merger les positions ngram composées

Les positions ngram composées sont relatives au document (token_pos × K + ngram_seq).
Elles ne changent PAS au merge — un token à position 3 dans le doc reste à position 3.
Le merger n'a pas besoin de réécrire les positions ngram.

Le merger existant préserve déjà les positions telles quelles dans
`write_postings_for_field()` (lignes ~456-488). Pas de changement nécessaire.

---

## Phase 4 — Supprimer le stored text du chemin de recherche

### Étape 4.1 : NgramContainsScorer sans stored text

Remplacer `NgramContainsScorer` par un nouveau `TokenMapContainsScorer` :

```rust
struct TokenMapContainsScorer {
    /// Candidats confirmés par Niveau 0 ou à vérifier par Niveau 1
    candidates: Vec<(DocId, Vec<u16>)>,  // (doc_id, [token_positions])
    cursor: usize,
    tmap_reader: TokenMapReader,
    verification: VerificationMode,
    bm25_weight: Bm25Weight,
    fieldnorm_reader: FieldNormReader,
    last_tf: u32,
    highlight_sink: Option<Arc<HighlightSink>>,
    highlight_field_name: String,
    segment_id: SegmentId,
}
```

Ce scorer n'a pas de `store_reader`. Il utilise le `tmap_reader` pour le Niveau 1
et n'a rien besoin de lire pour le Niveau 0.

### Étape 4.2 : ContainsScorer sans stored text

**Fichier :** `src/query/phrase_query/contains_scorer.rs`

Le `ContainsScorer` (multi-token avec posting list intersection) lit aussi le stored
text dans `validate_separators()` (lignes ~200-379). Remplacer par :

```rust
fn validate_separators_from_tmap(&mut self, starting_positions: &[u32]) -> Option<u32> {
    // Utiliser le token map pour lire les gaps entre positions
    // au lieu de store_reader.get(doc_id)
    let token_pos = (starting_positions[0] as u16); // position raw
    let gap = self.tmap_reader.read_gap_before(doc_id, token_pos + 1)?;
    // ... valider le séparateur ...
}
```

---

## Ordre d'exécution recommandé

```
1.1  IndexRecordOption::Basic → WithFreqsAndPositions      (5 min, 1 ligne)
1.2  Lire : NgramTokenizer, comprendre les positions       (lecture)
1.3  ComposedPositionNgramTokenizer                         (nouveau fichier)
1.4  ngram_candidates_with_positions()                      (refactor collecte)
1.5  verify_ngram_sequence()                                (nouvelle fonction)
1.6  NgramContainsWeight::scorer() Niveau 0                 (brancher le flow)
     ─── BUILD + TESTS ───
     cargo test --lib (vérifier que rien n'est cassé)
     maturin develop --release (bench Python)
     ─── BENCH : mesurer le gain Phase 1 seule ───

2.1  SegmentComponent::TokenMap                             (2 lignes)
2.2  TokenMapWriter                                         (nouveau fichier)
2.3  TokenMapReader                                         (nouveau fichier)
2.4  Brancher dans SegmentSerializer                        (4 lignes)
2.5  Capturer tokens dans segment_writer                    (10 lignes)
2.6  Brancher dans SegmentReader                            (5 lignes)
2.7  Utiliser dans le scorer (Niveau 1)                     (refactor scorer)
     ─── BUILD + TESTS ───
     ─── BENCH : mesurer le gain Phase 2 ───

3.1  Merger token maps                                      (nouveau code)
3.2  Vérifier positions ngram au merge                      (normalement rien)
     ─── BUILD + TESTS de merge ───

4.1  TokenMapContainsScorer (remplace NgramContainsScorer)  (refactor)
4.2  ContainsScorer sans stored text                        (refactor)
     ─── TESTS COMPLETS + BENCH FINAL ───
```

## Risques

- **Phase 1 seule** suffit pour le gros du gain (d=0, query ≥ 3 chars). Si le token
  map est trop complexe ou trop gros, on peut s'arrêter à la phase 1 et garder le
  stored text pour le Niveau 1.

- **Taille de l'index** : le ngram passe de Basic à WithFreqsAndPositions (~2x plus
  gros) ET le .tmap ajoute ~76KB/doc. Sur 5201 docs c'est ~400MB de tmap +
  posting list ngram plus gros. À mesurer.

- **ComposedPositionNgramTokenizer** : doit tokeniser avec le raw tokenizer PUIS
  générer les trigrams. Si le raw tokenizer et le ngram tokenizer ne sont pas
  cohérents (ex: différent handling des accents, de la casse), les positions ne
  correspondront pas. Le raw tokenizer = "default" (lowercase only), le ngram
  tokenizer doit appliquer le même lowercase.

- **Position composée et POSITION_GAP** : le segment writer ajoute un `POSITION_GAP`
  entre les valeurs d'un même champ multi-valué (lignes ~180-183 de postings_writer.rs).
  Le ComposedPositionNgramTokenizer doit gérer ça correctement — les positions
  composées ne doivent pas chevaucher entre documents ou valeurs.
