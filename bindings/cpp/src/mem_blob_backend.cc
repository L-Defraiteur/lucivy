#include "lucivy/mem_blob_backend.h"

namespace lucivy {

MemBlobBackend::MemBlobBackend(std::shared_ptr<MemBlobMap> map)
    : map_(std::move(map)) {}

bool MemBlobBackend::load(rust::Str index_name, rust::Str file_name,
                          std::vector<uint8_t>& out) const {
  std::lock_guard<std::mutex> lock(map_->mutex);
  auto index = map_->blobs.find(std::string(index_name));
  if (index == map_->blobs.end()) return false;
  auto file = index->second.find(std::string(file_name));
  if (file == index->second.end()) return false;
  out = file->second;
  return true;
}

void MemBlobBackend::save(rust::Str index_name, rust::Str file_name,
                          rust::Slice<const uint8_t> data) const {
  std::lock_guard<std::mutex> lock(map_->mutex);
  map_->blobs[std::string(index_name)][std::string(file_name)]
      .assign(data.data(), data.data() + data.size());
}

void MemBlobBackend::remove(rust::Str index_name, rust::Str file_name) const {
  std::lock_guard<std::mutex> lock(map_->mutex);
  auto index = map_->blobs.find(std::string(index_name));
  if (index != map_->blobs.end()) index->second.erase(std::string(file_name));
}

bool MemBlobBackend::exists(rust::Str index_name, rust::Str file_name) const {
  std::lock_guard<std::mutex> lock(map_->mutex);
  auto index = map_->blobs.find(std::string(index_name));
  return index != map_->blobs.end() &&
         index->second.count(std::string(file_name)) != 0;
}

rust::Vec<rust::String> MemBlobBackend::list(rust::Str index_name) const {
  std::lock_guard<std::mutex> lock(map_->mutex);
  rust::Vec<rust::String> names;
  auto index = map_->blobs.find(std::string(index_name));
  if (index != map_->blobs.end()) {
    for (const auto& entry : index->second) names.push_back(entry.first);
  }
  return names;
}

bool MemBlobBackend::blob_len(rust::Str index_name, rust::Str file_name,
                              uint64_t& out) const {
  std::lock_guard<std::mutex> lock(map_->mutex);
  auto index = map_->blobs.find(std::string(index_name));
  if (index == map_->blobs.end()) return false;
  auto file = index->second.find(std::string(file_name));
  if (file == index->second.end()) return false;
  out = file->second.size();
  return true;
}

bool MemBlobBackend::load_range(rust::Str index_name, rust::Str file_name,
                                uint64_t offset, uint64_t len,
                                std::vector<uint8_t>& out) const {
  std::lock_guard<std::mutex> lock(map_->mutex);
  auto index = map_->blobs.find(std::string(index_name));
  if (index == map_->blobs.end()) return false;
  auto file = index->second.find(std::string(file_name));
  if (file == index->second.end()) return false;
  const auto& bytes = file->second;
  uint64_t start = offset < bytes.size() ? offset : bytes.size();
  uint64_t end = start + len < bytes.size() ? start + len : bytes.size();
  out.assign(bytes.begin() + start, bytes.begin() + end);
  return true;
}

std::shared_ptr<MemBlobMap> new_mem_blob_map() {
  return std::make_shared<MemBlobMap>();
}

std::unique_ptr<BlobBackend> new_mem_blob_backend(std::shared_ptr<MemBlobMap> map) {
  return std::make_unique<MemBlobBackend>(std::move(map));
}

}  // namespace lucivy
