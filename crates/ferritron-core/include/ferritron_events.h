/* Helpers shims share for the events they hand to process().
 *
 * Shims collect parameter points and notes from the host in whatever order
 * the format delivers them and then call ferritron_sort_events() once:
 * O(n log n), stable (events at the same offset keep their order), and it
 * allocates nothing on the heap. A block that is already in order (one
 * parameter's points, or notes alone) costs a single pass. */
#ifndef FERRITRON_EVENTS_H
#define FERRITRON_EVENTS_H

#include "ferritron_abi.h"

#ifdef __cplusplus
extern "C" {
#endif

/* Whether `events[0..n)` is sorted by offset. */
static inline bool ferritron_events_sorted(const ferritron_event* events, uint32_t n) {
    for (uint32_t i = 1; i < n; ++i)
        if (events[i - 1].offset > events[i].offset)
            return false;
    return true;
}

/* Sorts `events[0..n)` by offset. `scratch` holds n events (the caller's
 * stack or a preallocated buffer); it is only touched when the input is not
 * already in order. Bottom-up merge sort: stable. */
static inline void ferritron_sort_events(ferritron_event* events, ferritron_event* scratch, uint32_t n) {
    if (ferritron_events_sorted(events, n))
        return;
    ferritron_event* from = events;
    ferritron_event* to = scratch;
    for (uint32_t width = 1; width < n; width *= 2) {
        for (uint32_t lo = 0; lo < n; lo += 2 * width) {
            uint32_t mid = lo + width < n ? lo + width : n;
            uint32_t hi = lo + 2 * width < n ? lo + 2 * width : n;
            uint32_t a = lo, b = mid, k = lo;
            while (a < mid && b < hi)
                to[k++] = from[b].offset < from[a].offset ? from[b++] : from[a++];
            while (a < mid)
                to[k++] = from[a++];
            while (b < hi)
                to[k++] = from[b++];
        }
        ferritron_event* t = from;
        from = to;
        to = t;
    }
    if (from != events)
        for (uint32_t i = 0; i < n; ++i)
            events[i] = from[i];
}

#ifdef __cplusplus
}
#endif

#endif /* FERRITRON_EVENTS_H */
