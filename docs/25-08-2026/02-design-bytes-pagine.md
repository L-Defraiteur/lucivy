# Design — accès paginé aux fichiers de l'index (mmap émulé au grain page)

Nuit du 24 au 25 août 2026. Décision de Lucie : go, **à condition d'être
additif** — aucune régression sur le pipeline natif.

## 1. Le problème, chiffré

Dans le navigateur, une requête `content` relit les sidecars de tous les
segments depuis OPFS (cache de fichiers entiers borné à 768 Mo, index de
4,3-6 Go) : 4-14 s par requête, en release comme en debug. Le natif lit
les mêmes fichiers via mmap et n'y touche qu'aux pages utiles.

Ce qu'une requête touche vraiment (mesure `fincore` après éviction du page
cache, index kernel 50 k naturel de 9,97 Go, natif) : **voir §5** — l'ordre
de grandeur attendu est de quelques Mo à quelques dizaines de Mo par
requête, contre des Go matérialisés aujourd'hui en WASM.

## 2. Principe : un trait à coût zéro, deux implémentations

Tous les accès au FST passent par `Fst::node(addr)` →
`Node::new(version, addr, &data[..=addr])`, qui décode **en arrière** à
partir de `addr` : l'octet d'état, les tailles, puis les transitions et
sorties, toutes dans `[end, addr]` (`end` n'est connu qu'après avoir lu les
tailles). Les adresses de transition sont des deltas par rapport à
`node.end` (`unpack_delta(&data[i..], tsize, node.end)`).

Frontière choisie : un trait dans `lucivy-fst`

```rust
pub trait FstData {
    fn len(&self) -> usize;
    /// Une fenêtre contiguë d'octets se terminant en `addr` (inclus) :
    /// (octets, position locale de `addr` dans la fenêtre, base absolue).
    fn window(&self, addr: usize, min_len: usize) -> (&[u8], usize, usize);
}
impl<D: AsRef<[u8]>> FstData for D {  // natif : exactement le code actuel
    fn window(&self, addr, _) -> (&self.as_ref()[..=addr], addr, 0)
}
```

`Node` garde toute son arithmétique **locale** à la fenêtre ; il gagne un
champ `base` et l'ajoute là où une adresse sort (`trans_addr`, `addr()`).
Pour l'implémentation contiguë, `base = 0` et la fenêtre est la tranche
d'aujourd'hui : après monomorphisation, même code, même vitesse (à
vérifier au panel 50 k — plancher 26 ms, `include` 46 ms, `uint64_t`
relaxed 47 ms — c'est le test de non-régression, chiffré).

Implémentation paginée (`PagedFstData`) : pages de 64 Ko lues à la
demande via `FileHandle::read_bytes(range)` (donc OPFS par plage, ou
n'importe quel `Directory`), tenues dans une **arène append-only** vivante
le temps du lecteur — les `Node<'f>` empruntent `&'f [u8]` dedans sans
garde. `window(addr, min_len)` sert la page contenant `addr` ; si le nœud
déborde à gauche (`end < début de page`), on assemble les deux pages dans
un tampon de l'arène et on recommence (rare : les nœuds font des dizaines
d'octets, jamais 64 Ko). Un lecteur (`SfxFileReaderV3`) est ouvert par
segment et par requête : la mémoire d'une requête ∝ pages touchées, libérée
avec le lecteur. Étape suivante, séparée : un LRU partagé entre requêtes
pour les pages chaudes.

## 3. Les autres sidecars

`posmap`, `bytemap`, `termtexts`, `sibling_v3`, `word_sfxpost`, `sfxpost`,
`word_pos_map` : tous « table d'offsets + entrée par ordinal/doc », lus par
tranche (`&data[start..end]`). Même trait, plus simple : `slice(range) ->
&[u8]` servi depuis l'arène de pages (une entrée tient dans une ou deux
pages). Leurs lecteurs prennent aujourd'hui `&'a [u8]` : ils deviennent
génériques sur `B: ByteSource` avec `impl ByteSource for [u8]` identique au
code actuel.

Le `.sfx` étant 36 % de l'index et le seul à exiger un `&[u8]` contigu
aujourd'hui, c'est lui qu'on fait en premier ; les autres suivent avec le
même patron.

## 4. Ce qui ne change pas

- `MmapDirectory` natif : `Fst<OwnedBytes>` comme aujourd'hui, `base = 0`.
- Le format des fichiers : aucun changement, aucun bump de version.
- Les tests existants : ils exercent l'implémentation contiguë ; la paginée
  reçoit ses propres tests natifs (un `Directory` qui compte les lectures
  par plage, pour vérifier que `kmalloc` sur 50 k ne lit pas plus que la
  mesure §5).

## 5. Mesure « octets touchés par requête » (à remplir)

Protocole : `posix_fadvise(DONTNEED)` sur chaque fichier de l'index, une
requête via `test_sfx_v3_ground_truth` (`V3_QUERIES=<une>`), `fincore`
sur chaque fichier, somme par extension. Fault-around noyau à 64 Ko :
c'est une borne supérieure, du même grain que nos pages.

| requête | total touché | sfx | word_sfxpost | sfxpost | bytemap | termtexts | sibling | posmap |
|---|---|---|---|---|---|---|---|---|
| (voir journal) | | | | | | | | |

## 6. Étapes

1. Trait `FstData` + `Node::base` dans le fork, impl contiguë, panel 50 k
   inchangé (non-régression chiffrée). Additif, mergeable seul.
2. `PagedFstData` sur `FileHandle`, arène par lecteur, tests natifs avec
   un `Directory` compteur ; brancher `SfxFileReaderV3::open_paged` utilisé
   par `StdFsDirectory` (WASM) seulement.
3. `ByteSource` pour les autres sidecars, même patron.
4. Navigateur : rejouer le panel 15 440 docs, viser des requêtes en
   centaines de ms ; puis LRU partagé.

## 7. Décision du matin (Lucie) : lots de shards d'abord

Avant la pagination fine : **les shards passent par lots** dimensionnés
par un budget mémoire (`LUCIVY_SHARD_BATCH_BYTES`), en deux passes comme
le distribué (prescan par lot + libération → fusion globale → un poids →
recherche par lot). Puis **shards fins (~500 docs) + file d'admission**
(threads × mémoire) pour borner l'indexation et les fusions, et **bloom de
trigrammes par shard** pour ne pas ouvrir les shards qui ne peuvent pas
répondre. La pagination fine reste la réponse pour les requêtes fréquentes
(tous les shards répondent). Les §2-6 restent valables comme étape
suivante ; la mesure §5 est à refaire avec un ouvreur nu (le harnais de
vérité terrain refait des fusions à l'ouverture et fausse `fincore`).
