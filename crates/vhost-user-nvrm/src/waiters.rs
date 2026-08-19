// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! The semaphore-surface waiter poller: one thread, `poll(2)`, no epoll.
//!
//! A semaphore surface is RM's `NV_SEMAPHORE_SURFACE` object -- RM being
//! NVIDIA's Resource Manager, the kernel driver behind /dev/nvidiactl and
//! /dev/nvidiaN. Its waiters are the fences of nvidia-drm, and the
//! callback a fired waiter releases belongs to NVKMS, NVIDIA's modesetting
//! kernel module. An OS event is the fd RM signals through
//! (`NV_ESC_ALLOC_OS_EVENT`).
//!
//! WHY A THREAD, when the device already owns an epoll set. A substituted
//! waiter (session.rs, the semsurf section) is an OS event that RM fires
//! WITHOUT event data (`sem_surf.c:1591`, hard-coded): the only trace of the
//! firing is `nvlfp->dataless_event_pending`, and `nvidia_poll` CLEARS that
//! flag on every poll of the fd (nv.c:2316-2321) -- whoever polls, eats it.
//! `epoll_ctl(ADD)` polls the fd to seed its ready list (`ep_insert` ->
//! `ep_item_poll`) and `epoll_wait` polls it AGAIN to confirm -- so a firing
//! that lands before the ADD is consumed by the ADD and reported by nobody.
//! That is not a rare race to shrug at: the fence for freshly submitted GPU
//! work routinely signals within the round-trip of the registration itself.
//!
//! `poll(2)` has the property epoll lacks: the flag stays set until someone
//! polls, and the poll that consumes it is the same call that REPORTS it.
//! Insertion order stops mattering -- a firing that predates the first
//! `poll` is simply returned by that first poll. So the waiter fds live in
//! a plain `poll` list on this thread, exactly the way a native user-space
//! consumer of `NV_ESC_ALLOC_OS_EVENT` holds them.
//!
//! The thread touches no backend state: fires go into a mutex'd list, and a
//! nonblocking eventfd -- whose readability epoll CAN be trusted with, its
//! counter is not self-clearing -- wakes the device worker to drain it.
//!
//! One entry is one armed waiter, and it is ONE-SHOT: RM removes the
//! listener when it notifies (`sem_surf.c`, `_semsurfNotifyCompleted`), so
//! the first readable poll retires the entry. A pooled-and-reused slot is
//! re-armed by a fresh [`WaiterPoller::watch`]. Stale entries for fds a
//! session closed answer POLLNVAL and are dropped; the session keeps every
//! slot fd open until this poller confirmed the removal
//! (`Session::take_unwatch` -> [`WaiterPoller::unwatch`]), so a number here
//! is never somebody else's file.

use std::os::fd::{AsRawFd, RawFd};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vmm_sys_util::eventfd::EventFd;

/// The host-side frame rate limiter: how often this poller may consume
/// waiter firings, in Hz. `None` = off, which is the default.
///
/// WHY HERE and nowhere else. NVIDIA's own vGPU does the same thing under
/// the name FRL: it caps RENDERING, separately from vsync, because the EVO
/// path that would carry a real vblank (the per-frame vertical-blank
/// tick) needs kernel-privileged RM controls a
/// user-space backend cannot have (OPEN-QUESTIONS 25). The measured hook is
/// the semaphore-surface fence -- `DRM_NVIDIA_SEMSURF_FENCE_CREATE` is
/// exactly 1.00 per frame -- and this thread is the one place in the stack
/// that can delay it safely:
///
///   - it holds NO lock while it waits (the state mutex is taken only for
///     the snapshot and the drain), so a delay here paces one thread and
///     stalls nobody;
///   - it delays without HOLDING anything. `dataless_event_pending` stays
///     set until somebody polls the fd (nv.c:2316-2319), so a firing we do
///     not consume simply stays in the kernel. Nothing is buffered here,
///     nothing can be dropped, and there is no held completion to flush
///     before the guest tears a fence context down -- which is the hazard
///     that rules out every other candidate point.
///
/// The alternative -- delaying in the device's `on_poll` -- would run
/// under the backend's `RwLock` write guard with BOTH virtqueues on one
/// worker. That is not a brake, it is a stall of every ioctl of every guest
/// process; the `poll` field of `NvrmDevice` and `nvrm::serve` spell that
/// lock out.
///
/// What it does to the CLIENT's rate is a measurement, not a
/// derivation: nvidia-drm arms at most one waiter per fence context, so
/// pacing consumption to N Hz should pace each context to N completions a
/// second -- but the measured ~1.45 firings per frame says a frame does not
/// always cost exactly one. Sweep it against a swap counter before believing
/// a number.
fn frl_interval() -> Option<Duration> {
    frl_interval_from(std::env::var("LEA_FRL_HZ").ok().as_deref())
}

