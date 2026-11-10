use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::cron::{ParsedCron, next_fire, parse};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CronEntry {
    pub id: String,
    pub cron: String,
    pub prompt: String,
    #[serde(default = "default_recurring")]
    pub recurring: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, alias = "createdAt", skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiredCron {
    pub entry: CronEntry,
    pub coalesced_count: u32,
    pub stale: bool,
}

fn default_recurring() -> bool {
    true
}

pub fn next_fire_for_entry(entry: &CronEntry, from_ms: i64, tz_offset_minutes: i32) -> Option<i64> {
    let parsed = parse(&entry.cron).ok()?;
    let ideal = next_fire(&parsed, from_ms, tz_offset_minutes)?;
    Some(if entry.recurring {
        crate::cron::jitter::jittered_next_run_ms(
            &entry.id,
            &parsed,
            ideal,
            tz_offset_minutes,
            false,
        )
    } else {
        crate::cron::jitter::one_shot_jittered_run_ms(
            &entry.id,
            entry.created_at,
            ideal,
            tz_offset_minutes,
            false,
        )
    })
}

#[derive(Debug, Clone)]
struct ScheduledEntry {
    entry: CronEntry,
    parsed: ParsedCron,
}

#[derive(Debug)]
pub struct CronScheduler {
    entries: Vec<ScheduledEntry>,
    tz_offset_minutes: i32,
}

impl CronScheduler {
    pub fn new(entries: Vec<CronEntry>, tz_offset_minutes: i32) -> Self {
        let entries = entries
            .into_iter()
            .filter_map(|entry| {
                parse(&entry.cron)
                    .ok()
                    .map(|parsed| ScheduledEntry { entry, parsed })
            })
            .collect();
        Self {
            entries,
            tz_offset_minutes,
        }
    }

    pub fn add_entry(&mut self, entry: CronEntry) -> bool {
        if let Ok(parsed) = parse(&entry.cron) {
            if let Some(pos) = self.entries.iter().position(|s| s.entry.id == entry.id) {
                self.entries[pos] = ScheduledEntry { entry, parsed };
            } else {
                self.entries.push(ScheduledEntry { entry, parsed });
            }
            true
        } else {
            false
        }
    }

    pub fn remove_entry(&mut self, id: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|s| s.entry.id != id);
        self.entries.len() < before
    }

    pub fn list_entries(&self) -> Vec<CronEntry> {
        self.entries.iter().map(|s| s.entry.clone()).collect()
    }

    pub fn next_fire_at(&self, from_ms: i64) -> Option<i64> {
        self.entries
            .iter()
            .filter_map(|s| {
                let ideal = next_fire(&s.parsed, from_ms, self.tz_offset_minutes)?;
                Some(Self::jittered_fire_impl(
                    &s.entry,
                    &s.parsed,
                    ideal,
                    self.tz_offset_minutes,
                ))
            })
            .min()
    }

    fn jittered_fire_impl(
        entry: &CronEntry,
        parsed: &ParsedCron,
        ideal_ms: i64,
        tz_offset_minutes: i32,
    ) -> i64 {
        if entry.recurring {
            crate::cron::jitter::jittered_next_run_ms(
                &entry.id,
                parsed,
                ideal_ms,
                tz_offset_minutes,
                false,
            )
        } else {
            crate::cron::jitter::one_shot_jittered_run_ms(
                &entry.id,
                entry.created_at,
                ideal_ms,
                tz_offset_minutes,
                false,
            )
        }
    }

    pub fn tick(&mut self, from_ms: i64, now_ms: i64) -> Vec<FiredCron> {
        let tz = self.tz_offset_minutes;
        let mut fired: Vec<(i64, FiredCron)> = Vec::new();
        let mut kept: Vec<ScheduledEntry> = Vec::with_capacity(self.entries.len());
        for s in self.entries.drain(..) {
            let Some(ideal) = next_fire(&s.parsed, from_ms, tz) else {
                kept.push(s);
                continue;
            };
            let fire_at = Self::jittered_fire_impl(&s.entry, &s.parsed, ideal, tz);
            if fire_at > now_ms {
                kept.push(s);
                continue;
            }
            let (coalesced_count, _) = crate::cron::jitter::count_coalesced(
                &s.entry.id,
                s.entry.recurring,
                s.entry.created_at,
                &s.parsed,
                ideal,
                now_ms,
                tz,
            );
            let stale = s.entry.recurring
                && crate::cron::jitter::is_stale_at(s.entry.created_at, now_ms, false);
            fired.push((
                fire_at,
                FiredCron {
                    entry: s.entry.clone(),
                    coalesced_count,
                    stale,
                },
            ));
            if s.entry.recurring && !stale {
                kept.push(s);
            }
        }
        self.entries = kept;
        fired.sort_by_key(|(fire, _)| *fire);
        fired.into_iter().map(|(_, entry)| entry).collect()
    }

    pub fn tick_if_idle<F>(&mut self, from_ms: i64, now_ms: i64, busy: F) -> Option<Vec<FiredCron>>
    where
        F: Fn(&CronEntry) -> bool,
    {
        if self.entries.iter().any(|s| busy(&s.entry)) {
            return None;
        }
        Some(self.tick(from_ms, now_ms))
    }

    pub fn start<F>(
        entries: Vec<CronEntry>,
        tz_offset_minutes: i32,
        on_fire: F,
    ) -> tokio::task::JoinHandle<()>
    where
        F: Fn(&FiredCron) + Send + Sync + 'static,
    {
        let mut sched = Self::new(entries, tz_offset_minutes);
        tokio::spawn(async move {
            loop {
                let now = now_ms();
                let Some(next) = sched.next_fire_at(now) else {
                    return;
                };
                let wait = Duration::from_millis((next - now).max(0) as u64);
                tokio::time::sleep(wait).await;
                let fired = sched.tick(now, now_ms());
                for entry in &fired {
                    on_fire(entry);
                }
            }
        })
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_717_200_000_000;

    fn at(minutes: i64) -> i64 {
        T0 + minutes * 60_000
    }

    fn entry(id: &str, cron: &str, prompt: &str, recurring: bool) -> CronEntry {
        CronEntry {
            id: id.into(),
            cron: cron.into(),
            prompt: prompt.into(),
            recurring,
            session_id: None,
            created_at: None,
        }
    }

    fn fired_ids(fired: &[FiredCron]) -> Vec<&str> {
        fired.iter().map(|f| f.entry.id.as_str()).collect()
    }

    fn jf(id: &str, expr: &str, ideal_ms: i64, tz: i32) -> i64 {
        let parsed = crate::cron::parse(expr).unwrap();
        crate::cron::jitter::jittered_next_run_ms(id, &parsed, ideal_ms, tz, false)
    }

    fn jf_oneshot(id: &str, ideal_ms: i64, tz: i32) -> i64 {
        crate::cron::jitter::one_shot_jittered_run_ms(id, None, ideal_ms, tz, false)
    }

    #[test]
    fn next_fire_at_earliest_across_entries() {
        let sched = CronScheduler::new(
            vec![
                entry("a", "30 14 * * *", "afternoon", true),
                entry("b", "0 9 * * *", "morning", true),
            ],
            0,
        );
        assert_eq!(
            sched.next_fire_at(T0),
            Some(jf("b", "0 9 * * *", at(540), 0))
        );
        assert_eq!(
            sched.next_fire_at(at(540)),
            Some(jf("a", "30 14 * * *", at(870), 0))
        );
        assert_eq!(
            sched.next_fire_at(at(870)),
            Some(jf("b", "0 9 * * *", at(1980), 0))
        );

        assert_eq!(
            sched.list_entries(),
            vec![
                entry("a", "30 14 * * *", "afternoon", true),
                entry("b", "0 9 * * *", "morning", true),
            ]
        );

        assert_eq!(CronScheduler::new(vec![], 0).next_fire_at(T0), None);
    }

    #[test]
    fn next_fire_at_respects_tz_offset() {
        let entries = vec![
            entry("a", "30 14 * * *", "afternoon", true),
            entry("b", "0 9 * * *", "morning", true),
        ];

        let sched_east = CronScheduler::new(entries.clone(), 480);
        assert_eq!(
            sched_east.next_fire_at(T0),
            Some(jf("b", "0 9 * * *", at(60), 480))
        );

        let sched_west = CronScheduler::new(entries, -300);
        assert_eq!(
            sched_west.next_fire_at(T0),
            Some(jf("b", "0 9 * * *", at(840), -300))
        );
    }

    #[test]
    fn tick_fires_due_entries_in_fire_order() {
        let mut sched = CronScheduler::new(
            vec![
                entry("a", "10 * * * *", "ten", true),
                entry("b", "5 * * * *", "five", true),
                entry("c", "20 * * * *", "twenty", true),
            ],
            0,
        );
        let fired = sched.tick(T0, at(30));
        let mut expected = [
            (jf("b", "5 * * * *", at(5), 0), "b"),
            (jf("a", "10 * * * *", at(10), 0), "a"),
            (jf("c", "20 * * * *", at(20), 0), "c"),
        ];
        expected.sort();
        assert_eq!(
            fired_ids(&fired),
            expected.iter().map(|(_, id)| *id).collect::<Vec<_>>()
        );
        assert_eq!(sched.list_entries().len(), 3);
        assert_eq!(
            sched.next_fire_at(at(30)),
            Some(jf("b", "5 * * * *", at(65), 0))
        );
    }

    #[test]
    fn tick_removes_one_shot_after_firing() {
        let mut sched = CronScheduler::new(
            vec![
                entry("one", "5 * * * *", "once", false),
                entry("rec", "10 * * * *", "repeat", true),
            ],
            0,
        );
        let fired = sched.tick(T0, at(5));
        assert_eq!(fired_ids(&fired), vec!["one"]);
        assert_eq!(fired[0].coalesced_count, 1);
        assert!(!fired[0].stale);
        assert_eq!(
            sched.list_entries(),
            vec![entry("rec", "10 * * * *", "repeat", true)]
        );
        assert_eq!(
            sched.next_fire_at(at(5)),
            Some(jf("rec", "10 * * * *", at(10), 0))
        );

        let jittered = jf("rec", "10 * * * *", at(10), 0);
        assert!(jittered >= at(10));
        assert_eq!(
            fired_ids(&sched.tick(at(5), jittered - 1)),
            Vec::<&str>::new()
        );
        let fired = sched.tick(at(5), jittered);
        assert_eq!(fired_ids(&fired), vec!["rec"]);
        assert_eq!(fired[0].coalesced_count, 1);
        assert_eq!(
            sched.list_entries(),
            vec![entry("rec", "10 * * * *", "repeat", true)]
        );
        assert_eq!(
            sched.next_fire_at(at(10)),
            Some(jf("rec", "10 * * * *", at(70), 0))
        );

        let mut solo = CronScheduler::new(vec![entry("lone", "5 * * * *", "solo", false)], 0);
        let fired_solo = solo.tick(T0, at(5));
        assert_eq!(fired_ids(&fired_solo), vec!["lone"]);
        assert!(solo.list_entries().is_empty());
        assert_eq!(solo.next_fire_at(at(5)), None);
    }

    #[test]
    fn tick_recurring_keeps_firing() {
        let expected_task = entry("r", "*/5 * * * *", "tick", true);
        let mut sched = CronScheduler::new(vec![expected_task.clone()], 0);
        for minute in [5, 10, 15] {
            let fired = sched.tick(at(minute - 5), at(minute + 1));
            assert_eq!(
                fired_ids(&fired),
                vec!["r"],
                "fire at +{minute} min must return the entry"
            );
            assert_eq!(fired[0].coalesced_count, 1);
            assert!(!fired[0].stale);
            assert_eq!(sched.list_entries(), vec![expected_task.clone()]);
        }
        assert_eq!(
            sched.next_fire_at(at(15)),
            Some(jf("r", "*/5 * * * *", at(20), 0))
        );
    }

    #[test]
    fn tick_skips_entries_not_yet_due() {
        let expected_task = entry("d", "30 14 * * *", "daily", true);
        let mut sched = CronScheduler::new(vec![expected_task.clone()], 0);

        assert_eq!(fired_ids(&sched.tick(T0, at(10))), Vec::<&str>::new());
        assert_eq!(sched.list_entries(), vec![expected_task.clone()]);
        let jittered = jf("d", "30 14 * * *", at(870), 0);
        assert!(jittered >= at(870));
        assert_eq!(sched.next_fire_at(at(10)), Some(jittered));

        assert_eq!(fired_ids(&sched.tick(at(10), at(870))), Vec::<&str>::new());
        let fired = sched.tick(at(10), jittered);
        assert_eq!(fired_ids(&fired), vec!["d"]);
        assert_eq!(fired[0].coalesced_count, 1);
        assert_eq!(sched.list_entries(), vec![expected_task]);
        assert_eq!(
            sched.next_fire_at(at(870)),
            Some(jf("d", "30 14 * * *", at(870 + 1440), 0))
        );
    }

    #[test]
    fn tick_late_wake_coalesces_multiple_fires_to_single_fire() {
        let task = entry("r", "*/5 * * * *", "tick", true);
        let mut sched = CronScheduler::new(vec![task.clone()], 0);

        let fired = sched.tick(T0, at(60));
        assert_eq!(
            fired_ids(&fired),
            vec!["r"],
            "must coalesce to a single fire, never burst"
        );
        assert_eq!(fired[0].coalesced_count, 11);
        assert!(!fired[0].stale);
        assert_eq!(
            sched.next_fire_at(at(60)),
            Some(jf("r", "*/5 * * * *", at(65), 0))
        );
        assert_eq!(sched.list_entries(), vec![task]);
    }

    #[test]
    fn tick_interval_boundary_conditions() {
        let task = entry("t", "5 * * * *", "test", true);
        let mut sched = CronScheduler::new(vec![task.clone()], 0);
        let jittered = jf("t", "5 * * * *", at(5), 0);
        assert!(jittered >= at(5));

        assert_eq!(fired_ids(&sched.tick(T0, jittered - 1)), Vec::<&str>::new());
        assert_eq!(sched.next_fire_at(T0), Some(jittered));

        let fired = sched.tick(T0, jittered);
        assert_eq!(fired_ids(&fired), vec!["t"]);

        assert_eq!(fired_ids(&sched.tick(at(5), at(5))), Vec::<&str>::new());

        assert_eq!(fired_ids(&sched.tick(at(10), at(5))), Vec::<&str>::new());
    }

    #[test]
    fn tick_multiple_entries_same_fire_time() {
        let entry1 = entry("e1", "0 9 * * *", "first", true);
        let entry2 = entry("e2", "0 9 * * *", "second", false);
        let mut sched = CronScheduler::new(vec![entry1.clone(), entry2.clone()], 0);

        let fired = sched.tick(T0, at(540 + 16));
        assert_eq!(fired_ids(&fired), vec!["e2", "e1"]);
        assert_eq!(sched.list_entries(), vec![entry1]);
    }

    #[test]
    fn tick_if_idle_defers_a_running_loop_and_fires_next_round() {
        let one_shot = entry("one", "5 * * * *", "once", false);
        let mut sched = CronScheduler::new(vec![one_shot.clone()], 0);

        let busy = |_: &CronEntry| true;
        assert_eq!(
            sched.tick_if_idle(T0, at(5), busy),
            None,
            "a busy loop must skip the round outright"
        );
        assert_eq!(
            sched.list_entries(),
            vec![one_shot.clone()],
            "a skipped round must not consume the one-shot"
        );

        let idle = |_: &CronEntry| false;
        let fired = sched
            .tick_if_idle(T0, at(5), idle)
            .expect("an idle loop runs the round");
        assert_eq!(fired_ids(&fired), vec!["one"]);
        assert_eq!(fired[0].coalesced_count, 1);
        assert!(!fired[0].stale);
        assert!(
            sched.list_entries().is_empty(),
            "the one-shot leaves only once it actually fires"
        );
    }

    #[test]
    fn tick_if_idle_defers_the_whole_round_not_just_the_busy_entry() {
        let mut busy_entry = entry("busy", "5 * * * *", "running session", true);
        busy_entry.session_id = Some("sess-a".into());
        let mut idle_entry = entry("idle", "5 * * * *", "idle session", true);
        idle_entry.session_id = Some("sess-b".into());
        let mut sched = CronScheduler::new(vec![busy_entry, idle_entry], 0);
        let now = at(30);

        let only_a_busy = |e: &CronEntry| e.session_id.as_deref() == Some("sess-a");
        assert_eq!(
            sched.tick_if_idle(T0, now, only_a_busy),
            None,
            "one busy session defers the round"
        );
        assert_eq!(
            sched.list_entries().len(),
            2,
            "no entry may be consumed by a skipped round"
        );

        let none_busy = |_: &CronEntry| false;
        let fired = sched
            .tick_if_idle(T0, now, none_busy)
            .expect("idle round runs");
        let mut ids = fired_ids(&fired);
        ids.sort();
        assert_eq!(ids, vec!["busy", "idle"]);
    }

    #[test]
    fn a_rebuilt_scheduler_does_not_replay_the_pre_restart_window() {
        let task = entry("r", "*/5 * * * *", "tick", true);

        let mut first = CronScheduler::new(vec![task.clone()], 0);
        let fired = first.tick(T0, at(10));
        assert_eq!(fired_ids(&fired), vec!["r"]);

        let mut second = CronScheduler::new(vec![task], 0);
        assert_eq!(
            fired_ids(&second.tick(at(10), at(10))),
            Vec::<&str>::new(),
            "restart must not replay the pre-restart window"
        );
        assert_eq!(second.list_entries().len(), 1);
    }

    #[test]
    fn tick_stale_recurring_task_fires_once_then_leaves() {
        let old = T0 - 8 * 24 * 60 * 60_000;
        let mut stale_entry = entry("old", "*/5 * * * *", "stale", true);
        stale_entry.created_at = Some(old);
        let mut sched = CronScheduler::new(vec![stale_entry], 0);
        let fired = sched.tick(T0, at(60));
        assert_eq!(fired_ids(&fired), vec!["old"]);
        assert!(fired[0].stale, "an 8-day-old task must be flagged stale");
        assert!(fired[0].coalesced_count >= 1);
        assert!(
            sched.list_entries().is_empty(),
            "a stale task leaves after its last fire"
        );

        let mut fresh_entry = entry("fresh", "*/5 * * * *", "fresh", true);
        fresh_entry.created_at = Some(T0 - 6 * 24 * 60 * 60_000);
        let mut sched = CronScheduler::new(vec![fresh_entry], 0);
        let fired = sched.tick(T0, at(60));
        assert_eq!(fired_ids(&fired), vec!["fresh"]);
        assert!(!fired[0].stale);
        assert_eq!(sched.list_entries().len(), 1);
    }

    #[test]
    fn invalid_or_never_firing_entries_are_skipped() {
        let sched = CronScheduler::new(vec![entry("bad", "not a cron", "x", true)], 0);
        assert!(
            sched.list_entries().is_empty(),
            "unparseable entries must be dropped at construction"
        );
        assert_eq!(sched.next_fire_at(T0), None);

        let never_entry = entry("never", "0 0 30 2 *", "x", true);
        let mut sched = CronScheduler::new(vec![never_entry.clone()], 0);
        assert_eq!(
            sched.list_entries(),
            vec![never_entry],
            "syntactically valid never-firing entries must be retained"
        );
        assert_eq!(sched.next_fire_at(T0), None);
        assert_eq!(sched.tick(T0, at(100_000)), vec![]);
        assert_eq!(sched.list_entries().len(), 1);
    }

    #[tokio::test]
    async fn start_ends_when_nothing_is_schedulable() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let fired = Arc::new(AtomicBool::new(false));
        let fired_clone = fired.clone();
        let handle = CronScheduler::start(vec![], 0, move |_| {
            fired_clone.store(true, Ordering::SeqCst);
        });
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("task should end promptly")
            .expect("task should not panic");
        assert!(
            !fired.load(Ordering::SeqCst),
            "callback must not be called when empty"
        );

        let fired = Arc::new(AtomicBool::new(false));
        let fired_clone = fired.clone();
        let handle = CronScheduler::start(
            vec![entry("never", "0 0 30 2 *", "x", true)],
            0,
            move |_| {
                fired_clone.store(true, Ordering::SeqCst);
            },
        );
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("task should end promptly")
            .expect("task should not panic");
        assert!(
            !fired.load(Ordering::SeqCst),
            "callback must not be called for never-firing"
        );

        let fired = Arc::new(AtomicBool::new(false));
        let fired_clone = fired.clone();
        let handle =
            CronScheduler::start(vec![entry("bad", "not a cron", "x", true)], 0, move |_| {
                fired_clone.store(true, Ordering::SeqCst);
            });
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("task should end promptly")
            .expect("task should not panic");
        assert!(
            !fired.load(Ordering::SeqCst),
            "callback must not be called for unparseable"
        );
    }

    #[test]
    fn test_dynamic_add_remove_and_list_entries() {
        let entry_a = entry("a", "0 9 * * *", "job a", true);
        let mut sched = CronScheduler::new(vec![entry_a.clone()], 0);
        assert_eq!(sched.list_entries(), vec![entry_a.clone()]);
        assert_eq!(
            sched.next_fire_at(T0),
            Some(jf("a", "0 9 * * *", at(540), 0))
        );

        let entry_b = entry("b", "30 14 * * *", "job b", true);
        assert!(sched.add_entry(entry_b.clone()));
        assert_eq!(sched.list_entries(), vec![entry_a.clone(), entry_b.clone()]);
        assert_eq!(
            sched.next_fire_at(T0),
            Some(jf("a", "0 9 * * *", at(540), 0))
        );

        assert!(!sched.add_entry(entry("c", "invalid cron", "job c", true)));
        assert_eq!(sched.list_entries(), vec![entry_a.clone(), entry_b.clone()]);

        assert!(!sched.add_entry(entry("a", "bad cron", "job a corrupt", true)));
        assert_eq!(sched.list_entries(), vec![entry_a, entry_b.clone()]);

        let entry_a_updated = entry("a", "0 10 * * *", "job a updated", false);
        assert!(sched.add_entry(entry_a_updated.clone()));
        assert_eq!(sched.list_entries(), vec![entry_a_updated, entry_b.clone()]);
        assert_eq!(sched.next_fire_at(T0), Some(jf_oneshot("a", at(600), 0)));

        assert!(!sched.remove_entry("nonexistent"));
        assert_eq!(sched.list_entries().len(), 2);

        assert!(sched.remove_entry("a"));
        assert_eq!(sched.list_entries(), vec![entry_b.clone()]);
        assert_eq!(
            sched.next_fire_at(T0),
            Some(jf("b", "30 14 * * *", at(870), 0))
        );

        assert!(!sched.remove_entry("a"));

        assert!(sched.remove_entry("b"));
        assert!(sched.list_entries().is_empty());
        assert_eq!(sched.next_fire_at(T0), None);
    }

    #[test]
    fn cron_entry_serde() {
        let e = entry("task-1", "0 9 * * *", "hello world", true);
        let json = serde_json::to_string(&e).unwrap();
        let deserialized: CronEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, e);

        let wire_json = r#"{
            "id": "task-2",
            "cron": "*/5 * * * *",
            "prompt": "run check",
            "createdAt": 1717200000000,
            "nextFireAt": "2024-06-01T00:05:00.000Z",
            "stale": false,
            "humanSchedule": "every 5 minutes"
        }"#;
        let from_wire: CronEntry = serde_json::from_str(wire_json).unwrap();
        assert_eq!(
            from_wire,
            CronEntry {
                id: "task-2".into(),
                cron: "*/5 * * * *".into(),
                prompt: "run check".into(),
                recurring: true,
                session_id: None,
                created_at: Some(1717200000000),
            }
        );

        let one_shot_json =
            r#"{"id":"task-3","cron":"0 0 1 1 *","prompt":"yearly","recurring":false}"#;
        let one_shot: CronEntry = serde_json::from_str(one_shot_json).unwrap();
        assert!(!one_shot.recurring);
        assert_eq!(one_shot.id, "task-3");
        assert_eq!(one_shot.cron, "0 0 1 1 *");
        assert_eq!(one_shot.prompt, "yearly");
    }

    #[test]
    fn next_fire_for_entry_answers_per_entry() {
        let tz = 0;
        let daily = entry("daily", "0 9 * * *", "hi", true);
        let fire = next_fire_for_entry(&daily, T0, tz).expect("a daily entry always fires");
        assert!(fire > T0, "the next fire is in the future");
        assert!(fire < T0 + 24 * 60 * 60 * 1000, "and within a day");

        let one_shot = entry("once", "0 9 * * *", "hi", false);
        assert!(next_fire_for_entry(&one_shot, T0, tz).expect("fires once") > T0);

        let broken = entry("broken", "not a cron", "hi", true);
        assert_eq!(next_fire_for_entry(&broken, T0, tz), None);

        let sched = CronScheduler::new(vec![daily.clone()], tz);
        assert_eq!(sched.next_fire_at(T0), next_fire_for_entry(&daily, T0, tz));
    }
}
