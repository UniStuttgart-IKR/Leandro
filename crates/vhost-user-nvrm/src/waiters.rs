// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! One-shot semaphore-surface waiter notifications.
//!
//! RM posts these events without event data (sem_surf.c:1591). Polling
//! consumes dataless_event_pending (nv.c:2316-2321). epoll can consume it
//! while adding the FD, then find nothing when delivering the wake.
//! A dedicated poll(2) thread consumes and reports the flag in one call.
//!
//! The poll thread owns registrations and each snapshot retains its FDs.
//! Cancellation acknowledges that the old poll is finished before slot reuse.
//! Completed arms carry a generation so stale delivery cannot retire a new arm.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use vmm_sys_util::eventfd::EventFd;

/// Optional LEA_FRL_HZ limit on waiter consumption per guest process.
///
/// Delay before poll: consuming a dataless event and withholding its
/// notification loses the firing. This thread holds no backend lock while
/// waiting. The resulting frame rate must be measured; firings and frames
/// are not necessarily one-to-one.
fn frl_interval() -> Option<Duration> {
    frl_interval_from(std::env::var("LEA_FRL_HZ").ok().as_deref())
}

/// Parse without changing the process environment, which tests share.
fn frl_interval_from(s: Option<&str>) -> Option<Duration> {
    let hz: f64 = s?.parse().ok()?;
    // Invalid or unrepresentable rates disable pacing.
    if !hz.is_finite() || hz <= 0.0 {
        return None;
    }
    let period = Duration::try_from_secs_f64(1.0 / hz).ok()?;
    if period.is_zero() {
        return None;
    }
    Instant::now().checked_add(period)?;
    Some(period)
}

/// One use of a pooled OS event. Generations never repeat within a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RegistrationId {
    pub event_id: u32,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fired {
    pub guest_proc: u32,
    pub registration: RegistrationId,
}

#[derive(Clone, Debug)]
pub struct Watch {
    pub fired: Fired,
    pub fd: Arc<OwnedFd>,
}

#[derive(Default)]
struct State {
    fired: Vec<Fired>,
    failure: Option<String>,
}

struct Shared {
    state: Mutex<State>,
    kick: EventFd,
    notify: EventFd,
}

enum Operation {
    Watch(Watch),
    CancelSlots { guest_proc: u32, ids: Vec<u32> },
    CancelProcess(u32),
    Shutdown,
}

struct Command {
    operation: Operation,
    ack: Option<mpsc::Sender<Result<(), String>>>,
}

pub struct WaiterPoller {
    shared: Arc<Shared>,
    commands: mpsc::Sender<Command>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl WaiterPoller {
    pub fn new() -> anyhow::Result<Self> {
        Self::start(poll, frl_interval())
    }

    fn start<P>(poll: P, pace: Option<Duration>) -> anyhow::Result<Self>
    where
        P: FnMut(&mut [libc::pollfd], i32) -> std::io::Result<()> + Send + 'static,
    {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            kick: EventFd::new(libc::EFD_NONBLOCK)?,
            notify: EventFd::new(libc::EFD_NONBLOCK)?,
        });
        let (commands, receiver) = mpsc::channel();
        let worker = shared.clone();
        let thread = std::thread::Builder::new()
            .name("nvrm-waiters".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(&worker, receiver, poll, pace)
                }));
                let error = match result {
                    Ok(Ok(())) => return,
                    Ok(Err(e)) => e.to_string(),
                    Err(_) => "waiter thread panicked".to_string(),
                };
                worker.state.lock().unwrap().failure = Some(error);
                let _ = wake(&worker.notify);
            })?;
        Ok(Self {
            shared,
            commands,
            thread: Some(thread),
        })
    }

    pub fn notify_fd(&self) -> RawFd {
        self.shared.notify.as_raw_fd()
    }

    fn request(&self, operation: Operation) -> std::io::Result<()> {
        let (ack, reply) = mpsc::channel();
        self.commands
            .send(Command {
                operation,
                ack: Some(ack),
            })
            .map_err(|_| std::io::Error::other("waiter thread stopped"))?;
        wake(&self.shared.kick)?;
        reply
            .recv()
            .map_err(|_| std::io::Error::other("waiter thread stopped before acknowledgement"))?
            .map_err(std::io::Error::other)
    }

    /// Install an arm only after its previous use completed or was cancelled.
    pub fn watch(&self, watch: Watch) -> std::io::Result<()> {
        self.request(Operation::Watch(watch))
    }

    /// On return, no old poll or queued completion can use these slots.
    pub fn cancel_slots(&self, guest_proc: u32, ids: &[u32]) -> std::io::Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        self.request(Operation::CancelSlots {
            guest_proc,
            ids: ids.to_vec(),
        })
    }

    /// Cancel every arm before its session is destroyed or its ID is reused.
    pub fn cancel_process(&self, guest_proc: u32) -> std::io::Result<()> {
        self.request(Operation::CancelProcess(guest_proc))
    }

    pub fn take_fired(&self) -> std::io::Result<Vec<Fired>> {
        let _ = self.shared.notify.read();
        let mut state = self.shared.state.lock().unwrap();
        if let Some(error) = &state.failure {
            return Err(std::io::Error::other(error.clone()));
        }
        Ok(std::mem::take(&mut state.fired))
    }
}

