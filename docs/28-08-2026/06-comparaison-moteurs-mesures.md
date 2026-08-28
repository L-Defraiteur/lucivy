# Comparaison lucivy / tantivy / Elasticsearch — ce qui est mesuré

28 août 2026. Travail interrompu volontairement : la taille d'index doit être
réduite avant de publier quoi que ce soit de comparatif. Ce document existe
pour que la reprise ne remesure pas ce qui l'est déjà.

**Machine** : Intel Core Ultra 7 270K Plus (24 cœurs), 93 Go de RAM, NVMe,
Linux 7.1. Charge basse à chaque mesure.

---

## 1. Le corpus, et pourquoi il est matérialisé

`/tmp/lucivy-cmp-90k` — **93 983 fichiers, 857 Mo de texte**, extraits d'un
clone du noyau Linux par les règles du harnais de vérité terrain (≤ 100 000
octets, non vide, sans octet nul, UTF-8 valide, dossiers `target`,
`node_modules`, `.git`, `build`, `__pycache__`, `playground` exclus).

Il est **copié sur disque** plutôt que filtré à la volée par chaque moteur. La
raison est une erreur commise le jour même : le filtre Python retenait 94 072
fichiers là où le filtre Rust en retenait 93 605 — 467 d'écart, probablement
les liens symboliques que `is_file()` suit et que `os.walk` ignore. Comparer
9 289 à 9 293 dans ces conditions ne prouve rien. Un corpus matérialisé,
sans liens symboliques, supprime la question.

Il existe aussi `/tmp/lucivy-cmp` (10 000 fichiers, 41,5 Mo) pour les essais
rapides.

Reconstruire : le script est dans l'historique de cette session ; la règle est
« même filtre, copie réelle, ordre trié ».

## 2. Ce que fait lucivy sur ce corpus

`V3_CORPUS=/tmp/lucivy-cmp-90k … v3_ground_truth_demo` — **9 lignes vérifiées,
0 échec**, comptes *et* spans comparés octet par octet au disque.

| requête | mode | documents | spans | moteur | scan naïf |
|---|---|---|---|---|---|
| `mutex_lock` | strict | 5 145 | 20 797 | 85,4 ms | 8 643 ms |
| `mutex_lock` | relax | 5 825 | 22 817 | 76,5 ms | 5 838 ms |
| `spin_lock` | strict | 6 569 | 34 667 | 77,7 ms | 5 673 ms |
| `sched` | mot entier | 5 284 | 27 881 | 62,2 ms | 4 543 ms |
| `sched` | sous-chaîne | 9 289 | 53 211 | 67,1 ms | 3 852 ms |
| `printk` | début de token | 4 460 | 24 719 | 74,5 ms | 4 173 ms |
| `schdule` | fuzzy 1 | 5 196 | 18 825 | 118,7 ms | 13 760 ms |
| `regsiter` | fuzzy 2 | 34 451 | 265 797 | 711,2 ms | 13 745 ms |
| `spin_lock_[a-z]+` | regex | 5 510 | 24 368 | 193,7 ms | 830 ms |

Indexation : **142 s** (662 docs/s).

## 3. Taille d'index — le sujet de la reprise

Même corpus, 857 Mo de texte :

| moteur | index | rapport au texte |
|---|---|---|
| tantivy, tokenizer par défaut | 615 Mo | ×0,7 |
| tantivy, trigrammes | 681 Mo | ×0,8 |
| Elasticsearch, standard | 759 Mo | ×0,9 |
| Elasticsearch, trigrammes + `wildcard` | 3 084 Mo | ×3,6 |
| **lucivy, non compacté** | **18 Go** | **×21** |

Sur le corpus de 10 000 fichiers (41,5 Mo), où le détail a été relevé :

| | index | rapport |
|---|---|---|
| Elasticsearch trigramme | 25 Mo | ×0,6 |
| lucivy, 320 segments | 1,2 Go | ×29 |
| **lucivy, compacté à 24 segments** | **733 Mo** | **×17,6** |

