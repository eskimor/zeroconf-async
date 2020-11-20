#![deny(clippy::pedantic)]
#![deny(clippy::clippy::unwrap_used)]

use std::os::unix::io::{
    RawFd,
    AsRawFd
};

use avahi_sys::{
    AvahiPoll,
    AvahiTimeout,
    AvahiWatchCallback,
    AvahiTimeoutCallback,
    AvahiWatchEvent,
    AvahiWatch,
    timeval
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
pub struct Poll{
    api: AvahiPoll,
    /// Filedescriptors we are supposed to watch:
    /// We use pointers here, because this is unsafe, no reason to disguise.
    watches: Vec<*mut Watch>,
    timeouts: Vec<*mut Timeout>,
}

/// Data about a single watch.
pub struct Watch {
    /// The callback to call in case one of the events fires for the given fd.
    callback: AvahiWatchCallback,
    /// User data to pass to the callback.
    userdata: *mut ::libc::c_void,
    /// The events we care about.
    events: AvahiWatchEvent,
    /// The filedescriptor to watch.
    fd: Async<RawFdWrapper>,
    /// Whether or not we want to get rid of this watch.
    dead: bool,
}

pub struct Timeout {
    callback: AvahiTimeoutCallback,
    userdata: *mut ::libc::c_void,
    fire_time: timeval,
    dead: bool,
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
        let mut poll= Box::new(Poll {
            api,
            watches: Vec::new(),
            timeouts: Vec::new(),
        });
        let p_poll: *mut Poll = &mut *poll;
        (*poll).api.userdata = p_poll.cast();
        poll
    }

  /// Get access to the representation needed for the C APIs.
  pub fn as_avahi_poll(self: &mut Box<Poll>) -> *mut AvahiPoll {
      let p: *mut Poll = &mut **self;
      assert_ne!(p, std::ptr::null_mut());
      for w in (*p).watches {
      }
  }

  pub async fn run(&mut self) {
  }

  unsafe fn handle_watch(watch: *mut Watch) {
      let watch: &mut Watch = watch.as_mut().expect("We don't expect any null pointers in the watches vector.");

      if watch.events & AvahiWatchEvent_AVAHI_WATCH_IN != 0 {
          watch.fd.j
      }
      (*watch).fd.
  }
}



impl Drop for Poll {
    fn drop(&mut self) {
        unsafe {
            for w in &self.watches {
                Box::from_raw(*w);
            }
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
    let b_watch = Box::new(Watch {
        fd: Async::new(RawFdWrapper(fd)).expect("Async::new() must succeed for this library to work."),
        events: event,
        callback,
        userdata,
        dead: false,
    });
    let watch = Box::into_raw(b_watch);
    let s: &mut Poll = (*api).userdata.cast::<Poll>().as_mut().expect("Userdata should have been initialized and must not be null.");
    s.watches.push(watch);
    watch as *mut AvahiWatch
}

unsafe extern "C" fn watch_update(w: *mut AvahiWatch, event: AvahiWatchEvent) {
    let w = w as *mut Watch;
    (*w).events = event;
}

/// Returns events that actually happened.
///
/// # Panics:
///
/// This function is not implemented at all right now.
///
/// The supposed functionality is based on the epoll kernel interface, but we are not operating on
/// this level of abstraction. Supporting this function would add additional overhead and
/// complexity, so let's see whether Avahi functionality actually makes any use of it.
unsafe extern "C" fn watch_get_events (w: *mut AvahiWatch) -> AvahiWatchEvent {
    // If this function is really needed, we would need to emulate the basic epoll interface and
    // keep track of happened events instead of just reporting them.
    panic!("Not implemented, as we are operating on a much higher level than an actual event loop.");
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
    let b_timer = Box::new(Timeout {
        callback,
        userdata,
        fire_time: *tv,
        dead: false,
    });
    let timer = Box::into_raw(b_timer);
    let s: &mut Poll = (*api).userdata.cast::<Poll>().as_mut().expect("Userdata should have been initialized and must not be null.");
    s.timeouts.push(timer);
    timer as *mut AvahiTimeout
}


unsafe extern "C" fn timeout_update(arg1: *mut AvahiTimeout, tv: *const timeval) {
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
