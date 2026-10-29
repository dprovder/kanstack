//! `serve`: the factory's pull loop. Starts backlog items as recipe runs while fewer than N
//! are running, moves each item's state as its run progresses, and retries failed steps
//! within their budget before the item counts as failed.
