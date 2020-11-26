#![deny(clippy::pedantic)]
#![deny(clippy::clippy::unwrap_used)]

use std::{
    collections::HashMap,
    future::Future,
    io,
    os::unix::io::{AsRawFd, RawFd},
    pin::Pin,
    task::Context,
    task::Poll as TaskPoll,
    task::Waker,
    time::Instant,
};

use futures::task::noop_waker;

use avahi_sys::{
    timeval, AvahiPoll, AvahiTimeout, AvahiTimeoutCallback, AvahiWatch, AvahiWatchCallback,
    AvahiWatchEvent, AvahiWatchEvent_AVAHI_WATCH_ERR, AvahiWatchEvent_AVAHI_WATCH_IN,
    AvahiWatchEvent_AVAHI_WATCH_OUT,
};

use avahi_sys as avahi;

use async_io::{Async, Timer};

///
/// # Safety:
///
/// The C API offers pointers pointing into values hold by this struct. Once
/// this struct gets dropped those values will be freed. You must therefore
/// ensure that the users of the C API do not outlive this struct. It is also
/// assumed that once a watch or a timeout is freed (`timeout_free` and
/// `watch_free` functions in the AvahiPoll struct) by the C API, it will no
/// longer be accessed and it is thus safe to free it.
pub struct Poll {
    api: AvahiPoll,
    /// Filedescriptors we are supposed to watch:
    ///
    /// indexed by file descriptor so we can look it up based on user `Watch`es.
    watches: HashMap<RawFd, WatchedFd>,

    /// One shot timers, will be removed once they fired.
    timeouts: Vec<Box<Timeout>>,
    /// Waker for waking up the poll.
    /// Will be triggered anytime the set of interesting watches changes.
    waker: Waker,
}

/// Single file descriptor being watched.
///
/// Including references to interested clients.
struct WatchedFd {
    /// File descriptor to watch for changes.
    fd: Async<RawFdWrapper>,
    /// Received events in current poll.
    revents: AvahiWatchEvent,
    /// All the clients interested in this file descriptor.
    ///
    /// We need to box these Watches because the C side will keep pointers to
    /// those values.
    clients: Vec<Box<Watch>>,
}

/// Data about a single watch.
///
/// as returned by `watch_new`.
pub struct Watch {
    /// Access to the containing `Poll`.
    poll: *mut Poll,
    /// The file descriptor we are watching.
    fd: RawFd,
    /// The callback to call in case one of the events fires for the given fd.
    callback: AvahiWatchCallback,
    /// User data to pass to the callback.
    userdata: *mut ::libc::c_void,
    /// The events we care about.
    events: AvahiWatchEvent,
    /// Whether or not we want to get rid of this watch.
    dead: bool,
}

pub struct Timeout {
    callback: AvahiTimeoutCallback,
    userdata: *mut ::libc::c_void,
    timer: Timer,
    dead: bool,
}

impl Future for Timeout {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> TaskPoll<Self::Output> {
        if self.dead {
            return TaskPoll::Ready(());
        }
        match (Pin::new(&mut self.timer)).poll(cx) {
            TaskPoll::Ready(_) => {
                self.call_client();
                self.dead = true;
                TaskPoll::Ready(())
            }
            TaskPoll::Pending => TaskPoll::Pending,
        }
    }
}

impl Timeout {
    fn call_client(&mut self) {
        let callback = self.callback.expect("Callback must not be null");
        let p: *mut Timeout = &mut *self;
        // Assuming C code does not mess it up, this should be safe:
        unsafe {
            callback(p as *mut AvahiTimeout, self.userdata);
        }
    }
}

impl WatchedFd {
    /// Cleanup any dead clients.
    /// 
    /// Returns: true if this watch still has clients afterwards, false if it should be removed.
    fn cleanup(&mut self) -> bool {
        self.clients.retain(|c| !(*c).dead);
        !self.clients.is_empty()
    }

