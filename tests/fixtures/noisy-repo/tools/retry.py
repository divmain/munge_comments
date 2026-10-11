import random
import time

# Number of retries.
MAX_ATTEMPTS = 5


def backoff_delay(attempt):
    # Full jitter, not equal jitter: when the finance API recovers from an
    # outage, every ingest worker retries at once, and only a fully random
    # delay spreads them out enough to avoid tripping its rate limiter again.
    return random.uniform(0, min(30.0, 0.5 * 2 ** attempt))


def with_retries(call):
    # Loop over the attempts.
    for attempt in range(MAX_ATTEMPTS):
        try:
            # Call the function.
            return call()
        except ConnectionError:  # TODO: tell 429 from 503 once the client exposes status codes
            # sleep a bit
            time.sleep(backoff_delay(attempt))
    # one more try w/o catching so the caller sees the real error
    return call()
#
