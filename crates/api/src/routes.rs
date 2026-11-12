//! REST API (see openapi.yaml), Telegram webhook, SSE, static Mini App.

use crate::appstore::{TrackedWallet, User, UserSettings};
use crate::auth::{new_token, secret_eq, token_hash, verify_init_data};
use crate::service::{self, ServiceError};
use crate::state::AppState;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream::Stream;
use serde::Deserialize;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::time::Duration;
use trader_domain::{AmountSpec, Bps, Lamports, OrderState, Side};
use uuid::Uuid;

pub struct ApiError(pub StatusCode, pub &'static str, pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.0,
            Json(json!({"error": {"code": self.1, "message": self.2}})),
        )
            .into_response()
    }
}

impl From<ServiceError> for ApiError {
    fn from(e: ServiceError) -> Self {
        let status = match &e {
            ServiceError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ServiceError::NotFound => StatusCode::NOT_FOUND,
            ServiceError::Forbidden | ServiceError::NotAllowlisted => StatusCode::FORBIDDEN,
            ServiceError::NoWallet => StatusCode::CONFLICT,
            ServiceError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            ServiceError::Engine(trader_orders::EngineError::Busy) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            ServiceError::Engine(trader_orders::EngineError::Forbidden) => StatusCode::NOT_FOUND,
            ServiceError::Engine(trader_orders::EngineError::Unavailable(_)) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            ServiceError::Engine(_) => StatusCode::UNPROCESSABLE_ENTITY,
            ServiceError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let msg = if status == StatusCode::INTERNAL_SERVER_ERROR {
            "internal error".to_string()
        } else {
            e.to_string()
        };
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %trader_observability::redact(&e.to_string()), "internal error");
        }
        ApiError(status, e.code(), msg)
    }
}

type ApiResult<T> = Result<T, ApiError>;

fn bad(m: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, "bad_request", m.into())
}

pub struct AuthUser(pub User);

impl axum::extract::FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        st: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let unauth = || {
            ApiError(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "missing or invalid session".into(),
            )
        };
        let h = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(unauth)?;
        let token = h.strip_prefix("Bearer ").ok_or_else(unauth)?;
        if token.len() > 128 {
            return Err(unauth());
        }
        let uid = st
            .app
            .session_user(&token_hash(token))
            .await
            .map_err(|_| unauth())?
            .ok_or_else(unauth)?;
        let u = st.app.get_user(uid).await.map_err(|_| unauth())?;
        Ok(AuthUser(u))
    }
}

pub struct Admin;

impl axum::extract::FromRequestParts<AppState> for Admin {
    type Rejection = ApiError;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        st: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let deny = || ApiError(StatusCode::NOT_FOUND, "not_found", "not found".into());
        let expected = st.cfg.admin_token.as_deref().ok_or_else(deny)?;
        let got = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .ok_or_else(deny)?;
        if !secret_eq(got, expected) {
            return Err(deny());
        }
        Ok(Admin)
    }
}

async fn security_headers(req: Request, next: Next) -> Response {
    let mut r = next.run(req).await;
    let h = r.headers_mut();
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    h.insert("cache-control", HeaderValue::from_static("no-store"));
    r
}

