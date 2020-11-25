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
};

use futures::task::noop_waker;

use avahi_sys::{
    timeval, AvahiPoll, AvahiTimeout, AvahiTimeoutCallback, AvahiWatch, AvahiWatchCallback,
    AvahiWatchEvent, AvahiWatchEvent_AVAHI_WATCH_ERR, AvahiWatchEvent_AVAHI_WATCH_IN,
    AvahiWatchEvent_AVAHI_WATCH_OUT,
};

use async_io::Async;

///
/// # Safety:
///
/// The C API offers pointers pointing into values hold by this struct. Once this struct gets
/// dropped those values will be freed. You must therefore ensure that the users of the C API do
/// not outlive this struct. It is also assumed that once a watch or a timeout is freed
/// (`timeout_free` and `watch_free` functions in the AvahiPoll struct) by the C
/// API, it will no longer be accessed and it is thus safe to free it.
pub struct Poll {
    api: AvahiPoll,
    /// Filedescriptors we are supposed to watch:
    watches: HashMap<RawFd, WatchedFd>,
    timeouts: Vec<*mut Timeout>,
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
    /// We need to box these Watches because the C side will keep pointers to those values.
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
    fire_time: timeval,
    dead: bool,
}

impl WatchedFd {
    /// Check filedescriptor for new events and set revents accordingly.
    ///
    /// Returns: The found events (zero in case fd was not ready):
    fn do_poll(&mut self, cx: &mut Context<'_>) -> AvahiWatchEvent {
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
        },
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
                    revents & self.events,
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
            // watch_get_events: Some(watch_get_events),
            watch_get_events: None,
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
}

impl Future for Poll {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> TaskPoll<Self::Output> {
        self.waker = cx.waker().clone();
        for wfd in self.watches.values_mut() {
            wfd.do_poll(cx);
            wfd.call_clients();
        }
        TaskPoll::Pending
    }
}

impl Drop for Poll {
    fn drop(&mut self) {
        unsafe {
            for t in &self.timeouts {
                Box::from_raw(*t);
            }
        }
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
    // TODO: Apart from properly implementing this: all waker!
    assert_ne!(api, std::ptr::null());
    let b_timer = Box::new(Timeout {
        callback,
        userdata,
        fire_time: *tv,
        dead: false,
    });
    let timer = Box::into_raw(b_timer);
    let s: &mut Poll = (*api)
        .userdata
        .cast::<Poll>()
        .as_mut()
        .expect("Userdata should have been initialized and must not be null.");
    s.timeouts.push(timer);
    timer as *mut AvahiTimeout
}

unsafe extern "C" fn timeout_update(arg1: *mut AvahiTimeout, tv: *const timeval) {
    // TODO: Apart from properly implementing this: all waker!
    let timeout = arg1 as *mut Timeout;
    (*timeout).fire_time = *tv;
}

unsafe extern "C" fn timeout_free(t: *mut AvahiTimeout) {
    let timeout = t as *mut Timeout;
    (*timeout).dead = true;
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