/// The parsing half of [`frl_interval`], split out so it can be tested.
///
/// The environment must NOT be touched from a test: `std::env::set_var` is
/// process-global and the test threads run in parallel, so a test that set
/// `LEA_FRL_HZ` would decide what an unrelated test in another module
/// reads. `None` here means "the variable is not set", which is the same
/// thing `var(..).ok()` says.
fn frl_interval_from(s: Option<&str>) -> Option<Duration> {
    let hz: f64 = s?.parse().ok()?;
    // Finite and positive, or off: NaN and infinity are not rates (an
    // infinite rate would be a zero period, i.e. no brake at all -- say
    // "off" rather than run the pacing code for nothing).
    if !hz.is_finite() || hz <= 0.0 {
        return None;
    }
    // `try_from`, not `from`: a rate like 1e-300 gives a period that no
    // Duration can hold, and `Duration::from_secs_f64` PANICS on it -- on
    // this poller's own thread, after which every semaphore-surface fence
    // in the guest waits forever. A rate that cannot be paced is "off".
    Duration::try_from_secs_f64(1.0 / hz).ok()
}

/// One armed waiter under watch, and (in `fired`) one retired firing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Watch {
    pub guest_proc: u32,
    pub id: u32,
    pub fd: RawFd,
}

struct State {
    watch: Vec<Watch>,
    fired: Vec<Watch>,
    shutdown: bool,
}

struct Shared {
    state: Mutex<State>,
    /// Wakes the poll thread out of `poll(2)` on any list change.
    kick: EventFd,
    /// Wakes the DEVICE worker (this one sits in the device's epoll set).
    notify: EventFd,
}

pub struct WaiterPoller {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl WaiterPoller {
    pub fn new() -> anyhow::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State { watch: Vec::new(), fired: Vec::new(), shutdown: false }),
            kick: EventFd::new(libc::EFD_NONBLOCK)
                .map_err(|e| anyhow::anyhow!("waiter kick eventfd: {e}"))?,
            notify: EventFd::new(libc::EFD_NONBLOCK)
                .map_err(|e| anyhow::anyhow!("waiter notify eventfd: {e}"))?,
        });
        let t = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("nvrm-waiters".into())
                .spawn(move || run(&shared))
                .map_err(|e| anyhow::anyhow!("waiter poller thread: {e}"))?
        };
        Ok(Self { shared, thread: Some(t) })
    }

    /// The fd the device's epoll watches: readable = [`WaiterPoller::take_fired`] has rows.
    pub fn notify_fd(&self) -> RawFd {
        self.shared.notify.as_raw_fd()
    }

    /// Arm a waiter. An existing entry for the same fd is REPLACED: the fd
    /// is a pooled slot, and whatever the old entry described has been
    /// recycled by the session that owns it.
    pub fn watch(&self, w: Watch) {
        let mut st = self.shared.state.lock().unwrap();
        st.watch.retain(|e| e.fd != w.fd);
        st.watch.push(w);
        drop(st);
        let _ = self.shared.kick.write(1);
    }

    /// Forget fds the session is about to close (client freed, pool gone).
    pub fn unwatch(&self, fds: &[RawFd]) {
        if fds.is_empty() {
            return;
        }
        let mut st = self.shared.state.lock().unwrap();
        st.watch.retain(|e| !fds.contains(&e.fd));
        drop(st);
        let _ = self.shared.kick.write(1);
    }

    /// Forget everything a session reported, on its way out.
    pub fn unwatch_proc(&self, guest_proc: u32) {
        let mut st = self.shared.state.lock().unwrap();
        st.watch.retain(|e| e.guest_proc != guest_proc);
        st.fired.retain(|e| e.guest_proc != guest_proc);
        drop(st);
        let _ = self.shared.kick.write(1);
    }

    /// Drain the retired firings. Called when `notify_fd` reports readable.
    pub fn take_fired(&self) -> Vec<Watch> {
        let _ = self.shared.notify.read();
        std::mem::take(&mut self.shared.state.lock().unwrap().fired)
    }
}