pub fn router(st: AppState) -> Router {
    let api = Router::new()
        .route("/v1/health", get(health))
        .route("/v1/ready", get(ready))
        .route("/metrics", get(metrics))
        .route("/v1/provider-health", get(provider_health))
        .route("/v1/auth/telegram", post(auth_telegram))
        .route("/v1/me", get(me))
        .route("/v1/wallets", get(list_wallets).post(create_wallet))
        .route("/v1/portfolio", get(portfolio))
        .route("/v1/tokens/{mint}", get(token))
        .route("/v1/tokens/{mint}/quote", get(token_quote))
        .route("/v1/orders/preview", post(order_preview))
        .route("/v1/orders", get(list_orders).post(create_order))
        .route("/v1/orders/{id}", get(get_order))
        .route("/v1/orders/{id}/approve", post(approve_order))
        .route("/v1/orders/{id}/cancel", post(cancel_order))
        .route("/v1/transactions/{signature}", get(get_transaction))
        .route(
            "/v1/tracked-wallets",
            get(list_tracked).post(add_tracked).delete(remove_tracked),
        )
        .route(
            "/v1/copy-rules",
            get(not_implemented)
                .post(not_implemented)
                .delete(not_implemented),
        )
        .route("/v1/referrals", get(not_implemented))
        .route("/v1/settings", get(get_settings).post(set_settings))
        .route("/v1/events", get(events))
        .route("/v1/dev/airdrop", post(dev_airdrop))
        .route("/telegram/webhook", post(telegram_webhook))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(middleware::from_fn(security_headers));
    let mut app = api.with_state(st.clone());
    if let Some(origin) = st
        .cfg
        .cors_origin
        .as_ref()
        .and_then(|o| o.parse::<HeaderValue>().ok())
    {
        use tower_http::cors::CorsLayer;
        app = app.layer(
            CorsLayer::new()
                .allow_origin(origin)
                .allow_methods([
                    axum::http::Method::GET,
                    axum::http::Method::POST,
                    axum::http::Method::DELETE,
                ])
                .allow_headers([
                    axum::http::header::AUTHORIZATION,
                    axum::http::header::CONTENT_TYPE,
                    axum::http::HeaderName::from_static("idempotency-key"),
                ]),
        );
    }
    if let Some(dir) = &st.cfg.miniapp_dir {
        let index = format!("{dir}/index.html");
        app = app.fallback_service(
            tower_http::services::ServeDir::new(dir)
                .fallback(tower_http::services::ServeFile::new(index)),
        );
    }
    app.layer(tower_http::timeout::TimeoutLayer::with_status_code(
        StatusCode::REQUEST_TIMEOUT,
        Duration::from_secs(30),
    ))
}

async fn health(State(st): State<AppState>) -> Json<Value> {
    Json(
        json!({"status": "ok", "mode": st.cfg.mode, "version": env!("CARGO_PKG_VERSION"), "mainnet_trading_enabled": st.cfg.mainnet_trading_enabled, "mock_demo_mint": st.mock.as_ref().map(|m| m.demo_mint.to_string())}),
    )
}

async fn ready(State(st): State<AppState>) -> Response {
    let bh_ok = st.engine.blockhash.get(Duration::from_secs(10)).is_ok();
    let rpc_ok = tokio::time::timeout(
        Duration::from_secs(2),
        st.rpc.get_slot(trader_domain::Commitment::Processed),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false);
    let status = if bh_ok && rpc_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(json!({"blockhash_fresh": bh_ok, "rpc_reachable": rpc_ok})),
    )
        .into_response()
}

async fn metrics(_a: Admin, State(st): State<AppState>) -> String {
    st.metrics.render()
}

async fn provider_health(_a: Admin, State(st): State<AppState>) -> Json<Value> {
    Json(service::provider_health(&st))
}

#[derive(Deserialize)]
struct AuthBody {
    init_data: String,
}

async fn auth_telegram(
    State(st): State<AppState>,
    Json(b): Json<AuthBody>,
) -> ApiResult<Json<Value>> {
    let bot = st.cfg.bot_token.as_deref().ok_or_else(|| {
        ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "telegram_disabled",
            "Telegram auth not configured".into(),
        )
    })?;
    let now = chrono::Utc::now().timestamp();
    let v = verify_init_data(&b.init_data, bot, now, st.cfg.initdata_max_age_secs)
        .map_err(|e| ApiError(StatusCode::UNAUTHORIZED, "invalid_init_data", e.to_string()))?;
    let fresh = st
        .app
        .remember_init_data(
            &v.hash,
            chrono::Utc::now() + chrono::Duration::seconds(st.cfg.initdata_max_age_secs + 60),
        )
        .await
        .map_err(|e| ApiError::from(ServiceError::from(e)))?;
    if !fresh {
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "init_data_replayed",
            "this initData was already exchanged; reopen the Mini App".into(),
        ));
    }
    let user = service::ensure_tg_user(&st, v.user.id, v.user.username.clone()).await?;
    let token = new_token();
    let expires = chrono::Utc::now() + chrono::Duration::seconds(st.cfg.session_ttl_secs);
    st.app
        .create_session(&token_hash(&token), user.id, expires)
        .await
        .map_err(|e| ApiError::from(ServiceError::from(e)))?;
    st.app
        .audit(
            Some(user.id),
            "session_created",
            json!({"via": "miniapp_initdata"}),
        )
        .await
        .ok();
    Ok(Json(
        json!({"token": token, "expires_at": expires, "user": user, "start_param": v.start_param}),
    ))
}

