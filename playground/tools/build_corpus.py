#!/usr/bin/env python3
"""Build the corpora the playground serves next to the page.

`playground/corpora.json` names them: a source (a GitHub repository at a ref,
or the URL of a tarball), a licence line, the queries the terminal replays.
This script downloads the source, keeps the text files the page would index
(the same extension list and the same 100 000-byte cap as `index.html`, no
NUL byte in the first 512), repacks them as `corpus-<name>.tar.gz` under a
single top directory (the page strips it), and writes the measured counts
back into the manifest (`stats`), which the page shows before indexing.

    python3 playground/tools/build_corpus.py all
    python3 playground/tools/build_corpus.py mdn linux
    python3 playground/tools/build_corpus.py --dry-run all      # counts only

Downloads are cached in `--cache` (default `~/.cache/lucivy-corpora`). Only
the standard library is used, so the GitHub Pages workflow runs it as is.
The archives are ignored by git: they are deployment products.
"""
import argparse
import gzip
import io
import json
import os
import sys
import tarfile
import time
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
PLAYGROUND = HERE.parent
MANIFEST = PLAYGROUND / "corpora.json"

# Mirrors TEXT_EXTENSIONS / isTextFilename / MAX_FILE_SIZE in index.html.
TEXT_EXTENSIONS = {
    ".txt", ".md", ".rs", ".py", ".js", ".ts", ".jsx", ".tsx", ".json", ".toml",
    ".yaml", ".yml", ".html", ".htm", ".css", ".scss", ".less", ".go", ".java",
    ".c", ".cpp", ".cc", ".h", ".hpp", ".rb", ".sh", ".bash", ".zsh", ".fish",
    ".sql", ".xml", ".csv", ".tsv", ".r", ".swift", ".kt", ".scala",
    ".lua", ".vim", ".el", ".ex", ".exs", ".erl", ".hs", ".ml", ".mli",
    ".clj", ".lisp", ".php", ".pl", ".pm", ".tcl", ".awk", ".sed",
    ".makefile", ".cmake", ".dockerfile", ".gitignore", ".env",
    ".cfg", ".ini", ".conf", ".properties", ".lock",
  ".rst", ".adoc", ".tex", ".gd", ".tscn", ".tres", ".cs", ".m", ".mm", ".zig",
  ".dart", ".proto", ".vue", ".svelte", ".mjs", ".cjs", ".mts", ".cts", ".sgml",
  ".po", ".groovy", ".gradle", ".bat", ".ps1", ".def", ".s", ".S", ".asm",
}
TEXT_BASENAMES = {"makefile", "dockerfile", "readme", "license", "changelog", "authors", "cargo.lock"}
MAX_FILE_SIZE = 100_000


def is_text_filename(name: str) -> bool:
    lower = name.lower()
    dot = lower.rfind(".")
    if dot >= 0 and lower[dot:] in TEXT_EXTENSIONS:
        return True
    return lower.rsplit("/", 1)[-1] in TEXT_BASENAMES


def source_url(source: str) -> str:
    """`github:owner/repo@ref` → codeload tarball; anything else is a URL."""
    if source.startswith("github:"):
        spec = source[len("github:"):]
        repo, _, ref = spec.partition("@")
        ref = ref or "main"
        return f"https://codeload.github.com/{repo}/tar.gz/{ref}"
    return source


def download(url: str, dest: Path) -> None:
    if dest.exists() and dest.stat().st_size > 0:
        print(f"  cached: {dest} ({dest.stat().st_size / 1048576:.1f} MB)")
        return
    dest.parent.mkdir(parents=True, exist_ok=True)
    tmp = dest.with_suffix(dest.suffix + ".part")
    print(f"  downloading {url}")
    req = urllib.request.Request(url, headers={"User-Agent": "lucivy-playground-corpus/1.0"})
    t0 = time.time()
    with urllib.request.urlopen(req, timeout=120) as resp, open(tmp, "wb") as out:
        total = 0
        while True:
            chunk = resp.read(1 << 20)
            if not chunk:
                break
            out.write(chunk)
            total += len(chunk)
            if total % (32 << 20) < (1 << 20):
                print(f"    {total / 1048576:.0f} MB", flush=True)
    tmp.rename(dest)
    print(f"  {dest.stat().st_size / 1048576:.1f} MB in {time.time() - t0:.0f} s")


def build(name: str, entry: dict, cache: Path, dry_run: bool) -> dict:
    url = source_url(entry["source"])
    src = cache / (name + "-" + url.rsplit("/", 1)[-1].replace("?", "_"))
    if not src.suffix:
        src = src.with_suffix(".tar.gz")
    download(url, src)

    out_path = PLAYGROUND / entry.get("file", f"corpus-{name}.tar.gz")
    kept = skipped_size = skipped_binary = 0
    text_bytes = 0
    t0 = time.time()
    out_tar = None
    out_gz = None
    if not dry_run:
        tmp_out = out_path.with_suffix(".tmp")
        out_gz = gzip.open(tmp_out, "wb", compresslevel=9)
        out_tar = tarfile.open(fileobj=out_gz, mode="w|")
    with tarfile.open(src, mode="r|*") as tar:
        for member in tar:
            if not member.isfile() or not is_text_filename(member.name):
                continue
            if member.size > MAX_FILE_SIZE:
                skipped_size += 1
                continue
            data = tar.extractfile(member).read()
            if b"\0" in data[:512]:
                skipped_binary += 1
                continue
            rel = member.name.split("/", 1)[1] if "/" in member.name else member.name
            if not rel:
                continue
            kept += 1
            text_bytes += len(data)
            if out_tar is not None:
                info = tarfile.TarInfo(name=f"{name}/{rel}")
                info.size = len(data)
                info.mtime = 0
                info.mode = 0o644
                out_tar.addfile(info, io.BytesIO(data))
    if out_tar is not None:
        out_tar.close()
        out_gz.close()
        tmp_out.rename(out_path)
    archive_bytes = out_path.stat().st_size if out_path.exists() else 0
    stats = {
        "files": kept,
        "text_bytes": text_bytes,
        "archive_bytes": archive_bytes,
        "skipped_over_100k": skipped_size,
        "skipped_binary": skipped_binary,
    }
    print(f"  {name}: {kept} text files, {text_bytes / 1048576:.1f} MB of text, "
          f"archive {archive_bytes / 1048576:.1f} MB, {skipped_size} over 100 KB, "
          f"{skipped_binary} binary, {time.time() - t0:.0f} s")
    return stats


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("names", nargs="+", help="corpus names from corpora.json, or `all`")
    ap.add_argument("--cache", default=os.environ.get("LUCIVY_CORPUS_CACHE", str(Path.home() / ".cache" / "lucivy-corpora")))
    ap.add_argument("--dry-run", action="store_true", help="count, do not write archives")
    args = ap.parse_args()

    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    corpora = manifest["corpora"]
    names = list(corpora) if args.names == ["all"] else args.names
    unknown = [n for n in names if n not in corpora]
    if unknown:
        print(f"unknown corpora: {', '.join(unknown)} — known: {', '.join(corpora)}", file=sys.stderr)
        return 2

    cache = Path(args.cache)
    for name in names:
        print(f"== {name}")
        stats = build(name, corpora[name], cache, args.dry_run)
        if not args.dry_run:
            corpora[name]["stats"] = stats
    if not args.dry_run:
        MANIFEST.write_text(json.dumps(manifest, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
        print(f"stats written to {MANIFEST}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
