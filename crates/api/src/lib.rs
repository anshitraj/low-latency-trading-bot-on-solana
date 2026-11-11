//! trader-api: HTTP API, Telegram bot, stores and wiring.

pub mod appstore;
pub mod auth;
pub mod config;
pub mod outbox;
pub mod pg;
pub mod providers;
pub mod routes;
pub mod service;
pub mod state;
pub mod telegram;

use std::sync::Arc;

/// Spawn background workers: observer, notifier, outbox sender, Telegram update workers.
pub async fn spawn_background(built: state::Built) -> state::AppState {
    let st = built.state.clone();
    built.observer.clone().spawn();
    let recovered = st.engine.recover().await.unwrap_or(0);
    if recovered > 0 {
        tracing::warn!(recovered, "reconciling unresolved orders from previous run");
    }
    tokio::spawn(outbox::run_notifier(st.clone()));
    tokio::spawn(outbox::run_sender(st.clone()));
    let sem = Arc::new(tokio::sync::Semaphore::new(16));
    let mut rx = built.tg_rx;
    let st2 = st.clone();
    tokio::spawn(async move {
        while let Some(upd) = rx.recv().await {
            let permit = sem.clone().acquire_owned().await.expect("semaphore");
            let st3 = st2.clone();
            tokio::spawn(async move {
                telegram::bot::handle_update(&st3, upd).await;
                drop(permit);
            });
        }
    });
    st
}
