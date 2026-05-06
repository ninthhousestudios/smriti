use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsEventKind {
    Create,
    Modify,
    Delete,
    CloseWrite,
    MovedFrom { cookie: u32 },
    MovedTo { cookie: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlushedKind {
    Create,
    Modify,
    Delete,
    Moved { from: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlushedEvent {
    pub path: PathBuf,
    pub kind: FlushedKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BufferedKind {
    Create,
    Modify,
    Delete,
    Moved { from: PathBuf },
}

#[derive(Debug)]
struct BufferedEntry {
    kind: BufferedKind,
    first_seen: Instant,
    last_seen: Instant,
    close_write_seen: bool,
}

#[derive(Debug)]
struct PendingMove {
    from_path: PathBuf,
    received_at: Instant,
}

pub struct DebounceBuffer {
    entries: HashMap<PathBuf, BufferedEntry>,
    pending_moves: HashMap<u32, PendingMove>,
    idle_window: Duration,
    max_wait: Duration,
    close_write_idle: Duration,
    move_cookie_ttl: Duration,
}

impl DebounceBuffer {
    pub fn new(
        idle_window: Duration,
        max_wait: Duration,
        close_write_idle: Duration,
        move_cookie_ttl: Duration,
    ) -> Self {
        Self {
            entries: HashMap::new(),
            pending_moves: HashMap::new(),
            idle_window,
            max_wait,
            close_write_idle,
            move_cookie_ttl,
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(
            Duration::from_secs(1),
            Duration::from_secs(5),
            Duration::from_millis(100),
            Duration::from_secs(1),
        )
    }

    pub fn insert(&mut self, path: PathBuf, kind: FsEventKind, now: Instant) {
        match kind {
            FsEventKind::MovedFrom { cookie } => {
                self.entries.remove(&path);
                self.pending_moves.insert(
                    cookie,
                    PendingMove {
                        from_path: path,
                        received_at: now,
                    },
                );
            }
            FsEventKind::MovedTo { cookie } => {
                if let Some(pending) = self.pending_moves.remove(&cookie) {
                    self.entries.insert(
                        path,
                        BufferedEntry {
                            kind: BufferedKind::Moved {
                                from: pending.from_path,
                            },
                            first_seen: now,
                            last_seen: now,
                            close_write_seen: false,
                        },
                    );
                } else {
                    self.upsert(path, BufferedKind::Create, now);
                }
            }
            FsEventKind::CloseWrite => {
                if let Some(entry) = self.entries.get_mut(&path) {
                    entry.last_seen = now;
                    entry.close_write_seen = true;
                } else {
                    let entry = BufferedEntry {
                        kind: BufferedKind::Modify,
                        first_seen: now,
                        last_seen: now,
                        close_write_seen: true,
                    };
                    self.entries.insert(path, entry);
                }
            }
            FsEventKind::Create => self.upsert(path, BufferedKind::Create, now),
            FsEventKind::Modify => self.upsert(path, BufferedKind::Modify, now),
            FsEventKind::Delete => self.upsert(path, BufferedKind::Delete, now),
        }
    }

    fn upsert(&mut self, path: PathBuf, kind: BufferedKind, now: Instant) {
        if let Some(entry) = self.entries.get_mut(&path) {
            entry.kind = kind;
            entry.last_seen = now;
        } else {
            self.entries.insert(
                path,
                BufferedEntry {
                    kind,
                    first_seen: now,
                    last_seen: now,
                    close_write_seen: false,
                },
            );
        }
    }

    pub fn flush(&mut self, now: Instant) -> Vec<FlushedEvent> {
        let mut flushed = Vec::new();

        let expired_cookies: Vec<u32> = self
            .pending_moves
            .iter()
            .filter(|(_, pm)| now.duration_since(pm.received_at) >= self.move_cookie_ttl)
            .map(|(&cookie, _)| cookie)
            .collect();

        for cookie in expired_cookies {
            if let Some(pm) = self.pending_moves.remove(&cookie) {
                flushed.push(FlushedEvent {
                    path: pm.from_path,
                    kind: FlushedKind::Delete,
                });
            }
        }

        let ready_paths: Vec<PathBuf> = self
            .entries
            .iter()
            .filter(|(_, entry)| {
                let idle = if entry.close_write_seen {
                    self.close_write_idle
                } else {
                    self.idle_window
                };
                now.duration_since(entry.last_seen) >= idle
                    || now.duration_since(entry.first_seen) >= self.max_wait
            })
            .map(|(path, _)| path.clone())
            .collect();

        for path in ready_paths {
            if let Some(entry) = self.entries.remove(&path) {
                let kind = match entry.kind {
                    BufferedKind::Create => FlushedKind::Create,
                    BufferedKind::Modify => FlushedKind::Modify,
                    BufferedKind::Delete => FlushedKind::Delete,
                    BufferedKind::Moved { from } => FlushedKind::Moved { from },
                };
                flushed.push(FlushedEvent { path, kind });
            }
        }

        flushed
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        let entry_deadline = self.entries.values().map(|entry| {
            let idle = if entry.close_write_seen {
                self.close_write_idle
            } else {
                self.idle_window
            };
            let idle_at = entry.last_seen + idle;
            let max_at = entry.first_seen + self.max_wait;
            idle_at.min(max_at)
        });

        let move_deadline = self
            .pending_moves
            .values()
            .map(|pm| pm.received_at + self.move_cookie_ttl);

        entry_deadline.chain(move_deadline).min()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.pending_moves.is_empty()
    }

    pub fn pending_count(&self) -> usize {
        self.entries.len() + self.pending_moves.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    fn path(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn idle_window_triggers_flush() {
        let mut buf = DebounceBuffer::new(ms(1000), ms(5000), ms(100), ms(1000));
        let t0 = Instant::now();

        buf.insert(path("/a.txt"), FsEventKind::Create, t0);

        assert!(buf.flush(t0 + ms(500)).is_empty());

        let flushed = buf.flush(t0 + ms(1000));
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].path, path("/a.txt"));
        assert_eq!(flushed[0].kind, FlushedKind::Create);
    }

    #[test]
    fn max_wait_forces_flush() {
        let mut buf = DebounceBuffer::new(ms(1000), ms(5000), ms(100), ms(1000));
        let t0 = Instant::now();

        buf.insert(path("/a.txt"), FsEventKind::Modify, t0);

        // Keep poking it just before idle expires
        for i in 1..=6 {
            buf.insert(path("/a.txt"), FsEventKind::Modify, t0 + ms(i * 900));
        }

        // idle not expired (last_seen = t0+5400, now = t0+5500, diff = 100ms < 1000ms)
        // but max_wait expired (first_seen = t0, now = t0+5500, diff = 5500ms >= 5000ms)
        let flushed = buf.flush(t0 + ms(5500));
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].kind, FlushedKind::Modify);
    }

    #[test]
    fn close_write_shortens_idle() {
        let mut buf = DebounceBuffer::new(ms(1000), ms(5000), ms(100), ms(1000));
        let t0 = Instant::now();

        buf.insert(path("/a.txt"), FsEventKind::Modify, t0);
        buf.insert(path("/a.txt"), FsEventKind::CloseWrite, t0 + ms(10));

        // At t0+50, normal idle (1000ms) not expired, but we shouldn't flush yet
        assert!(buf.flush(t0 + ms(50)).is_empty());

        // At t0+110, close_write idle (100ms from last_seen=t0+10) has expired
        let flushed = buf.flush(t0 + ms(110));
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].kind, FlushedKind::Modify);
    }

    #[test]
    fn matched_move_pair() {
        let mut buf = DebounceBuffer::new(ms(1000), ms(5000), ms(100), ms(1000));
        let t0 = Instant::now();

        buf.insert(path("/old.txt"), FsEventKind::MovedFrom { cookie: 42 }, t0);
        buf.insert(
            path("/new.txt"),
            FsEventKind::MovedTo { cookie: 42 },
            t0 + ms(1),
        );

        let flushed = buf.flush(t0 + ms(1001));
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].path, path("/new.txt"));
        assert_eq!(
            flushed[0].kind,
            FlushedKind::Moved {
                from: path("/old.txt")
            }
        );
    }

    #[test]
    fn orphan_moved_from_becomes_delete() {
        let mut buf = DebounceBuffer::new(ms(1000), ms(5000), ms(100), ms(1000));
        let t0 = Instant::now();

        buf.insert(path("/gone.txt"), FsEventKind::MovedFrom { cookie: 99 }, t0);

        // Before TTL: nothing flushed
        assert!(buf.flush(t0 + ms(500)).is_empty());

        // After TTL: orphan becomes Delete
        let flushed = buf.flush(t0 + ms(1000));
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].path, path("/gone.txt"));
        assert_eq!(flushed[0].kind, FlushedKind::Delete);
    }

    #[test]
    fn latest_event_wins() {
        let mut buf = DebounceBuffer::new(ms(1000), ms(5000), ms(100), ms(1000));
        let t0 = Instant::now();

        buf.insert(path("/a.txt"), FsEventKind::Create, t0);
        buf.insert(path("/a.txt"), FsEventKind::Modify, t0 + ms(100));
        buf.insert(path("/a.txt"), FsEventKind::Delete, t0 + ms(200));

        let flushed = buf.flush(t0 + ms(1200));
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].kind, FlushedKind::Delete);
    }

    #[test]
    fn moved_to_without_from_is_create() {
        let mut buf = DebounceBuffer::new(ms(1000), ms(5000), ms(100), ms(1000));
        let t0 = Instant::now();

        buf.insert(path("/appeared.txt"), FsEventKind::MovedTo { cookie: 77 }, t0);

        let flushed = buf.flush(t0 + ms(1000));
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].kind, FlushedKind::Create);
    }

    #[test]
    fn next_deadline_reflects_earliest() {
        let mut buf = DebounceBuffer::new(ms(1000), ms(5000), ms(100), ms(1000));
        let t0 = Instant::now();

        buf.insert(path("/a.txt"), FsEventKind::Create, t0);
        buf.insert(path("/b.txt"), FsEventKind::Create, t0 + ms(500));

        let deadline = buf.next_deadline().unwrap();
        assert_eq!(deadline, t0 + ms(1000));
    }

    #[test]
    fn close_write_deadline_earlier_than_normal() {
        let mut buf = DebounceBuffer::new(ms(1000), ms(5000), ms(100), ms(1000));
        let t0 = Instant::now();

        buf.insert(path("/a.txt"), FsEventKind::Create, t0);
        buf.insert(path("/b.txt"), FsEventKind::CloseWrite, t0 + ms(50));

        let deadline = buf.next_deadline().unwrap();
        // /b.txt close_write idle: t0+50+100 = t0+150
        // /a.txt normal idle: t0+1000
        assert_eq!(deadline, t0 + ms(150));
    }

    #[test]
    fn empty_buffer() {
        let buf = DebounceBuffer::with_defaults();
        assert!(buf.is_empty());
        assert!(buf.next_deadline().is_none());
    }

    #[test]
    fn moved_from_removes_existing_entry() {
        let mut buf = DebounceBuffer::new(ms(1000), ms(5000), ms(100), ms(1000));
        let t0 = Instant::now();

        buf.insert(path("/a.txt"), FsEventKind::Create, t0);
        buf.insert(path("/a.txt"), FsEventKind::MovedFrom { cookie: 10 }, t0 + ms(50));
        buf.insert(path("/b.txt"), FsEventKind::MovedTo { cookie: 10 }, t0 + ms(51));

        let flushed = buf.flush(t0 + ms(1100));
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].path, path("/b.txt"));
        assert_eq!(
            flushed[0].kind,
            FlushedKind::Moved {
                from: path("/a.txt")
            }
        );
    }
}
