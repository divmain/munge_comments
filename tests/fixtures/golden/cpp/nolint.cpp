#include <cstdlib>

int risky(const char *text) {
    // NOLINTNEXTLINE(cert-err34-c)
    // Prose between the directive and its target is protected.
    return std::atoi(text);
}

int other(const char *text) {
    return std::atoi(text); // NOLINT(cert-err34-c)
}
