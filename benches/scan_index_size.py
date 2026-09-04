#!/usr/bin/env python3
"""Read-only scan of a lucivy v3 segment directory: section sizes and stats."""
import os, sys, struct, glob, time, collections
from array import array

D = sys.argv[1]
DEEP_SEG = sys.argv[2] if len(sys.argv) > 2 else None  # segment uuid (no dashes) to scan deeply

def varint(b, pos):
    shift = 0; val = 0
    while True:
        c = b[pos]; pos += 1
        val |= (c & 0x7f) << shift
        if c < 0x80: return val, pos
        shift += 7

def sections(b):
    assert b[4:5] and b[0:4] in (b"SFX3", b"TTX3"), b[0:4]
    n = struct.unpack_from("<H", b, 5)[0]
    out = {}
    for i in range(n):
        sid, _, off, ln = struct.unpack_from("<HHII", b, 7 + 12 * i)
        base = 7 + 12 * n
        out[sid] = (base + off, ln)
    return out

def scan_sfx(path, deep):
    b = open(path, "rb").read()
    s = sections(b)
    fst_off, fst_len = s[1]; par_off, par_len = s[2]
    r = {"file": len(b), "fst": fst_len, "parents": par_len}
    if deep:
        pos = par_off; end = par_off + par_len
        nrec = 0; nparents = 0; hist = collections.Counter(); maxc = 0
        while pos < end:
            ln, pos = varint(b, pos)
            cnt = struct.unpack_from("<I", b, pos)[0]
            nrec += 1; nparents += cnt; maxc = max(maxc, cnt)
            if cnt <= 2: hist["2"] += 1
            elif cnt <= 10: hist["3-10"] += 1
            elif cnt <= 100: hist["11-100"] += 1
            elif cnt <= 1000: hist["101-1k"] += 1
            elif cnt <= 10000: hist["1k-10k"] += 1
            else: hist[">10k"] += 1
            pos += ln
        r.update(records=nrec, parents=nparents, max_parents=maxc, hist=dict(hist))
    return r

def scan_termtexts(path, deep):
    b = open(path, "rb").read()
    s = sections(b)
    toff, tlen = s[1]; moff, mlen = s[2]
    n = struct.unpack_from("<I", b, toff)[0]
    offs = array("I"); offs.frombytes(b[toff + 4: toff + 4 + 4 * (n + 1)])
    text_bytes = offs[n]
    r = {"file": len(b), "num_terms": n, "text_bytes": text_bytes, "offsets_bytes": 4 * (n + 1), "meta_bytes": mlen}
    if deep:
        data_start = toff + 4 + 4 * (n + 1)
        mcount = struct.unpack_from("<I", b, moff)[0]
        mb = b[moff + 4: moff + 4 + 6 * mcount]
        n_ws = 0; ws_text = 0; ch_text = 0; n_ovl = 0; n_sep = 0
        keys_chunk = 0; markers = 0; keys_ws = 0
        lcp_total = 0; prev = b""
        len_hist = collections.Counter()
        own_gt_255 = 0
        for i in range(n):
            own, sep, ovl, ws_start, is_ws = struct.unpack_from("<HBBBB", mb, 6 * i)
            t = b[data_start + offs[i]: data_start + offs[i + 1]]
            L = len(t)
            # common prefix with previous text (front coding potential)
            m = min(len(prev), L); k = 0
            while k < m and prev[k] == t[k]: k += 1
            lcp_total += k; prev = t
            if is_ws:
                n_ws += 1; ws_text += L
                keys_ws += min(L - ovl, 256) if L - ovl > 0 else 0
                len_hist["ws:" + ("<=8" if L <= 8 else "9-16" if L <= 16 else "17-32" if L <= 32 else "33-256" if L <= 256 else ">256")] += 1
            else:
                ch_text += L
                keys_chunk += min(L, 256)
                if ovl > 0: n_ovl += 1; markers += min(own, L)  # one marker per si < own_len
                if sep > 0: n_sep += 1
            if own > 255: own_gt_255 += 1
        r.update(n_ws=n_ws, n_chunk=n - n_ws, ws_text_bytes=ws_text, chunk_text_bytes=ch_text,
                 n_chunk_with_overlap=n_ovl, n_chunk_with_sep=n_sep,
                 est_keys_chunk_suffixes=keys_chunk, est_keys_chunk_markers=markers, est_keys_ws=keys_ws,
                 lcp_total=lcp_total, own_gt_255=own_gt_255, len_hist=dict(len_hist))
    return r

