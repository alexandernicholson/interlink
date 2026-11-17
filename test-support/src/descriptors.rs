//! Open-file exhaustion for accept-error tests.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

use rustix::process::{getrlimit, setrlimit, Resource, Rlimit};

/// Holds every free descriptor slot but one, so the process's next socket (a
/// client connect) succeeds and the one after it (a proxy's accept) fails with
/// EMFILE. Dropping it closes the fillers and restores the open-file limit.
///
/// The limit is process-wide: use it only where no other test runs concurrently.
pub struct DescriptorExhaustion {
    fillers: Vec<File>,
    original: Rlimit,
}

impl DescriptorExhaustion {
    pub fn leave_one_free() -> io::Result<Self> {
        let original = getrlimit(Resource::Nofile);
        // Descriptors are allocated lowest-first: cap 64 slots above the lowest
        // free one, then fill every free slot below the cap (gaps included).
        let lowest_free = File::open("/dev/null")?.as_raw_fd();
        let ceiling = u64::try_from(lowest_free).map_err(io::Error::other)? + 64;
        let mut exhaustion = Self {
            fillers: Vec::new(),
            original,
        };
        setrlimit(
            Resource::Nofile,
            Rlimit {
                current: Some(ceiling),
                maximum: original.maximum,
            },
        )?;
        loop {
            match File::open("/dev/null") {
                Ok(file) => exhaustion.fillers.push(file),
                Err(error) if error.raw_os_error() == Some(rustix::io::Errno::MFILE.raw_os_error()) => {
                    break
                }
                Err(error) => return Err(error),
            }
        }
        exhaustion.fillers.pop();
        Ok(exhaustion)
    }
}

impl Drop for DescriptorExhaustion {
    fn drop(&mut self) {
        self.fillers.clear();
        let _ = setrlimit(Resource::Nofile, self.original);
    }
}
