//! One-shot Monad WS check. Never submits transactions or loops indefinitely.
use skew_execution_host::monad::feed::{observe_ws_heads, HeadTracker};
use std::{env, fs};
fn run() -> Result<(), String> {
    let url = match env::var("MONAD_WS_URL_FILE") {
        Ok(path) => fs::read_to_string(path)
            .map_err(|_| "Monad WS file unreadable")?
            .trim()
            .to_owned(),
        Err(_) => "wss://rpc.monad.xyz".into(),
    };
    let mut tracker = HeadTracker::default();
    let events = observe_ws_heads(&url, 4, &mut tracker)?;
    let finalized = events
        .iter()
        .filter(|e| {
            matches!(
                e.commit_state,
                skew_execution_host::monad::feed::CommitState::Finalized
                    | skew_execution_host::monad::feed::CommitState::Verified
            )
        })
        .count();
    println!(
        "Monad WS headers={} finalized_updates={} contiguous={}",
        events.len(),
        finalized,
        tracker.ready()
    );
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("monad-head-watch: {e}");
        std::process::exit(1);
    }
}
