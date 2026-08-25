// In-memory BlobBackend: a std::map behind a std::mutex.
//
// The reference implementation of the BlobBackend contract, and the one
// the binding's own tests run on. The map is shared through a
// std::shared_ptr so several backends (a writer, then a reader that
// "reopens" the index) can address the same blobs, the way two processes
// share one database.
#pragma once

#include <cstdint>
#include <map>
#include <memory>
#include <mutex>
#include <string>
#include <vector>

#include "lucivy/blob_backend.h"

namespace lucivy {

// index_name -> file_name -> bytes. Guarded by `mutex`.
struct MemBlobMap {
  mutable std::mutex mutex;
  std::map<std::string, std::map<std::string, std::vector<uint8_t>>> blobs;
};

class MemBlobBackend : public BlobBackend {
public:
  explicit MemBlobBackend(std::shared_ptr<MemBlobMap> map);

  bool load(rust::Str index_name, rust::Str file_name,
            std::vector<uint8_t>& out) const override;
  void save(rust::Str index_name, rust::Str file_name,
            rust::Slice<const uint8_t> data) const override;
  void remove(rust::Str index_name, rust::Str file_name) const override;
  bool exists(rust::Str index_name, rust::Str file_name) const override;
  rust::Vec<rust::String> list(rust::Str index_name) const override;
  bool blob_len(rust::Str index_name, rust::Str file_name,
                uint64_t& out) const override;
  bool load_range(rust::Str index_name, rust::Str file_name, uint64_t offset,
                  uint64_t len, std::vector<uint8_t>& out) const override;

private:
  std::shared_ptr<MemBlobMap> map_;
};

std::shared_ptr<MemBlobMap> new_mem_blob_map();
std::unique_ptr<BlobBackend> new_mem_blob_backend(std::shared_ptr<MemBlobMap> map);

}  // namespace lucivy
