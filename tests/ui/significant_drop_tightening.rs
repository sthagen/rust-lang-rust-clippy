// run-rustfix

#![warn(clippy::significant_drop_tightening)]

use std::sync::Mutex;

pub fn unnecessary_contention_with_multiple_owned_results() {
    {
        let mutex = Mutex::new(1i32);
        let lock = mutex.lock().unwrap();
        let _ = lock.abs();
        let _ = lock.is_positive();
    }

    {
        let mutex = Mutex::new(1i32);
        let lock = mutex.lock().unwrap();
        let rslt0 = lock.abs();
        let rslt1 = lock.is_positive();
        do_heavy_computation_that_takes_time((rslt0, rslt1));
    }
}

pub fn unnecessary_contention_with_single_owned_results() {
    {
        let mutex = Mutex::new(1i32);
        let lock = mutex.lock().unwrap();
        let _ = lock.abs();
    }
    {
        let mutex = Mutex::new(vec![1i32]);
        let mut lock = mutex.lock().unwrap();
        lock.clear();
    }

    {
        let mutex = Mutex::new(1i32);
        let lock = mutex.lock().unwrap();
        let rslt0 = lock.abs();
        do_heavy_computation_that_takes_time(rslt0);
    }
    {
        let mutex = Mutex::new(vec![1i32]);
        let mut lock = mutex.lock().unwrap();
        lock.clear();
        do_heavy_computation_that_takes_time(());
    }
}

// Marker used for illustration purposes.
pub fn do_heavy_computation_that_takes_time<T>(_: T) {}

fn main() {}