**La compaction fait gagner 40 %** et rien de plus : le rapport reste d'un
ordre de grandeur au-dessus des autres.

### Où vont les octets (index de 10 000 fichiers, non compacté, 1,2 Go)

| fichier | poids | ce que c'est |
|---|---|---|
| `.sfx` | **606 Mo** | la Suffix FST elle-même — la moitié de l'index |
| `.bytemap` | 159 Mo | |
| `.termtexts` | 87 Mo | |
| `.sfxpost` | 80 Mo | |
| `.word_sfxpost` | 60 Mo | |
| `.sibling_v3` | 45 Mo | |
| `.word_pos_map` | 31 Mo | |
| `.posmap` | 31 Mo | |
| `.store` | 18 Mo | le texte stocké — 1,5 % du total |

Le texte source n'est **pas** ce qui pèse. C'est la structure de suffixes.

**La piste de Lucie pour la reprise** : beaucoup de ces structures sont
peut-être reconstructibles à la lecture plutôt que stockées. La question à
instruire, fichier par fichier : que coûte sa reconstruction à la requête,
comparé à ce que son stockage coûte au disque ? `.bytemap`, `.posmap` et
`.word_pos_map` (221 Mo à eux trois) sont des tables de correspondance —
candidats naturels.

## 4. Ce qui est établi sur les concurrents

### Elasticsearch 8.19 (Lucene 9.12.2), en conteneur

Configuré **à son mieux**, pas par défaut : un index standard, et un index
avec un analyseur `ngram` (trigrammes, `token_chars: []` pour que les
séparateurs comptent) plus un champ de type `wildcard` pour la regex.

- **Il trouve exactement les mêmes documents que lucivy** sur la sous-chaîne
  et la regex. Au document près, sur quatre requêtes. Son tokenizer `ngram`
  incrémente les positions, donc `match_phrase` sur des trigrammes est exact.
- **Il est aussi rapide, parfois plus** : 9 ms contre 52 sur le fuzzy à deux
  fautes (corpus 10 000).
- **Les positions lui coûtent 64×.** Même requête, mêmes 48 documents, mêmes
  199 spans : 2,7 ms pour lucivy contre **173,6 ms** — 64,4 ms de requête avec
  `highlight`, puis **109,1 ms de reparsing** du balisage, et 0,8 Mo de texte
  rapatriés. Les 199 positions obtenues sont **toutes justes**, revérifiées
  contre les fichiers.
- **Le fuzzy cross-token lui est inaccessible** : sa `fuzziness` s'applique
  terme par terme. 58 documents contre 228 (corpus 10 000).

### Le tableau, corpus identique (93 983 fichiers)

Elasticsearch rejoué sur le corpus matérialisé, donc directement comparable.

| requête | vérité (grep) | lucivy | Elasticsearch |
|---|---|---|---|
| `mutex_lock` sous-chaîne | 5 145 | **5 145** | **5 145** |
| `spin_lock` sous-chaîne | 6 569 | **6 569** | **6 569** |
| `sched` sous-chaîne | 9 289 | **9 289** | **9 289** |
| `spin_lock_[a-z]+` regex | 5 510 | **5 510** | 5 440 — **70 manquants** |
| `schdule` fuzzy 1 | 5 196 | **5 196** | 1 544 |
| `regsiter` fuzzy 2 | 34 451 | **34 451** | 21 321 |

Temps : la regex coûte **469 ms** à Elasticsearch contre 194 à lucivy ; la
sous-chaîne lui coûte 3 à 7 ms contre 67 à 85 — il est plus rapide là-dessus.

**Sur la sous-chaîne, Elasticsearch est exact trois fois sur trois.** Il faut
le dire, et arrêter de prétendre le contraire.

**Question ouverte : les 70 documents manqués par la regex.** Le corpus étant
identique, l'écart est réel cette fois. Hypothèse à vérifier : le type de
champ `wildcard` a une limite de longueur, et les documents les plus gros ne
seraient pas couverts entièrement. Si c'est ça, c'est un manquement à
documenter précisément — pas à supposer.

