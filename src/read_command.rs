use std::{
    cell::RefCell,
    io,
    os::unix::process::CommandExt,
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
};

#[derive(Clone, Default)]
pub struct Cancellation(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    cancelled: bool,
    group: Option<u32>,
}

thread_local! {
    static ACTIVE: RefCell<Option<(Cancellation, bool)>> = const { RefCell::new(None) };
}

impl Cancellation {
    pub fn scope<T>(&self, allow_network: bool, work: impl FnOnce() -> T) -> T {
        ACTIVE.with(|active| *active.borrow_mut() = Some((self.clone(), allow_network)));
        let result = work();
        ACTIVE.with(|active| active.borrow_mut().take());
        result
    }

    pub fn cancel(&self) {
        let mut state = self.0.lock().unwrap();
        state.cancelled = true;
        if let Some(group) = state.group.take() {
            // This PID was returned by our spawn with process_group(0).
            // Killing the group also stops a child git helper or ssh process.
            unsafe { libc::killpg(group as libc::pid_t, libc::SIGKILL) };
        }
    }
}

pub fn network_allowed() -> bool {
    ACTIVE.with(|active| active.borrow().as_ref().is_none_or(|(_, network)| *network))
}

pub fn output(command: &mut Command) -> io::Result<Output> {
    let Some((stop, _)) = ACTIVE.with(|active| active.borrow().clone()) else {
        return command.output();
    };
    let child = {
        let mut state = stop.0.lock().unwrap();
        if state.cancelled {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Catalog load cancelled",
            ));
        }
        let child = command
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        state.group = Some(child.id());
        child
    };
    let result = child.wait_with_output();
    stop.0.lock().unwrap().group = None;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };

    #[test]
    fn cancelling_stops_the_whole_output_process_group() {
        let cancel = Cancellation::default();
        let worker = cancel.clone();
        let (sender, receiver) = mpsc::channel();
        let job = thread::spawn(move || {
            let result = worker.scope(false, || {
                output(Command::new("/bin/sh").args(["-c", "sleep 30 & wait"]))
            });
            sender.send(result).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while cancel.0.lock().unwrap().group.is_none() {
            assert!(Instant::now() < deadline, "command was never spawned");
            thread::sleep(Duration::from_millis(10));
        }
        cancel.cancel();
        assert!(
            !receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("a killed descendant still holds the output pipe")
                .unwrap()
                .status
                .success()
        );
        job.join().unwrap();
    }
}