    /// Check filedescriptor for new events, set revents accordingly and call
    /// callbacks.
    ///
    /// Returns: The found events (zero in case fd was not ready):
    pub fn poll(&mut self, cx: &mut Context<'_>) -> AvahiWatchEvent {
        let asked_events = self
            .clients
            .iter()
            .map(|w| (*w).events)
            .fold(0, |out, inp| out | inp);
        let mut revents = 0;
        if asked_events & AvahiWatchEvent_AVAHI_WATCH_IN != 0 {
            revents =
                revents | get_revents(&self.fd.poll_readable(cx), AvahiWatchEvent_AVAHI_WATCH_IN);
        }
        if asked_events & AvahiWatchEvent_AVAHI_WATCH_OUT != 0 {
            revents =
                revents | get_revents(&self.fd.poll_writable(cx), AvahiWatchEvent_AVAHI_WATCH_OUT);
        }
        self.revents = revents;

        self.call_clients();

        revents
    }

    /// Call clients if revents is not 0.
    ///
    /// Afterwards set revents to 0.
    fn call_clients(&mut self) {
        if self.revents != 0 {
            for c in &mut self.clients {
                (*c).call_client(self.revents);
            }
            self.revents = 0;
        }
    }
}

fn get_revents(p: &TaskPoll<io::Result<()>>, e: AvahiWatchEvent) -> AvahiWatchEvent {
    match &p {
        TaskPoll::Ready(Ok(())) => e,
        TaskPoll::Ready(Err(err)) => {
            log::warn!("Checking for event {:?} failed with: {}", e, err);
            AvahiWatchEvent_AVAHI_WATCH_ERR
        }
        TaskPoll::Pending => 0,
    }
}

impl Watch {
    fn call_client(&mut self, revents: AvahiWatchEvent) {
        let callback = self.callback.expect("Callback must not be null");
        let p: *mut Watch = &mut *self;
        let revents = revents & self.events;
        if revents != 0 {
            // Assuming C code does not mess it up, this should be safe:
            unsafe {
                callback(
                    p as *mut AvahiWatch,
                    self.fd,
                    revents,
                    self.userdata,
                );
            }
        }
    }
}

impl Poll {
    pub fn new() -> Box<Poll> {
        let api = AvahiPoll {
            // Will be filled out later:
            userdata: std::ptr::null_mut(),
            watch_new: Some(watch_new),
            watch_update: Some(watch_update),
            watch_get_events: Some(watch_get_events),
            watch_free: Some(watch_free),
            timeout_new: Some(timeout_new),
            timeout_update: Some(timeout_update),
            timeout_free: Some(timeout_free),
        };
        let mut poll = Box::new(Poll {
            api,
            watches: HashMap::new(),
            timeouts: Vec::new(),
            waker: noop_waker(),
        });
        let p_poll: *mut Poll = &mut *poll;
        (*poll).api.userdata = p_poll.cast();
        poll
    }

    /// Get access to the representation needed for the C APIs.
    pub fn as_avahi_poll(self: &mut Box<Poll>) -> *mut AvahiPoll {
        let p: *mut Poll = &mut **self;
        assert_ne!(p, std::ptr::null_mut());
        p as *mut AvahiPoll
    }

    /// Remove dead watches and timers.
    fn cleanup_dead(&mut self) {
        self.watches.retain(|_, v| v.cleanup());
        self.timeouts.retain(|t| !t.dead);
    }
}

impl Future for Poll {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> TaskPoll<Self::Output> {
        self.waker = cx.waker().clone();

        self.cleanup_dead();

        for wfd in self.watches.values_mut() {
            wfd.poll(cx);
        }

        for t in &mut self.timeouts {
            // We don't care, we always return `Pending` and will wait for more events.
            let _ = Pin::new(&mut *t).poll(cx);
        }
        TaskPoll::Pending
    }
}

/// Dummy wrapper to get a `AsRawFd` usable for `Async::new`.
struct RawFdWrapper(RawFd);

