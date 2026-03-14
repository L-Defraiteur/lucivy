#!/usr/bin/env python3
"""Build the playground dataset.luce from the lucivy source tree."""

import os
import lucivy
import tempfile
import shutil

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

EXCLUDE_DIRS = {"target", "node_modules", "__pycache__", ".venv", ".pytest_cache", "pkg", ".git", "playground"}
EXCLUDE_FILES = {"package-lock.json", ".env", ".gitignore"}
MAX_FILE_SIZE = 100_000  # skip files > 100KB (ascii_folding_filter.rs etc.)

def is_text_file(path, sample_size=8192):
    """Detect if a file is text by checking for null bytes and valid UTF-8."""
    try:
        with open(path, "rb") as f:
            chunk = f.read(sample_size)
        if not chunk:
            return False
        if b"\x00" in chunk:
            return False
        chunk.decode("utf-8")
        return True
    except (UnicodeDecodeError, OSError):
        return False

def collect_files():
    files = []
    for root, dirs, filenames in os.walk(REPO_ROOT):
        dirs[:] = [d for d in dirs if d not in EXCLUDE_DIRS]
        for fname in filenames:
            if fname in EXCLUDE_FILES:
                continue
            full = os.path.join(root, fname)
            if os.path.getsize(full) > MAX_FILE_SIZE:
                continue
            if not is_text_file(full):
                continue
            rel = os.path.relpath(full, REPO_ROOT)
            try:
                content = open(full, "r", encoding="utf-8", errors="ignore").read()
            except Exception:
                continue
            if not content.strip():
                continue
            files.append((rel, content))
    return files

def main():
    files = collect_files()
    print(f"Collected {len(files)} files")

    tmp = tempfile.mkdtemp(prefix="lucivy_playground_")
    try:
        idx = lucivy.Index.create(tmp, fields=[
            {"name": "path", "type": "text"},
            {"name": "content", "type": "text"},
            {"name": "extension", "type": "text"},
        ])

        for i, (path, content) in enumerate(files):
            ext = os.path.splitext(path)[1]
            idx.add(i, path=path, content=content, extension=ext)

        idx.commit()
        print(f"Indexed {idx.num_docs} documents")

        out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "dataset.luce")
        idx.export_snapshot_to(out)
        size = os.path.getsize(out)
        print(f"Exported to {out} ({size:,} bytes, {size/1024/1024:.1f} MB)")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

if __name__ == "__main__":
    main()