impl Drop for WaiterPoller {
    fn drop(&mut self) {
        let _ = self.commands.send(Command {
            operation: Operation::Shutdown,
            ack: None,
        });
        let _ = wake(&self.shared.kick);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn wake(fd: &EventFd) -> std::io::Result<()> {
    match fd.write(1) {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
        result => result,
    }
}

fn poll(fds: &mut [libc::pollfd], timeout: i32) -> std::io::Result<()> {
    // SAFETY: the slice owns initialized pollfds for the duration of the call.
    if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) } < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn run<P>(
    shared: &Shared,
    commands: mpsc::Receiver<Command>,
    mut poll: P,
    pace: Option<Duration>,
) -> std::io::Result<()>
where
    P: FnMut(&mut [libc::pollfd], i32) -> std::io::Result<()>,
{
    let mut watches: Vec<Watch> = Vec::new();
    let mut next_at = std::collections::HashMap::<u32, Instant>::new();
    if let Some(period) = pace {
        eprintln!(
            "vhost-user-nvrm: waiter pacing interval {} us",
            period.as_micros()
        );
    }
    loop {
        // Commands run only between polls. An acknowledgement also proves
        // that the preceding snapshot has been dropped.
        for command in commands.try_iter() {
            let result = match command.operation {
                Operation::Shutdown => return Ok(()),
                Operation::Watch(watch) => {
                    let slot = watch.fired;
                    if watches.iter().any(|w| {
                        w.fd.as_raw_fd() == watch.fd.as_raw_fd()
                            || (w.fired.guest_proc == slot.guest_proc
                                && w.fired.registration.event_id == slot.registration.event_id)
                    }) {
                        Err("waiter slot is already armed".to_string())
                    } else {
                        watches.push(watch);
                        Ok(())
                    }
                }
                Operation::CancelSlots { guest_proc, ids } => {
                    let keep = |f: &Fired| {
                        f.guest_proc != guest_proc || !ids.contains(&f.registration.event_id)
                    };
                    watches.retain(|w| keep(&w.fired));
                    shared.state.lock().unwrap().fired.retain(keep);
                    Ok(())
                }
                Operation::CancelProcess(guest_proc) => {
                    watches.retain(|w| w.fired.guest_proc != guest_proc);
                    shared
                        .state
                        .lock()
                        .unwrap()
                        .fired
                        .retain(|f| f.guest_proc != guest_proc);
                    next_at.remove(&guest_proc);
                    Ok(())
                }
            };
            if let Some(ack) = command.ack {
                let _ = ack.send(result);
            }
        }

        let now = Instant::now();
        next_at.retain(|_, deadline| *deadline > now);
        let mut soonest: Option<Instant> = None;
        let snapshot: Vec<Watch> = watches
            .iter()
            .filter(|w| {
                if let Some(&deadline) = next_at.get(&w.fired.guest_proc) {
                    soonest = Some(soonest.map_or(deadline, |s| s.min(deadline)));
                    false
                } else {
                    true
                }
            })
            .cloned()
            .collect();
        let mut fds = Vec::with_capacity(snapshot.len() + 1);
        fds.push(libc::pollfd {
            fd: shared.kick.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        fds.extend(snapshot.iter().map(|w| libc::pollfd {
            fd: w.fd.as_raw_fd(),
            events: libc::POLLIN | libc::POLLPRI,
            revents: 0,
        }));
        let timeout = soonest.map_or(-1, |deadline| {
            deadline
                .saturating_duration_since(Instant::now())
                .as_micros()
                .div_ceil(1000)
                .clamp(1, i32::MAX as u128) as i32
        });
        match poll(&mut fds, timeout) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
            Ok(()) => {}
        }
        if fds[0].revents != 0 {
            let _ = shared.kick.read();
        }
        let mut fired = Vec::new();
        for (watch, fd) in snapshot.iter().zip(&fds[1..]) {
            if fd.revents & libc::POLLNVAL != 0 {
                return Err(std::io::Error::other("owned waiter FD became invalid"));
            }
            if fd.revents != 0 {
                fired.push(watch.fired);
                watches.retain(|w| w.fired != watch.fired);
            }
        }
        // Publishing a completion permits immediate reuse by Session. No old
        // poll may remain that could consume the new arm's dataless flag.
        drop(fds);
        drop(snapshot);
        if !fired.is_empty() {
            let now = Instant::now();
            for f in &fired {
                if let Some(deadline) = pace.and_then(|p| now.checked_add(p)) {
                    next_at.insert(f.guest_proc, deadline);
                }
            }
            shared.state.lock().unwrap().fired.extend(fired);
            wake(&shared.notify)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{BorrowedFd, FromRawFd};

    const WAIT: Duration = Duration::from_secs(2);

    fn descriptor(event: &EventFd) -> Arc<OwnedFd> {
        // SAFETY: event owns this FD throughout the borrow and duplication.
        Arc::new(
            unsafe { BorrowedFd::borrow_raw(event.as_raw_fd()) }
                .try_clone_to_owned()
                .unwrap(),
        )
    }

    fn watch(guest_proc: u32, id: u32, generation: u64, fd: Arc<OwnedFd>) -> Watch {
        Watch {
            fired: Fired {
                guest_proc,
                registration: RegistrationId {
                    event_id: id,
                    generation,
                },
            },
            fd,
        }
    }

    fn notified(poller: &WaiterPoller) {
        let mut fds = [libc::pollfd {
            fd: poller.notify_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        poll(&mut fds, WAIT.as_millis() as i32).unwrap();
        assert_ne!(
            fds[0].revents & libc::POLLIN,
            0,
            "no completion notification"
        );
    }

    /// Model NVIDIA's consuming poll; ordinary eventfd poll leaves its count set.
    fn consuming_poll(fds: &mut [libc::pollfd], timeout: i32) -> std::io::Result<()> {
        poll(fds, timeout)?;
        for fd in &fds[1..] {
            if fd.revents & libc::POLLIN != 0 {
                let mut value = 0u64;
                // SAFETY: the test registrations own eventfds; value holds eight bytes.
                if unsafe { libc::read(fd.fd, (&mut value as *mut u64).cast(), 8) } != 8 {
                    return Err(std::io::Error::last_os_error());
                }
            }
        }
        Ok(())
    }

    fn queue(poller: &WaiterPoller, operation: Operation) -> mpsc::Receiver<Result<(), String>> {
        let (ack, reply) = mpsc::channel();
        poller
            .commands
            .send(Command {
                operation,
                ack: Some(ack),
            })
            .unwrap();
        wake(&poller.shared.kick).unwrap();
        reply
    }

    #[test]
    fn a_readable_waiter_is_reported_once() {
        let p = WaiterPoller::start(poll, None).unwrap();
        let ev = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        let w = watch(42, 7, 1, descriptor(&ev));
        p.watch(w.clone()).unwrap();
        ev.write(1).unwrap();
        notified(&p);
        assert_eq!(p.take_fired().unwrap(), vec![w.fired]);
        // The still-readable FD is no longer watched. A command acknowledgement
        // synchronizes with the worker instead of waiting for a quiet interval.
        p.cancel_slots(42, &[99]).unwrap();
        assert!(p.take_fired().unwrap().is_empty());
    }

    #[test]
    fn an_active_slot_cannot_be_replaced() {
        let p = WaiterPoller::start(poll, None).unwrap();
        let ev = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        let fd = descriptor(&ev);
        let old = watch(1, 10, 1, fd.clone());
        p.watch(old.clone()).unwrap();
        assert!(p.watch(watch(1, 10, 2, fd)).is_err());
        ev.write(1).unwrap();
        notified(&p);
        assert_eq!(p.take_fired().unwrap(), vec![old.fired]);
    }

    #[test]
    fn cancellation_discards_a_queued_completion() {
        let p = WaiterPoller::start(consuming_poll, None).unwrap();
        let ev = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        p.watch(watch(3, 1, 1, descriptor(&ev))).unwrap();
        ev.write(1).unwrap();
        notified(&p);
        p.cancel_slots(3, &[1]).unwrap();
        assert!(p.take_fired().unwrap().is_empty());
    }

    #[test]
    fn cancellation_waits_for_the_consuming_poll_before_rearm() {
        let (entered, paused) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        let mut first = true;
        let p = WaiterPoller::start(
            move |fds, timeout| {
                if first && fds.len() > 1 {
                    first = false;
                    entered.send(()).unwrap();
                    resumed.recv_timeout(WAIT).unwrap();
                }
                consuming_poll(fds, timeout)
            },
            None,
        )
        .unwrap();
        let ev = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        let fd = descriptor(&ev);
        p.watch(watch(3, 1, 1, fd.clone())).unwrap();
        paused.recv_timeout(WAIT).unwrap();
        let ack = queue(
            &p,
            Operation::CancelSlots {
                guest_proc: 3,
                ids: vec![1],
            },
        );
        assert!(matches!(ack.try_recv(), Err(mpsc::TryRecvError::Empty)));
        ev.write(1).unwrap(); // Old readiness arrives while cancellation waits.
        resume.send(()).unwrap();
        ack.recv_timeout(WAIT).unwrap().unwrap();
        assert!(p.take_fired().unwrap().is_empty());

        let new = watch(3, 1, 2, fd);
        p.watch(new.clone()).unwrap();
        ev.write(1).unwrap();
        notified(&p);
        assert_eq!(p.take_fired().unwrap(), vec![new.fired]);
    }

    #[test]
    fn completion_is_published_only_after_its_snapshot_is_retired() {
        let (entered, paused) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        let mut first = true;
        let p = WaiterPoller::start(
            move |fds, timeout| {
                consuming_poll(fds, timeout)?;
                if first && fds.get(1).is_some_and(|fd| fd.revents != 0) {
                    first = false;
                    entered.send(()).unwrap();
                    resumed.recv_timeout(WAIT).unwrap();
                }
                Ok(())
            },
            None,
        )
        .unwrap();
        let ev = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        let fd = descriptor(&ev);
        let old = watch(3, 1, 1, fd.clone());
        let old_id = old.fired;
        p.watch(old).unwrap();
        ev.write(1).unwrap();
        paused.recv_timeout(WAIT).unwrap();
        assert!(p.take_fired().unwrap().is_empty());
        assert_eq!(Arc::strong_count(&fd), 3, "owner, registration, snapshot");
        resume.send(()).unwrap();
        notified(&p);
        assert_eq!(p.take_fired().unwrap(), vec![old_id]);
        assert_eq!(Arc::strong_count(&fd), 1, "only the external owner remains");

        let new = watch(3, 1, 2, fd);
        p.watch(new.clone()).unwrap();
        ev.write(1).unwrap();
        notified(&p);
        assert_eq!(p.take_fired().unwrap(), vec![new.fired]);
    }

    #[test]
    fn snapshot_retains_the_descriptor_until_cancellation_acknowledges() {
        let (entered, paused) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        let mut first = true;
        let p = WaiterPoller::start(
            move |fds, timeout| {
                if first && fds.len() > 1 {
                    first = false;
                    entered.send(()).unwrap();
                    resumed.recv_timeout(WAIT).unwrap();
                }
                poll(fds, timeout)
            },
            None,
        )
        .unwrap();
        let ev = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        // Reserve above normal test descriptors so another parallel test cannot
        // reuse this number between the cancellation and our next duplication.
        // SAFETY: F_DUPFD_CLOEXEC creates a new owned descriptor without closing one.
        let high = unsafe { libc::fcntl(ev.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 512) };
        assert!(high >= 512);
        // SAFETY: high is the fresh descriptor returned above.
        let fd = Arc::new(unsafe { OwnedFd::from_raw_fd(high) });
        let weak = Arc::downgrade(&fd);
        p.watch(watch(8, 1, 1, fd)).unwrap();
        paused.recv_timeout(WAIT).unwrap();
        assert!(weak.upgrade().is_some(), "worker must retain the FD");
        let ack = queue(
            &p,
            Operation::CancelSlots {
                guest_proc: 8,
                ids: vec![1],
            },
        );
        assert!(matches!(ack.try_recv(), Err(mpsc::TryRecvError::Empty)));
        resume.send(()).unwrap();
        ack.recv_timeout(WAIT).unwrap().unwrap();
        assert!(
            weak.upgrade().is_none(),
            "ack must release all worker references"
        );

        let replacement = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        // SAFETY: this creates a descriptor at the lowest free number >= high.
        let reused = unsafe { libc::fcntl(replacement.as_raw_fd(), libc::F_DUPFD_CLOEXEC, high) };
        assert_eq!(reused, high);
        // SAFETY: reused is a fresh owned descriptor.
        let w = watch(8, 2, 2, Arc::new(unsafe { OwnedFd::from_raw_fd(reused) }));
        p.watch(w.clone()).unwrap();
        replacement.write(1).unwrap();
        notified(&p);
        assert_eq!(p.take_fired().unwrap(), vec![w.fired]);
    }

    #[test]
    fn process_cancellation_preserves_other_processes() {
        let p = WaiterPoller::start(consuming_poll, None).unwrap();
        let gone = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        let live = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        p.watch(watch(5, 1, 1, descriptor(&gone))).unwrap();
        let survivor = watch(6, 1, 1, descriptor(&live));
        p.watch(survivor.clone()).unwrap();
        p.cancel_process(5).unwrap();
        gone.write(1).unwrap();
        live.write(1).unwrap();
        notified(&p);
        assert_eq!(p.take_fired().unwrap(), vec![survivor.fired]);
    }

    #[test]
    fn a_poll_failure_disconnects_a_waiting_cancellation() {
        let (entered, paused) = mpsc::channel();
        let (resume, resumed) = mpsc::channel();
        let p = WaiterPoller::start(
            move |fds, timeout| {
                if fds.len() > 1 {
                    entered.send(()).unwrap();
                    resumed.recv_timeout(WAIT).unwrap();
                    return Err(std::io::Error::other("injected poll failure"));
                }
                poll(fds, timeout)
            },
            None,
        )
        .unwrap();
        let ev = EventFd::new(libc::EFD_NONBLOCK).unwrap();
        p.watch(watch(3, 1, 1, descriptor(&ev))).unwrap();
        paused.recv_timeout(WAIT).unwrap();
        let ack = queue(&p, Operation::CancelProcess(3));
        resume.send(()).unwrap();
        assert!(matches!(
            ack.recv_timeout(WAIT),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
        notified(&p);
        assert!(p
            .take_fired()
            .unwrap_err()
            .to_string()
            .contains("injected poll failure"));
        assert!(p.cancel_process(3).is_err());
    }

    #[test]
    fn dropping_the_poller_joins_its_thread() {
        let p = WaiterPoller::start(poll, None).unwrap();
        let weak = Arc::downgrade(&p.shared);
        drop(p);
        assert!(weak.upgrade().is_none());
    }
    #[test]
    fn frl_interval_turns_a_rate_into_a_period() {
        assert_eq!(frl_interval_from(None), None, "unset = off, the default");
        assert_eq!(frl_interval_from(Some("0")), None, "0 Hz is off, not 1/0");
        assert_eq!(frl_interval_from(Some("abc")), None, "unparsable is off");
        assert_eq!(frl_interval_from(Some("")), None, "empty is off");
        assert_eq!(
            frl_interval_from(Some("-60")),
            None,
            "a negative rate is off"
        );
        // A rate whose period no Duration can hold is off rather than a
        // panic on the poller thread (Duration::from_secs_f64 panics on
        // overflow; that thread dying leaves every semsurf fence hanging).
        assert_eq!(
            frl_interval_from(Some("1e-300")),
            None,
            "an unrepresentable period is off"
        );
        assert_eq!(frl_interval_from(Some("NaN")), None, "NaN is off");
        assert_eq!(frl_interval_from(Some("inf")), None, "infinity is off");
        assert_eq!(
            frl_interval_from(Some("1e-19")),
            None,
            "Instant cannot hold this period"
        );
        assert_eq!(
            frl_interval_from(Some("1e300")),
            None,
            "a rounded zero period is off"
        );

        let sixty = frl_interval_from(Some("60")).expect("60 Hz is a rate");
        assert_eq!(sixty, Duration::from_secs_f64(1.0 / 60.0));
        // 16.666.. ms; the number the poller's `div_ceil` comment is about.
        assert_eq!(sixty.as_micros(), 16_666);
        assert_eq!(frl_interval_from(Some("30")).unwrap().as_micros(), 33_333);
        assert_eq!(
            frl_interval_from(Some("1000")).unwrap(),
            Duration::from_millis(1)
        );
    }
}
