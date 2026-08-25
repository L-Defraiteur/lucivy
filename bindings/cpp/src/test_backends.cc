#include "test_backends.h"

#include <stdexcept>

namespace lucivy {

namespace {

std::string key_of(rust::Str index_name, rust::Str file_name) {
  return std::string(index_name) + "/" + std::string(file_name);
}

class ProbedBackend : public BlobBackend {
public:
  ProbedBackend(std::shared_ptr<MemBlobMap> map, std::shared_ptr<BackendProbe> probe)
      : inner_(std::move(map)), probe_(std::move(probe)) {}

  bool load(rust::Str index_name, rust::Str file_name,
            std::vector<uint8_t>& out) const override {
    {
      std::lock_guard<std::mutex> lock(probe_->mutex);
      probe_->loads[key_of(index_name, file_name)] += 1;
    }
    return inner_.load(index_name, file_name, out);
  }

  void save(rust::Str index_name, rust::Str file_name,
            rust::Slice<const uint8_t> data) const override {
    if (probe_->fail_saves.load()) {
      throw std::runtime_error("injected save failure for " +
                               key_of(index_name, file_name));
    }
    inner_.save(index_name, file_name, data);
  }

  void remove(rust::Str index_name, rust::Str file_name) const override {
    inner_.remove(index_name, file_name);
  }

  bool exists(rust::Str index_name, rust::Str file_name) const override {
    return inner_.exists(index_name, file_name);
  }

  rust::Vec<rust::String> list(rust::Str index_name) const override {
    return inner_.list(index_name);
  }

  bool blob_len(rust::Str index_name, rust::Str file_name,
                uint64_t& out) const override {
    if (!probe_->lazy.load()) return false;
    return inner_.blob_len(index_name, file_name, out);
  }

  bool load_range(rust::Str index_name, rust::Str file_name, uint64_t offset,
                  uint64_t len, std::vector<uint8_t>& out) const override {
    if (!probe_->lazy.load()) return false;
    {
      std::lock_guard<std::mutex> lock(probe_->mutex);
      probe_->range_loads[key_of(index_name, file_name)] += 1;
    }
    return inner_.load_range(index_name, file_name, offset, len, out);
  }

private:
  MemBlobBackend inner_;
  std::shared_ptr<BackendProbe> probe_;
};

}  // namespace

std::shared_ptr<BackendProbe> new_backend_probe() {
  return std::make_shared<BackendProbe>();
}

std::unique_ptr<BlobBackend> new_probed_backend(std::shared_ptr<MemBlobMap> map,
                                                std::shared_ptr<BackendProbe> probe) {
  return std::make_unique<ProbedBackend>(std::move(map), std::move(probe));
}

void probe_set_fail_saves(const BackendProbe& probe, bool fail) {
  probe.fail_saves.store(fail);
}

void probe_set_lazy(const BackendProbe& probe, bool lazy) {
  probe.lazy.store(lazy);
}

uint64_t probe_loads(const BackendProbe& probe, rust::Str key) {
  std::lock_guard<std::mutex> lock(probe.mutex);
  auto it = probe.loads.find(std::string(key));
  return it == probe.loads.end() ? 0 : it->second;
}

rust::Vec<rust::String> probe_loaded_keys(const BackendProbe& probe) {
  std::lock_guard<std::mutex> lock(probe.mutex);
  rust::Vec<rust::String> keys;
  for (const auto& entry : probe.loads) keys.push_back(entry.first);
  return keys;
}

uint64_t probe_range_loads(const BackendProbe& probe) {
  std::lock_guard<std::mutex> lock(probe.mutex);
  uint64_t total = 0;
  for (const auto& entry : probe.range_loads) total += entry.second;
  return total;
}

void probe_reset(const BackendProbe& probe) {
  std::lock_guard<std::mutex> lock(probe.mutex);
  probe.loads.clear();
  probe.range_loads.clear();
}

}  // namespace lucivy