**Les deux lignes fuzzy ne se comparent pas telles quelles** : la `fuzziness`
d'Elasticsearch s'applique terme par terme, celle de lucivy à une sous-chaîne
qui peut enjamber des séparateurs. Les comptes diffèrent parce que les
*questions* diffèrent. À présenter comme une capacité absente, jamais comme
une erreur de comptage.

Indexation sur ce corpus : standard 32,4 s / 815 Mo — trigramme + `wildcard`
123,3 s / 3 083 Mo (×3,8).

### tantivy 0.25.0 — bien vérifier la provenance

`tantivy = "0.25"` en dev-dependency, `source = registry+…crates.io-index`,
checksum `502915c7…`, aucun `[patch]` dans le workspace. C'est **tantivy
amont**, pas le fork.

- **Son tokenizer n-grammes n'émet aucune position.** Ce n'est pas une mesure,
  c'est écrit dans leur source (`src/tokenizer/ngram_tokenizer.rs`) : « With
  this tokenizer, the `position` is always 0 », et `self.token.position = 0`.
  Vérifié en le faisant tokeniser `"a spin_lock b"` : les onze trigrammes
  sortent tous à `pos=0`.
- **Conséquence** : aucune requête de phrase ne peut s'appuyer dessus. La
  recherche de sous-chaîne dans tantivy ne peut être qu'un **ensemble de
  candidats sur-large** — un ET de trigrammes — que l'application doit
  vérifier elle-même contre le texte stocké. C'est le point le plus solide de
  la comparaison, parce qu'il ne repose pas sur notre mesure.
- **La regex sur l'index par défaut rend 0 document** pour
  `spin_lock_[a-z]+` : le tokenizer a déjà coupé `spin`, `lock`, `irqsave`.
- Indexation : 1,3 s (défaut) et 5,4 s (trigrammes) — bien plus rapide que
  nos 142 s.

**Piège rencontré, à ne pas refaire** : un premier passage a rendu 7 206
documents pour `spin_lock` (vérité : 6 569) et 7 789 pour `spinlock`, soit
*plus* que `spin_lock` — impossible. C'était `QueryParser` traitant la phrase
comme un sac de trigrammes. Une comparaison ne vaut que ce que vaut la requête
qu'on met dans la bouche de l'autre moteur.

## 4 bis. La conclusion, en un tableau

Chacun sait faire un côté ; lucivy fait les deux, et dit lequel on obtient.
Corpus commun, 93 983 fichiers.

| | séparateurs stricts | séparateurs relâchés | fuzzy à travers les tokens |
|---|---|---|---|
| **lucivy** | ✓ 6 577 | ✓ 9 552 | ✓ 10 034 |
| **Elasticsearch** | ✓ 6 577 (trigrammes) | ✗ inexprimable | ✗ 3 549 |
| **tantivy** | ✗ le séparateur n'est pas indexé | ✓ phrase par défaut | ✗ |

**Elasticsearch ne peut pas relâcher les séparateurs.** Son index trigramme les
porte, donc `spinlock` rend 6 577 — exactement le compte *strict*. Les 2 975
documents que le relâché ajoute lui sont hors d'atteinte, et sa phrase
`"spin lock"` sur l'index standard n'en rend que 173 (son analyseur garde
`spin_lock` en un seul token).

**tantivy ne peut pas être strict.** Son tokenizer par défaut rend le même flux
pour les trois orthographes — vérifié en l'interrogeant :

```
spin_lock  -> spin@0 lock@1
spin-lock  -> spin@0 lock@1
spin lock  -> spin@0 lock@1
```

Le séparateur n'entre jamais dans l'index. Et l'index n-grammes ne rattrape
rien, ses positions étant toutes à 0.