impl Drop for WaiterPoller {
    fn drop(&mut self) {
        self.shared.state.lock().unwrap().shutdown = true;
        let _ = self.shared.kick.write(1);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(shared: &Shared) {
    let pace = frl_interval();
    if let Some(p) = pace {
        eprintln!(
            "vhost-user-nvrm: frame limiter on -- at most one waiter firing per guest \
             process every {} us",
            p.as_micros()
        );
    }
    // Per guest process: the earliest instant its NEXT firing may be
    // consumed. Thread-local; nothing else needs to see it.
    let mut next_at: std::collections::HashMap<u32, Instant> = Default::default();
    loop {
        // Snapshot under the lock, poll without it: `watch` may change while
        // this thread sleeps, and the kick eventfd is what turns that change
        // into a wake-up.
        let mut fds: Vec<libc::pollfd> =
            vec![libc::pollfd { fd: shared.kick.as_raw_fd(), events: libc::POLLIN, revents: 0 }];
        // THE BRAKE, and it has to sit exactly here.
        //
        // `poll(2)` CONSUMES `dataless_event_pending` for every fd it
        // reports (nv.c:2316-2321) -- that is the whole reason this poller
        // exists instead of an epoll set. So retiring fewer entries than the
        // poll reported would not DELAY those firings, it would LOSE them:
        // a fence nobody ever signals, and a NVKMS callback never freed.
        // The only safe way to hold a firing back is therefore to keep its
        // fd OUT of the poll list, where the kernel goes on holding the flag
        // for us. Nothing is buffered here and nothing can be dropped.
        let mut soonest: Option<Instant> = None;
        {
            let st = shared.state.lock().unwrap();
            if st.shutdown {
                return;
            }
            let now = Instant::now();
            for w in st.watch.iter() {
                if pace.is_some() {
                    if let Some(&t) = next_at.get(&w.guest_proc) {
                        if now < t {
                            soonest = Some(soonest.map_or(t, |s: Instant| s.min(t)));
                            continue;
                        }
                    }
                }
                fds.push(libc::pollfd {
                    fd: w.fd,
                    events: libc::POLLIN | libc::POLLPRI,
                    revents: 0,
                });
            }
            // Prune by AGE, never by "is this process still in `watch`".
            //
            // It was written the second way first and the limiter then
            // did not brake at all -- glxgears ran at 781 FPS against a 60 Hz
            // cap while firing 770 times a second. A waiter is ONE-SHOT: it
            // leaves `watch` the instant it fires, so between firing and
            // re-arming its process has no entry at all, and a prune keyed on
            // the watch list deletes exactly the deadline that was supposed
            // to hold the next frame back. An expired deadline restrains
            // nothing, so dropping those bounds the map just as well and
            // cannot erase a live one.
            next_at.retain(|_, &mut t| t > now);
        }
        // Round the wait UP: at 60 Hz the interval is 16.666 ms, and a
        // timeout truncated to 16 would let the rate drift above target.
        let timeout = match soonest {
            Some(t) => {
                let d = t.saturating_duration_since(Instant::now());
                (d.as_micros().div_ceil(1000).max(1)).min(i32::MAX as u128) as i32
            }
            None => -1,
        };
        // SAFETY: `fds` is a valid, owned array for the duration of the call.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            eprintln!("vhost-user-nvrm: waiter poll: {e} -- poller stops, waiter fences will hang");
            return;
        }
        if fds[0].revents != 0 {
            let _ = shared.kick.read();
        }
        let mut any = false;
        {
            let mut st = shared.state.lock().unwrap();
            let now = Instant::now();
            for pfd in &fds[1..] {
                if pfd.revents == 0 {
                    continue;
                }
                if pfd.revents & libc::POLLNVAL != 0 {
                    // Closed underneath us (session gone): the entry is
                    // stale, nothing fires.
                    st.watch.retain(|e| e.fd != pfd.fd);
                    continue;
                }
                // POLLIN/POLLPRI -- and POLLERR/POLLHUP too: a dying fd is
                // reported rather than silently unwatched, so the device can
                // at least drop the waiter's books. One-shot either way.
                //
                // Every reported entry is retired, without exception: see
                // the brake comment above for why picking and choosing here
                // would lose firings rather than delay them.
                if let Some(pos) = st.watch.iter().position(|e| e.fd == pfd.fd) {
                    let w = st.watch.swap_remove(pos);
                    if let Some(p) = pace {
                        next_at.insert(w.guest_proc, now + p);
                    }
                    st.fired.push(w);
                    any = true;
                }
            }
        }
        if any {
            let _ = shared.notify.write(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How long a test waits for something that MUST happen. Generous
    /// (the assertion is "eventually", not "fast"), but bounded, so a
    /// poller that never reports fails instead of hanging the suite.
    const EXPECT_MS: u64 = 2_000;
    /// How long a test waits to convince itself that nothing happens.
    const QUIET_MS: u64 = 300;

    /// Is `fd` readable within `ms`? Bounded `poll(2)`, retried across
    /// EINTR until the deadline -- never an untimed wait.
    fn readable_within(fd: RawFd, ms: u64) -> bool {
        let deadline = Instant::now() + Duration::from_millis(ms);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid, owned pollfd for the duration of the call.
            let n = unsafe {
                libc::poll(&mut p, 1, left.as_millis().min(i32::MAX as u128) as i32)
            };
            if n > 0 {
                return p.revents & libc::POLLIN != 0;
            }
            if n == 0 || left.is_zero() {
                return false;
            }
            // n < 0: EINTR. Round again, against the same deadline.
        }
    }

    /// A file-descriptor NUMBER whose file is closed, and which nothing
    /// else in this test binary will be handed while the test runs.
    ///
    /// The kernel always allocates the LOWEST free descriptor, so a number
    /// just under the process limit is one no `open` in a parallel test can
    /// land on. That matters here: the property under test is what the
    /// poller does with a number whose file is gone, and a number that got
    /// recycled into somebody else's file would test the opposite.
    fn closed_fd_number() -> RawFd {
        let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: fills in an owned, fully initialized struct.
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) }, 0, "getrlimit");
        let mut n = (lim.rlim_cur.min(1024) as RawFd) - 1;
        // SAFETY: F_GETFD only reads; an unused number answers -1/EBADF.
        while n > 3 && unsafe { libc::fcntl(n, libc::F_GETFD) } >= 0 {
            n -= 1;
        }
        assert!(n > 3, "no free high fd number to borrow");
        let ev = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        // SAFETY: `n` was just shown to be free, so dup2 closes nothing,
        // and `ev` stays open and owned by this frame.
        let got = unsafe { libc::dup2(ev.as_raw_fd(), n) };
        assert_eq!(got, n, "dup2 onto {n}: {}", std::io::Error::last_os_error());
        // SAFETY: `n` is the duplicate this function just made; `ev` keeps its own.
        assert_eq!(unsafe { libc::close(n) }, 0, "close({n})");
        n
    }

