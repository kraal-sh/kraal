//! The threads kraal names: those of a pane, of a client, of the server, and its helpers.

/// Their stack, 64 KiB short of the 2 MiB Rust gives a thread. musl keeps a thread's own data
/// above its stack, in the same mapping: with 2 MiB of stack, that mapping holds a whole 2 MiB
/// page wherever it happens to end on such a boundary, and Linux, where transparent huge pages
/// are always on, then gives the thread that page at its first step: 2 MiB of memory for a
/// thread that uses a few kilobytes. A mapping under 2 MiB never holds one.
const STACK: usize = (2 << 20) - (64 << 10);

/// A thread to start under `name`, with a stack of [`STACK`].
pub fn named(name: impl Into<String>) -> std::thread::Builder {
    std::thread::Builder::new().name(name.into()).stack_size(STACK)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The size of the calling thread's stack.
    #[cfg(target_os = "linux")]
    fn stack() -> usize {
        // SAFETY: the attributes are read into a zeroed structure that is then destroyed.
        unsafe {
            let mut attr: libc::pthread_attr_t = std::mem::zeroed();
            assert_eq!(libc::pthread_getattr_np(libc::pthread_self(), &mut attr), 0);
            let mut size = 0;
            assert_eq!(libc::pthread_attr_getstacksize(&attr, &mut size), 0);
            libc::pthread_attr_destroy(&mut attr);
            size
        }
    }

    #[cfg(target_os = "macos")]
    fn stack() -> usize {
        // SAFETY: asks about the calling thread.
        unsafe { libc::pthread_get_stacksize_np(libc::pthread_self()) }
    }

    #[test]
    fn a_named_thread_has_its_name_and_a_stack_under_two_megabytes() {
        let thread = named("worker").spawn(|| (std::thread::current().name().map(str::to_owned), stack()));
        let (name, stack) = thread.unwrap().join().unwrap();
        assert_eq!(name.as_deref(), Some("worker"));
        // Under 2 MiB with the thread's own data, which musl counts in. glibc may give a thread
        // the stack of one that ended, when it is large enough: here one of the tests' own
        // threads, of 2 MiB (its guard page counted in, so no huge page fits there either).
        let limit = if cfg!(target_env = "gnu") { (2 << 20) + 1 } else { 2 << 20 };
        assert!((STACK / 2..limit).contains(&stack), "{stack}");
    }
}
