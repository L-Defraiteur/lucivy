// Integration test for lucivy-cpp binding.
// Build: cargo build -p lucivy-cpp --release
// Then link against the static library and this test file.
//
// Usage (example on Linux):
//   g++ -std=c++17 -o test_lucivy test.cpp \
//     -I ../../target/release/build/lucivy-cpp-*/out/cxxbridge/include \
//     -L ../../target/release -llucivy_cpp \
//     -lpthread -ldl -lm
//   ./test_lucivy

#include <cassert>
#include <cstdio>
#include <cstdlib>
#include <filesystem>
#include <string>

#include "lucivy-cpp/src/lib.rs.h"

namespace fs = std::filesystem;

int main() {
    auto tmp = fs::temp_directory_path() / ("lucivy_cpp_test_" + std::to_string(std::rand()));
    fs::create_directories(tmp);
    auto path = tmp.string();

    try {
        // Phase 1: create, add, search, delete, update
        {
            auto idx = lucivy::lucivy_create(
                path,
                R"([
                    {"name": "title", "type": "text"},
                    {"name": "body", "type": "text"},
                    {"name": "year", "type": "i64", "indexed": true, "fast": true}
                ])",
                "english"
            );
            printf("Created index at: %s\n", std::string(idx->get_path()).c_str());

            // Add documents
            idx->add(1, R"({"title": "Rust programming guide", "body": "Learn systems programming with Rust", "year": 2024})");
            idx->add(2, R"({"title": "Python for data science", "body": "Data analysis with pandas and numpy", "year": 2023})");
            idx->add(3, R"({"title": "C++ template metaprogramming", "body": "Advanced C++ techniques", "year": 2022})");
            idx->commit();
            printf("Num docs: %lu\n", idx->num_docs());
            assert(idx->num_docs() == 3);

            // String search (contains_split on all text fields)
            printf("\n--- String search: \"rust programming\" ---\n");
            auto r1 = idx->search("\"rust programming\"", 10);
            printf("Results: %zu\n", r1.size());
            assert(r1.size() >= 1);
            assert(r1[0].doc_id == 1);

            // Contains query with highlights
            printf("\n--- Contains \"programming\" with highlights ---\n");
            auto r2 = idx->search_with_highlights(
                R"({"type": "contains", "field": "body", "value": "programming"})",
                10
            );
            printf("Results: %zu\n", r2.size());
            assert(r2.size() >= 1);
            for (auto& r : r2) {
                printf("  doc_id=%lu score=%.4f highlights=%zu fields\n",
                    r.doc_id, r.score, r.highlights.size());
            }

            // Boolean query
            printf("\n--- Boolean: must \"programming\", must_not \"python\" ---\n");
            auto r3 = idx->search(
                R"({"type": "boolean",
                    "must": [{"type": "contains", "field": "body", "value": "programming"}],
                    "must_not": [{"type": "contains", "field": "body", "value": "python"}]})",
                10
            );
            printf("Results: %zu\n", r3.size());
            for (auto& r : r3) {
                printf("  doc_id=%lu score=%.4f\n", r.doc_id, r.score);
                assert(r.doc_id != 2);
            }

            // Contains with fuzzy
            printf("\n--- Contains fuzzy: \"programing\" (distance 1) ---\n");
            auto r4 = idx->search(
                R"({"type": "contains", "field": "body", "value": "programing", "distance": 1})",
                10
            );
            printf("Results: %zu\n", r4.size());
            assert(r4.size() >= 1);

            // Contains with regex
            printf("\n--- Contains regex: \"program[a-z]+\" ---\n");
            auto r5 = idx->search(
                R"({"type": "contains", "field": "body", "value": "program[a-z]+", "regex": true})",
                10
            );
            printf("Results: %zu\n", r5.size());
            assert(r5.size() >= 1);

            // Filtered search (allowed_ids)
            printf("\n--- Filtered search: allowed_ids [1, 3] ---\n");
            uint64_t ids[] = {1, 3};
            auto r6 = idx->search_filtered("\"programming\"", 10, rust::Slice<const uint64_t>(ids, 2));
            printf("Results: %zu\n", r6.size());
            for (auto& r : r6) {
                assert(r.doc_id == 1 || r.doc_id == 3);
            }

            // Delete + update
            idx->remove(2);
            idx->update(3, R"({"title": "Modern C++", "body": "C++ best practices", "year": 2025})");
            idx->commit();
            printf("\nAfter delete+update, num docs: %lu\n", idx->num_docs());
            assert(idx->num_docs() == 2);

            // Batch add
            idx->add_many(R"([
                {"docId": 10, "title": "Go concurrency", "body": "Goroutines and channels", "year": 2024},
                {"docId": 11, "title": "Zig systems programming", "body": "Zig is a systems language", "year": 2024}
            ])");
            idx->commit();
            assert(idx->num_docs() == 4);

            // Schema info
            auto schema = idx->get_schema();
            printf("\nSchema fields:\n");
            for (auto& f : schema) {
                printf("  %s: %s\n", std::string(f.name).c_str(), std::string(f.field_type).c_str());
            }
        }
        // idx dropped here — writer lock released

        // Phase 2: reopen from disk
        {
            auto idx2 = lucivy::lucivy_open(path);
            assert(idx2->num_docs() == 4);
            auto r7 = idx2->search("\"goroutines\"", 10);
            assert(r7.size() >= 1);
            assert(r7[0].doc_id == 10);
            printf("\nReopen: found %zu results for 'goroutines'\n", r7.size());
        }

        // Phase 3: snapshot export/import
        {
            printf("\n--- Snapshot: export/import roundtrip ---\n");
            auto idx3 = lucivy::lucivy_open(path);
            auto blob = idx3->export_snapshot();
            printf("Snapshot size: %zu bytes\n", blob.size());
            assert(blob.size() > 12);
            assert(blob[0] == 'L' && blob[1] == 'U' && blob[2] == 'C' && blob[3] == 'E');

            auto snap_dst = (tmp / "snap_dst").string();
            auto idx4 = lucivy::lucivy_import_snapshot(
                rust::Slice<const uint8_t>(blob.data(), blob.size()),
                snap_dst);
            assert(idx4->num_docs() == 4);
            auto r8 = idx4->search("\"goroutines\"", 10);
            assert(r8.size() >= 1);
            printf("Import OK, numDocs: %lu\n", idx4->num_docs());
        }

        // Phase 4: snapshot to/from file
        {
            printf("\n--- Snapshot: file export/import ---\n");
            auto idx5 = lucivy::lucivy_open(path);
            auto snap_file = (tmp / "test.luce").string();
            idx5->export_snapshot_to(snap_file);

            auto snap_dst2 = (tmp / "snap_file_dst").string();
            auto idx6 = lucivy::lucivy_import_snapshot_from(snap_file, snap_dst2);
            assert(idx6->num_docs() == 4);
            printf("File import OK, numDocs: %lu\n", idx6->num_docs());
        }

        // Phase 5: uncommitted export should throw
        {
            printf("\n--- Snapshot: uncommitted should throw ---\n");
            auto snap_uncommit = (tmp / "snap_uncommit").string();
            fs::create_directories(snap_uncommit);
            auto idx7 = lucivy::lucivy_create(
                snap_uncommit,
                R"([{"name":"t","type":"text"}])",
                "");
            idx7->add(1, R"({"t":"hello"})");
            bool threw = false;
            try {
                idx7->export_snapshot();
            } catch (const std::exception& e) {
                threw = true;
                printf("Correctly threw: %s\n", e.what());
            }
            assert(threw);
        }

        printf("\nAll tests passed!\n");
    } catch (const std::exception& e) {
        fprintf(stderr, "Error: %s\n", e.what());
        fs::remove_all(tmp);
        return 1;
    }

    fs::remove_all(tmp);
    return 0;
}
