// How often the run screens ask the core for anything. A screen never asks faster than once a second, and a screen that is not shown does not ask.

/** The fastest a screen asks the core for anything: once a second. */
export const MIN_POLL_MS = 1000;

/** The pace a run that is going is read again at. */
export const RUN_POLL_MS = 2000;

/** The pace the list of runs is read again at while one of them is going. */
export const LIST_POLL_MS = 4000;

/** The pace "is a task running" is asked at, for the steering box. */
export const BUSY_POLL_MS = 2000;
