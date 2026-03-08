// Direct test worker - no lucivy.js abstraction layer
import init, { Index } from './pkg/lucivy_wasm.js';

self.onmessage = async (e) => {
    function log(msg) { self.postMessage({type: 'log', msg}); }
    try {
        log('init wasm...');
        await init();
        log('init done');

        log('creating index...');
        const idx = new Index('/test', JSON.stringify([{name:'title',type:'text'}]), 'english');
        log('index created');

        log('adding doc...');
        idx.add(0, JSON.stringify({title: 'hello world test'}));
        log('doc added');

        log('committing...');
        idx.commit();
        log('committed!');

        log('searching...');
        const results = idx.search(JSON.stringify('hello'), 10, false);
        log('results: ' + results);

        // ── Snapshot tests ──────────────────────────────────
        log('--- Snapshot: export/import roundtrip ---');
        const blob = idx.exportSnapshot();
        if (blob[0] !== 0x4C || blob[1] !== 0x55 || blob[2] !== 0x43 || blob[3] !== 0x45) {
            throw new Error('FAIL: bad LUCE magic');
        }
        log('snapshot size: ' + blob.length + ' bytes');

        const idx2 = Index.importSnapshot(blob, '/test_snap');
        if (idx2.numDocs !== 1) {
            throw new Error('FAIL: expected 1 doc after import, got ' + idx2.numDocs);
        }
        const snapResults = idx2.search(JSON.stringify('hello'), 10, false);
        const snapParsed = JSON.parse(snapResults);
        if (snapParsed.length === 0) {
            throw new Error('FAIL: search after snapshot import returned no results');
        }
        log('import OK, numDocs: ' + idx2.numDocs);

        // Uncommitted export should throw
        log('--- Snapshot: uncommitted should throw ---');
        const idx3 = new Index('/test_uncommit', JSON.stringify([{name:'t',type:'text'}]), '');
        idx3.add(0, JSON.stringify({t: 'uncommitted'}));
        try {
            idx3.exportSnapshot();
            throw new Error('FAIL: should have thrown for uncommitted');
        } catch (e2) {
            if (!e2.message.includes('uncommitted')) {
                throw new Error('FAIL: wrong error: ' + e2.message);
            }
            log('correctly threw for uncommitted: ' + e2.message);
        }

        self.postMessage({type: 'done', result: 'PASS'});
    } catch(e) {
        self.postMessage({type: 'done', result: 'FAIL: ' + e.message, stack: e.stack});
    }
};
