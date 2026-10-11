import { useEffect, useState } from "react";

// A hook that debounces a value.
export function useDebouncedValue<T,>(value: T, delayMs: number): T {
  // State for the debounced value.
  const [debounced, setDebounced] = useState(value);
  useEffect(() => {
    // Set a timeout.
    const handle = setTimeout(() => setDebounced(value), delayMs);
    // Clearing on every change is what makes this a debounce rather than a
    // throttle: only the last value within the window is ever committed.
    return () => clearTimeout(handle);
  }, [value, delayMs]);
  return debounced;
}

// 300ms felt laggy in user testing so we use 150, dont bump it w/o asking design
export const SEARCH_DEBOUNCE_MS = 150;