async fn me(State(st): State<AppState>, AuthUser(u): AuthUser) -> ApiResult<Json<Value>> {
    let s = service::settings(&st, &u).await?;
    Ok(Json(json!({"user": u, "settings": s, "mode": st.cfg.mode})))
}

async fn list_wallets(State(st): State<AppState>, AuthUser(u): AuthUser) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({"wallets": st.app.list_wallets(u.id).await.map_err(ServiceError::from)?}),
    ))
}

async fn create_wallet(
    State(st): State<AppState>,
    AuthUser(u): AuthUser,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let w = service::create_wallet(&st, &u).await?;
    Ok((
        StatusCode::CREATED,
        Json(
            json!({"wallet": w, "custody": "managed (custodial) test wallet; envelope-encrypted at rest; private beta only"}),
        ),
    ))
}

async fn portfolio(State(st): State<AppState>, AuthUser(u): AuthUser) -> ApiResult<Json<Value>> {
    Ok(Json(service::portfolio(&st, &u).await?))
}

async fn token(State(st): State<AppState>, Path(mint): Path<String>) -> ApiResult<Json<Value>> {
    let m = service::parse_mint_arg(&mint)?;
    Ok(Json(
        serde_json::to_value(service::token_info(&st, m).await).unwrap(),
    ))
}

#[derive(Deserialize)]
struct QuoteQ {
    side: String,
    amount: Option<String>,
    percent: Option<u32>,
}

#[derive(Deserialize)]
struct OrderBody {
    mint: String,
    side: String,
    amount_sol: Option<String>,
    percent: Option<u32>,
    amount_raw: Option<String>,
}

fn amount_spec(
    side: &str,
    sol: Option<&str>,
    pct: Option<u32>,
    raw: Option<&str>,
) -> ApiResult<(Side, AmountSpec)> {
    match side {
        "buy" => {
            let s = sol.ok_or_else(|| bad("amount_sol required for buy"))?;
            Ok((
                Side::Buy,
                AmountSpec::ExactSolIn {
                    lamports: Lamports::from_sol_str(s).map_err(|e| bad(e.to_string()))?,
                },
            ))
        }
        "sell" => match (pct, raw) {
            (Some(p), None) if (1..=100).contains(&p) => Ok((
                Side::Sell,
                AmountSpec::SellPercent {
                    bps: Bps::new(p * 100).unwrap(),
                },
            )),
            (None, Some(r)) => Ok((
                Side::Sell,
                AmountSpec::ExactTokenIn {
                    raw: r
                        .parse()
                        .map_err(|_| bad("amount_raw must be an integer string"))?,
                },
            )),
            _ => Err(bad("sell requires percent (1-100) or amount_raw")),
        },
        _ => Err(bad("side must be buy or sell")),
    }
}

async fn wallet_for(st: &AppState, u: &User) -> ApiResult<crate::appstore::Wallet> {
    Ok(service::default_wallet(st, u)
        .await?
        .ok_or(ServiceError::NoWallet)?)
}

async fn token_quote(
    State(st): State<AppState>,
    AuthUser(u): AuthUser,
    Path(mint): Path<String>,
    Query(q): Query<QuoteQ>,
) -> ApiResult<Json<Value>> {
    let (side, amount) = amount_spec(&q.side, q.amount.as_deref(), q.percent, None)?;
    let w = wallet_for(&st, &u).await?;
    let intent = service::build_intent(
        &st,
        &u,
        &w,
        side,
        service::parse_mint_arg(&mint)?,
        amount,
        format!("quote:{}", Uuid::new_v4()),
        Some(true),
    )
    .await?;
    Ok(Json(
        json!({"quote": service::preview(&st, &u, &intent).await?}),
    ))
}

async fn order_preview(
    State(st): State<AppState>,
    AuthUser(u): AuthUser,
    Json(b): Json<OrderBody>,
) -> ApiResult<Json<Value>> {
    let (side, amount) = amount_spec(
        &b.side,
        b.amount_sol.as_deref(),
        b.percent,
        b.amount_raw.as_deref(),
    )?;
    let w = wallet_for(&st, &u).await?;
    let intent = service::build_intent(
        &st,
        &u,
        &w,
        side,
        service::parse_mint_arg(&b.mint)?,
        amount,
        format!("preview:{}", Uuid::new_v4()),
        Some(true),
    )
    .await?;
    Ok(Json(
        json!({"quote": service::preview(&st, &u, &intent).await?, "note": "preview only: nothing persisted or signed"}),
    ))
}

