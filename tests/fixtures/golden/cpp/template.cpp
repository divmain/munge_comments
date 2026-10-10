#include <vector>

// Sums any container of numbers.
template <typename Container>
typename Container::value_type sum(const Container &items) {
    typename Container::value_type total{}; // zero-initialized
    for (const auto &item : items) {
        total += item;
    }
    return total;
}

/* Explicit use, so the template is instantiated. */
int useSum() {
    std::vector<int> values{1, 2, 3};
    return sum(values);
}