    /// A watched fd that becomes readable is reported exactly once, as the
    /// `Watch` that was armed for it.
    ///
    /// "Exactly once" is not a nicety: a substituted waiter is ONE-SHOT
    /// (RM drops the listener when it notifies), so a second report would
    /// name a listener that no longer exists, and the guest would complete
    /// a fence nobody signalled.
    #[test]
    fn a_readable_waiter_is_reported_once() {
        let p = WaiterPoller::new().unwrap();
        let ev = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        let w = Watch { guest_proc: 42, id: 7, fd: ev.as_raw_fd() };

        p.watch(w);
        ev.write(1).unwrap();

        assert!(readable_within(p.notify_fd(), EXPECT_MS), "notify_fd never became readable");
        assert_eq!(p.take_fired(), vec![w], "the armed Watch, unchanged");

        // The fd is STILL readable (nobody drained the eventfd), so a
        // poller that did not retire the entry would report it again.
        assert!(!readable_within(p.notify_fd(), QUIET_MS), "fired a second time");
        assert!(p.take_fired().is_empty());
    }

    /// A second `watch` for the same fd REPLACES the first, and only the
    /// newer `Watch` fires.
    ///
    /// The fd is a pooled semaphore slot: when the session re-arms it, the
    /// waiter the old entry described has already been recycled. Reporting
    /// the stale `id` would complete a fence that belongs to whatever now
    /// owns the slot.
    #[test]
    fn a_second_watch_on_the_same_fd_replaces_the_first() {
        let p = WaiterPoller::new().unwrap();
        let ev = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        let stale = Watch { guest_proc: 1, id: 10, fd: ev.as_raw_fd() };
        let fresh = Watch { guest_proc: 1, id: 11, fd: ev.as_raw_fd() };

        // Nothing can fire between these two: the fd is not readable yet.
        p.watch(stale);
        p.watch(fresh);
        ev.write(1).unwrap();

        assert!(readable_within(p.notify_fd(), EXPECT_MS), "notify_fd never became readable");
        assert_eq!(p.take_fired(), vec![fresh], "the newer Watch, and it alone");
    }

