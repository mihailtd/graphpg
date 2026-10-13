//! Cron schedules (`events.schedules.<name>.cron`): their slots become events, delivered like
//! any other.
//!
//! Every instance computes the due slots, and inserts each as an event with `scheduled_for`
//! set; a unique index on `(trigger_name, scheduled_for)` and `ON CONFLICT DO NOTHING` keep one
//! event per slot, whichever instance inserts it first. No leader, no lock.
//!
//! Slots missed while no instance ran: `catch_up: latest` enqueues the most recent one (the
//! last enqueued slot is read from the event table at start), `none` skips them.
use super::cron::Schedule;
use crate::compiler::quote_ident;
use crate::config::models::{CatchUp, EventTriggersConfig};
use crate::server::ServerState;
use chrono::{DateTime, Utc};
use std::sync::Arc;
use std::time::Duration;

/// Longest sleep between looks at the clock.
const MAX_SLEEP: Duration = Duration::from_secs(30);

/// A schedule and its next slot.
struct Due {
    name: String,
    schedule: Schedule,
    next: Option<DateTime<Utc>>,
}

/// Enqueues the slot `at` of schedule `name`, unless it is already; `true` if this call did.
pub async fn enqueue(
    state: &ServerState,
    config: &EventTriggersConfig,
    name: &str,
    at: DateTime<Utc>,
) -> Result<bool, String> {
    let client = state.pool.get().await.map_err(|e| e.to_string())?;
    let payload = config
        .schedules
        .get(name)
        .map(|s| s.payload.clone())
        .unwrap_or(serde_json::Value::Null);
    let payload = (!payload.is_null()).then_some(payload);
    let inserted = client
        .execute(
            &format!(
                "INSERT INTO {}.event (trigger_name, op, payload, scheduled_for, deliver_at)
                 VALUES ($1, 'schedule', $2, $3, $3)
                 ON CONFLICT (trigger_name, scheduled_for) WHERE scheduled_for IS NOT NULL DO NOTHING",
                quote_ident(&config.schema)
            ),
            &[&name, &payload, &at],
        )
        .await
        .map_err(|e| e.to_string())?;
    if inserted > 0 {
        let _ = client
            .execute("SELECT pg_notify($1, '')", &[&config.schema])
            .await;
    }
    Ok(inserted > 0)
}

/// The slot to catch up on for `schedule` at `now`, given the last one enqueued.
pub fn missed_slot(
    schedule: &Schedule,
    last: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    catch_up: CatchUp,
) -> Option<DateTime<Utc>> {
    if catch_up == CatchUp::None {
        return None;
    }
    let mut slot = schedule.next_after(last?)?;
    if slot > now {
        return None;
    }
    // The most recent slot at or before now
    while let Some(next) = schedule.next_after(slot) {
        if next > now {
            break;
        }
        slot = next;
    }
    Some(slot)
}

/// Runs the schedules until the process ends.
pub fn start(state: Arc<ServerState>, config: EventTriggersConfig) {
    let mut due: Vec<Due> = config
        .schedules
        .iter()
        .filter_map(|(name, s)| {
            let schedule = Schedule::parse(s.cron.as_deref()?).ok()?;
            Some(Due {
                name: name.clone(),
                schedule,
                next: None,
            })
        })
        .collect();
    if due.is_empty() {
        return;
    }
    tokio::spawn(async move {
        // Missed slots, then the next ones
        let now = Utc::now();
        for d in &mut due {
            let last = last_slot(&state, &config, &d.name).await;
            let catch_up = config.schedules[&d.name].catch_up;
            if let Some(slot) = missed_slot(&d.schedule, last, now, catch_up) {
                match enqueue(&state, &config, &d.name, slot).await {
                    Ok(true) => {
                        tracing::info!(schedule = %d.name, %slot, "enqueued a missed schedule slot")
                    }
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!(schedule = %d.name, error = %e, "could not enqueue a missed slot")
                    }
                }
            }
            d.next = d.schedule.next_after(now);
        }
        loop {
            let now = Utc::now();
            for d in &mut due {
                while let Some(slot) = d.next.filter(|s| *s <= now) {
                    if let Err(e) = enqueue(&state, &config, &d.name, slot).await {
                        tracing::warn!(schedule = %d.name, error = %e, "could not enqueue a schedule slot; retrying");
                        break;
                    }
                    d.next = d.schedule.next_after(slot);
                }
            }
            let sleep = due
                .iter()
                .filter_map(|d| d.next)
                .min()
                .and_then(|n| (n - Utc::now()).to_std().ok())
                .unwrap_or(MAX_SLEEP)
                .min(MAX_SLEEP);
            tokio::time::sleep(sleep + Duration::from_millis(10)).await;
        }
    });
}

async fn last_slot(
    state: &ServerState,
    config: &EventTriggersConfig,
    name: &str,
) -> Option<DateTime<Utc>> {
    let client = state.pool.get().await.ok()?;
    client
        .query_one(
            &format!(
                "SELECT max(scheduled_for) FROM {}.event WHERE trigger_name = $1",
                quote_ident(&config.schema)
            ),
            &[&name],
        )
        .await
        .ok()?
        .get(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn catches_up_on_the_latest_missed_slot_only() {
        let hourly = Schedule::parse("@hourly").unwrap();
        let now = at("2026-10-13T10:30:00Z");
        // Down since 06:00: the 10:00 slot, not 07, 08, 09
        assert_eq!(
            missed_slot(
                &hourly,
                Some(at("2026-10-13T06:00:00Z")),
                now,
                CatchUp::Latest
            ),
            Some(at("2026-10-13T10:00:00Z"))
        );
        // Nothing missed, never run, or catch-up off
        assert_eq!(
            missed_slot(
                &hourly,
                Some(at("2026-10-13T10:00:00Z")),
                now,
                CatchUp::Latest
            ),
            None
        );
        assert_eq!(missed_slot(&hourly, None, now, CatchUp::Latest), None);
        assert_eq!(
            missed_slot(
                &hourly,
                Some(at("2026-10-13T06:00:00Z")),
                now,
                CatchUp::None
            ),
            None
        );
    }
}
