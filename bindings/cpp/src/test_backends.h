// Test-only backends for the binding's own test suite: an in-memory backend
// that can be told to fail, and that records which blobs lucivy asked for.
// Not part of the public API (lives under src/, not include/).
#pragma once

#include <atomic>
#include <cstdint>
#include <map>
#include <memory>
#include <mutex>
#include <string>

#include "lucivy/mem_blob_backend.h"

namespace lucivy {

// Shared between a ProbedBackend and the test that inspects it.
// The bridge hands the probe around as const; everything in it is state,
// not identity, hence mutable (the maps always under `mutex`).
struct BackendProbe {
  // save() throws while set.
  mutable std::atomic<bool> fail_saves{false};
  // Whether blob_len()/load_range() are answered (lazy mode) or refused.
  mutable std::atomic<bool> lazy{false};

  mutable std::mutex mutex;
  // "index/file" -> number of whole loads.
  mutable std::map<std::string, uint64_t> loads;
  // "index/file" -> number of range reads.
  mutable std::map<std::string, uint64_t> range_loads;
};

std::shared_ptr<BackendProbe> new_backend_probe();
std::unique_ptr<BlobBackend> new_probed_backend(std::shared_ptr<MemBlobMap> map,
                                                std::shared_ptr<BackendProbe> probe);

void probe_set_fail_saves(const BackendProbe& probe, bool fail);
void probe_set_lazy(const BackendProbe& probe, bool lazy);
// Whole loads of "index/file" since the last reset.
uint64_t probe_loads(const BackendProbe& probe, rust::Str key);
// Every "index/file" loaded whole since the last reset.
rust::Vec<rust::String> probe_loaded_keys(const BackendProbe& probe);
// Range reads over every file since the last reset.
uint64_t probe_range_loads(const BackendProbe& probe);
void probe_reset(const BackendProbe& probe);

}  // namespace lucivy
