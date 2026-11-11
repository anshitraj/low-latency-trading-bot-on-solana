//! Durable notification outbox.
//! - `run_notifier`: turns engine OrderUpdates into user-facing messages
//!   (dedupe key = order_id:state, so a redelivered update never double-posts).
//! - `run_sender`: delivers due messages to Telegram with retry/backoff and
//!   `retry_after` handling; edits the order's status message in place when known.
//!
//! Telegram delivery is never on the trade hot path.

use crate::appstore::{CallbackAction, OutboxMessage};
use crate::state::AppState;
use crate::telegram::bot::button;
use crate::telegram::client::TgError;
use crate::telegram::render;
use chrono::Utc;
use serde_json::json;
use std::time::Duration;
use trader_domain::OrderState;
use uuid::Uuid;

pub async fn run_notifier(st: AppState) {
    let mut rx = st.updates.subscribe();
    loop {
        let u = match rx.recv().await {
            Ok(u) => u,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(
                    lagged = n,
                    "notifier lagged; some intermediate statuses skipped"
                );
                continue;
            }
            Err(_) => return,
        };
        st.metrics
            .inc("order_updates_total", &[("state", u.state.as_str())]);
        // Observed is transient; skip to reduce noise (confirmed follows quickly).
        if u.state == OrderState::Observed {
            continue;
        }
        let Ok(user) = st.app.get_user(u.user_id).await else {
            continue;
        };
        let Ok(order) = st.orders.get_order(u.order_id).await else {
            continue;
        };
        let chat_id = user.telegram_id;
        let mut o = order.clone();
        o.state = u.state; // render the state this event announces
        if let Some(f) = &u.fill {
            o.fill = Some(f.clone());
        }
        let text = render::order_status(&o, st.cfg.mode, &u.detail);
        let buttons = if u.state == OrderState::AwaitingAuth {
            let ttl = o
                .quote
                .as_ref()
                .map(|q| (q.expires_at - Utc::now()).num_seconds().max(5))
                .unwrap_or(30);
            json!({"inline_keyboard": [[
                button(&st, &user, chat_id, "✅ Confirm", CallbackAction::Approve { order_id: o.id }, ttl).await,
                button(&st, &user, chat_id, "✖ Cancel", CallbackAction::Cancel { order_id: o.id }, ttl).await,
            ]]})
        } else {
            json!(null)
        };
        let edit = st
            .app
            .order_message(o.id)
            .await
            .ok()
            .flatten()
            .map(|(_, m)| m);
        let m = OutboxMessage {
            id: Uuid::new_v4(),
            chat_id,
            text,
            buttons,
            dedupe_key: format!("order:{}:{}", o.id, u.state.as_str()),
            attempts: 0,
            next_attempt_at: Utc::now(),
            edit_message_id: if u.state == OrderState::AwaitingAuth {
                None
            } else {
                edit
            },
        };
        if let Ok(true) = st.app.enqueue(&m).await {
            st.outbox_notify.notify_one();
        }
    }
}

pub async fn run_sender(st: AppState) {
    loop {
        tokio::select! {
            _ = st.outbox_notify.notified() => {}
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
        let due = match st.app.due_messages(25).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, "outbox read failed");
                continue;
            }
        };
        for m in due {
            let markup = if m.buttons.is_null() {
                None
            } else {
                Some(m.buttons.clone())
            };
            let Some(tg) = &st.tg else {
                // No bot token (mock/dev): record as delivered locally.
                tracing::info!(
                    chat_id = m.chat_id,
                    "telegram disabled; outbox message recorded only"
                );
                let _ = st.app.mark_sent(m.id, None).await;
                continue;
            };
            let res = match m.edit_message_id {
                Some(mid) => tg
                    .edit_message(m.chat_id, mid, &m.text, markup)
                    .await
                    .map(|_| mid),
                None => tg.send_message(m.chat_id, &m.text, markup).await,
            };
            match res {
                Ok(mid) => {
                    let _ = st.app.mark_sent(m.id, Some(mid)).await;
                    if let Some(order_id) = m
                        .dedupe_key
                        .strip_prefix("order:")
                        .and_then(|r| r.split(':').next())
                        .and_then(|s| s.parse::<Uuid>().ok())
                    {
                        if m.edit_message_id.is_none() {
                            let _ = st.app.set_order_message(order_id, m.chat_id, mid).await;
                        }
                    }
                    st.metrics.inc("telegram_sent_total", &[]);
                }
                Err(e) => {
                    let delay = match &e {
                        TgError::RetryAfter(s) => *s as i64,
                        _ => 2i64.pow(m.attempts.min(8)),
                    };
                    let give_up = m.attempts >= 10
                        || matches!(
                            &e,
                            TgError::Api {
                                code: 400 | 403,
                                ..
                            }
                        );
                    let at = if give_up {
                        Utc::now() + chrono::Duration::days(3650)
                    } else {
                        Utc::now() + chrono::Duration::seconds(delay)
                    };
                    let _ = st.app.mark_retry(m.id, at, &e.to_string()).await;
                    st.metrics.inc("telegram_send_errors_total", &[]);
                }
            }
        }
    }
}
