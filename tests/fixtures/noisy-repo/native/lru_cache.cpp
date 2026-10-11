#include "lru_cache.hpp"

#include <cstdlib>

// Get a value from the cache.
bool LruCache::get(const std::string &key, std::string &value) {
    // Look up the key.
    auto found = index_.find(key);
    if (found == index_.end()) {
        return false;
    }
    // move to front so it doesnt get evicted next, splice keeps the iterator in index_ valid
    order_.splice(order_.begin(), order_, found->second);
    value = found->second->second;
    return true;
}

void LruCache::put(const std::string &key, const std::string &value) {
    // A capacity of zero means caching is disabled by configuration; the
    // early return keeps the eviction loop below from popping an empty list.
    if (capacity_ == 0) {
        return;
    }
    auto found = index_.find(key);
    if (found != index_.end()) {
        // Update the value.
        found->second->second = value;
        order_.splice(order_.begin(), order_, found->second);
        return;
    }
    // Insert at the front.
    order_.emplace_front(key, value);
    index_[key] = order_.begin();
    // Evict the least recently used entry when over capacity.
    if (order_.size() > capacity_) {
        index_.erase(order_.back().first);
        order_.pop_back();
    }
}

int capacity_from_env(const char *text) {
    // NOLINTNEXTLINE(cert-err34-c)
    // atoi's silent 0 on garbage is intended: 0 disables the cache.
    return std::atoi(text);
}