async fn create_order(
    State(st): State<AppState>,
    AuthUser(u): AuthUser,
    headers: HeaderMap,
    Json(b): Json<OrderBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| bad("Idempotency-Key header required"))?;
    trader_domain::intent::validate_idempotency_key(key).map_err(bad)?;
    let (side, amount) = amount_spec(
        &b.side,
        b.amount_sol.as_deref(),
        b.percent,
        b.amount_raw.as_deref(),
    )?;
    let w = wallet_for(&st, &u).await?;
    // Always AWAITING_AUTH: the client shows the server's exact quote and the
    // user confirms via /approve. The executed thresholds are the confirmed ones.
    let intent = service::build_intent(
        &st,
        &u,
        &w,
        side,
        service::parse_mint_arg(&b.mint)?,
        amount,
        format!("api:{key}"),
        Some(true),
    )
    .await?;
    let o = service::submit(&st, &u, intent, Some(trader_domain::LatencyTrace::start())).await?;
    // Wait briefly for the quote so the client can render the confirmation sheet.
    let mut o = o;
    for _ in 0..60 {
        if o.state != OrderState::Created {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        o = st
            .orders
            .get_order(o.id)
            .await
            .map_err(|e| ServiceError::Internal(e.to_string()))?;
    }
    Ok((StatusCode::CREATED, Json(json!({"order": o}))))
}

async fn list_orders(State(st): State<AppState>, AuthUser(u): AuthUser) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({"orders": st.orders.list_orders(u.id, 50).await.map_err(|e| ServiceError::Internal(e.to_string()))?}),
    ))
}

async fn get_order(
    State(st): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let o = service::order_for_user(&st, &u, id).await?;
    let ev = st.orders.events(id).await.unwrap_or_default();
    Ok(Json(
        json!({"order": o, "status_text": trader_domain::user_status_with_fill(o.state, o.fill.is_some()), "events": ev}),
    ))
}

async fn approve_order(
    State(st): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    service::order_for_user(&st, &u, id).await?;
    let o = st
        .engine
        .approve(id, u.id)
        .await
        .map_err(ServiceError::from)?;
    Ok(Json(json!({"order": o})))
}

async fn cancel_order(
    State(st): State<AppState>,
    AuthUser(u): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    service::order_for_user(&st, &u, id).await?;
    let o = st
        .engine
        .cancel(id, u.id)
        .await
        .map_err(ServiceError::from)?;
    Ok(Json(json!({"order": o})))
}

async fn get_transaction(
    State(st): State<AppState>,
    AuthUser(u): AuthUser,
    Path(sig): Path<String>,
) -> ApiResult<Json<Value>> {
    if sig.len() > 100 {
        return Err(bad("invalid signature"));
    }
    let o = st
        .orders
        .find_by_signature(&sig)
        .await
        .map_err(|e| ServiceError::Internal(e.to_string()))?
        .filter(|o| o.user_id == u.id)
        .ok_or(ServiceError::NotFound)?;
    Ok(Json(
        json!({"signature": sig, "order_id": o.id, "state": o.state, "fill": o.fill, "network": o.intent.network}),
    ))
}

#[derive(Deserialize)]
struct TrackBody {
    address: String,
    label: Option<String>,
}

async fn list_tracked(State(st): State<AppState>, AuthUser(u): AuthUser) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({"tracked": st.app.list_tracked(u.id).await.map_err(ServiceError::from)?, "alerts": "not implemented (P1): watchlist only"}),
    ))
}