    /// `unwatch` forgets the entry, so a later firing of that fd reports
    /// nothing.
    ///
    /// The session closes those slot fds right after this call returns.
    /// An entry that survived would poll a NUMBER that the next `open` in
    /// this process hands to somebody else -- which is precisely the
    /// mistake the module header promises this ordering prevents.
    #[test]
    fn unwatch_drops_the_entry_before_it_can_fire() {
        let p = WaiterPoller::new().unwrap();
        let ev = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        let fd = ev.as_raw_fd();

        p.watch(Watch { guest_proc: 3, id: 1, fd });
        p.unwatch(&[fd]);
        ev.write(1).unwrap();

        assert!(!readable_within(p.notify_fd(), QUIET_MS), "an unwatched fd fired");
        assert!(p.take_fired().is_empty());
    }

    /// `unwatch_proc` forgets that guest process's entries and NOBODY
    /// else's -- a dying session must not disarm its neighbours' waiters.
    #[test]
    fn unwatch_proc_drops_only_that_processes_entries() {
        let p = WaiterPoller::new().unwrap();
        let gone = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        let live = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        let dead_w = Watch { guest_proc: 5, id: 1, fd: gone.as_raw_fd() };
        let live_w = Watch { guest_proc: 6, id: 2, fd: live.as_raw_fd() };

        p.watch(dead_w);
        p.watch(live_w);
        p.unwatch_proc(5);
        gone.write(1).unwrap();
        live.write(1).unwrap();

        assert!(readable_within(p.notify_fd(), EXPECT_MS), "notify_fd never became readable");
        assert_eq!(p.take_fired(), vec![live_w], "process 6 keeps its waiter");
        assert!(!readable_within(p.notify_fd(), QUIET_MS), "process 5's waiter fired anyway");
        assert!(p.take_fired().is_empty());
    }

    /// An fd that was closed underneath the poller answers POLLNVAL and is
    /// dropped SILENTLY -- no firing.
    ///
    /// A firing here would be a lie twice over: the waiter it names is
    /// gone with the session that owned the fd, and the number may already
    /// belong to another file. Note that POLLNVAL also makes `poll(2)`
    /// return at once, so an entry that were not dropped would additionally
    /// spin this thread at 100% CPU.
    #[test]
    fn an_fd_closed_underneath_the_poller_never_fires() {
        let p = WaiterPoller::new().unwrap();
        let live = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        let dead_w = Watch { guest_proc: 8, id: 1, fd: closed_fd_number() };
        let live_w = Watch { guest_proc: 8, id: 2, fd: live.as_raw_fd() };

        p.watch(dead_w);
        p.watch(live_w);
        live.write(1).unwrap();

        assert!(readable_within(p.notify_fd(), EXPECT_MS), "the live waiter never fired");
        assert_eq!(p.take_fired(), vec![live_w], "the closed fd must not appear here");
        assert!(!readable_within(p.notify_fd(), QUIET_MS));
        assert!(p.take_fired().is_empty());
    }

