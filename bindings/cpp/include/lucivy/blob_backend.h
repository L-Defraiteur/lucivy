// lucivy-cpp — bring your own storage.
//
// Subclass BlobBackend to make lucivy persist an index in whatever you can
// address by (index_name, file_name): a database table, an object store, a
// key-value store. Hand an instance to lucivy_create_with_blob_store() /
// lucivy_open_with_blob_store(); lucivy then calls these methods for every
// file it writes, reads, lists or deletes. The backend is the source of
// truth; the local cache directory only serves reads through mmap and can
// be thrown away at any time.
//
// Contract
// - Thread safety: every method is const and is called concurrently from
//   lucivy's scheduler threads (one shard actor per shard, plus background
//   merges). Guard your state with a mutex, or use a connection pool.
// - No re-entrancy: a method must not call back into the index it stores
//   (no search, add or commit from inside save()) — the calling thread may
//   be the one holding the shard's writer lock.
// - Errors: throw any std::exception for a failure; the message reaches the
//   caller as the error string of the lucivy call that triggered the I/O
//   (commit(), search(), the open itself).
// - Namespaces: an index named "products" stores its per-shard files under
//   "Lucivy_products/shard_0", "Lucivy_products/shard_1", ... and its root
//   files (_shard_config.json, _shard_stats.bin) under "products". Keep
//   index_name and file_name as opaque strings and store both.
#pragma once

#include <cstdint>
#include <vector>

#include "rust/cxx.h"

namespace lucivy {

class BlobBackend {
public:
  virtual ~BlobBackend() = default;

  // Fill `out` with the whole blob and return true. Return false when the
  // blob does not exist (lucivy treats that as NotFound, which is a normal
  // answer during open). Throw for any other failure.
  virtual bool load(rust::Str index_name, rust::Str file_name,
                    std::vector<uint8_t>& out) const = 0;

  // Create or overwrite the blob.
  virtual void save(rust::Str index_name, rust::Str file_name,
                    rust::Slice<const uint8_t> data) const = 0;

  // Delete the blob. Deleting a blob that does not exist is not an error.
  virtual void remove(rust::Str index_name, rust::Str file_name) const = 0;

  virtual bool exists(rust::Str index_name, rust::Str file_name) const = 0;

  // Every file_name stored under index_name (empty when the index is unknown).
  virtual rust::Vec<rust::String> list(rust::Str index_name) const = 0;

  // Optional, for lazy loading (lucivy_*_with_blob_store(..., lazy = true)).
  //
  // Size of the blob without loading it (LENGTH(_data) in SQL, a HEAD
  // request on an object store). Return false when unknown: lucivy then
  // loads that file whole when it is first opened instead of on first read.
  virtual bool blob_len(rust::Str index_name, rust::Str file_name,
                        uint64_t& out) const {
    (void)index_name; (void)file_name; (void)out;
    return false;
  }

  // `len` bytes of the blob starting at `offset`, without loading it whole
  // (SUBSTRING(_data FROM offset + 1 FOR len) in SQL, a ranged GET). Return
  // false when unsupported: lucivy then loads the whole blob instead.
  // Lucivy never asks past the end it learnt from blob_len().
  virtual bool load_range(rust::Str index_name, rust::Str file_name,
                          uint64_t offset, uint64_t len,
                          std::vector<uint8_t>& out) const {
    (void)index_name; (void)file_name; (void)offset; (void)len; (void)out;
    return false;
  }
};

}  // namespace lucivy