async fn add_tracked(
    State(st): State<AppState>,
    AuthUser(u): AuthUser,
    Json(b): Json<TrackBody>,
) -> ApiResult<StatusCode> {
    let pk = service::parse_mint_arg(&b.address)?;
    let label = b.label.map(|l| l.chars().take(32).collect());
    st.app
        .add_tracked(&TrackedWallet {
            user_id: u.id,
            address: pk.to_string(),
            label,
            created_at: chrono::Utc::now(),
        })
        .await
        .map_err(|e| match e {
            crate::appstore::AppStoreError::Conflict(m) => {
                ApiError(StatusCode::CONFLICT, "limit", m)
            }
            o => ApiError::from(ServiceError::from(o)),
        })?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_tracked(
    State(st): State<AppState>,
    AuthUser(u): AuthUser,
    Json(b): Json<TrackBody>,
) -> ApiResult<StatusCode> {
    st.app
        .remove_tracked(u.id, &b.address)
        .await
        .map_err(ServiceError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn not_implemented() -> ApiError {
    ApiError(
        StatusCode::NOT_IMPLEMENTED,
        "not_implemented",
        "feature not implemented in this build (see docs/KNOWN_LIMITATIONS.md)".into(),
    )
}

async fn get_settings(State(st): State<AppState>, AuthUser(u): AuthUser) -> ApiResult<Json<Value>> {
    let s = service::settings(&st, &u).await?;
    let w = service::default_wallet(&st, &u).await?;
    let policy = match &w {
        Some(w) => Some(
            st.app
                .get_policy(w.id)
                .await
                .map_err(ServiceError::from)?
                .unwrap_or_else(|| crate::providers::default_policy(st.cfg.mode, &st.senders)),
        ),
        None => None,
    };
    Ok(Json(
        json!({"settings": s, "policy": policy, "available_senders": st.senders}),
    ))
}

#[derive(Deserialize)]
struct SettingsBody {
    settings: Option<UserSettings>,
    paused: Option<bool>,
}

async fn set_settings(
    State(st): State<AppState>,
    AuthUser(u): AuthUser,
    Json(b): Json<SettingsBody>,
) -> ApiResult<Json<Value>> {
    if let Some(s) = &b.settings {
        s.validate().map_err(bad)?;
        st.app
            .set_settings(u.id, s)
            .await
            .map_err(ServiceError::from)?;
    }
    if let Some(p) = b.paused {
        let w = wallet_for(&st, &u).await?;
        let mut pol = st
            .app
            .get_policy(w.id)
            .await
            .map_err(ServiceError::from)?
            .unwrap_or_else(|| crate::providers::default_policy(st.cfg.mode, &st.senders));
        pol.paused = p;
        st.app
            .set_policy(w.id, &pol)
            .await
            .map_err(ServiceError::from)?;
    }
    st.app
        .audit(Some(u.id), "settings_changed", json!({"via": "api"}))
        .await
        .ok();
    get_settings(State(st), AuthUser(u)).await
}

async fn events(
    State(st): State<AppState>,
    AuthUser(u): AuthUser,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = st.updates.subscribe();
    let uid = u.id;
    let stream = futures::stream::unfold(rx, move |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(upd) if upd.user_id == uid => {
                    let ev = Event::default()
                        .event("order")
                        .json_data(&upd)
                        .unwrap_or_default();
                    return Some((Ok(ev), rx));
                }
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

async fn dev_airdrop(State(st): State<AppState>, AuthUser(u): AuthUser) -> ApiResult<Json<Value>> {
    let l = service::airdrop_mock(&st, &u).await?;
    Ok(Json(
        json!({"requested_lamports": l, "note": "test funds only"}),
    ))
}

async fn telegram_webhook(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let Some(expected) = st.cfg.webhook_secret.as_deref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let got = headers
        .get("x-telegram-bot-api-secret-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !secret_eq(got, expected) {
        st.metrics
            .inc("telegram_webhook_rejected_total", &[("reason", "secret")]);
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(upd) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(update_id) = upd.get("update_id").and_then(|v| v.as_i64()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    // Reserve queue capacity BEFORE marking seen, so a full queue makes
    // Telegram retry instead of silently dropping the update.
    let permit = match st.tg_queue.try_reserve() {
        Ok(p) => p,
        Err(_) => {
            st.metrics
                .inc("telegram_webhook_rejected_total", &[("reason", "busy")]);
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    match st.app.mark_update(update_id).await {
        Ok(true) => {
            permit.send(upd);
            st.metrics.inc("telegram_updates_total", &[]);
        }
        Ok(false) => st.metrics.inc("telegram_updates_duplicate_total", &[]),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
    StatusCode::OK.into_response()
}