    /// Dropping the poller JOINS its thread: once `drop` returns, the
    /// thread is gone.
    ///
    /// The thread polls fds that belong to the sessions, and the daemon
    /// drops the device (and with it this poller) before it tears those
    /// down. A detached thread would go on polling numbers whose files are
    /// being closed and reopened underneath it.
    ///
    /// The Arc is the witness: the thread holds the only other strong
    /// reference for its whole life, so "no strong reference left" is the
    /// same statement as "the thread has run to its end". It is read on
    /// the very next instruction after `drop` returns -- a `drop` that only
    /// DETACHED the thread would still find it there, whereas a moment
    /// later the thread would have exited on its own and proved nothing.
    ///
    /// The drop runs on a helper thread with a bounded wait, so a poller
    /// that never notices the shutdown flag fails this test rather than
    /// hanging the whole suite on an untimed `join`.
    #[test]
    fn dropping_the_poller_joins_its_thread() {
        let p = WaiterPoller::new().unwrap();
        let weak = Arc::downgrade(&p.shared);
        assert_eq!(weak.strong_count(), 2, "the poller and its thread, one each");

        let (tx, rx) = std::sync::mpsc::channel();
        let after = weak.clone();
        std::thread::spawn(move || {
            drop(p);
            let _ = tx.send(after.strong_count());
        });
        let left = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("drop never returned -- the poll thread missed the shutdown flag");

        assert_eq!(left, 0, "the poll thread was still running when drop returned");
        assert!(weak.upgrade().is_none(), "the poll thread outlived the poller");
    }

    /// `LEA_FRL_HZ` parsing: a rate becomes a period, and everything that
    /// is not a positive rate turns the limiter OFF rather than failing.
    ///
    /// Off-by-default is the point. This variable is read once, on a
    /// thread that has no way to report anything, so an unparsable value
    /// must not panic (`1.0 / 0.0` would be an infinite `Duration`, which
    /// `Duration::from_secs_f64` rejects with a panic), and it must not
    /// silently enable a brake nobody asked for.
    ///
    /// Deliberately NOT through the environment: `set_var` is
    /// process-global and these tests run in parallel.
    #[test]
    fn frl_interval_turns_a_rate_into_a_period() {
        assert_eq!(frl_interval_from(None), None, "unset = off, the default");
        assert_eq!(frl_interval_from(Some("0")), None, "0 Hz is off, not 1/0");
        assert_eq!(frl_interval_from(Some("abc")), None, "unparsable is off");
        assert_eq!(frl_interval_from(Some("")), None, "empty is off");
        assert_eq!(frl_interval_from(Some("-60")), None, "a negative rate is off");
        // A rate whose period no Duration can hold is off rather than a
        // panic on the poller thread (Duration::from_secs_f64 panics on
        // overflow; that thread dying leaves every semsurf fence hanging).
        assert_eq!(frl_interval_from(Some("1e-300")), None, "an unrepresentable period is off");
        assert_eq!(frl_interval_from(Some("NaN")), None, "NaN is off");
        assert_eq!(frl_interval_from(Some("inf")), None, "infinity is off");

        let sixty = frl_interval_from(Some("60")).expect("60 Hz is a rate");
        assert_eq!(sixty, Duration::from_secs_f64(1.0 / 60.0));
        // 16.666.. ms -- the number the poller's `div_ceil` comment is about.
        assert_eq!(sixty.as_micros(), 16_666);
        assert_eq!(frl_interval_from(Some("30")).unwrap().as_micros(), 33_333);
        assert_eq!(frl_interval_from(Some("1000")).unwrap(), Duration::from_millis(1));
    }
}
