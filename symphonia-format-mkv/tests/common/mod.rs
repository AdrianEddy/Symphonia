// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What the integration tests share.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

/// How long a test that seeks may run before it fails as hung. It detects a seek that never
/// returns, and is far above what any run of the tests takes, not a bound on their speed.
const DEADLINE: Duration = Duration::from_secs(60);

/// Run `test` on a thread of its own and wait for it until [`DEADLINE`], so a seek that never
/// returns fails the test instead of hanging the test run. A panic of `test` is the test's.
pub fn within_deadline(test: impl FnOnce() + Send + 'static) {
    let (done, finished) = mpsc::channel();
    let name = thread::current().name().unwrap_or("test").to_owned();

    let runner = thread::Builder::new()
        .name(name)
        .spawn(move || {
            test();
            // The receiver is gone only if the deadline passed, which fails the test already.
            let _ = done.send(());
        })
        .expect("a thread for the test");

    match finished.recv_timeout(DEADLINE) {
        Ok(()) => runner.join().expect("the test ended"),
        Err(RecvTimeoutError::Disconnected) => match runner.join() {
            Err(panic) => std::panic::resume_unwind(panic),
            Ok(()) => unreachable!("the test reports its end before it ends"),
        },
        // Nothing stops a thread from outside: it runs on until the test binary exits.
        Err(RecvTimeoutError::Timeout) => panic!("the test did not end within {DEADLINE:?}"),
    }
}
