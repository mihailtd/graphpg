//! Event triggers (see `docs/events.md`): webhooks called when rows change and on schedules.
//!
//! - [`install`]: the event table, the capture functions and the triggers on the tables,
//!   writing events in the same transaction as the change (a transactional outbox).
//! - [`worker`]: claims due events with a lease (safe across instances), delivers them signed
//!   per Standard Webhooks, retries with backoff, dead-letters, and cleans up.
//! - [`scheduler`]: enqueues the slots of cron schedules, once across instances.
//! - [`cron`]: the schedule syntax.
pub mod cron;
pub mod install;
pub mod scheduler;
pub mod worker;
