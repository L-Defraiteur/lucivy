//! La purge des documents orphelins d'une table du plein texte de rag3weaver (14 novembre 2026, session
//! hotfixs de rag3db, ticket `docs/recherche/tickets/2026-11-14-un-scope-efface-du-graphe-garde-son-document-plein-texte.md`).
//!
//! Sur une COPIE d'une table `Lucivy_<T>` (l'ouverture écrit) : le rapport d'index avant, un banc de
//! requêtes `contains` qui relève tous les `_node_id` touchés, l'effacement des `_node_id` de la liste,
//! la validation, la fin des fusions, le rapport après, puis le banc rejoué. Attendu : chaque requête rend
//! les mêmes `_node_id` qu'avant, moins les orphelins, et le compte des documents baisse d'autant.
//! Puis la table est rouverte, et le compte relu.
//!
//!     PURGE_TABLE=<copie>/x.rag3db.fts/Lucivy_Scope PURGE_IDS=<un _node_id par ligne> \
//!     PURGE_CHAMP=content PURGE_MOTS=fn,self,commit \
//!     cargo test --release -p lucivy-core --test diag_purge_orphelins -- --ignored --nocapture

use std::collections::{BTreeMap, BTreeSet};

use lucivy_core::query::QueryConfig;
use lucivy_core::sharded_handle::ShardedHandle;

const TOUT: usize = 10_000_000;

fn banc(index: &ShardedHandle, champ: &str, mots: &[String]) -> BTreeMap<String, BTreeSet<u64>> {
    mots.iter()
        .map(|mot| {
            let q = QueryConfig { query_type: "contains".into(), field: Some(champ.into()), value: Some(mot.clone()), ..Default::default() };
            let res = index.search(&q, TOUT, None).unwrap();
            assert!(!index.last_search_truncated(), "« {mot} » : recherche tronquée");
            (mot.clone(), index.node_ids_of(&res).unwrap().into_iter().collect())
        })
        .collect()
}

#[test]
#[ignore]
fn purge_orphelins() {
    let table = std::env::var("PURGE_TABLE").expect("PURGE_TABLE");
    let ids: BTreeSet<u64> = std::fs::read_to_string(std::env::var("PURGE_IDS").expect("PURGE_IDS"))
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().parse().expect("un _node_id par ligne"))
        .collect();
    let champ = std::env::var("PURGE_CHAMP").unwrap_or_else(|_| "content".into());
    let mots: Vec<String> = std::env::var("PURGE_MOTS").expect("PURGE_MOTS").split(',').map(str::to_string).collect();

    let index = ShardedHandle::open(&table).unwrap();
    let avant_docs = index.num_docs();
    let avant = index.index_report().unwrap();
    eprintln!("RAPPORT AVANT\n{}", avant.summary());
    let banc_avant = banc(&index, &champ, &mots);
    let mut touches = BTreeSet::new();
    for (mot, hits) in &banc_avant {
        let orph = hits.intersection(&ids).count();
        touches.extend(hits.intersection(&ids).copied());
        eprintln!("BANC AVANT « {mot} » : {} hits, dont {orph} orphelins", hits.len());
    }

    let t = std::time::Instant::now();
    for &id in &ids {
        index.delete_by_node_id(id).unwrap();
    }
    index.commit().unwrap();
    let fusions = index.wait_merges_quiet().unwrap();
    eprintln!("PURGE : {} _node_id effacés, validés, {fusions} fusions attendues, {} ms", ids.len(), t.elapsed().as_millis());

    let apres_docs = index.num_docs();
    let apres = index.index_report().unwrap();
    eprintln!("RAPPORT APRÈS\n{}", apres.summary());
    let banc_apres = banc(&index, &champ, &mots);
    let mut ecarts = Vec::new();
    for (mot, hits) in &banc_apres {
        let attendu: BTreeSet<u64> = banc_avant[mot].difference(&ids).copied().collect();
        eprintln!("BANC APRÈS « {mot} » : {} hits (attendus {})", hits.len(), attendu.len());
        if *hits != attendu {
            ecarts.push(format!("« {mot} » : {} en trop, {} en moins", hits.difference(&attendu).count(), attendu.difference(hits).count()));
        }
    }
    index.close().unwrap();
    drop(index);
    let rouvert = ShardedHandle::open(&table).unwrap().num_docs();
    eprintln!(
        "BILAN {table} : documents {avant_docs} → {apres_docs} (rouvert : {rouvert}), {} orphelins effacés, {} touchés par le banc ; \
         octets d'index {} → {}, supprimés en segments {} → {}",
        ids.len(),
        touches.len(),
        avant.index_bytes,
        apres.index_bytes,
        avant.segments.iter().map(|s| s.deleted as u64).sum::<u64>(),
        apres.segments.iter().map(|s| s.deleted as u64).sum::<u64>(),
    );
    assert!(ecarts.is_empty(), "[check: memes-hits-moins-les-orphelins] {ecarts:?}");
    assert_eq!(avant_docs - apres_docs, ids.len() as u64, "[check: le-compte-baisse-d-autant]");
    assert_eq!(rouvert, apres_docs, "[check: la-purge-survit-a-la-reouverture]");
    assert!(!touches.is_empty(), "[check: le-banc-touche-des-orphelins] aucun orphelin dans le banc : le témoin ne prouverait rien");
}
