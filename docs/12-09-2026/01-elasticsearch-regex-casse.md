# Les « 70 documents manquants » d'Elasticsearch : c'était notre motif

Question de Mark Harwood (ex-Elastic) sur LinkedIn, le 12 septembre : « Are you saying there's a bug
in elasticsearch's wildcard field ("70 short")? What patterns did it fail to match? »

**Réponse : non. Aucun défaut du champ `wildcard`. Notre tableau comparait deux questions
différentes, et le chiffre publié était faux.** Corrigé partout le 12 septembre.

## Ce que disait la ligne

| demandé | vérité | lucivy | Elasticsearch |
|---|---|---|---|
| `spin_lock_[a-z]+`, une regex | 5 510 | **5 510**, 219 ms | 5 440 (wildcard field, 70 short), 480 ms |

## Ce que la vérification montre

L'index du banc tourne encore (Elasticsearch 8.19.0, `cmp_ngram`, 93 983 documents). Elasticsearch
garde la valeur entière de chaque champ `wildcard` dans son `_source` : on peut donc calculer la
vérité **sur les octets qu'il détient lui-même**, sans dépendre d'un clone du noyau.

| calcul sur les 93 983 valeurs stockées | documents |
|---|---|
| regex `spin_lock_[a-z]+`, sensible à la casse | 5 440 |
| la même, insensible à la casse | 5 510 |

Et ce qu'Elasticsearch rend sur le même index :

| requête `regexp` sur `raw` | `case_insensitive` | documents |
|---|---|---|
| `.*spin_lock_[a-z]+.*` | true | 5 440 |
| `.*spin_lock_[a-zA-Z]+.*` | true | **5 510** |
| `.*spin_lock_.*` | true | **5 510** |
| `.*[sS][pP][iI][nN]_[lL][oO][cC][kK]_[a-zA-Z]+.*` | false | **5 510** |

Zéro manquant, zéro en trop, dans les deux régimes : **Elasticsearch répond exactement à la question
qu'on lui pose.**

## La cause

`case_insensitive: true` de Lucene replie les **littéraux** du motif, pas ses **classes de
caractères**. Dans `.*spin_lock_[a-z]+.*`, le littéral `spin_lock_` attrape donc bien `SPIN_LOCK_`,
mais `[a-z]+` refuse `UNLOCKED`. Les 70 documents en cause sont ceux dont la seule occurrence est en
majuscules — `__ARCH_SPIN_LOCK_UNLOCKED` dans `arch/*/include/asm/spinlock_types.h`, `arch/arc/kernel/smp.c`…

Notre vérité, elle, est insensible à la casse (`grep_spans_regex` : `RegexBuilder::case_insensitive(true)`,
« as the engine » — le moteur de lucivy replie la casse jusque dans les classes). Le tableau
comparait donc une réponse sensible à la casse à une vérité qui ne l'est pas.

**Ce n'est pas propre au type `wildcard`** : le même motif se comporte pareil sur un champ `keyword`.

## Reproduction, quatre lignes

```bash
curl -XPUT localhost:9200/wc_case -H 'Content-Type: application/json' \
  -d '{"mappings":{"properties":{"w":{"type":"wildcard"},"k":{"type":"keyword"}}}}'
curl -XPOST 'localhost:9200/wc_case/_doc/upper?refresh' -H 'Content-Type: application/json' \
  -d '{"w":"__ARCH_SPIN_LOCK_UNLOCKED { 0 }","k":"__ARCH_SPIN_LOCK_UNLOCKED { 0 }"}'
curl -XPOST 'localhost:9200/wc_case/_doc/lower?refresh' -H 'Content-Type: application/json' \
  -d '{"w":"spin_lock_irqsave(&lock, flags);","k":"spin_lock_irqsave(&lock, flags);"}'
curl -s localhost:9200/wc_case/_search -H 'Content-Type: application/json' \
  -d '{"query":{"regexp":{"w":{"value":".*spin_lock_[a-z]+.*","flags":"ALL","case_insensitive":true}}}}'
```

| motif (champ `wildcard` ou `keyword`, même résultat) | `case_insensitive` | documents rendus |
|---|---|---|
| `.*spin_lock_.*` | false / true | `lower` / `lower`, `upper` |
| `.*spin_lock_[a-z]+.*` | false / true | `lower` / `lower` |
| `.*spin_lock_[a-zA-Z]+.*` | false / true | `lower` / `lower`, `upper` |
| `.*spin_lock_\w+.*` | false / true | `lower` / `lower`, `upper` |
| requête `wildcard` (non regex) `*spin_lock_*` | true | `lower`, `upper` |

## Ce qui a été corrigé

- La ligne du tableau : README principal, les quatre README de bindings, la page de démonstration,
  l'article `every-engine-lies-a-little`, le rapport `docs/compare-engines-2026-09-05.md`
  (avec une note de correction datée). Elle lit maintenant **5 510 pour Elasticsearch**, la vérité,
  en 1 ms à chaud — les 480 ms publiés étaient une première exécution, index froid.
- Le banc lui-même (`benches/compare_elasticsearch.py`) demande désormais `[a-zA-Z]+`, avec le
  commentaire qui explique pourquoi, dans les deux tableaux où la ligne apparaît.
- Les brouillons datés des posts Reddit ne sont pas retouchés : ce sont des traces de ce qui a été
  publié. Le chiffre corrigé vit ici et dans le rapport.

## Deux leçons pour le banc

1. **Un moteur qui rend moins que la vérité doit être suspecté à l'envers d'abord** : notre question,
   pas sa réponse. Le calcul décisif ne coûtait rien — comparer sa réponse à un balayage des octets
   qu'il stocke lui-même.
2. **La casse est une question, pas un détail** : chaque ligne du banc doit dire dans quel régime elle
   est posée, et le motif envoyé à chaque moteur doit exprimer ce régime dans *sa* syntaxe.