**Le fuzzy s'arrête à leurs frontières de tokens.** `spinlokc` en distance 2 :
lucivy 10 034 documents (57 261 spans exacts), Elasticsearch 3 549. Sa
`fuzziness` compare des termes entiers, donc elle peut approcher un token
`spinlock` mais jamais `spin_lock`, déjà coupé en deux.

### Ce qu'ils savent faire et qu'on aurait eu tort de nier

- **Elasticsearch sait faire une phrase floue**, via `span_near` de
  `span_multi` : `retrun -ENOMEM` rend 14 457 documents en 79 ms, contre
  14 427 pour la forme exacte. C'est verbeux, mais c'est juste. Ne pas
  prétendre le contraire.
- **La sous-chaîne exacte lui est acquise**, quatre requêtes sur quatre, y
  compris à cheval sur un `/` (`ude/lin` : 889 des deux côtés).

### Le plancher des n-grammes

Un index de trigrammes ne peut pas répondre à une requête de moins de trois
caractères : il n'y a aucun trigramme à chercher.

| requête | vérité | lucivy | Elasticsearch |
|---|---|---|---|
| `ude` (3 car.) | 69 245 | 69 245 | 69 245 |
| `de` (2 car.) | 93 009 | 93 009 | **0** |

Zéro, silencieusement, en 2 ms. Cela vaut pour tout index n-grammes, tantivy
compris. On *peut* indexer en `min_gram: 2` ou 1 — le coût en taille n'a pas
été mesuré, donc ne pas l'avancer.

**Et un défaut de notre harnais, révélé par le même essai** : sur `de`, le
panel affiche `FAIL` alors que le moteur s'est comporté correctement — il a
atteint `LUCIVY_HIGHLIGHT_SPAN_CAP` et l'a signalé par
`last_search_truncated()`. Avec `LUCIVY_HIGHLIGHT_SPAN_CAP=0` il rend ses
7 695 534 spans, exacts. Le panel doit lire ce drapeau et écrire « tronqué »
plutôt que « faux » : un panel qui crie au loup sur un comportement voulu perd
sa valeur d'alerte.

## 5. Ce qui reste à faire

1. **Réduire la taille d'index.** C'est le préalable ; tout le reste attend.
2. **Le chemin honnête pour tantivy** : candidats par ET de trigrammes, puis
   vérification sur le texte stocké, chronométrée et mise à son compte —
   exactement ce que lucivy fait en interne. Le harnais est écrit
   (`lucivy_core/benches/compare_tantivy.rs`), il ne manque que cette étape.
3. **Élucider les 70 documents que la regex d'Elasticsearch manque** — limite
   de longueur du champ `wildcard`, ou autre chose.
4. **Faire lire `last_search_truncated()` au panel** — voir 4 bis.
5. **Puis** la section comparative du README, et la réponse aux issues #12
   et #15.

## 6. Comment relancer

```bash
# Elasticsearch
docker run -d --name lucivy-es -p 9200:9200 \
  -e discovery.type=single-node -e xpack.security.enabled=false \
  -e ES_JAVA_OPTS=-Xms8g -Xmx8g \
  docker.elastic.co/elasticsearch/elasticsearch:8.19.0
python3 benches/compare_elasticsearch.py /tmp/lucivy-cmp-90k

# tantivy
CMP_CORPUS=/tmp/lucivy-cmp-90k cargo test --release -p lucivy-core \
    --test compare_tantivy -- --ignored --nocapture

# lucivy, la référence
V3_CORPUS=/tmp/lucivy-cmp-90k V3_INDEX_DIR=/tmp/lucivy-idx-90k \
  cargo test --release -p lucivy-core --test test_sfx_v3_ground_truth \
    v3_ground_truth_demo -- --ignored --nocapture
```

Le conteneur `lucivy-es` tourne encore ; `docker rm -f lucivy-es` pour le
retirer. Les index d'essai sont dans `/tmp/tv_default`, `/tmp/tv_ngram`,
`/tmp/lucivy-idx-90k` (18 Go) — à supprimer si la place manque.