def scan_bytemap(path, deep):
    b = open(path, "rb").read()
    n = struct.unpack_from("<I", b, 4)[0]
    r = {"file": len(b), "num_ordinals": n}
    if deep:
        distinct = set(); pure_sep = 0
        content_mask = bytearray(32)
        for c in range(256):
            if (48 <= c <= 57) or (65 <= c <= 90) or (97 <= c <= 122) or c >= 128:
                content_mask[c // 8] |= 1 << (c % 8)
        for i in range(n):
            bm = b[8 + 32 * i: 8 + 32 * i + 32]
            distinct.add(bm)
            if not any(bm[j] & content_mask[j] for j in range(32)): pure_sep += 1
        r.update(distinct_bitmaps=len(distinct), pure_sep_ordinals=pure_sep)
    return r

def scan_posmap(path, deep, magic):
    b = open(path, "rb").read()
    if len(b) < 8: return {"file": len(b), "empty": True}
    assert b[0:4] == magic, b[0:4]
    n = struct.unpack_from("<I", b, 4)[0]
    data = b[8 + 8 * (n + 1):]
    slots = len(data) // 4
    r = {"file": len(b), "num_docs": n, "offset_table_bytes": 8 * (n + 1), "slots": slots}
    if deep:
        a = array("I"); a.frombytes(data[: slots * 4])
        empty = a.count(0xFFFFFFFF)
        r.update(empty_slots=empty)
        if magic == b"PMAP":
            r.update(max_ordinal=max(x for x in a if x != 0xFFFFFFFF))
        else:
            spans = collections.Counter()
            for x in a:
                if x != 0xFFFFFFFF: spans[min(x >> 24, 4)] += 1
            r.update(span_hist={str(k): v for k, v in sorted(spans.items())})
    return r

def scan_sibling(path, deep):
    b = open(path, "rb").read()
    assert struct.unpack_from("<I", b, 0)[0] == 0xFFFFFFFF and b[4:8] == b"SIB2"
    n = struct.unpack_from("<I", b, 8)[0]
    head = 12 + 4 * (n + 1)
    r = {"file": len(b), "num_ordinals": n, "offset_table_bytes": 4 * (n + 1), "entries_bytes": len(b) - head}
    if deep:
        pos = head; end = len(b); cnt = 0; gap_nonzero = 0; gap_bytes = 0; ord_with_entries = 0
        offs = array("I"); offs.frombytes(b[12: 12 + 4 * (n + 1)])
        for o in range(n):
            if offs[o + 1] > offs[o]: ord_with_entries += 1
        while pos < end:
            tok, pos = varint(b, pos); cnt += 1
            if tok & 1:
                p0 = pos; _, pos = varint(b, pos); gap_nonzero += 1; gap_bytes += pos - p0
        r.update(entries=cnt, gap_nonzero=gap_nonzero, gap_bytes=gap_bytes, ordinals_with_entries=ord_with_entries)
    return r

def scan_sfxpost(path, deep):
    b = open(path, "rb").read()
    assert b[0:4] == b"SFP3"
    n = struct.unpack_from("<I", b, 4)[0]
    r = {"file": len(b), "num_terms": n, "offset_table_bytes": 4 * (n + 1), "entry_bytes": len(b) - 8 - 4 * (n + 1)}
    if deep:
        offs = array("I"); offs.frombytes(b[8: 8 + 4 * (n + 1)])
        base = 8 + 4 * (n + 1)
        docs = 0; entries = 0; hdr_bytes = 0; ckpt_bytes = 0; payload_bytes = 0; nonempty = 0
        for o in range(n):
            s, e = base + offs[o], base + offs[o + 1]
            if e <= s: continue
            nonempty += 1
            pos = s
            nd, pos = varint(b, pos); hl, pos = varint(b, pos)
            c = 0 if nd == 0 else (nd - 1) // 8
            ckpt_bytes += 12 * c; pos += 12 * c
            hstart = pos; hend = pos + hl
            docs += nd
            for _ in range(nd):
                _, pos = varint(b, pos); _, pos = varint(b, pos); ec, pos = varint(b, pos); entries += ec
            hdr_bytes += hl
            payload_bytes += e - hend
        r.update(docs=docs, entries=entries, header_bytes=hdr_bytes, checkpoint_bytes=ckpt_bytes,
                 payload_bytes=payload_bytes, nonempty_ordinals=nonempty)
    return r

def scan_wsp(path, deep):
    b = open(path, "rb").read()
    assert b[0:4] == b"WSP3"
    n = struct.unpack_from("<I", b, 4)[0]
    r = {"file": len(b), "num_ordinals": n, "offset_table_bytes": 4 * (n + 1), "entry_bytes": len(b) - 8 - 4 * (n + 1)}
    if deep:
        offs = array("I"); offs.frombytes(b[8: 8 + 4 * (n + 1)])
        entries = 0; nonempty = 0; ckpt = 0
        for o in range(n):
            s, e = offs[o], offs[o + 1]
            if e <= s: continue
            nonempty += 1
            ne, _ = varint(b, s); entries += ne
            ckpt += 16 * (0 if ne == 0 else (ne - 1) // 32)
        r.update(entries=entries, nonempty_ordinals=nonempty, checkpoint_bytes=ckpt)
    return r

totals = collections.defaultdict(lambda: collections.Counter())
deep_results = {}
t0 = time.time()
for path in sorted(glob.glob(os.path.join(D, "*"))):
    name = os.path.basename(path)
    parts = name.split(".")
    if len(parts) != 3: continue
    seg, field, ext = parts
    deep = (DEEP_SEG is not None and seg == DEEP_SEG)
    try:
        if ext == "sfx": r = scan_sfx(path, deep)
        elif ext == "termtexts": r = scan_termtexts(path, deep)
        elif ext == "bytemap": r = scan_bytemap(path, deep)
        elif ext == "posmap": r = scan_posmap(path, deep, b"PMAP")
        elif ext == "word_pos_map": r = scan_posmap(path, deep, b"WMP2")
        elif ext == "sibling_v3": r = scan_sibling(path, deep)
        elif ext == "sfxpost": r = scan_sfxpost(path, deep)
        elif ext == "word_sfxpost": r = scan_wsp(path, deep)
        else: continue
    except Exception as ex:
        print("ERR", name, ex); continue
    for k, v in r.items():
        if isinstance(v, int): totals[ext][k] += v
    if deep:
        deep_results[(field, ext)] = r
        print(f"[deep {time.time()-t0:.0f}s] {name}: {r}", flush=True)

print("\n=== TOTALS over all segments (ints summed) ===")
for ext in sorted(totals):
    print(ext, dict(totals[ext]))
