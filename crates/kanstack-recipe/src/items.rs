//! The `items` subcommands: add, list, promote and propose work items in the queue, through
//! whichever tracker `kanstack_tracker::select` picks for the repository. The queue itself
//! lives behind the tracker; nothing here keeps state of its own.
