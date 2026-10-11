#pragma once

#include <cstddef>
#include <list>
#include <string>
#include <unordered_map>

// An LRU cache.
class LruCache {
public:
    // Constructor.
    explicit LruCache(std::size_t capacity) : capacity_(capacity) {}

    bool get(const std::string &key, std::string &value);
    void put(const std::string &key, const std::string &value);

private:
    // The capacity.
    std::size_t capacity_;
    // Front is most recently used. The map stores list iterators, which
    // std::list keeps valid across splice, so touching an entry never has
    // to rewrite the map.
    std::list<std::pair<std::string, std::string>> order_;
    std::unordered_map<std::string, decltype(order_)::iterator> index_;
};
