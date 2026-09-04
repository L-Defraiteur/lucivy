# Journal des étapes de réduction — mesures

Une section par étape du plan ([01](01-recap-findings-et-plan-d-action.md),
§3), écrite après la mesure, jamais avant. Protocole : §4 du même document.

**Référence** : 10 000 fichiers de `/tmp/lucivy-cmp` (65 Mo sur disque),
harnais `v3_ground_truth_demo` avec `V3_INDEX_DIR`, 160 segments de 64
documents, machine au repos (charge 0,8). Tailles par
`benches/scan_index_size.py`.

```bash
V3_CORPUS=/tmp/lucivy-cmp V3_INDEX_DIR=/chemin/idx cargo test --release -p lucivy-core \
    --test test_sfx_v3_ground_truth v3_ground_truth_demo -- --ignored --nocapture
python3 benches/scan_index_size.py /chemin/idx <uuid du plus gros segment>
```

Index de référence v3 : **1 152,4 Mo** scannés (`.store` non compté),
dont `.sfx` 635,9 Mo = FST 256,9 + parents 368,9.

---

## Étape 1 — table de parents compacte (4 septembre)

**Changement.** Un record multi-parents était `[u32 count]` + 11 octets par
parent. Il est maintenant `[varint count]` + le même `u64` packé que la
valeur inline de la FST (`encode_single_parent_v3`), 8 octets. Un seul
décodeur pour les deux formes. Octet de version du conteneur `.sfx` : 3 → 4,
magic inchangé (`SFX3`), le lecteur accepte les deux
(`SfxFileReaderV3::container_version()`). Le builder refuse désormais un
ordinal au-delà de 24 bits **dans tous les cas**, pas seulement pour un
parent unique — avant, un ordinal de record était écrit sur 32 bits.

Fichiers : `src/suffix_fst/builder_v3.rs`, `src/suffix_fst/file_v3.rs`,
`benches/scan_index_size.py`.

**Taille.**

| fichier | avant | après | delta |
|---|---|---|---|
| `.sfx` — parents | 368,9 Mo | 261,9 Mo | **−29,0 %** |
| `.sfx` — FST | 256,9 Mo | 256,9 Mo | 0 |
| `.sfx` total | 635,9 Mo | 525,8 Mo | −17,3 % |
| tous les autres fichiers | identiques | | 0 |
| **index** | **1 152,4 Mo** | **1 042,2 Mo** | **−9,6 %** |

L'audit prévoyait −8,7 % sur l'index de 93 605 fichiers ; ici −9,6 %, la
part des records étant un peu plus forte sur des petits segments.

**Justesse.** Le panel rend les **mêmes comptes et les mêmes spans** sur les
neuf requêtes vérifiées (16, 24, 48, 94, 286, 74, 228, 3 630, 23 documents ;
spans « exact » partout). Et l'index de référence v3, non reconstruit,
**rouvre avec le nouveau binaire** et passe le même panel : la compatibilité
de lecture est prouvée, pas supposée.

**Temps.** Dans le bruit, à la demi-milliseconde près, sur des requêtes de
2 à 48 ms :

| requête | avant | après |
|---|---|---|
| `regsiter` fz2 (la plus lourde) | 48,3 ms | 45,1 ms |
| `schdule` fz1 | 6,3 ms | 6,4 ms |
| `spin_lock_[a-z]+` rx | 4,8 ms | 4,3 ms |
| `mutex_lock` strict | 2,6 ms | 2,4 ms |

Une seule passe chacune : ce panel de 10 000 fichiers ne discrimine pas des
écarts de cette taille. La mesure qui compte se fera sur l'index de 93 605
fichiers à la fin des étapes 1 à 4.

**Tests.** `cargo test --lib` : 1 438 verts, 0 rouge (3 tests ajoutés :
record ancien contre record packé sur 300 parents, valeurs maximales avec
70 000 parents, fichier de version 3 relu par le lecteur).
`cargo test -p lucivy-core` : 184 verts, 0 rouge, 31 ignorés (les bancs).