impl AsRawFd for RawFdWrapper {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

unsafe extern "C" fn watch_new(
    api: *const AvahiPoll,
    fd: ::libc::c_int,
    event: AvahiWatchEvent,
    callback: AvahiWatchCallback,
    userdata: *mut ::libc::c_void,
) -> *mut AvahiWatch {
    assert_ne!(api, std::ptr::null());
    let s: &mut Poll = (*api)
        .userdata
        .cast::<Poll>()
        .as_mut()
        .expect("Userdata should have been initialized and must not be null.");

    let mut b_watch = Box::new(Watch {
        poll: s,
        fd,
        events: event,
        callback,
        userdata,
        dead: false,
    });

    let entry = s.watches.entry(fd).or_insert_with(|| WatchedFd {
        fd: Async::new(RawFdWrapper(fd))
            .expect("Async::new() must succeed for this library to work."),
        revents: 0,
        clients: Vec::new(),
    });
    let watch: *mut Watch = &mut *b_watch;
    entry.clients.push(b_watch);
    s.waker.clone().wake();
    watch as *mut AvahiWatch
}

unsafe extern "C" fn watch_update(w: *mut AvahiWatch, event: AvahiWatchEvent) {
    let w = w as *mut Watch;
    (*w).events = event;
    (*(*w).poll).waker.clone().wake();
}

/// Returns events that actually happened.
unsafe extern "C" fn watch_get_events(w: *mut AvahiWatch) -> AvahiWatchEvent {
    let w = (w as *mut Watch)
        .as_ref()
        .expect("AvahiWatch pointer must not be null for `watch_get_events!");
    let poll = w.poll;
    let fd_desc = (*poll)
        .watches
        .get(&w.fd)
        .expect("watch_get_events expects a valid Watch!");
    // Return happened events as of interest to the caller:
    fd_desc.revents & w.events
}

unsafe extern "C" fn watch_free(w: *mut AvahiWatch) {
    let w = w as *mut Watch;
    (*w).dead = true;
}

unsafe extern "C" fn timeout_new(
    api: *const AvahiPoll,
    tv: *const timeval,
    callback: AvahiTimeoutCallback,
    userdata: *mut ::libc::c_void,
) -> *mut AvahiTimeout {
    assert_ne!(api, std::ptr::null());
    let mut b_timer = Box::new(Timeout {
        callback,
        userdata,
        timer: Timer::at(timeval_to_instant(
            tv.as_ref().expect("Passed timeval must not be null!"),
        )),
        dead: false,
    });
    let timer: *mut Timeout = &mut *b_timer;
    let s: &mut Poll = (*api)
        .userdata
        .cast::<Poll>()
        .as_mut()
        .expect("Userdata should have been initialized and must not be null.");
    s.timeouts.push(b_timer);
    s.waker.clone().wake();
    timer as *mut AvahiTimeout
}

unsafe extern "C" fn timeout_update(arg1: *mut AvahiTimeout, tv: *const timeval) {
    // TODO: HANDLE DEAD TIMERS!
    let timeout = arg1 as *mut Timeout;
    (*timeout).timer.set_at(timeval_to_instant(
        tv.as_ref().expect("Passed timeval must not be null!"),
    ));
}

unsafe extern "C" fn timeout_free(t: *mut AvahiTimeout) {
    let timeout = t as *mut Timeout;
    (*timeout).dead = true;
}

/// Get a Rust `Instant` from a timeval as used on the C side.
///
/// We need this for briding the avahi poll API for timers with async-io timers,
/// which accept `Instants`.
fn timeval_to_instant(val: &avahi::timeval) -> Instant {
    timespec_to_instant(&timeval_to_timespec(val))
}

fn timespec_to_instant(spec: &avahi::timespec) -> Instant {
    // This is safe, because as of this writing an `Instant` is a libc::timespec under the
    // hood.
    //
    // This might change, we therefore have a test case in the test suit of this library which
    // should catch if this ever changes. Combined with the size check of `transmute` this should
    // be reasonably safe.
    unsafe { std::mem::transmute::<avahi::timespec, Instant>(*spec) }
}
fn timeval_to_timespec(val: &avahi::timeval) -> avahi::timespec {
    avahi::timespec {
        tv_sec: val.tv_sec,
        tv_nsec: val.tv_usec * 1000,
    }
}

#[cfg(test)]
mod tests {
    use super::timeval_to_instant;
    #[test]
    fn check_timeval_to_instant_correctness() {
        let t = avahi_sys::timeval {
            tv_sec: 1_576_800_000,
            tv_usec: 314_159,
        };
        let instant = timeval_to_instant(&t);
        assert_eq!(
            format!("{:?}", instant),
            "Instant { tv_sec: 1576800000, tv_nsec: 314159000 }"
        );
    }
}
