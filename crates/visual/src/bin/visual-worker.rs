//! A bare render worker, for the crate's own isolation tests.

fn main() {
    if std::env::args().nth(1).as_deref() == Some(else_visual::isolate::WORKER_ARG) {
        else_visual::isolate::run_worker();
    }
    eprintln!("usage: visual-worker {}", else_visual::isolate::WORKER_ARG);
    std::process::exit(2);
}
